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
