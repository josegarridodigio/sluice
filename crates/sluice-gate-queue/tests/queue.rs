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

/// A remote whose `pre-receive` hook refuses `main` like a protected branch.
fn protect_remote(fx: &Fixture) {
    use std::os::unix::fs::PermissionsExt;
    let hook = fx.bare.join("hooks/pre-receive");
    let text = "#!/bin/sh\necho 'remote: error: GH006: Protected branch update failed for \
                refs/heads/main.' >&2\nexit 1\n";
    std::fs::write(&hook, text).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Enqueues `branch`, waits for its train and approves it.
async fn approve_branch(d: &Daemon, fx: &Fixture, branch: &str, file: &str) -> String {
    let (wt, sha) = fx.branch(branch, "main", &[(file, "n\n")]);
    let j = d.enqueue_ok(fx, &wt, branch, &sha, None).await;
    let t = d.wait_train("ready", ready).await;
    let hash = t.train_hash.as_deref().unwrap();
    d.approve(fx, &t.train_id, hash, ApproveVia::Cli).await;
    j.job_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_protected_target_fails_as_target_protected_and_is_remembered() {
    let fx = Fixture::new(GATE_TRUE);
    let base = fx.bare_ref("refs/heads/main").unwrap();
    protect_remote(&fx);
    let d = Daemon::start(&fx, user_config(false)).await;
    let first = approve_branch(&d, &fx, "feat/a", "a1.txt").await;
    let job = d.wait_job(&first, J::Failed).await;
    assert_eq!(job.reason.as_deref(), Some("target_protected"));
    let out = job.detail.unwrap()["output"].as_str().unwrap().to_owned();
    assert!(out.contains("GH006"), "{out}");
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), base);
    // The next enqueue to the target carries the learned fact, and queues.
    let (wb, b) = fx.branch("feat/b", "main", &[("b.txt", "b\n")]);
    let second = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    let learned = second.learned_protection.expect("learned");
    assert!(learned.evidence.contains("GH006"), "{learned:?}");
    assert_ne!(learned.first_seen, "");
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_landing_push_forgets_the_learned_protection() {
    let fx = Fixture::new(GATE_TRUE);
    protect_remote(&fx);
    let d = Daemon::start(&fx, user_config(false)).await;
    let first = approve_branch(&d, &fx, "feat/a", "a1.txt").await;
    d.wait_job(&first, J::Failed).await;
    // Protection removed: the next train lands and the fact is cleared.
    std::fs::remove_file(fx.bare.join("hooks/pre-receive")).unwrap();
    let (wb, b) = fx.branch("feat/b", "main", &[("b.txt", "b\n")]);
    let second = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    assert!(second.learned_protection.is_some());
    let t = d
        .wait_train("ready", |t| ready(t) && t.members.len() == 1)
        .await;
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    d.wait_job(&second.job_id, J::Landed).await;
    let (wc, c) = fx.branch("feat/c", "main", &[("c.txt", "c\n")]);
    let third = d.enqueue_ok(&fx, &wc, "feat/c", &c, None).await;
    assert_eq!(third.learned_protection, None);
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
            trust_member_config: false,
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
    // The gate left an untracked file: no marker, said on the gate.
    let note = t.gates[0].note.as_deref().unwrap_or_default();
    assert!(note.starts_with("not cached: "), "{:?}", t.gates);
    assert_eq!(t.gates[0].status, "pass");
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
    // The head's `.sluice.toml` is invalid (the remote starts with '-'): it
    // cannot pick the default target, so an enqueue without `--base` is
    // refused, not silently sent to the built-in default.
    match d.enqueue(&fx, &wa, "feat/a", &a, None).await {
        ServerMessage::Error { code, message, .. } => {
            assert_eq!(code, ErrorCode::BadRequest);
            assert!(message.starts_with("invalid_config:"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // With an explicit base the head's config is not needed to enqueue; the
    // train then refuses the invalid merged configuration before any network
    // call or gate (a Sluice pre-push hook could not read it either).
    let j = d
        .enqueue_ok(&fx, &wa, "feat/a", &a, Some("origin/main"))
        .await;
    assert_eq!(j.remote, "origin");
    let failed = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(failed.reason.as_deref(), Some("member_config_invalid"));
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

// ------------------------------------------------- path-scoped keys (D1)

/// `.sluice.toml` with one pre-push gate scoped to `src`, counting its runs
/// in a file outside the repository.
fn scoped_toml(counter: &std::path::Path) -> String {
    format!(
        "version = 1\n[gates.test]\ncmd = \"echo run >> {}\"\nhooks = [\"pre-push\"]\n\
         paths = [\"src\"]\n",
        counter.display()
    )
}

/// Records a pass of `gate` for the clean worktree `wt` exactly as an
/// agent's `verify` would (key derived by the client-side lookup).
fn mark_verified(fx: &Fixture, wt: &std::path::Path, gate: &str) {
    use sluice_gate_core::lookup::{self, Lookup};
    use sluice_gate_core::marker::Marker;
    use sluice_gate_core::mode::CacheMode;

    let mut env = fx.env.clone();
    let run = fx.root.join("agent-run");
    std::fs::create_dir_all(&run).unwrap();
    env.insert("XDG_RUNTIME_DIR".into(), run.display().to_string());
    env.insert(
        "SLUICE_CONFIG".into(),
        fx.root.join("none.toml").display().to_string(),
    );
    let r = lookup::resolve(wt, &env, gate, &CacheMode::Enabled).unwrap();
    let Lookup::Miss { tree, key, cmd, .. } = lookup::lookup(&r, &CacheMode::Enabled, &[]) else {
        panic!("expected a miss in {}", wt.display());
    };
    r.store
        .write(&Marker {
            format: sluice_gate_core::CACHE_FORMAT_VERSION,
            key,
            tree,
            gate: gate.into(),
            cmd,
            passed_at: sluice_gate_core::dirs::now_rfc3339(),
            duration_ms: 1,
            exit_code: 0,
            job_id: "agent-job".into(),
            sluice_version: sluice_gate_core::SLUICE_VERSION.into(),
            committed_tree: None,
        })
        .unwrap();
}

fn runs(counter: &std::path::Path) -> usize {
    std::fs::read_to_string(counter).map_or(0, |s| s.lines().count())
}

/// Design section 9 case 24: a single-member train fast-forwarded to the
/// tree an agent verified hits the scoped gate; nothing runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scoped_gate_hits_the_agents_verified_tree() {
    let fx = Fixture::new("");
    let counter = fx.root.join("counter");
    std::fs::write(fx.repo.join(".sluice.toml"), scoped_toml(&counter)).unwrap();
    fx.git(&["commit", "-q", "-am", "scoped gate"]);
    fx.git(&["push", "-q", "origin", "main"]);
    let (wa, a) = fx.branch("feat/a", "main", &[("src/a.rs", "a\n")]);
    mark_verified(&fx, &wa, "test");
    let d = Daemon::start(&fx, user_config(false)).await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    assert_eq!(t.gates[0].status, "cached", "{t:#?}");
    assert!(t.gates[0].job_id.is_none());
    assert_eq!(runs(&counter), 0);
    d.shutdown().await;
}

/// Design section 9 case 25: the second member only touches files outside
/// the scope, so the combined tree hits the first member's pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_scope_member_keeps_the_scoped_hit() {
    let fx = Fixture::new("");
    let counter = fx.root.join("counter");
    std::fs::write(fx.repo.join(".sluice.toml"), scoped_toml(&counter)).unwrap();
    fx.git(&["commit", "-q", "-am", "scoped gate"]);
    fx.git(&["push", "-q", "origin", "main"]);
    let (wa, a) = fx.branch("feat/a", "main", &[("src/a.rs", "a\n")]);
    let (wb, b) = fx.branch("feat/b", "main", &[("docs/b.md", "b\n")]);
    mark_verified(&fx, &wa, "test");
    let d = Daemon::start(&fx, user_config(false)).await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let jb = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    let t = d
        .wait_train("a ready train with B", |t| {
            t.state == T::Ready && t.members.contains(&jb.job_id)
        })
        .await;
    assert_eq!(t.gates[0].status, "cached", "{t:#?}");
    assert_eq!(runs(&counter), 0, "no gate ran");
    d.shutdown().await;
}

// ------------------------------------------------------ train results (W2)

/// A gate that edits a tracked file of the merge worktree fails the train
/// with `gate_modified_tree` (it used to be dissolved by the pre-push check,
/// rebuilt and asked for approval again forever).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gate_that_edits_a_tracked_file_fails_the_train() {
    let toml =
        "version = 1\n[gates.test]\ncmd = \"echo changed >> a.txt\"\nhooks = [\"pre-push\"]\n";
    let fx = Fixture::new(toml);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let failed = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(failed.reason.as_deref(), Some("gate_modified_tree"));
    let detail = failed.detail.unwrap();
    assert_eq!(detail["gates"], serde_json::json!(["test"]));
    assert_eq!(detail["paths"], serde_json::json!(["a.txt"]));
    // No loop: the train is dissolved and nothing is built again.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let st = d.status().await;
    assert_eq!(st.trains.len(), 1, "{:#?}", st.trains);
    assert_eq!(st.trains[0].state, T::Dissolved);
    assert_eq!(st.jobs[0].state, J::Failed);
    assert!(!d.state.join("merge").join(&st.trains[0].train_id).exists());
    d.shutdown().await;
}

/// Two members whose gate edits a tracked file: the pair fails, the members
/// split, and each fails alone with the same reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_modified_tree_splits_the_train_then_fails_each_member() {
    let toml =
        "version = 1\n[gates.test]\ncmd = \"echo changed >> a.txt\"\nhooks = [\"pre-push\"]\n";
    let fx = Fixture::new(toml);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let (wb, b) = fx.branch("feat/b", "main", &[("m.txt", "m\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let jb = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    for j in [&ja, &jb] {
        let failed = d.wait_job(&j.job_id, J::Failed).await;
        assert_eq!(failed.reason.as_deref(), Some("gate_modified_tree"));
    }
    let st = d.status().await;
    let pair = st.trains.iter().find(|t| t.members.len() == 2).unwrap();
    assert_eq!(pair.state, T::Dissolved);
    assert!(
        pair.note
            .as_deref()
            .is_some_and(|n| n.ends_with("members split")),
        "{pair:?}"
    );
    assert_eq!(st.trains.len(), 3, "the pair and one solo train each");
    d.shutdown().await;
}

/// `config_changed` compares the member with the commit it forked from: a
/// branch that never touched `.sluice.toml` is not flagged because the base
/// did change it meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_changed_ignores_what_the_base_changed() {
    let fx = Fixture::new(GATE_TRUE);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    // The remote's main edits `.sluice.toml` after the branch forked.
    let scratch = fx.root.join("base-edit");
    common::git_in(
        &fx.root,
        &fx.env,
        &[
            "clone",
            "-q",
            fx.bare.to_str().unwrap(),
            scratch.to_str().unwrap(),
        ],
    );
    std::fs::write(
        scratch.join(".sluice.toml"),
        format!("{GATE_TRUE}# edited on the base\n"),
    )
    .unwrap();
    common::git_in(&scratch, &fx.env, &["add", "-A"]);
    common::git_in(&scratch, &fx.env, &["commit", "-q", "-m", "base edit"]);
    common::git_in(&scratch, &fx.env, &["push", "-q", "origin", "HEAD:main"]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    d.wait_train("ready", ready).await;
    let job = d.wait_job(&j.job_id, J::Ready).await;
    assert!(
        job.detail
            .as_ref()
            .and_then(|d| d.get("config_changed"))
            .is_none(),
        "{job:?}"
    );
    d.shutdown().await;
}

/// A gate failure keeps `config_changed` in the job's detail instead of
/// overwriting it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gate_failure_keeps_config_changed() {
    let toml = "version = 1\n[gates.test]\ncmd = \"test ! -e bad.txt\"\nhooks = [\"pre-push\"]\n";
    let fx = Fixture::new(toml);
    let edited = format!("{toml}# edited by the agent\n");
    let (wa, a) = fx.branch(
        "feat/a",
        "main",
        &[("bad.txt", "bad\n"), (".sluice.toml", &edited)],
    );
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let failed = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(failed.reason.as_deref(), Some("gate_failed"));
    let detail = failed.detail.unwrap();
    assert_eq!(detail["config_changed"], true);
    assert_eq!(detail["gate"], "test");
    d.shutdown().await;
}

/// A member whose `.sluice.toml` adds a gate for the pre-push hook cannot land
/// through a train that runs the target's configuration: the push's hook
/// would ask for a gate nobody verified. It fails at build time, before any
/// gate runs or any approval, with the gate names and the way out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_adding_a_pre_push_gate_fails_at_build_time() {
    let fx = Fixture::new(GATE_TRUE);
    let ran = fx.root.join("lint-ran");
    let edited = format!(
        "{GATE_TRUE}[gates.lint]\ncmd = \"touch {}\"\nhooks = [\"pre-push\"]\n",
        ran.display()
    );
    let (wa, a) = fx.branch(
        "feat/a",
        "main",
        &[("n.txt", "n\n"), (".sluice.toml", &edited)],
    );
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let failed = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(failed.reason.as_deref(), Some("hook_gate_unverified"));
    let detail = failed.detail.unwrap();
    assert_eq!(detail["gates"], serde_json::json!(["lint"]), "{detail}");
    assert_eq!(detail["config_changed"], true);
    let hint = detail["hint"].as_str().unwrap();
    assert!(
        hint.contains("--trust-member-config") && hint.contains(&j.job_id),
        "{hint}"
    );
    assert!(!ran.exists(), "no gate runs for such a train");
    let st = d.status().await;
    assert!(
        st.trains.iter().all(|t| t.gates.is_empty()),
        "{:#?}",
        st.trains
    );
    d.shutdown().await;
}

/// Only the member that changed the configuration fails; the other is
/// requeued (not split) and lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_other_member_of_such_a_train_is_requeued_and_lands() {
    let fx = Fixture::new(GATE_TRUE);
    let edited = format!("{GATE_TRUE}[gates.lint]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n");
    let (wa, a) = fx.branch("feat/a", "main", &[(".sluice.toml", &edited)]);
    let (wb, b) = fx.branch("feat/b", "main", &[("b.txt", "b\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let jb = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    let failed = d.wait_job(&ja.job_id, J::Failed).await;
    assert_eq!(failed.reason.as_deref(), Some("hook_gate_unverified"));
    let t = d
        .wait_train("B alone ready", |t| {
            t.state == T::Ready && t.members == vec![jb.job_id.clone()]
        })
        .await;
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    d.wait_job(&jb.job_id, J::Landed).await;
    d.shutdown().await;
}

/// A merged configuration that does not parse fails the member that changed
/// it with `member_config_invalid` and the parse error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_breaking_the_config_fails_with_member_config_invalid() {
    let fx = Fixture::new(GATE_TRUE);
    let (wa, a) = fx.branch("feat/a", "main", &[(".sluice.toml", "version = [\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    // An explicit base: without one the enqueue itself is refused
    // (`invalid_config`, the head's file cannot pick the default target).
    let j = d
        .enqueue_ok(&fx, &wa, "feat/a", &a, Some("origin/main"))
        .await;
    let failed = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(failed.reason.as_deref(), Some("member_config_invalid"));
    assert!(failed.detail.unwrap()["error"].is_string());
    d.shutdown().await;
}

/// A hung hook's last output lands in the `push_timeout` note.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_push_timeout_note_carries_the_hooks_output() {
    let toml = format!("{GATE_TRUE}[queue]\npush_timeout = \"2s\"\n");
    let fx = Fixture::new(&toml);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let fifo = fx.root.join("fifo");
    let st = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(st.success());
    fx.write_hook(
        "pre-push",
        &format!(
            "echo 'running the slow suite' >&2\ncat '{}' >/dev/null",
            fifo.display()
        ),
    );
    let d = Daemon::start(&fx, user_config(false)).await;
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
    assert!(note.starts_with("push_timeout: "), "{note}");
    assert!(note.contains("running the slow suite"), "{note}");
    d.shutdown().await;
}

/// A failing `key_inputs` command in the merge worktree is nobody's fault:
/// the job goes back to `queued` with `key_input_failed` (never `internal`),
/// and becomes `failed` after three attempts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_key_inputs_requeue_then_fail_after_three_attempts() {
    let toml = "version = 1\n[gates.test]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n\
                key_inputs = [\"false\"]\n";
    let fx = Fixture::new(toml);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    // The first attempt records the reason on the waiting job.
    d.wait_for("key_input_failed recorded", |s| {
        s.jobs
            .iter()
            .any(|x| x.job_id == j.job_id && x.reason.as_deref() == Some("key_input_failed"))
    })
    .await;
    let failed = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(failed.reason.as_deref(), Some("key_input_failed"));
    let detail = failed.detail.unwrap();
    assert_eq!(detail["failures"], 3);
    assert!(
        detail["error"].as_str().unwrap().contains("gate test"),
        "{detail}"
    );
    let st = d.status().await;
    assert_eq!(st.trains.len(), 3, "one train per attempt");
    assert!(st.trains.iter().all(|t| t.state == T::Dissolved));
    d.shutdown().await;
}

/// Dropping a train's gate runs kills them even under
/// `last_subscriber_gone = "finish"`: the failing gate ends the train, and
/// the slow one that was still running in the doomed worktree is cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_train_kills_its_other_gates_under_the_finish_policy() {
    use sluice_gate_daemon::server::LastSubscriberGone;

    let fx = Fixture::new("");
    let pidfile = fx.root.join("slow.pid");
    let toml = format!(
        "version = 1\n\
         [gates.fail]\ncmd = \"while [ ! -s {pid} ]; do sleep 0.1; done; exit 1\"\n\
         hooks = [\"pre-push\"]\nweight = 1\n\
         [gates.slow]\ncmd = \"echo $$ > {pid}; sleep 60\"\n\
         hooks = [\"pre-push\"]\nweight = 1\n",
        pid = pidfile.display()
    );
    std::fs::write(fx.repo.join(".sluice.toml"), toml).unwrap();
    fx.git(&["commit", "-q", "-am", "gates"]);
    fx.git(&["push", "-q", "origin", "main"]);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start_policy(&fx, user_config(false), None, LastSubscriberGone::Finish).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let failed = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(failed.reason.as_deref(), Some("gate_failed"));
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The slow gate sleeps 60 s: it is gone long before that only if the
    // train cancelled it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the slow gate kept running in the removed worktree"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    d.shutdown().await;
}

// ------------------------------------------------- `[queue] prepare` (W4)

/// `.sluice.toml` with one pre-push gate and a `[queue] prepare`.
fn prepare_toml(gate: &str, prepare: &str) -> String {
    format!(
        "version = 1\n[gates.test]\ncmd = \"{gate}\"\nhooks = [\"pre-push\"]\n\
         [queue]\nprepare = \"{prepare}\"\n"
    )
}

/// Commits `.sluice.toml` (and a `.gitignore` for `deps/`) on the target.
fn commit_config(fx: &Fixture, toml: &str) {
    std::fs::write(fx.repo.join(".sluice.toml"), toml).unwrap();
    std::fs::write(fx.repo.join(".gitignore"), "deps/\n").unwrap();
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "config"]);
    fx.git(&["push", "-q", "origin", "main"]);
}

/// The lines a prepare command appended to `counter` (`pwd`: the merge
/// worktree, or `<train>-base` for the base probe).
fn prepare_dirs(counter: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(counter)
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// A gate that needs a git-ignored file only `prepare` creates passes, the
/// keys (whose `key_inputs` need that file too) are derived after the
/// prepare, and the train lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_provides_the_ignored_files_the_gates_need() {
    let fx = Fixture::new("");
    let toml = "version = 1\n[gates.test]\ncmd = \"test -f deps/ready\"\n\
                hooks = [\"pre-push\"]\nkey_inputs = [\"cat deps/ready\"]\n\
                [queue]\nprepare = \"mkdir -p deps && echo ok > deps/ready\"\n";
    commit_config(&fx, toml);
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    let prepared = t.prepare.clone().expect("the train records its prepare");
    assert_eq!(prepared.status, "pass", "{t:#?}");
    assert!(prepared.job_id.is_some() && prepared.duration_ms.is_some());
    assert_eq!(t.gates[0].status, "pass", "{t:#?}");
    assert!(t.gates[0].key.is_some());
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    d.wait_job(&j.job_id, J::Landed).await;
    d.shutdown().await;
}

/// A prepare that fails on the base too is nobody's fault: the members go
/// back to `queued` together (not solo) with `prepare_failed`, `failed` after
/// three attempts, and the base is probed again on every failure (no probe
/// is ever remembered: a failure may be transient).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prepare_failing_on_the_base_requeues_the_members_then_fails_them() {
    let fx = Fixture::new("");
    let counter = fx.root.join("prepares");
    commit_config(
        &fx,
        &prepare_toml("true", &format!("pwd >> {}; exit 1", counter.display())),
    );
    let (wa, a) = fx.branch("feat/a", "main", &[("a1.txt", "a\n")]);
    let (wb, b) = fx.branch("feat/b", "main", &[("b1.txt", "b\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let jb = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    let waiting = d
        .wait_for("prepare_failed recorded", |s| {
            s.jobs
                .iter()
                .any(|x| x.state == J::Queued && x.reason.as_deref() == Some("prepare_failed"))
        })
        .await;
    let w = waiting
        .jobs
        .iter()
        .find(|x| x.reason.as_deref() == Some("prepare_failed"))
        .unwrap();
    assert_eq!(w.detail.as_ref().unwrap()["base"], true, "{w:?}");
    for j in [&ja, &jb] {
        let failed = d.wait_job(&j.job_id, J::Failed).await;
        assert_eq!(failed.reason.as_deref(), Some("prepare_failed"));
        let detail = failed.detail.unwrap();
        assert_eq!(detail["failures"], 3, "{detail}");
        assert_eq!(detail["base"], true);
        assert_eq!(detail["status"], "fail");
        assert!(detail["job_id"].is_string() && detail["log_path"].is_string());
        assert!(detail["error"].as_str().unwrap().contains("prepare"));
    }
    let st = d.status().await;
    assert_eq!(
        st.trains.len(),
        3,
        "one train per attempt: {:#?}",
        st.trains
    );
    for t in &st.trains {
        assert_eq!(t.state, T::Dissolved);
        assert_eq!(t.members.len(), 2, "requeued together, never solo");
        assert_eq!(t.prepare.as_ref().map(|p| p.status.as_str()), Some("fail"));
    }
    let dirs = prepare_dirs(&counter);
    let probes = dirs.iter().filter(|l| l.ends_with("-base")).count();
    assert_eq!(probes, 3, "no probe is remembered: {dirs:?}");
    assert_eq!(dirs.len(), 6, "three trains and three probes: {dirs:?}");
    let leftovers: Vec<_> = std::fs::read_dir(d.state.join("merge"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with("-base"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    d.shutdown().await;
}

/// A prepare that fails only with one member's change is that member's
/// fault: the pair splits, the good member reaches `ready`, the bad one fails
/// with `prepare_failed` (`base: false`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prepare_failing_only_with_a_member_splits_the_train() {
    let fx = Fixture::new("");
    commit_config(&fx, &prepare_toml("true", "test ! -e bad.txt"));
    let (wa, a) = fx.branch("feat/a", "main", &[("ok.txt", "ok\n")]);
    let (wb, b) = fx.branch("feat/b", "main", &[("bad.txt", "bad\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let jb = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    let ta = d
        .wait_train("A alone ready", |t| {
            t.state == T::Ready && t.members == vec![ja.job_id.clone()]
        })
        .await;
    assert_eq!(ta.prepare.as_ref().map(|p| p.status.as_str()), Some("pass"));
    // B's solo train is built only once A's train is no longer active (one
    // train per target at a time): land A, then B prepares on the new base.
    d.approve(
        &fx,
        &ta.train_id,
        ta.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    d.wait_job(&ja.job_id, J::Landed).await;
    let fb = d.wait_job(&jb.job_id, J::Failed).await;
    assert_eq!(fb.reason.as_deref(), Some("prepare_failed"));
    let detail = fb.detail.unwrap();
    assert_eq!(detail["base"], false, "{detail}");
    assert_eq!(detail["status"], "fail");
    assert!(detail["log_path"].is_string());
    let st = d.status().await;
    let pair = st.trains.iter().find(|t| t.members.len() == 2).unwrap();
    assert_eq!(pair.state, T::Dissolved);
    assert!(
        pair.note
            .as_deref()
            .is_some_and(|n| n.ends_with("members split")),
        "{pair:?}"
    );
    assert_eq!(
        pair.prepare.as_ref().map(|p| p.status.as_str()),
        Some("fail")
    );
    d.shutdown().await;
}

/// A prepare that leaves files the tree does not have: when only a member's
/// change makes it do so the member fails with `prepare_dirty` and the paths;
/// when the base does the same nobody is blamed and the member is requeued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prepare_that_dirties_the_tree_is_reported_with_the_paths() {
    // Dirty only with the member's file.
    let fx = Fixture::new("");
    commit_config(
        &fx,
        &prepare_toml("true", "test ! -e n.txt || echo x > made.txt"),
    );
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let failed = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(
        failed.reason.as_deref(),
        Some("prepare_dirty"),
        "{failed:?}"
    );
    let detail = failed.detail.unwrap();
    assert_eq!(detail["paths"], serde_json::json!(["made.txt"]));
    assert_eq!(detail["base"], false);
    let st = d.status().await;
    assert_eq!(st.trains.len(), 1, "{:#?}", st.trains);
    assert_eq!(
        st.trains[0].prepare.as_ref().map(|p| p.status.as_str()),
        Some("dirty")
    );
    d.shutdown().await;

    // Dirty on the base as well: an environment problem, requeued.
    let fx = Fixture::new("");
    commit_config(&fx, &prepare_toml("true", "echo x > made.txt"));
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let st = d
        .wait_for("requeued with prepare_failed", |s| {
            s.jobs
                .iter()
                .any(|x| x.reason.as_deref() == Some("prepare_failed"))
        })
        .await;
    let detail = st.jobs[0].detail.clone().unwrap();
    assert_eq!(detail["base"], true, "{detail}");
    assert_eq!(detail["paths"], serde_json::json!(["made.txt"]));
    assert!(
        detail["error"].as_str().unwrap().contains("made.txt"),
        "{detail}"
    );
    d.shutdown().await;
}

/// A train whose gates all hit the cache never runs its prepare: an already
/// verified fast-forward costs nothing, not even an installation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_train_whose_gates_all_hit_skips_the_prepare() {
    let fx = Fixture::new("");
    let gate_runs = fx.root.join("gate-runs");
    let prepares = fx.root.join("prepares");
    let toml = format!(
        "version = 1\n[gates.test]\ncmd = \"echo run >> {}\"\nhooks = [\"pre-push\"]\n\
         paths = [\"src\"]\n[queue]\nprepare = \"echo run >> {}\"\n",
        gate_runs.display(),
        prepares.display()
    );
    commit_config(&fx, &toml);
    let (wa, a) = fx.branch("feat/a", "main", &[("src/a.rs", "a\n")]);
    mark_verified(&fx, &wa, "test");
    let d = Daemon::start(&fx, user_config(false)).await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let t = d.wait_train("ready", ready).await;
    assert_eq!(t.gates[0].status, "cached", "{t:#?}");
    assert!(t.prepare.is_none(), "{t:#?}");
    assert_eq!(runs(&prepares), 0, "the prepare did not run");
    assert_eq!(runs(&gate_runs), 0, "no gate ran");
    d.shutdown().await;
}

/// Cancelling a member while the prepare runs kills it and dissolves the
/// train.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_member_kills_a_running_prepare() {
    let fx = Fixture::new("");
    let pidfile = fx.root.join("prepare.pid");
    commit_config(
        &fx,
        &prepare_toml(
            "true",
            &format!("echo $$ > {}; sleep 60", pidfile.display()),
        ),
    );
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let running = d
        .wait_train("a running prepare", |t| {
            t.prepare.as_ref().is_some_and(|p| p.status == "running")
        })
        .await;
    assert_eq!(running.state, T::Verifying);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let pid: u32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the prepare never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert!(matches!(
        d.cancel(&j.job_id).await,
        ServerMessage::QueueJob { .. }
    ));
    d.wait_job(&j.job_id, J::Cancelled).await;
    d.wait_train("dissolved", |t| {
        t.train_id == running.train_id && t.state == T::Dissolved
    })
    .await;
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the prepare kept running after its train was dissolved"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    d.shutdown().await;
}

// ---------------------------------------------------------------------------
// `--trust-member-config` (ADR-0018 section 6)
// ---------------------------------------------------------------------------

const GATE_FALSE: &str = "version = 1\n[gates.test]\ncmd = \"false\"\nhooks = [\"pre-push\"]\n";

/// A `queue-enqueue` as the given surface, with extra client environment.
async fn enqueue_trusting(
    d: &Daemon,
    fx: &Fixture,
    wt: &std::path::Path,
    (branch, sha): (&str, &str),
    via: ApproveVia,
    caller: sluice_gate_proto::Caller,
    extra_env: &[(&str, &str)],
) -> ServerMessage {
    let mut env = fx.client_env();
    for (k, v) in extra_env {
        env.insert((*k).to_owned(), (*v).to_owned());
    }
    d.ask(&sluice_gate_proto::ClientMessage::QueueEnqueue {
        id: 1,
        request: sluice_gate_proto::EnqueueRequest {
            repo: fx.common.clone(),
            worktree: wt.to_path_buf(),
            branch: branch.into(),
            head_sha: sha.into(),
            base: None,
            caller,
            session: None,
            env,
            via: Some(via),
            trust_member_config: true,
        },
    })
    .await
}

/// A `queue-retry` with `trust_member_config` as the given surface.
async fn retry_trusting(
    d: &Daemon,
    fx: &Fixture,
    job: &str,
    head: &str,
    via: ApproveVia,
    extra_env: &[(&str, &str)],
) -> ServerMessage {
    let mut env = fx.client_env();
    for (k, v) in extra_env {
        env.insert((*k).to_owned(), (*v).to_owned());
    }
    d.ask(&sluice_gate_proto::ClientMessage::QueueRetry {
        id: 6,
        job_id: job.into(),
        head_sha: head.into(),
        via: Some(via),
        trust_member_config: true,
        env,
    })
    .await
}

fn refused_as_not_allowed(m: &ServerMessage) {
    match m {
        ServerMessage::Error { code, message, .. } => {
            assert_eq!(*code, ErrorCode::NotAllowed, "{message}");
            assert!(message.contains("reserved to a human"), "{message}");
        }
        other => panic!("expected not-allowed, got {other:?}"),
    }
}

/// The flag is for humans at the CLI: refused over MCP and for an agent's
/// environment, whatever `allow_agent_approval` says.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trusting_the_members_config_is_refused_to_agents() {
    use sluice_gate_proto::Caller;
    let fx = Fixture::new(GATE_FALSE);
    let (wa, a) = fx.branch("feat/a", "main", &[("a.txt", "a\n")]);
    let (wb, b) = fx.branch("feat/b", "main", &[("b.txt", "b\n")]);
    // Agents may approve: the flag is still not theirs.
    let d = Daemon::start(&fx, user_config(true)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    d.wait_job(&ja.job_id, J::Failed).await;

    refused_as_not_allowed(
        &enqueue_trusting(
            &d,
            &fx,
            &wb,
            ("feat/b", &b),
            ApproveVia::Mcp,
            Caller::Agent,
            &[],
        )
        .await,
    );
    refused_as_not_allowed(
        &enqueue_trusting(
            &d,
            &fx,
            &wb,
            ("feat/b", &b),
            ApproveVia::Cli,
            Caller::Human,
            &[("CLAUDECODE", "1")],
        )
        .await,
    );
    refused_as_not_allowed(
        &enqueue_trusting(
            &d,
            &fx,
            &wb,
            ("feat/b", &b),
            ApproveVia::Cli,
            Caller::Agent,
            &[],
        )
        .await,
    );
    refused_as_not_allowed(&retry_trusting(&d, &fx, &ja.job_id, &a, ApproveVia::Mcp, &[]).await);
    refused_as_not_allowed(
        &retry_trusting(
            &d,
            &fx,
            &ja.job_id,
            &a,
            ApproveVia::Cli,
            &[("SLUICE_AGENT", "1")],
        )
        .await,
    );
    // Nothing was queued or requeued by the refused requests.
    let st = d.status().await;
    assert!(st.jobs.iter().all(|j| j.branch != "feat/b"), "{st:#?}");
    assert_eq!(
        st.jobs
            .iter()
            .find(|j| j.job_id == ja.job_id)
            .unwrap()
            .state,
        J::Failed
    );
    d.shutdown().await;
}

/// The dead end: the target's gate always fails, the branch replaces it. A
/// plain train fails, a trusted retry builds a solo train from the branch's
/// own file, shows the diff, lands only after a human approval, and an
/// agent (even with `allow_agent_approval`) cannot approve it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_human_can_land_a_fix_of_the_configuration_the_target_cannot_pass() {
    let fx = Fixture::new(GATE_FALSE);
    let (wa, a) = fx.branch("feat/a", "main", &[(".sluice.toml", GATE_TRUE)]);
    let d = Daemon::start(&fx, user_config(true)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let failed = d.wait_job(&ja.job_id, J::Failed).await;
    assert_eq!(failed.reason.as_deref(), Some("gate_failed"));
    assert!(
        d.status()
            .await
            .trains
            .iter()
            .all(|t| t.config_source.is_none()),
        "an ordinary train has no config source"
    );

    let r = retry_trusting(&d, &fx, &ja.job_id, &a, ApproveVia::Cli, &[]).await;
    assert!(matches!(r, ServerMessage::QueueJob { .. }), "{r:?}");
    let t = d.wait_train("trusted train ready", ready).await;
    assert_eq!(t.config_source.as_deref(), Some("member"), "{t:#?}");
    assert_eq!(t.members, vec![ja.job_id.clone()]);
    assert_eq!(t.gates.len(), 1);
    assert_eq!(t.gates[0].status, "pass", "{t:#?}");
    let diff = t.config_diff.as_deref().expect("the diff is shown");
    assert!(diff.contains("-cmd = \"false\""), "{diff}");
    assert!(diff.contains("+cmd = \"true\""), "{diff}");
    let hash = t.train_hash.clone().unwrap();

    // Not over MCP and not from an agent's terminal, `allow_agent_approval`
    // notwithstanding; the train stays ready.
    for r in [d.approve(&fx, &t.train_id, &hash, ApproveVia::Mcp).await, {
        let mut env = fx.client_env();
        env.insert("CLAUDECODE".into(), "1".into());
        d.ask(&sluice_gate_proto::ClientMessage::QueueApprove {
            id: 3,
            train_id: t.train_id.clone(),
            train_hash: hash.clone(),
            via: ApproveVia::Cli,
            env,
            pr_landing: true,
        })
        .await
    }] {
        match r {
            ServerMessage::Error { code, message, .. } => {
                assert_eq!(code, ErrorCode::NotAllowed, "{message}");
                assert!(message.contains("own .sluice.toml"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        d.wait_train("still ready", |x| x.train_id == t.train_id)
            .await
            .state,
        T::Ready
    );
    assert_eq!(
        fx.bare_ref("refs/heads/main").unwrap(),
        fx.local_ref("origin/main").unwrap()
    );

    let ok = d.approve(&fx, &t.train_id, &hash, ApproveVia::Cli).await;
    assert!(matches!(ok, ServerMessage::QueueApproved { .. }), "{ok:?}");
    d.wait_job(&ja.job_id, J::Landed).await;
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), a);
    d.shutdown().await;
}

/// The trust belongs to one `retry`: a plain retry goes back to the base's
/// configuration. The two configs name their failing gate differently, so
/// the failure says whose gates ran. A trusted train is alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plain_retry_clears_the_trust() {
    use sluice_gate_proto::Caller;
    let base = "version = 1\n[gates.basegate]\ncmd = \"false\"\nhooks = [\"pre-push\"]\n";
    let member = "version = 1\n[gates.membergate]\ncmd = \"false\"\nhooks = [\"pre-push\"]\n";
    let fx = Fixture::new(base);
    let (wa, a) = fx.branch("feat/a", "main", &[(".sluice.toml", member)]);
    let d = Daemon::start(&fx, user_config(false)).await;
    // Enqueued with the flag by a human: the member's gate runs.
    let m = enqueue_trusting(
        &d,
        &fx,
        &wa,
        ("feat/a", &a),
        ApproveVia::Cli,
        Caller::Human,
        &[],
    )
    .await;
    let ServerMessage::QueueJob { job, .. } = m else {
        panic!("{m:?}");
    };
    let failed = d.wait_job(&job.job_id, J::Failed).await;
    assert_eq!(
        failed.detail.as_ref().unwrap()["gate"],
        "membergate",
        "{failed:?}"
    );
    let st = d.status().await;
    let trusted: Vec<_> = st
        .trains
        .iter()
        .filter(|t| t.config_source.as_deref() == Some("member"))
        .collect();
    assert_eq!(trusted.len(), 1, "{st:#?}");
    assert_eq!(trusted[0].members.len(), 1);

    // A plain retry builds an ordinary train again: the base's gates judge,
    // so the member's own pre-push gate is one the train would not verify,
    // and the job is refused at build time (no trusted train this time).
    let r = d.retry(&job.job_id, &a).await;
    assert!(matches!(r, ServerMessage::QueueJob { .. }), "{r:?}");
    let st = d
        .wait_for("refused under the base's gates", |s| {
            s.jobs.iter().any(|j| {
                j.job_id == job.job_id
                    && j.state == J::Failed
                    && j.reason.as_deref() == Some("hook_gate_unverified")
            })
        })
        .await;
    let trusted = st
        .trains
        .iter()
        .filter(|t| t.config_source.as_deref() == Some("member"))
        .count();
    assert_eq!(trusted, 1, "the retry built no trusted train: {st:#?}");
    d.shutdown().await;
}

/// A member whose `.sluice.toml` does not parse fails its job once, with the
/// parse error; nothing is retried in a loop and no train is built.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalid_member_config_fails_the_trusted_job_once() {
    let fx = Fixture::new(GATE_FALSE);
    let (wa, a) = fx.branch("feat/a", "main", &[("a.txt", "a\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;
    let ja = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    d.wait_job(&ja.job_id, J::Failed).await;
    std::fs::write(wa.join(".sluice.toml"), "bogus_key = 1\n").unwrap();
    common::git_in(&wa, &fx.env, &["add", "-A"]);
    common::git_in(&wa, &fx.env, &["commit", "-q", "-m", "broken config"]);
    let head = common::git_in(&wa, &fx.env, &["rev-parse", "HEAD"]);
    let trains_before = d.status().await.trains.len();
    let r = retry_trusting(&d, &fx, &ja.job_id, &head, ApproveVia::Cli, &[]).await;
    assert!(matches!(r, ServerMessage::QueueJob { .. }), "{r:?}");
    let j = d
        .wait_for("member_config_invalid", |s| {
            s.jobs.iter().any(|j| {
                j.job_id == ja.job_id
                    && j.state == J::Failed
                    && j.reason.as_deref() == Some("member_config_invalid")
            })
        })
        .await;
    let j = j.jobs.into_iter().find(|j| j.job_id == ja.job_id).unwrap();
    let error = j.detail.as_ref().unwrap()["error"].as_str().unwrap();
    assert!(
        error.contains("invalid .sluice.toml on the branch"),
        "{error}"
    );
    // No loop: still failed, same attempts, no new train.
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    let st = d.status().await;
    let again = st.jobs.iter().find(|x| x.job_id == ja.job_id).unwrap();
    assert_eq!((again.state, again.attempts), (J::Failed, j.attempts));
    assert_eq!(st.trains.len(), trains_before, "{st:#?}");
    d.shutdown().await;
}

/// A repository whose gate needs the program `sluice-fake-tool` for its
/// `key_inputs`, installed in a directory that is not in the daemon's `PATH`
/// (ADR-0020). Returns the fixture, the tool directory and a fake login
/// shell that puts it in `PATH` (the user's toolchain, set by an rc file).
fn tool_fixture() -> (Fixture, std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;

    let toml = "version = 1\n[gates.test]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n\
                key_inputs = [\"sluice-fake-tool --version\"]\n";
    let fx = Fixture::new(toml);
    let bin = fx.root.join("toolbin");
    std::fs::create_dir_all(&bin).unwrap();
    let write_exec = |path: &std::path::Path, body: &str| {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    write_exec(&bin.join("sluice-fake-tool"), "#!/bin/sh\necho v1\n");
    let shell = fx.root.join("fake-login-shell");
    write_exec(
        &shell,
        &format!(
            "#!/bin/sh\necho 'rc noise'\nexport PATH={}:\"$PATH\"\neval \"$4\"\n",
            bin.display()
        ),
    );
    (fx, bin, shell)
}

/// The daemon's own `PATH` lacks the tool a `key_inputs` command needs but
/// the user's login environment has it: the train reaches `ready`, and the
/// status reports where the trains' tools come from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trains_find_the_tools_of_the_login_shell() {
    let (fx, bin, shell) = tool_fixture();
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start_login_shell(&fx, user_config(false), shell).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let train = d.wait_train("ready train", ready).await;
    assert_eq!(train.members, vec![j.job_id.clone()]);
    assert_eq!(train.gates[0].status, "pass");
    let tools = d.daemon_status().await.queue.unwrap().tool_env.unwrap();
    assert_eq!(tools.source, "login-shell");
    assert!(
        tools.path.unwrap().starts_with(&bin.display().to_string()),
        "the login PATH leads"
    );
    d.shutdown().await;
}

/// The same setup with `tool_env = "daemon"` fails: the switch is real, and
/// the failure is `key_input_failed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_env_daemon_does_not_use_the_login_shell() {
    let (fx, _bin, shell) = tool_fixture();
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let mut user = user_config(false);
    user.queue.tool_env = sluice_gate_core::config::ToolEnv::Daemon;
    let d = Daemon::start_login_shell(&fx, user, shell).await;
    let j = d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    d.wait_for("key_input_failed recorded", |s| {
        s.jobs
            .iter()
            .any(|x| x.job_id == j.job_id && x.reason.as_deref() == Some("key_input_failed"))
    })
    .await;
    let tools = d.daemon_status().await.queue.unwrap().tool_env.unwrap();
    assert_eq!((tools.source.as_str(), tools.reason), ("daemon", None));
    d.shutdown().await;
}

/// `[queue] tool_path` wins: the trains' `PATH` is that string, with no login
/// shell involved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_path_gives_the_trains_their_tools() {
    let (fx, bin, _shell) = tool_fixture();
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let mut user = user_config(false);
    user.queue.tool_path = Some(format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    ));
    let d = Daemon::start(&fx, user).await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    let train = d.wait_train("ready train", ready).await;
    assert_eq!(train.gates[0].status, "pass");
    let tools = d.daemon_status().await.queue.unwrap().tool_env.unwrap();
    assert_eq!(tools.source, "tool-path");
    d.shutdown().await;
}

/// A login shell that fails is not fatal: the trains fall back to the
/// daemon's environment and the status says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_login_shell_falls_back_to_the_daemon_env() {
    let fx = Fixture::new(GATE_TRUE);
    let shell = fx.root.join("failing-shell");
    std::fs::write(&shell, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let (wa, a) = fx.branch("feat/a", "main", &[("n.txt", "n\n")]);
    let d = Daemon::start_login_shell(&fx, user_config(false), shell).await;
    d.enqueue_ok(&fx, &wa, "feat/a", &a, None).await;
    d.wait_train("ready train", ready).await;
    let tools = d.daemon_status().await.queue.unwrap().tool_env.unwrap();
    assert_eq!(tools.source, "daemon");
    assert!(tools.reason.unwrap().contains("exited"), "reason");
    d.shutdown().await;
}
