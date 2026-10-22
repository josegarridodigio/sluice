//! The merge queue end to end: a real daemon with `QueueExtension`, a
//! repository with agent worktrees, and a local bare remote.

#![allow(
    clippy::many_single_char_names,
    reason = "short names keep the scenarios readable"
)]

mod common;

use common::{Daemon, Fixture, ready, user_config};
use sluice_gate_proto::{
    ApproveVia, ErrorCode, QueueJobState as J, QueueTrainState as T, ReconcileDecision,
    ServerMessage,
};

const GATE_TRUE: &str = "version = 1\n[gates.test]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n";

fn error_code(m: &ServerMessage) -> ErrorCode {
    match m {
        ServerMessage::Error { code, .. } => *code,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflict_ready_approval_and_landing() {
    let fx = Fixture::new(GATE_TRUE);
    let base = fx.bare_ref("refs/heads/main").unwrap();
    let (wa, a) = fx.branch("feat/a", "main", &[("a.txt", "from A\n"), ("x.txt", "x\n")]);
    let (wb, b) = fx.branch("feat/b", "main", &[("a.txt", "from B\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    assert_eq!((ja.remote.as_str(), ja.target.as_str()), ("origin", "main"));
    let jb = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    // Same branch twice → duplicate-branch.
    let dup = d.enqueue(&fx, &wa, "feat/a", &a, None).await;
    assert_eq!(error_code(&dup), ErrorCode::DuplicateBranch);

    let train = d.wait_train("ready train", ready).await;
    let st = d.status().await;
    let vb = st.jobs.iter().find(|j| j.job_id == jb.job_id).unwrap();
    assert_eq!(vb.state, J::Conflict);
    assert_eq!(vb.reason.as_deref(), Some("conflict"));
    assert_eq!(vb.detail.as_ref().unwrap()["paths"][0], "a.txt");
    assert_eq!(train.members, vec![ja.job_id.clone(), jb.job_id.clone()]);
    assert_eq!(train.gates.len(), 1);
    assert_eq!(train.gates[0].status, "pass");
    // Nothing pushed without approval.
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), base);
    let hash = train.train_hash.clone().unwrap();

    let wrong = d
        .approve(&fx, &train.train_id, "0000", ApproveVia::Cli)
        .await;
    assert_eq!(error_code(&wrong), ErrorCode::StalePlan);
    let mcp = d
        .approve(&fx, &train.train_id, &hash, ApproveVia::Mcp)
        .await;
    assert_eq!(error_code(&mcp), ErrorCode::NotAllowed);
    match d
        .approve(&fx, &train.train_id, &hash, ApproveVia::Cli)
        .await
    {
        ServerMessage::QueueApproved { train, .. } => {
            assert_eq!(train.state, T::Approved);
            assert_eq!(train.approved_via, Some(ApproveVia::Cli));
        }
        other => panic!("{other:?}"),
    }
    let landed = d
        .wait_train("landed", |t| {
            t.train_id == train.train_id && t.state == T::Landed
        })
        .await;
    let s = landed.merge_sha.unwrap();
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), s);
    assert_eq!(fx.bare_ref(&format!("refs/sluice/landed/{s}")).unwrap(), s);
    assert!(
        fx.local_ref(&format!("refs/sluice/pending/{}", train.train_id))
            .is_none()
    );
    assert_eq!(d.wait_job(&ja.job_id, J::Landed).await.state, J::Landed);
    // The merge commit has base and A as parents; B is untouched.
    let parents = fx.git(&["fetch", "-q", "origin"]);
    let _ = parents;
    let p = fx.git(&["rev-list", "--parents", "-n", "1", &s]);
    assert!(p.contains(&base) && p.contains(&a), "{p}");
    assert!(!d.state.join("merge").join(&train.train_id).exists());
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_approval_when_allowed() {
    let fx = Fixture::new(GATE_TRUE);
    // The member edits `.sluice.toml`: flagged, but gates come from the base.
    let cfg = format!("{GATE_TRUE}# edited by the agent\n");
    let (wa, a) = fx.branch(
        "feat/a",
        "main",
        &[("n.txt", "n\n"), (".sluice.toml", &cfg)],
    );
    let d = Daemon::start(&fx, user_config(true)).await;
    let j = d
        .enqueue_ok(&fx, &wa, "feat/a", &a, Some("origin/main"))
        .await;
    let t = d.wait_train("ready", ready).await;
    let job = d.wait_job(&j.job_id, J::Ready).await;
    assert_eq!(job.detail.unwrap()["config_changed"], true);
    let summary = match d
        .ask(&sluice_gate_proto::ClientMessage::Status { id: 9 })
        .await
    {
        ServerMessage::Status { status, .. } => status.queue.unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (summary.active, summary.ready, summary.needs_reconcile),
        (1, 1, 0)
    );
    // Single member, fast-forward: S is the agent's own commit.
    assert_eq!(t.merge_sha.as_deref(), Some(a.as_str()));
    let r = d
        .approve(
            &fx,
            &t.train_id,
            t.train_hash.as_deref().unwrap(),
            ApproveVia::Mcp,
        )
        .await;
    assert!(matches!(r, ServerMessage::QueueApproved { .. }), "{r:?}");
    d.wait_train("landed", |x| x.state == T::Landed).await;
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), a);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn marked_tree_needs_no_run() {
    let fx = Fixture::new("");
    let counter = fx.root.join("counter");
    let toml = format!(
        "version = 1\n[gates.test]\ncmd = \"echo run >> {}\"\nhooks = [\"pre-push\"]\n",
        counter.display()
    );
    std::fs::write(fx.repo.join(".sluice.toml"), &toml).unwrap();
    fx.git(&["commit", "-q", "-am", "gate"]);
    fx.git(&["push", "-q", "origin", "main"]);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j1 = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t1 = d.wait_train("first ready", ready).await;
    assert_eq!(t1.gates[0].status, "pass");
    assert_eq!(
        std::fs::read_to_string(&counter).unwrap().lines().count(),
        1
    );
    // Cancel (train dissolved), enqueue again: the combined tree is marked.
    assert!(matches!(
        d.cancel(&j1.job_id).await,
        ServerMessage::QueueJob { .. }
    ));
    d.wait_train("dissolved", |t| {
        t.train_id == t1.train_id && t.state == T::Dissolved
    })
    .await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t2 = d
        .wait_train("second ready", |t| {
            t.train_id != t1.train_id && t.state == T::Ready
        })
        .await;
    assert_eq!(t2.gates[0].status, "cached");
    assert!(t2.gates[0].job_id.is_none());
    assert_eq!(
        std::fs::read_to_string(&counter).unwrap().lines().count(),
        1
    );
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn base_moved_dissolves_and_voids_the_old_plan() {
    let fx = Fixture::new(GATE_TRUE);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t1 = d.wait_train("ready", ready).await;
    let old_hash = t1.train_hash.clone().unwrap();
    let moved = fx.move_remote_main("other.txt");
    d.wait_train("dissolved", |t| {
        t.train_id == t1.train_id && t.state == T::Dissolved
    })
    .await;
    let t2 = d
        .wait_train("rebuilt", |t| {
            t.train_id != t1.train_id && t.state == T::Ready
        })
        .await;
    assert_eq!(t2.base_sha, moved);
    assert_ne!(t2.train_hash.as_deref(), Some(old_hash.as_str()));
    assert_eq!(t2.members, vec![j.job_id.clone()]);
    let r = d
        .approve(&fx, &t1.train_id, &old_hash, ApproveVia::Cli)
        .await;
    assert_eq!(error_code(&r), ErrorCode::StalePlan);
    let job = d
        .status()
        .await
        .jobs
        .into_iter()
        .find(|x| x.job_id == j.job_id)
        .unwrap();
    assert_eq!(job.attempts, 2);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declining_pre_push_hook_fails_the_jobs() {
    let fx = Fixture::new(GATE_TRUE);
    let base = fx.bare_ref("refs/heads/main").unwrap();
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    fx.write_hook("pre-push", "echo 'policy says no' >&2\nexit 1");
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    let job = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(job.reason.as_deref(), Some("pre_push_hook"));
    let out = job.detail.unwrap()["output"].as_str().unwrap().to_owned();
    assert!(out.contains("policy says no"), "{out}");
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), base);
    let st = d.status().await;
    assert_eq!(
        st.trains
            .iter()
            .find(|x| x.train_id == t.train_id)
            .unwrap()
            .state,
        T::Dissolved
    );
    assert!(
        fx.local_ref(&format!("refs/sluice/pending/{}", t.train_id))
            .is_none()
    );
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hung_pre_push_hook_times_out_and_reconciles_to_ready() {
    let toml = format!("{GATE_TRUE}[queue]\npush_timeout = \"2s\"\n");
    let fx = Fixture::new(&toml);
    let base = fx.bare_ref("refs/heads/main").unwrap();
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let fifo = fx.root.join("fifo");
    let st = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(st.success());
    fx.write_hook("pre-push", &format!("cat '{}' >/dev/null", fifo.display()));
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    let parked = d
        .wait_train("needs_reconcile", |x| {
            x.train_id == t.train_id && x.state == T::NeedsReconcile
        })
        .await;
    assert_eq!(parked.note.as_deref(), Some("push_timeout"));
    assert_eq!(
        d.wait_job(&j.job_id, J::NeedsReconcile).await.state,
        J::NeedsReconcile
    );
    assert!(
        fx.local_ref(&format!("refs/sluice/pending/{}", t.train_id))
            .is_some()
    );
    let res = d.reconcile().await;
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].decision, ReconcileDecision::Ready, "{res:?}");
    let again = d
        .wait_train("ready again", |x| {
            x.train_id == t.train_id && x.state == T::Ready
        })
        .await;
    assert!(again.approved_via.is_none(), "approval voided");
    assert_eq!(again.train_hash, t.train_hash);
    assert!(
        fx.local_ref(&format!("refs/sluice/pending/{}", t.train_id))
            .is_none()
    );
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), base);
    // Nothing left to reconcile.
    assert_eq!(d.reconcile().await.len(), 0);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn marker_without_push_is_parked_at_start_then_ready() {
    let fx = Fixture::new(GATE_TRUE);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    let state = d.state.clone();
    d.shutdown().await;
    // Simulate a crash between the marker transaction and the push spawn.
    {
        let db = rusqlite::Connection::open(state.join("sluice.db")).unwrap();
        db.execute(
            "UPDATE queue_trains SET state = 'pushing' WHERE id = ?1",
            [&t.train_id],
        )
        .unwrap();
        db.execute(
            "UPDATE queue_jobs SET state = 'pushing' WHERE id = ?1",
            [&j.job_id],
        )
        .unwrap();
        db.execute(
            "INSERT INTO queue_push_markers(train_id, sha, base_sha, remote, endpoint_hash, \
             push_refs, audit_ref, pin_ref, claim_token, written_at) \
             SELECT id, merge_sha, base_sha, remote, endpoint_hash, '[]', NULL, \
             'refs/sluice/pending/' || id, claim_token, 0 FROM queue_trains WHERE id = ?1",
            [&t.train_id],
        )
        .unwrap();
    }
    let s = t.merge_sha.clone().unwrap();
    fx.git(&[
        "update-ref",
        &format!("refs/sluice/pending/{}", t.train_id),
        &s,
    ]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let st = d.status().await;
    let tr = st.trains.iter().find(|x| x.train_id == t.train_id).unwrap();
    assert_eq!(tr.state, T::NeedsReconcile);
    assert_eq!(
        st.jobs.iter().find(|x| x.job_id == j.job_id).unwrap().state,
        J::NeedsReconcile
    );
    // A cancel while parked is only recorded.
    match d.cancel(&j.job_id).await {
        ServerMessage::QueueJob { job, .. } => {
            assert!(job.cancel_requested);
            assert_eq!(job.state, J::NeedsReconcile);
        }
        other => panic!("{other:?}"),
    }
    let res = d.reconcile().await;
    // Nothing landed and a cancel was pending: row 7.
    assert_eq!(res[0].decision, ReconcileDecision::Cancelled, "{res:?}");
    assert_eq!(
        d.wait_job(&j.job_id, J::Cancelled).await.state,
        J::Cancelled
    );
    assert!(
        fx.local_ref(&format!("refs/sluice/pending/{}", t.train_id))
            .is_none()
    );
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn marker_without_push_reconciles_to_ready() {
    let fx = Fixture::new(GATE_TRUE);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    let state = d.state.clone();
    d.shutdown().await;
    {
        let db = rusqlite::Connection::open(state.join("sluice.db")).unwrap();
        db.execute(
            "UPDATE queue_trains SET state = 'pushing' WHERE id = ?1",
            [&t.train_id],
        )
        .unwrap();
        db.execute(
            "UPDATE queue_jobs SET state = 'pushing' WHERE id = ?1",
            [&j.job_id],
        )
        .unwrap();
        db.execute(
            "INSERT INTO queue_push_markers(train_id, sha, base_sha, remote, endpoint_hash, \
             push_refs, audit_ref, pin_ref, claim_token, written_at) \
             SELECT id, merge_sha, base_sha, remote, endpoint_hash, '[]', NULL, \
             'refs/sluice/pending/' || id, claim_token, 0 FROM queue_trains WHERE id = ?1",
            [&t.train_id],
        )
        .unwrap();
    }
    let d = Daemon::start(&fx, user_config(false)).await;
    d.wait_train("parked", |x| {
        x.train_id == t.train_id && x.state == T::NeedsReconcile
    })
    .await;
    let res = d.reconcile().await;
    assert_eq!(res[0].decision, ReconcileDecision::Ready, "{res:?}");
    let back = d
        .wait_train("ready", |x| x.train_id == t.train_id && x.state == T::Ready)
        .await;
    // Re-approval pushes it.
    d.approve(
        &fx,
        &back.train_id,
        back.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    d.wait_train("landed", |x| {
        x.train_id == t.train_id && x.state == T::Landed
    })
    .await;
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), a);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_target_lands_by_ref_update() {
    let fx = Fixture::new(GATE_TRUE);
    let remote_main = fx.bare_ref("refs/heads/main").unwrap();
    fx.git(&["branch", "integration", "main"]);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let (wb, b) = fx.branch("feat/b", "main", &[("m.txt", "m\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    // `main` is checked out in the main worktree.
    let r = d
        .enqueue(&fx, &wb, "feat/b", &b, Some("refs/heads/main"))
        .await;
    match &r {
        ServerMessage::Error { code, message, .. } => {
            assert_eq!(*code, ErrorCode::NotAllowed);
            assert!(message.starts_with("target_checked_out"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    let tag = d.enqueue(&fx, &wb, "feat/b", &b, Some("HEAD")).await;
    assert_eq!(error_code(&tag), ErrorCode::BadRequest);
    let j = d
        .enqueue_ok(&fx, &wa, "feat/a", &a, Some("integration"))
        .await;
    assert_eq!((j.remote.as_str(), j.target.as_str()), ("", "integration"));
    let t = d.wait_train("ready", ready).await;
    assert_eq!(t.remote, "");
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    let landed = d.wait_train("landed", |x| x.state == T::Landed).await;
    let s = landed.merge_sha.unwrap();
    assert_eq!(fx.local_ref("refs/heads/integration").unwrap(), s);
    assert_eq!(fx.local_ref(&format!("refs/sluice/landed/{s}")).unwrap(), s);
    assert_eq!(
        fx.bare_ref("refs/heads/main").unwrap(),
        remote_main,
        "no network"
    );
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_member_splits_the_train() {
    let toml = "version = 1\n[gates.test]\ncmd = \"test ! -e bad.txt\"\nhooks = [\"pre-push\"]\n";
    let fx = Fixture::new(toml);
    let (wa, a) = fx.branch("feat/a", "main", &[("ok.txt", "ok\n")]);
    let (wb, b) = fx.branch("feat/b", "main", &[("bad.txt", "bad\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let jb = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    // The pair fails, the members split, A alone passes.
    let ta = d
        .wait_train("A alone ready", |t| {
            t.state == T::Ready && t.members == vec![ja.job_id.clone()]
        })
        .await;
    let st = d.status().await;
    let first = st.trains.iter().find(|t| t.members.len() == 2).unwrap();
    assert_eq!(first.state, T::Dissolved);
    assert_eq!(first.gates[0].status, "fail");
    let vb = st.jobs.iter().find(|j| j.job_id == jb.job_id).unwrap();
    assert_eq!(vb.state, J::Queued);
    assert_eq!(vb.attempts, 2);
    d.approve(
        &fx,
        &ta.train_id,
        ta.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    d.wait_job(&ja.job_id, J::Landed).await;
    let fb = d.wait_job(&jb.job_id, J::Failed).await;
    assert_eq!(fb.reason.as_deref(), Some("gate_failed"));
    let detail = fb.detail.unwrap();
    assert_eq!(detail["gate"], "test");
    assert!(detail["log_path"].is_string());
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_queue_answers_not_supported() {
    let fx = Fixture::new(GATE_TRUE);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let mut user = user_config(false);
    user.queue.enabled = false;
    let d = Daemon::start(&fx, user).await;
    let r = d.enqueue(&fx, &wa, "feat/a", &a, None).await;
    assert_eq!(error_code(&r), ErrorCode::NotSupported);
    d.shutdown().await;
}

// ---------------------------------------------------------------- review fixes

/// Opens the daemon's database next to the running daemon.
fn db(d: &Daemon) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(d.state.join("sluice.db")).unwrap();
    db.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    db
}

async fn enqueue_via_mcp(
    d: &Daemon,
    fx: &Fixture,
    wt: &std::path::Path,
    branch: &str,
    sha: &str,
) -> sluice_gate_proto::QueueJobView {
    let msg = sluice_gate_proto::ClientMessage::QueueEnqueue {
        id: 1,
        request: sluice_gate_proto::EnqueueRequest {
            repo: fx.common.clone(),
            worktree: wt.to_path_buf(),
            branch: branch.into(),
            head_sha: sha.into(),
            base: None,
            caller: sluice_gate_proto::Caller::Agent,
            session: None,
            env: fx.client_env(),
            via: Some(ApproveVia::Mcp),
        },
    };
    match d.ask(&msg).await {
        ServerMessage::QueueJob { job, .. } => job,
        other => panic!("enqueue failed: {other:?}"),
    }
}

/// Findings 5, 10, 11, 13, 15: B conflicts and is cancelled while A is
/// ready → A stays ready and its approval lands; a gate's untracked output
/// does not dissolve the approved train; `FETCH_HEAD` is untouched; the MCP
/// enqueue is audited as `mcp`; credentials are dropped once rows finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_conflict_keeps_the_ready_train() {
    let toml =
        "version = 1\n[gates.test]\ncmd = \"touch gate-output.txt\"\nhooks = [\"pre-push\"]\n";
    let fx = Fixture::new(toml);
    let fetch_head = fx.common.join("FETCH_HEAD");
    std::fs::write(&fetch_head, "sentinel\n").unwrap();
    let (wa, a) = fx.branch("feat/a", "main", &[("a.txt", "from A\n")]);
    let (wb, b) = fx.branch("feat/b", "main", &[("a.txt", "from B\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let jb = enqueue_via_mcp(&d, &fx, &wb, "feat/b", &b).await;
    let t = d
        .wait_train("ready pair", |t| {
            ready(t) && t.members == vec![ja.job_id.clone(), jb.job_id.clone()]
        })
        .await;
    let vb = d.wait_job(&jb.job_id, J::Conflict).await;
    assert_eq!(vb.train_id, None, "a conflicting job leaves its train");
    assert!(matches!(
        d.cancel(&jb.job_id).await,
        ServerMessage::QueueJob { .. }
    ));
    // Many ticks: the ready train is not dissolved by B's cancel.
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let st = d.status().await;
    let same = st.trains.iter().find(|x| x.train_id == t.train_id).unwrap();
    assert_eq!(same.state, T::Ready, "{same:?}");
    assert_eq!(same.train_hash, t.train_hash);
    let r = d
        .approve(
            &fx,
            &t.train_id,
            t.train_hash.as_deref().unwrap(),
            ApproveVia::Cli,
        )
        .await;
    assert!(matches!(r, ServerMessage::QueueApproved { .. }), "{r:?}");
    let landed = d
        .wait_train("landed", |x| {
            x.train_id == t.train_id && x.state == T::Landed
        })
        .await;
    assert_eq!(
        fx.bare_ref("refs/heads/main"),
        landed.merge_sha,
        "the gate's untracked output did not dissolve the train"
    );
    assert_eq!(
        std::fs::read_to_string(&fetch_head).unwrap(),
        "sentinel\n",
        "the queue's fetches never write FETCH_HEAD"
    );
    let db = db(&d);
    let actor: String = db
        .query_row(
            "SELECT actor FROM queue_events WHERE job_id = ?1 AND from_state IS NULL",
            [&jb.job_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actor, "mcp");
    let cancel_actor: String = db
        .query_row(
            "SELECT actor FROM queue_events WHERE job_id = ?1 AND to_state = 'cancelled'",
            [&jb.job_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cancel_actor, "cli");
    let push_env: Option<String> = db
        .query_row(
            "SELECT push_env FROM queue_trains WHERE id = ?1",
            [&t.train_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(push_env, None, "push_env cleared on landed");
    for j in [&ja.job_id, &jb.job_id] {
        let env: Option<String> = db
            .query_row("SELECT env FROM queue_jobs WHERE id = ?1", [j], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(env, None, "job env cleared once {j} finished");
    }
    d.shutdown().await;
}

/// Finding 1: a remote whose transport stalls is cut by `[queue]
/// network_timeout` (its group is killed) and never blocks another
/// repository's train nor its push.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_remote_times_out_without_blocking_other_repositories() {
    let stalled = Fixture::new(&format!("{GATE_TRUE}[queue]\nnetwork_timeout = \"10s\"\n"));
    let (ws, s) = stalled.branch("feat/s", "main", &[("s.txt", "s\n")]);
    // Every transport call of this repository hangs (`ssh` that sleeps).
    stalled.git(&["remote", "set-url", "origin", "ssh://stall.invalid/r.git"]);
    stalled.git(&["config", "core.sshCommand", "sleep 600 #"]);
    let fx = Fixture::new(GATE_TRUE);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    // `origin/main` is known locally: the enqueue needs no network.
    let js = d
        .enqueue_ok(&stalled, &ws, "feat/s", &s, Some("origin/main"))
        .await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("other repo ready", ready).await;
    let r = d
        .approve(
            &fx,
            &t.train_id,
            t.train_hash.as_deref().unwrap(),
            ApproveVia::Cli,
        )
        .await;
    assert!(matches!(r, ServerMessage::QueueApproved { .. }), "{r:?}");
    d.wait_job(&ja.job_id, J::Landed).await;
    // The stalled fetch is still running: nothing recorded yet.
    let st = d.status().await;
    let vs = st.jobs.iter().find(|j| j.job_id == js.job_id).unwrap();
    assert_eq!(vs.state, J::Queued);
    assert_eq!(vs.reason, None, "landed while the other fetch stalled");
    let vs = d
        .wait_for("timed-out fetch recorded", |s| {
            s.jobs
                .iter()
                .any(|j| j.job_id == js.job_id && j.reason.is_some())
        })
        .await
        .jobs
        .into_iter()
        .find(|j| j.job_id == js.job_id)
        .unwrap();
    assert_eq!(vs.reason.as_deref(), Some("base_unavailable"));
    let err = vs.detail.unwrap()["error"].as_str().unwrap().to_owned();
    assert!(err.contains("timed out after 10s"), "{err}");
    d.shutdown().await;
}

/// Finding 3: fetch from A, push to B (triangular) lands on B without
/// looping; a push URL changed after `ready` dissolves the approved train
/// with `endpoint_changed` and pushes to neither URL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines, reason = "two scenarios on one fixture")]
async fn the_push_url_is_the_endpoint() {
    let fx = Fixture::new(GATE_TRUE);
    let a_main = fx.bare_ref("refs/heads/main").unwrap();
    // B: a copy of A with one more commit on main.
    let b = fx.root.join("b.git");
    let bare = fx.bare.display().to_string();
    common::git_in(
        &fx.root,
        &fx.env,
        &["clone", "-q", "--bare", &bare, b.to_str().unwrap()],
    );
    let scratch = fx.root.join("b-work");
    common::git_in(
        &fx.root,
        &fx.env,
        &[
            "clone",
            "-q",
            b.to_str().unwrap(),
            scratch.to_str().unwrap(),
        ],
    );
    std::fs::write(scratch.join("b-only.txt"), "b\n").unwrap();
    common::git_in(&scratch, &fx.env, &["add", "-A"]);
    common::git_in(&scratch, &fx.env, &["commit", "-q", "-m", "b only"]);
    common::git_in(&scratch, &fx.env, &["push", "-q", "origin", "HEAD:main"]);
    let b_ref = |r: &str| {
        common::git_in(
            &fx.root,
            &fx.env,
            &["--git-dir", b.to_str().unwrap(), "rev-parse", r],
        )
    };
    let b_main = b_ref("refs/heads/main");
    fx.git(&["remote", "set-url", "--push", "origin", b.to_str().unwrap()]);

    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let ja = d
        .enqueue_ok(&fx, &wa, "feat/a", &a, Some("origin/main"))
        .await;
    let t = d.wait_train("ready on B", ready).await;
    assert_eq!(t.base_sha, b_main, "the base comes from the push URL");
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    let landed = d
        .wait_train("landed on B", |x| {
            x.train_id == t.train_id && x.state == T::Landed
        })
        .await;
    assert_eq!(Some(b_ref("refs/heads/main")), landed.merge_sha);
    assert_eq!(
        fx.bare_ref("refs/heads/main").unwrap(),
        a_main,
        "A untouched"
    );
    assert_eq!(d.wait_job(&ja.job_id, J::Landed).await.state, J::Landed);
    let trains = d.status().await.trains;
    assert_eq!(trains.len(), 1, "no loop: {trains:?}");

    // Push URL changed after ready: never pushed to B nor to C.
    let b_now = b_ref("refs/heads/main");
    let c = fx.root.join("c.git");
    common::git_in(
        &fx.root,
        &fx.env,
        &[
            "clone",
            "-q",
            "--bare",
            b.to_str().unwrap(),
            c.to_str().unwrap(),
        ],
    );
    let (wb, bsha) = fx.branch("feat/b", "main", &[("m.txt", "m\n")]);
    d.enqueue_ok(&fx, &wb, "feat/b", &bsha, Some("origin/main"))
        .await;
    let t2 = d
        .wait_train("second ready", |x| x.train_id != t.train_id && ready(x))
        .await;
    fx.git(&["remote", "set-url", "--push", "origin", c.to_str().unwrap()]);
    let r = d
        .approve(
            &fx,
            &t2.train_id,
            t2.train_hash.as_deref().unwrap(),
            ApproveVia::Cli,
        )
        .await;
    assert!(matches!(r, ServerMessage::QueueApproved { .. }), "{r:?}");
    let gone = d
        .wait_train("dissolved", |x| {
            x.train_id == t2.train_id && x.state == T::Dissolved
        })
        .await;
    assert_eq!(gone.note.as_deref(), Some("endpoint_changed"));
    d.wait_train("rebuilt against C", |x| {
        x.train_id != t2.train_id && x.train_id != t.train_id && ready(x)
    })
    .await;
    assert_eq!(b_ref("refs/heads/main"), b_now, "nothing pushed to B");
    let c_main = common::git_in(
        &fx.root,
        &fx.env,
        &[
            "--git-dir",
            c.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ],
    );
    assert_eq!(c_main, b_now, "nothing pushed to C");
    d.shutdown().await;
}

/// Finding 2: an option-shaped `[queue] remote` or `--base` never reaches
/// git as an option (the witness file is never created).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn option_shaped_remotes_never_run() {
    let fx = Fixture::new(GATE_TRUE);
    let witness = fx.root.join("witness");
    let evil = format!(
        "{GATE_TRUE}[queue]\nremote = \"--upload-pack=touch {} #\"\n",
        witness.display()
    );
    let (wa, a) = fx.branch("feat/a", "main", &[(".sluice.toml", &evil)]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let base = format!("--upload-pack=touch {} #/main", witness.display());
    let r = d.enqueue(&fx, &wa, "feat/a", &a, Some(&base)).await;
    assert_eq!(error_code(&r), ErrorCode::BadRequest, "{r:?}");
    // The head's `.sluice.toml` is invalid (the remote starts with '-'):
    // the built-in defaults pick `origin`, and the train is built on it.
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    assert_eq!(j.remote, "origin");
    d.wait_train("ready", ready).await;
    assert!(!witness.exists(), "the injected command ran");
    d.shutdown().await;
}

/// Findings 4 and 6: a push that cannot be spawned after the marker parks
/// the train as `needs_reconcile`; a reconcile whose decision cannot be
/// recorded keeps the pin ref and the worktree; the next one succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_spawn_failure_parks_the_train_and_reconcile_cleans_up_only_on_success() {
    let fx = Fixture::new(GATE_TRUE);
    let base = fx.bare_ref("refs/heads/main").unwrap();
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start_with(
        &fx,
        user_config(false),
        Some(fx.root.join("no-such-git-binary")),
    )
    .await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    let parked = d
        .wait_train("needs_reconcile", |x| {
            x.train_id == t.train_id && x.state == T::NeedsReconcile
        })
        .await;
    let note = parked.note.unwrap_or_default();
    assert!(note.starts_with("push_spawn_failed"), "{note}");
    let pin = format!("refs/sluice/pending/{}", t.train_id);
    let wt = d.state.join("merge").join(&t.train_id);
    assert!(fx.local_ref(&pin).is_some());
    assert!(wt.exists());

    // The decision write is refused (the row's token changed under it).
    let db = db(&d);
    let token: String = db
        .query_row(
            "SELECT claim_token FROM queue_trains WHERE id = ?1",
            [&t.train_id],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "UPDATE queue_trains SET claim_token = 'someone-else' WHERE id = ?1",
        [&t.train_id],
    )
    .unwrap();
    let res = d.reconcile().await;
    assert_eq!(res[0].decision, ReconcileDecision::Refused, "{res:?}");
    assert!(fx.local_ref(&pin).is_some(), "pin ref kept");
    assert!(wt.exists(), "worktree kept");
    db.execute(
        "UPDATE queue_trains SET claim_token = ?2 WHERE id = ?1",
        [&t.train_id, &token],
    )
    .unwrap();
    let res = d.reconcile().await;
    assert_eq!(res[0].decision, ReconcileDecision::Ready, "{res:?}");
    assert!(fx.local_ref(&pin).is_none());
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), base);
    d.shutdown().await;
}
