//! The client side of landing through a pull request (ADR-0025): open the
//! pull request of an approved train, read it for `queue status` and
//! `queue reconcile`, close it for `queue cancel`.
//!
//! Everything here runs in the caller's environment with the caller's own
//! `gh` login. The daemon is told facts ([`PrFact`]) and decides; no token is
//! stored or sent. Sluice never merges: auto-merge is armed only when the
//! approver passed `--auto-merge`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, SystemTime};

use sluice_gate_core::agent;
use sluice_gate_core::config::UserConfig;
use sluice_gate_core::forge::landing::{self, TrainRef};
use sluice_gate_core::forge::{AutoMerge, Member, PrFact, PrObservation, describe};
use sluice_gate_core::git::EnvMap;
use sluice_gate_proto::{
    ClientMessage, QueueJobView, QueueTrainState, QueueTrainView, ReconcileDecision,
    ReconcileResult, ServerMessage,
};
use std::path::Path;

use super::{
    EXIT_REFUSED, Failure, Outcome, approve_env, decision_name, fetch_status, filter_env, send,
    short, target_name,
};

fn train_ref(train: &QueueTrainView) -> TrainRef {
    TrainRef {
        train_id: train.train_id.clone(),
        repo: train.repo.clone(),
        remote: train.remote.clone(),
        merge_sha: train.merge_sha.clone().unwrap_or_default(),
    }
}

/// Opening a pull request (and arming auto-merge) is a human's act: an agent
/// at the CLI is refused, as it is for the approval itself.
fn require_human(env: &EnvMap) -> Result<(), Failure> {
    let user = UserConfig::load(env).unwrap_or_default();
    let detection = &user.agent_detection;
    match agent::detect(
        &approve_env(env, &user),
        &detection.env_any_set,
        &detection.env_any_equals,
    ) {
        agent::Caller::Human => Ok(()),
        agent::Caller::Agent { rule } => Err(Failure {
            code: EXIT_REFUSED,
            message: format!(
                "opening the pull request of a train is reserved to humans and this client is an \
                 agent ({rule}); a human runs 'sluice queue approve' or 'sluice queue reconcile'"
            ),
        }),
    }
}

/// Tells the daemon what was read. Errors are the caller's to show.
fn report(
    env: &EnvMap,
    facts: Vec<PrFact>,
    with_env: bool,
) -> Result<Vec<ReconcileResult>, Failure> {
    let msg = ClientMessage::QueuePrFacts {
        id: 1,
        facts,
        env: if with_env {
            filter_env(env)
        } else {
            BTreeMap::default()
        },
    };
    match send(env, &msg, false)? {
        ServerMessage::QueueReconcile { results, .. } => Ok(results),
        other => Err(super::unexpected("queue-pr-facts", &other)),
    }
}

/// Reads the pull request of every train of `trains` with the caller's
/// `gh`, in parallel (each read is bounded).
fn observe_all(env: &EnvMap, trains: &[&QueueTrainView]) -> Vec<PrFact> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = trains
            .iter()
            .map(|t| {
                let tref = train_ref(t);
                scope.spawn(move || landing::observe(env, &tref))
            })
            .collect();
        handles
            .into_iter()
            .zip(trains)
            .map(|(h, t)| {
                h.join().unwrap_or_else(|_| {
                    PrFact::of(
                        &t.train_id,
                        PrObservation::Unreachable {
                            reason: "reading the pull request failed".into(),
                        },
                    )
                })
            })
            .collect()
    })
}

fn waiting_trains(report: &sluice_gate_proto::QueueStatusReport) -> Vec<&QueueTrainView> {
    report
        .trains
        .iter()
        .filter(|t| t.state == QueueTrainState::AwaitingMerge && t.pr.is_some())
        .collect()
}

/// `queue status`: reads the pull request of each train waiting for its
/// merge and reports it. True when the daemon was told something (the report
/// should be read again).
pub(super) fn refresh(env: &EnvMap, status: &sluice_gate_proto::QueueStatusReport) -> bool {
    let waiting = waiting_trains(status);
    if waiting.is_empty() {
        return false;
    }
    report(env, observe_all(env, &waiting), false).is_ok()
}

fn age(since: &str) -> Option<String> {
    let at = humantime::parse_rfc3339(since).ok()?;
    let secs = SystemTime::now().duration_since(at).ok()?.as_secs();
    // Minutes are precise enough for a wait that lasts hours or days.
    Some(humantime::format_duration(Duration::from_secs(secs / 60 * 60)).to_string())
}

/// The lines under a train that waits for its pull request to be merged.
pub(super) fn waiting_text(train: &QueueTrainView) -> String {
    let mut out = String::new();
    let Some(pr) = &train.pr else {
        return out;
    };
    match (pr.number, &pr.url) {
        (Some(n), Some(url)) => {
            let _ = writeln!(out, "  pull request: #{n} {url}");
        }
        (Some(n), None) => {
            let _ = writeln!(out, "  pull request: #{n}");
        }
        _ => {
            let _ = writeln!(
                out,
                "  pull request: not opened yet; a human runs 'sluice queue approve {}' (or \
                 'sluice queue reconcile') to open it",
                train.train_id
            );
        }
    }
    let merge = match pr.auto_merge {
        AutoMerge::Armed => "auto-merge armed",
        AutoMerge::Failed => "auto-merge requested but refused by the forge",
        AutoMerge::NotRequested => "auto-merge not armed",
    };
    let waited = pr
        .since
        .as_deref()
        .and_then(age)
        .map_or_else(String::new, |a| format!("waiting {a}, "));
    let seen = pr
        .observed_state
        .as_deref()
        .map_or_else(String::new, |s| format!(", last seen {s}"));
    let _ = writeln!(out, "  {waited}{merge}{seen}");
    if let Some(n) = pr.target_moved.filter(|n| *n > 0) {
        let _ = writeln!(
            out,
            "  target moved {n} commit{} since the train was built (not rebuilt: the forge's rules decide)",
            if n == 1 { "" } else { "s" }
        );
    }
    if let Some(note) = &pr.note {
        let _ = writeln!(out, "  note: {note}");
    }
    out.push_str("  Sluice never merges: the pull request is merged on the forge\n");
    out
}

/// The text of the pull request for `train`.
fn spec_of(train: &QueueTrainView, jobs: &[QueueJobView]) -> sluice_gate_core::forge::PrSpec {
    let members: Vec<Member> = train
        .members
        .iter()
        .filter_map(|id| jobs.iter().find(|j| &j.job_id == id))
        .map(|j| Member {
            branch: j.branch.clone(),
            head: j.head_sha.clone(),
            job: j.job_id.clone(),
        })
        .collect();
    let gates: Vec<String> = train
        .gates
        .iter()
        .filter(|g| matches!(g.status.as_str(), "pass" | "cached"))
        .map(|g| g.gate.clone())
        .collect();
    describe(
        &train.train_id,
        &train.target,
        train.merge_sha.as_deref().unwrap_or_default(),
        &members,
        &gates,
    )
}

/// A train that was approved before and has its branch out but no pull
/// request: finish it (idempotent: an existing pull request is found).
pub(super) fn resume(
    env: &EnvMap,
    train: &QueueTrainView,
    jobs: &[QueueJobView],
    auto_merge: bool,
) -> Outcome {
    require_human(env)?;
    if let Some(pr) = train.pr.as_ref().filter(|p| p.number.is_some()) {
        println!(
            "sluice: train {} already has its pull request #{} {}",
            train.train_id,
            pr.number.unwrap_or_default(),
            pr.url.as_deref().unwrap_or_default()
        );
        return Ok(());
    }
    finish(env, train, jobs, auto_merge)
}

/// The train branch is on the remote: find or open the pull request with the
/// caller's `gh`, arm auto-merge when asked, tell the daemon.
pub(super) fn finish(
    env: &EnvMap,
    train: &QueueTrainView,
    jobs: &[QueueJobView],
    auto_merge: bool,
) -> Outcome {
    require_human(env)?;
    let t = train_ref(train);
    println!(
        "sluice: {} is on {}; opening the pull request for {}\u{2026}",
        t.branch(),
        train.remote,
        target_name(&train.remote, &train.target)
    );
    let fact = landing::open(env, &t, &spec_of(train, jobs), auto_merge);
    let told = report(env, vec![fact.clone()], false);
    if let PrObservation::CreateFailed { reason } = &fact.observation {
        return Err(Failure::error(format!(
            "train {} is awaiting its merge but its pull request could not be opened: {reason}; \
             'sluice queue reconcile' (or approving the train again) opens it",
            train.train_id
        )));
    }
    let number = fact.number.unwrap_or_default();
    let url = fact.url.clone().unwrap_or_default();
    let how = if fact.created { "opened" } else { "found" };
    println!("sluice: pull request #{number} {how}: {url}");
    match fact.auto_merge {
        AutoMerge::Armed => println!(
            "sluice: auto-merge armed (only for {})",
            short(&t.merge_sha)
        ),
        AutoMerge::Failed => eprintln!(
            "sluice: warning: the forge refused auto-merge; the pull request stays open for a human"
        ),
        AutoMerge::NotRequested => {}
    }
    println!(
        "sluice: Sluice does not merge: merge #{number} on the forge{}; 'sluice queue status' \
         shows when it landed",
        if fact.auto_merge == AutoMerge::Armed {
            " (or let auto-merge do it)"
        } else {
            ""
        }
    );
    if let Err(f) = told {
        eprintln!(
            "sluice: warning: the daemon was not told about the pull request ({}); \
             'sluice queue status' tells it",
            f.message
        );
    }
    Ok(())
}

/// What `queue cancel` needs to know about a job that waits for its merge.
pub(super) struct Waiting {
    train: TrainRef,
    number: Option<u64>,
    created: bool,
}

/// The pull request of the job's train, when the job waits for its merge.
pub(super) fn before_cancel(env: &EnvMap, job_id: &str) -> Option<Waiting> {
    let status = fetch_status(env, None).ok()?;
    let job = status.jobs.iter().find(|j| j.job_id == job_id)?;
    if job.state != sluice_gate_proto::QueueJobState::AwaitingMerge {
        return None;
    }
    let train = status
        .trains
        .iter()
        .find(|t| Some(&t.train_id) == job.train_id.as_ref())?;
    let pr = train.pr.as_ref()?;
    Some(Waiting {
        train: train_ref(train),
        number: pr.number,
        created: pr.created_by_sluice,
    })
}

/// After the daemon dissolved the train: close the pull request when it was
/// Sluice's, then tell the daemon it is closed so it deletes the branch it
/// pushed (a lease on that commit). A pull request somebody else opened is
/// left alone.
pub(super) fn after_cancel(env: &EnvMap, w: &Waiting) {
    if let Some(n) = w.number {
        if !w.created {
            println!("sluice: pull request #{n} was not opened by Sluice; left as it is");
            return;
        }
        let comment = "Closed by `sluice queue cancel`: the train was withdrawn.";
        match landing::close(env, &w.train, n, comment) {
            Ok(()) => println!("sluice: closed pull request #{n}"),
            Err(why) => eprintln!(
                "sluice: warning: could not close pull request #{n}: {why}; close it by hand"
            ),
        }
    }
    let fact = landing::observe_now(env, &w.train);
    match report(env, vec![fact], true) {
        Ok(results) => {
            for r in results {
                println!("train {}: {}", r.train_id, branch_text(&r));
            }
        }
        Err(f) => eprintln!("sluice: warning: {}", f.message),
    }
}

fn branch_text(r: &ReconcileResult) -> String {
    match (r.decision, r.reason.as_deref()) {
        (ReconcileDecision::Cancelled, Some("branch_deleted")) => {
            "the train branch was deleted".to_owned()
        }
        (_, reason) => format!(
            "the train branch was left ({})",
            reason.unwrap_or("no reason given")
        ),
    }
}

/// `queue reconcile`, second part: every train waiting for its merge gets
/// its pull request found or opened (a human) and read.
pub(super) fn reconcile_waiting(env: &EnvMap, common: Option<&Path>, nothing: bool) -> Outcome {
    // The reconcile itself succeeded: a daemon that cannot list the trains
    // now only costs the pull-request part.
    let status = match fetch_status(env, common) {
        Ok(status) => status,
        Err(f) => {
            eprintln!(
                "sluice: warning: could not look for pull requests to read: {}",
                f.message
            );
            return Ok(());
        }
    };
    let waiting: Vec<&QueueTrainView> = status
        .trains
        .iter()
        .filter(|t| t.state == QueueTrainState::AwaitingMerge)
        .collect();
    if waiting.is_empty() {
        if nothing {
            println!("sluice: no train needs reconciling");
        }
        return Ok(());
    }
    let human = require_human(env).is_ok();
    let (without, with): (Vec<_>, Vec<_>) = waiting
        .into_iter()
        .partition(|t| t.pr.as_ref().is_none_or(|p| p.number.is_none()));
    let mut failure = None;
    let mut to_read = with;
    for t in without {
        if human {
            if let Err(f) = finish(env, t, &status.jobs, false) {
                failure = Some(f);
            }
        } else {
            println!(
                "train {}: its pull request is not opened yet; a human runs 'sluice queue \
                 reconcile' to open it",
                t.train_id
            );
            to_read.push(t);
        }
    }
    if !to_read.is_empty() {
        for r in report(env, observe_all(env, &to_read), false)? {
            println!(
                "train {}: {} ({})",
                r.train_id,
                decision_name(r.decision),
                r.reason.as_deref().unwrap_or("waiting for the merge")
            );
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_gate_proto::{PullRequestView, TrainGate};

    fn train() -> QueueTrainView {
        QueueTrainView {
            train_id: "t-1".into(),
            remote: "origin".into(),
            target: "main".into(),
            state: QueueTrainState::AwaitingMerge,
            members: vec!["j1".into()],
            merge_sha: Some("0123456789abcdef".into()),
            gates: vec![
                TrainGate {
                    gate: "test".into(),
                    status: "pass".into(),
                    ..TrainGate::default()
                },
                TrainGate {
                    gate: "slow".into(),
                    status: "fail".into(),
                    ..TrainGate::default()
                },
            ],
            land: Some("pull-request".into()),
            pr: Some(PullRequestView {
                branch: "sluice/train/t-1".into(),
                number: Some(12),
                url: Some("https://github.com/example/repo/pull/12".into()),
                created_by_sluice: true,
                auto_merge: AutoMerge::Armed,
                observed_state: Some("open".into()),
                target_moved: Some(3),
                note: Some("pr_head_changed".into()),
                ..PullRequestView::default()
            }),
            ..QueueTrainView::default()
        }
    }

    #[test]
    fn a_waiting_train_shows_the_pull_request_and_what_the_forge_decides() {
        let text = waiting_text(&train());
        for want in [
            "pull request: #12 https://github.com/example/repo/pull/12",
            "auto-merge armed, last seen open",
            "target moved 3 commits since the train was built",
            "note: pr_head_changed",
            "Sluice never merges",
        ] {
            assert!(text.contains(want), "{want}\n{text}");
        }
    }

    #[test]
    fn a_train_without_a_pull_request_says_who_opens_it() {
        let mut t = train();
        if let Some(pr) = &mut t.pr {
            pr.number = None;
            pr.url = None;
            pr.auto_merge = AutoMerge::NotRequested;
            pr.target_moved = None;
            pr.note = None;
        }
        let text = waiting_text(&t);
        assert!(text.contains("not opened yet"), "{text}");
        assert!(text.contains("sluice queue approve t-1"), "{text}");
        assert!(text.contains("auto-merge not armed"), "{text}");
    }

    #[test]
    fn the_request_names_the_members_and_the_passed_gates() {
        let jobs = vec![QueueJobView {
            job_id: "j1".into(),
            branch: "feat/a".into(),
            head_sha: "abcdef0123".into(),
            ..QueueJobView::default()
        }];
        let spec = spec_of(&train(), &jobs);
        assert_eq!(spec.head, "sluice/train/t-1");
        assert_eq!(spec.base, "main");
        assert!(spec.body.contains("`feat/a` at `abcdef0` (job `j1`)"));
        assert!(spec.body.contains("Gates passed on this commit: test."));
    }
}
