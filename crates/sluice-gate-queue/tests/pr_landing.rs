//! Landing through a pull request (ADR-0025) end to end: a real daemon with
//! `QueueExtension` and a local bare remote. The forge is never asked here:
//! what a client's `gh` would report arrives as `queue-pr-facts`, and the
//! daemon decides from them and from the remote's own refs.

mod common;

use std::os::unix::fs::PermissionsExt;

use common::{Daemon, EnvMap, Fixture, ready, user_config};
use sluice_gate_core::config::Land;
use sluice_gate_core::forge::{AutoMerge, PrFact, PrObservation};
use sluice_gate_proto::{
    ApproveVia, ErrorCode, QueueJobState as J, QueueStatusReport, QueueTrainState as T,
    QueueTrainView, ReconcileDecision, ReconcileResult, ServerMessage,
};

const PR_TOML: &str =
    "version = 1\n[gates.test]\ncmd = \"true\"\n[queue]\nland = \"pull-request\"\n";
const PUSH_TOML: &str = "version = 1\n[gates.test]\ncmd = \"true\"\n";

struct Waiting {
    train: String,
    s: String,
    job: String,
}

/// Enqueues `branch`, approves its train the way this version's CLI does and
/// waits for the daemon to push the train branch.
async fn awaiting(d: &Daemon, fx: &Fixture, branch: &str, file: &str) -> Waiting {
    let (wt, sha) = fx.branch(branch, "main", &[(file, "n\n")]);
    let j = d.enqueue_ok(fx, &wt, branch, &sha, None).await;
    let t = d.wait_train("ready", ready).await;
    let hash = t.train_hash.as_deref().unwrap();
    let r = d.approve(fx, &t.train_id, hash, ApproveVia::Cli).await;
    assert!(matches!(r, ServerMessage::QueueApproved { .. }), "{r:?}");
    let id = t.train_id.clone();
    let t = d
        .wait_train("awaiting merge", |x| {
            x.train_id == id && x.state == T::AwaitingMerge
        })
        .await;
    Waiting {
        train: t.train_id,
        s: t.merge_sha.unwrap(),
        job: j.job_id,
    }
}

fn branch_ref(w: &Waiting) -> String {
    format!("refs/heads/sluice/train/{}", w.train)
}

async fn tell(d: &Daemon, w: &Waiting, observation: PrObservation) -> ReconcileResult {
    d.pr_facts(vec![PrFact::of(&w.train, observation)], EnvMap::new())
        .await
        .remove(0)
}

fn train<'a>(st: &'a QueueStatusReport, w: &Waiting) -> &'a QueueTrainView {
    st.trains.iter().find(|t| t.train_id == w.train).unwrap()
}

fn merged(commit: &str) -> PrObservation {
    PrObservation::Merged {
        merge_commit: Some(commit.to_owned()),
        head: None,
    }
}

fn error_code(m: &ServerMessage) -> ErrorCode {
    match m {
        ServerMessage::Error { code, .. } => *code,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approval_pushes_the_train_branch_not_the_target_and_waits_for_the_merge() {
    let fx = Fixture::new(PR_TOML);
    let base = fx.bare_ref("refs/heads/main").unwrap();
    let d = Daemon::start(&fx, user_config(false)).await;
    let w = awaiting(&d, &fx, "feat/a", "a1.txt").await;
    // The verified commit is on the train branch; the target did not move
    // and there is no audit ref (the branch is the record).
    assert_eq!(fx.bare_ref(&branch_ref(&w)).unwrap(), w.s);
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), base);
    assert!(
        fx.bare_ref(&format!("refs/sluice/landed/{}", w.s))
            .is_none()
    );
    // The pin ref stays (it keeps S in the repository), the worktree goes.
    assert_eq!(
        fx.local_ref(&format!("refs/sluice/pending/{}", w.train))
            .unwrap(),
        w.s
    );
    assert!(!d.state.join("merge").join(&w.train).exists());
    let st = d.status().await;
    let t = train(&st, &w);
    assert_eq!(t.land.as_deref(), Some("pull-request"));
    let pr = t.pr.as_ref().expect("the marker names the branch");
    assert_eq!(pr.branch, format!("sluice/train/{}", w.train));
    assert_eq!((pr.number, pr.url.clone()), (None, None));
    assert!(pr.since.is_some());
    let job = st.jobs.iter().find(|j| j.job_id == w.job).unwrap();
    assert_eq!(job.state, J::AwaitingMerge);
    // The target is occupied: a branch queued meanwhile waits.
    let (wb, b) = fx.branch("feat/b", "main", &[("b.txt", "b\n")]);
    let queued = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let st = d.status().await;
    let other = st.jobs.iter().find(|j| j.job_id == queued.job_id).unwrap();
    assert_eq!(other.state, J::Queued, "no train is built while one waits");
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merge_of_the_verified_tree_lands_the_train() {
    let fx = Fixture::new(PR_TOML);
    let d = Daemon::start(&fx, user_config(false)).await;
    let w = awaiting(&d, &fx, "feat/a", "a1.txt").await;
    // A human merged the pull request with a fast-forward.
    fx.git(&["push", "-q", "origin", &format!("{}:refs/heads/main", w.s)]);
    let fact = PrFact {
        number: Some(12),
        url: Some("https://github.com/example/repo/pull/12".into()),
        created: true,
        auto_merge: AutoMerge::Armed,
        ..PrFact::of(&w.train, merged(&w.s))
    };
    let r = d.pr_facts(vec![fact], EnvMap::new()).await.remove(0);
    assert_eq!(r.decision, ReconcileDecision::Landed, "{r:?}");
    assert_eq!(r.reason, None, "the merged tree is the verified one");
    let st = d.status().await;
    let t = train(&st, &w);
    assert_eq!(t.state, T::Landed);
    assert_eq!(t.note, None);
    let pr = t.pr.as_ref().unwrap();
    assert_eq!(pr.number, Some(12));
    assert!(pr.created_by_sluice);
    assert_eq!(pr.auto_merge, AutoMerge::Armed);
    assert_eq!(pr.observed_state.as_deref(), Some("merged"));
    assert_eq!(d.wait_job(&w.job, J::Landed).await.state, J::Landed);
    assert!(
        fx.local_ref(&format!("refs/sluice/pending/{}", w.train))
            .is_none()
    );
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_merge_of_another_tree_is_landed_but_never_called_verified() {
    let fx = Fixture::new(PR_TOML);
    let daemon = Daemon::start(&fx, user_config(false)).await;
    let w = awaiting(&daemon, &fx, "feat/a", "a1.txt").await;
    // The forge (or a human) merged something else on top of S: a commit
    // whose tree is not the verified one.
    let (_, other) = fx.branch("other", "main", &[("x.txt", "x\n")]);
    let tree = fx.git(&["rev-parse", &format!("{other}^{{tree}}")]);
    let merge = fx.git(&["commit-tree", &tree, "-p", &w.s, "-m", "merged differently"]);
    fx.git(&["push", "-q", "origin", &format!("{merge}:refs/heads/main")]);
    let reply = tell(&daemon, &w, merged(&merge)).await;
    assert_eq!(reply.decision, ReconcileDecision::Landed, "{reply:?}");
    assert_eq!(reply.reason.as_deref(), Some("tree_differs_from_verified"));
    let st = daemon.status().await;
    let t = train(&st, &w);
    assert_eq!(t.state, T::Landed);
    assert_eq!(t.note.as_deref(), Some("tree_differs_from_verified"));
    daemon.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_pull_request_fails_the_jobs_and_retry_is_the_way_back() {
    let fx = Fixture::new(PR_TOML);
    let d = Daemon::start(&fx, user_config(false)).await;
    let w = awaiting(&d, &fx, "feat/a", "a1.txt").await;
    let r = tell(
        &d,
        &w,
        PrObservation::Closed {
            head: Some(w.s.clone()),
        },
    )
    .await;
    assert_eq!(r.decision, ReconcileDecision::Failed, "{r:?}");
    assert_eq!(r.reason.as_deref(), Some("pr_closed"));
    let job = d.wait_job(&w.job, J::Failed).await;
    assert_eq!(job.reason.as_deref(), Some("pr_closed"));
    let st = d.status().await;
    assert_eq!(train(&st, &w).state, T::Dissolved);
    assert!(
        fx.local_ref(&format!("refs/sluice/pending/{}", w.train))
            .is_none()
    );
    // A human closed it: the branch is not Sluice's to remove.
    assert!(fx.bare_ref(&branch_ref(&w)).is_some());
    let head = fx.git(&["rev-parse", "feat/a"]);
    let queued = d.retry(&w.job, &head).await;
    assert!(
        matches!(queued, ServerMessage::QueueJob { .. }),
        "{queued:?}"
    );
    d.wait_job(&w.job, J::Queued).await;
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn everything_else_the_forge_can_say_leaves_the_train_waiting_with_a_note() {
    let fx = Fixture::new(PR_TOML);
    let d = Daemon::start(&fx, user_config(false)).await;
    let w = awaiting(&d, &fx, "feat/a", "a1.txt").await;
    let note_of = |st: &QueueStatusReport| train(st, &w).pr.as_ref().unwrap().note.clone();

    // The CLI cannot be used: the state does not change.
    let r = tell(
        &d,
        &w,
        PrObservation::Unreachable {
            reason: "gh is not installed".into(),
        },
    )
    .await;
    assert_eq!(r.decision, ReconcileDecision::AwaitingMerge);
    assert_eq!(
        r.reason.as_deref(),
        Some("forge_unreachable: gh is not installed")
    );
    let st = d.status().await;
    assert_eq!(train(&st, &w).state, T::AwaitingMerge);
    assert_eq!(
        note_of(&st).as_deref(),
        Some("forge_unreachable: gh is not installed")
    );

    // Someone pushed to the pull request.
    let r = tell(
        &d,
        &w,
        PrObservation::Open {
            head: "deadbeef".into(),
        },
    )
    .await;
    assert_eq!(r.reason.as_deref(), Some("pr_head_changed"));

    // The pull request is not there (yet), the branch is.
    let r = tell(&d, &w, PrObservation::Missing).await;
    assert_eq!(r.reason.as_deref(), Some("pr_not_opened"));

    // Opening it failed.
    let r = tell(
        &d,
        &w,
        PrObservation::CreateFailed {
            reason: "HTTP 422".into(),
        },
    )
    .await;
    assert_eq!(r.reason.as_deref(), Some("pr_create_failed: HTTP 422"));

    // Merged, but the commit it names is not on the target.
    let (_, stray) = fx.branch("stray", "main", &[("s.txt", "s\n")]);
    let r = tell(&d, &w, merged(&stray)).await;
    assert_eq!(r.decision, ReconcileDecision::AwaitingMerge, "{r:?}");
    assert_eq!(r.reason.as_deref(), Some("merge_commit_not_on_target"));

    // The target moves while the pull request waits: reported, never rebuilt.
    fx.move_remote_main("m.txt");
    let r = tell(&d, &w, PrObservation::Open { head: w.s.clone() }).await;
    assert_eq!(r.decision, ReconcileDecision::AwaitingMerge);
    assert_eq!(r.reason, None, "a clean report clears the last remark");
    let st = d.status().await;
    let pr = train(&st, &w).pr.clone().unwrap();
    assert_eq!(pr.target_moved, Some(1));
    assert_eq!(pr.observed_state.as_deref(), Some("open"));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let st = d.status().await;
    assert_eq!(train(&st, &w).state, T::AwaitingMerge, "no requeue");
    let job = st.jobs.iter().find(|j| j.job_id == w.job).unwrap();
    assert_eq!(job.state, J::AwaitingMerge);

    // No pull request and no branch: nothing is left to merge.
    fx.git(&["push", "-q", "origin", &format!(":{}", branch_ref(&w))]);
    let r = tell(&d, &w, PrObservation::Missing).await;
    assert_eq!(r.decision, ReconcileDecision::Failed, "{r:?}");
    assert_eq!(r.reason.as_deref(), Some("pr_closed"));
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_waiting_job_dissolves_the_train_and_the_branch_goes_once_the_pull_request_is_closed()
 {
    let fx = Fixture::new(PR_TOML);
    let d = Daemon::start(&fx, user_config(true)).await;
    let w = awaiting(&d, &fx, "feat/a", "a1.txt").await;
    // An agent cannot withdraw what a human approved.
    let r = d
        .ask(&sluice_gate_proto::ClientMessage::QueueCancel {
            id: 4,
            job_id: w.job.clone(),
            via: Some(ApproveVia::Mcp),
        })
        .await;
    assert_eq!(error_code(&r), ErrorCode::NotAllowed, "{r:?}");
    assert_eq!(d.status().await.jobs[0].state, J::AwaitingMerge);
    // The human's cancel.
    let r = d.cancel(&w.job).await;
    assert!(matches!(r, ServerMessage::QueueJob { .. }), "{r:?}");
    let job = d.wait_job(&w.job, J::Cancelled).await;
    assert_eq!(job.state, J::Cancelled);
    let st = d.status().await;
    assert_eq!(train(&st, &w).state, T::Dissolved);
    assert!(
        fx.local_ref(&format!("refs/sluice/pending/{}", w.train))
            .is_none()
    );
    // The branch stays until the client reports the pull request closed ...
    assert!(fx.bare_ref(&branch_ref(&w)).is_some());
    let r = tell(&d, &w, PrObservation::Open { head: w.s.clone() }).await;
    assert_eq!(r.decision, ReconcileDecision::Refused);
    assert_eq!(r.reason.as_deref(), Some("train_dissolved"));
    assert!(fx.bare_ref(&branch_ref(&w)).is_some());
    // ... then Sluice deletes what it pushed, with a lease on that commit.
    let r = d
        .pr_facts(
            vec![PrFact::of(&w.train, PrObservation::Closed { head: None })],
            fx.client_env(),
        )
        .await
        .remove(0);
    assert_eq!(r.reason.as_deref(), Some("branch_deleted"), "{r:?}");
    assert!(fx.bare_ref(&branch_ref(&w)).is_none());
    let st = d.status().await;
    assert!(train(&st, &w).pr.as_ref().unwrap().branch_deleted);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_branch_of_a_pull_request_somebody_else_opened_is_left_alone() {
    let fx = Fixture::new(PR_TOML);
    let d = Daemon::start(&fx, user_config(false)).await;
    let w = awaiting(&d, &fx, "feat/a", "a1.txt").await;
    // The client found a pull request it did not open.
    let found = PrFact {
        number: Some(5),
        url: Some("https://github.com/example/repo/pull/5".into()),
        ..PrFact::of(&w.train, PrObservation::Open { head: w.s.clone() })
    };
    d.pr_facts(vec![found], EnvMap::new()).await;
    d.cancel(&w.job).await;
    d.wait_job(&w.job, J::Cancelled).await;
    let r = d
        .pr_facts(
            vec![PrFact::of(&w.train, PrObservation::Closed { head: None })],
            fx.client_env(),
        )
        .await
        .remove(0);
    assert_eq!(r.reason.as_deref(), Some("pull_request_not_ours"), "{r:?}");
    assert!(fx.bare_ref(&branch_ref(&w)).is_some());
    let st = d.status().await;
    assert!(!train(&st, &w).pr.as_ref().unwrap().branch_deleted);
    d.shutdown().await;
}

/// A remote that refuses every `sluice/train/*` branch like a repository
/// whose rules protect it.
fn refuse_train_branches(fx: &Fixture) {
    let hook = fx.bare.join("hooks/pre-receive");
    let text = "#!/bin/sh\nwhile read old new ref; do\ncase \"$ref\" in\nrefs/heads/sluice/train/*)\n\
                echo \"remote: error: GH006: Protected branch update failed for $ref.\" >&2\nexit 1;;\n\
                esac\ndone\nexit 0\n";
    std::fs::write(&hook, text).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_branch_push_is_a_plain_push_rejected_and_teaches_nothing_about_the_target() {
    let fx = Fixture::new(PR_TOML);
    refuse_train_branches(&fx);
    let d = Daemon::start(&fx, user_config(false)).await;
    let (wt, sha) = fx.branch("feat/a", "main", &[("a1.txt", "n\n")]);
    let j = d.enqueue_ok(&fx, &wt, "feat/a", &sha, None).await;
    let t = d.wait_train("ready", ready).await;
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    let job = d.wait_job(&j.job_id, J::Failed).await;
    assert_eq!(job.reason.as_deref(), Some("push_rejected"));
    let out = job.detail.unwrap()["output"].as_str().unwrap().to_owned();
    assert!(out.contains("GH006"), "{out}");
    let st = d.status().await;
    assert_eq!(st.trains[0].state, T::Dissolved);
    // The target was never asked: nothing is learned about it.
    let (wb, b) = fx.branch("feat/b", "main", &[("b.txt", "b\n")]);
    let second = d.enqueue_ok(&fx, &wb, "feat/b", &b, None).await;
    assert_eq!(second.learned_protection, None);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_in_doubt_is_reconciled_by_the_branch_not_the_target() {
    let fx = Fixture::new(PR_TOML);
    let base = fx.bare_ref("refs/heads/main").unwrap();
    let (wt, sha) = fx.branch("feat/a", "main", &[("a1.txt", "n\n")]);
    let d = Daemon::start_with(
        &fx,
        user_config(false),
        Some(fx.root.join("no-such-git-binary")),
    )
    .await;
    let j = d.enqueue_ok(&fx, &wt, "feat/a", &sha, None).await;
    let t = d.wait_train("ready", ready).await;
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    let id = t.train_id.clone();
    d.wait_train("needs_reconcile", |x| {
        x.train_id == id && x.state == T::NeedsReconcile
    })
    .await;
    // Nothing reached the remote: back to ready, approval voided.
    let res = d.reconcile().await;
    assert_eq!(res[0].decision, ReconcileDecision::Ready, "{res:?}");
    d.wait_job(&j.job_id, J::Ready).await;
    assert_eq!(fx.bare_ref("refs/heads/main").unwrap(), base);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_in_doubt_whose_branch_arrived_goes_on_to_wait_for_the_merge() {
    let fx = Fixture::new(PR_TOML);
    let (wt, sha) = fx.branch("feat/a", "main", &[("a1.txt", "n\n")]);
    let d = Daemon::start_with(
        &fx,
        user_config(false),
        Some(fx.root.join("no-such-git-binary")),
    )
    .await;
    let j = d.enqueue_ok(&fx, &wt, "feat/a", &sha, None).await;
    let t = d.wait_train("ready", ready).await;
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    let id = t.train_id.clone();
    let parked = d
        .wait_train("needs_reconcile", |x| {
            x.train_id == id && x.state == T::NeedsReconcile
        })
        .await;
    let s = parked.merge_sha.unwrap();
    // The push did land, the daemon just did not see it end.
    fx.git(&[
        "push",
        "-q",
        "origin",
        &format!("{s}:refs/heads/sluice/train/{id}"),
    ]);
    let res = d.reconcile().await;
    assert_eq!(res[0].decision, ReconcileDecision::AwaitingMerge, "{res:?}");
    d.wait_job(&j.job_id, J::AwaitingMerge).await;
    let st = d.status().await;
    assert_eq!(st.trains[0].state, T::AwaitingMerge);
    assert!(st.trains[0].pr.as_ref().unwrap().since.is_some());
    // The pin ref is kept for the pull request to come.
    assert!(fx.local_ref(&format!("refs/sluice/pending/{id}")).is_some());
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_a_human_with_a_client_that_can_open_the_pull_request_approves_it() {
    let fx = Fixture::new(PR_TOML);
    // Even with agent approval enabled for pushes.
    let d = Daemon::start(&fx, user_config(true)).await;
    let (wt, sha) = fx.branch("feat/a", "main", &[("a1.txt", "n\n")]);
    d.enqueue_ok(&fx, &wt, "feat/a", &sha, None).await;
    let t = d.wait_train("ready", ready).await;
    let hash = t.train_hash.clone().unwrap();
    let mut agent = fx.client_env();
    agent.insert("CLAUDECODE".into(), "1".into());
    let refusals = [
        d.approve_as(&t.train_id, &hash, ApproveVia::Mcp, false, fx.client_env())
            .await,
        d.approve_as(&t.train_id, &hash, ApproveVia::Mcp, true, fx.client_env())
            .await,
        d.approve_as(&t.train_id, &hash, ApproveVia::Cli, true, agent)
            .await,
        d.approve_as(&t.train_id, &hash, ApproveVia::Cli, false, fx.client_env())
            .await,
    ];
    for r in &refusals {
        match r {
            ServerMessage::Error { code, message, .. } => {
                assert_eq!(*code, ErrorCode::NotAllowed, "{message}");
                assert!(message.contains("pull request"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    // Nothing was pushed and the train is still waiting for a human.
    let st = d.status().await;
    assert_eq!(st.trains[0].state, T::Ready);
    assert!(
        fx.bare_ref(&format!("refs/heads/sluice/train/{}", t.train_id))
            .is_none()
    );
    let human = d
        .approve_as(&t.train_id, &hash, ApproveVia::Cli, true, fx.client_env())
        .await;
    assert!(
        matches!(human, ServerMessage::QueueApproved { .. }),
        "{human:?}"
    );
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_branch_cannot_choose_how_it_lands() {
    // The target says push; the branch's own file says pull-request.
    let fx = Fixture::new(PUSH_TOML);
    let (wt, sha) = fx.branch(
        "feat/a",
        "main",
        &[("a1.txt", "n\n"), (".sluice.toml", PR_TOML)],
    );
    let d = Daemon::start(&fx, user_config(false)).await;
    d.enqueue_ok(&fx, &wt, "feat/a", &sha, None).await;
    let t = d.wait_train("ready", ready).await;
    assert_eq!(t.land, None, "the base's configuration decides");
    d.approve(
        &fx,
        &t.train_id,
        t.train_hash.as_deref().unwrap(),
        ApproveVia::Cli,
    )
    .await;
    let landed = d.wait_train("landed", |x| x.state == T::Landed).await;
    assert_eq!(
        fx.bare_ref("refs/heads/main").unwrap(),
        landed.merge_sha.unwrap()
    );
    d.shutdown().await;
    // And the other way round: the target says pull-request, the branch push.
    let fx = Fixture::new(PR_TOML);
    let (wt, sha) = fx.branch(
        "feat/b",
        "main",
        &[
            ("b1.txt", "n\n"),
            (
                ".sluice.toml",
                "version = 1\n[gates.test]\ncmd = \"true\"\n[queue]\nland = \"push\"\n",
            ),
        ],
    );
    let d = Daemon::start(&fx, user_config(false)).await;
    d.enqueue_ok(&fx, &wt, "feat/b", &sha, None).await;
    let t = d.wait_train("ready", ready).await;
    assert_eq!(t.land.as_deref(), Some("pull-request"));
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_user_config_is_the_default_when_the_target_does_not_say() {
    let fx = Fixture::new(PUSH_TOML);
    let (wt, sha) = fx.branch("feat/a", "main", &[("a1.txt", "n\n")]);
    let mut user = user_config(false);
    user.queue.land = Some(Land::PullRequest);
    let d = Daemon::start(&fx, user).await;
    let j = d.enqueue_ok(&fx, &wt, "feat/a", &sha, None).await;
    assert_eq!(
        j.land.as_deref(),
        Some("pull-request"),
        "the enqueue says so"
    );
    let t = d.wait_train("ready", ready).await;
    assert_eq!(t.land.as_deref(), Some("pull-request"));
    d.shutdown().await;
}

#[test]
fn the_marker_columns_are_added_in_place_to_an_older_table() {
    // A database written by a release without pull-request landing: the
    // markers table has no `pr_*` columns. `init` adds them, keeps the rows.
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE queue_push_markers (
           train_id TEXT PRIMARY KEY, sha TEXT NOT NULL, base_sha TEXT NOT NULL,
           remote TEXT NOT NULL, endpoint_hash TEXT NOT NULL, push_refs TEXT NOT NULL,
           audit_ref TEXT, pin_ref TEXT NOT NULL, claim_token TEXT NOT NULL,
           push_pid INTEGER, push_start_ticks INTEGER, written_at INTEGER NOT NULL);
         INSERT INTO queue_push_markers VALUES
           ('t-1','s','b','origin','e','[]',NULL,'pin','tok',NULL,NULL,1);",
    )
    .unwrap();
    let mut conn = conn;
    let tx = conn.transaction().unwrap();
    sluice_gate_queue::store::init(&tx).unwrap();
    let marker = sluice_gate_queue::store::marker(&tx, "t-1")
        .unwrap()
        .unwrap();
    assert_eq!(marker.pr, sluice_gate_queue::store::PrColumns::default());
    assert_eq!(marker.sha, "s");
    // Idempotent.
    sluice_gate_queue::store::init(&tx).unwrap();
}
