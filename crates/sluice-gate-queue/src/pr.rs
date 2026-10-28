//! Reconcile for `land = "pull-request"` (ADR-0025): two decision tables
//! as pure functions over facts, and the functions that gather the facts the
//! daemon can read itself (the remote's refs, the target's history). The
//! forge's own facts (the state of the pull request) come from a client as a
//! [`PrObservation`]: the daemon never talks to a forge.
//!
//! - [`decide_branch`]: a `needs_reconcile` train whose push of the
//!   `sluice/train/<id>` branch ended in doubt.
//! - [`decide_pr`]: an `awaiting_merge` train, from what a client reported.

use std::path::Path;

use sluice_gate_core::forge::PrObservation;

use crate::git::Git;
use crate::target::endpoint;

/// What the remote says about the train branch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RemoteBranch {
    /// The remote could not be read.
    #[default]
    Unreachable,
    /// The remote answered: the branch does not exist.
    Absent,
    /// The remote answered: the branch is at this commit.
    At(String),
}

/// Facts about a `needs_reconcile` train that was pushing its branch.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BranchInput {
    /// The recorded push process is still alive.
    pub push_alive: bool,
    /// The remote resolves to the recorded endpoint.
    pub endpoint_matches: bool,
    /// The train branch on the remote.
    pub branch: RemoteBranch,
    /// Landing commit `S`.
    pub merge_sha: String,
    /// A cancel is pending on a member.
    pub cancel_requested: bool,
}

/// Outcome of [`decide_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchDecision {
    /// Unchanged; the reason token.
    Refuse(&'static str),
    /// The branch is on the remote at `S`: carry on to the pull request.
    AwaitMerge,
    /// Nothing was pushed: back to `ready`, approval voided.
    Ready,
    /// Nothing was pushed and a cancel was pending.
    Cancelled,
}

/// The decision table of a pull-request push in doubt; the first matching
/// row wins.
///
/// | # | Facts | Decision |
/// |---|---|---|
/// | 1 | the push process is alive | refuse `push_in_progress` |
/// | 2 | the remote is not the recorded endpoint | refuse `endpoint_changed` |
/// | 3 | the remote cannot be read | refuse `remote_unreachable` |
/// | 4 | the branch is at `S` | await the merge |
/// | 5 | the branch does not exist | `ready` (or cancelled when a cancel is pending) |
/// | 6 | the branch is at another commit | refuse `branch_diverged` |
#[must_use]
pub fn decide_branch(i: &BranchInput) -> BranchDecision {
    if i.push_alive {
        return BranchDecision::Refuse("push_in_progress");
    }
    if !i.endpoint_matches {
        return BranchDecision::Refuse("endpoint_changed");
    }
    match &i.branch {
        RemoteBranch::Unreachable => BranchDecision::Refuse("remote_unreachable"),
        RemoteBranch::At(sha) if *sha == i.merge_sha => BranchDecision::AwaitMerge,
        RemoteBranch::Absent if i.cancel_requested => BranchDecision::Cancelled,
        RemoteBranch::Absent => BranchDecision::Ready,
        RemoteBranch::At(_) => BranchDecision::Refuse("branch_diverged"),
    }
}

/// Facts the daemon needs about the train, to gather [`BranchInput`] and
/// [`PrInput`].
#[derive(Debug, Clone)]
pub struct TrainFacts<'a> {
    /// Git common dir.
    pub repo: &'a Path,
    /// Remote.
    pub remote: &'a str,
    /// Target branch.
    pub target: &'a str,
    /// Recorded endpoint hash.
    pub endpoint_hash: &'a str,
    /// `sluice/train/<id>`.
    pub branch: &'a str,
    /// `S`.
    pub merge_sha: &'a str,
    /// `T`, the tree of `S`.
    pub merge_tree: Option<&'a str>,
    /// Base the train was built on.
    pub base_sha: &'a str,
}

/// `url` of the train's remote while it still has the recorded endpoint.
fn bound(git: &Git, f: &TrainFacts<'_>) -> Option<String> {
    match endpoint(git, f.repo, f.remote) {
        Ok((url, hash)) if hash == f.endpoint_hash => Some(url),
        _ => None,
    }
}

/// Reads the remote for [`decide_branch`]. Blocking (git).
#[must_use]
pub fn gather_branch(
    git: &Git,
    f: &TrainFacts<'_>,
    push: Option<(u32, Option<u64>)>,
    cancel_requested: bool,
) -> BranchInput {
    let mut input = BranchInput {
        merge_sha: f.merge_sha.to_owned(),
        cancel_requested,
        endpoint_matches: true,
        ..BranchInput::default()
    };
    input.push_alive =
        push.is_some_and(|(pid, ticks)| sluice_gate_daemon::executor::process_matches(pid, ticks));
    if input.push_alive {
        return input;
    }
    let Some(url) = bound(git, f) else {
        input.endpoint_matches = false;
        return input;
    };
    let reference = format!("refs/heads/{}", f.branch);
    if let Ok(refs) = git.ls_remote(f.repo, &url, &[&reference]) {
        input.branch = refs
            .get(&reference)
            .map_or(RemoteBranch::Absent, |sha| RemoteBranch::At(sha.clone()));
    }
    input
}

/// Facts about an `awaiting_merge` train and what a client reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInput {
    /// What the client's forge CLI said.
    pub observation: PrObservation,
    /// Landing commit `S`.
    pub merge_sha: String,
    /// The remote resolves to the recorded endpoint.
    pub endpoint_matches: bool,
    /// The remote could be read.
    pub reachable: bool,
    /// The reported merge commit is reachable from the target (`None` =
    /// cannot be told).
    pub merge_in_target: Option<bool>,
    /// `tree(merge commit) == tree(S)` (`None` = cannot be told).
    pub tree_same: Option<bool>,
    /// The train branch still exists on the remote (`None` = unknown).
    pub branch_present: Option<bool>,
    /// Commits the target gained since the base (`None` = unknown).
    pub target_moved: Option<u64>,
}

/// Outcome of [`decide_pr`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrDecision {
    /// The pull request was merged and its commit is on the target: the
    /// train landed. The note is `tree_differs_from_verified` (a human or the
    /// forge merged something else) or `tree_unchecked`; `None` when the
    /// merged tree is the verified one.
    Landed(Option<&'static str>),
    /// The members fail with this reason (`pr_closed`); the train is
    /// dissolved and `retry` is the way back.
    Failed(&'static str),
    /// Nothing changes but the remark and the measured drift of the target:
    /// the train waits. The target moving is never a reason to requeue.
    Wait {
        /// Remark to show (`None` clears the last one).
        note: Option<String>,
        /// Commits the target gained since the train was built.
        target_moved: Option<u64>,
    },
}

fn wait(note: &str) -> PrDecision {
    PrDecision::Wait {
        note: Some(note.to_owned()),
        target_moved: None,
    }
}

/// The decision table of an `awaiting_merge` train; the first matching row
/// wins.
///
/// | # | Reported | Daemon's facts | Decision |
/// |---|---|---|---|
/// | 1 | forge unusable | | wait, `forge_unreachable: <why>` |
/// | 2 | opening the pull request failed | | wait, `pr_create_failed: <why>` |
/// | 3 | closed unmerged | | fail `pr_closed` |
/// | 4 | open, head is not `S` | | wait, `pr_head_changed` (+ drift) |
/// | 5 | open, head is `S` | | wait (+ drift) |
/// | 6 | no pull request | endpoint changed | wait, `endpoint_changed` |
/// | 7 | no pull request | remote unreadable or branch unknown | wait, `remote_unreachable` |
/// | 8 | no pull request | branch gone | fail `pr_closed` |
/// | 9 | no pull request | branch present | wait, `pr_not_opened` |
/// | 10 | merged, no merge commit named | | wait, `merge_commit_unknown` |
/// | 11 | merged | endpoint changed | wait, `endpoint_changed` |
/// | 12 | merged | remote unreadable | wait, `remote_unreachable` |
/// | 13 | merged | commit not on the target | wait, `merge_commit_not_on_target` |
/// | 14 | merged | reachability unknown | wait, `ancestry_unknown` |
/// | 15 | merged | on the target, same tree | landed |
/// | 16 | merged | on the target, other tree | landed, `tree_differs_from_verified` |
/// | 17 | merged | on the target, tree unknown | landed, `tree_unchecked` |
#[must_use]
pub fn decide_pr(i: &PrInput) -> PrDecision {
    match &i.observation {
        PrObservation::Unreachable { reason } => wait(&format!("forge_unreachable: {reason}")),
        PrObservation::CreateFailed { reason } => wait(&format!("pr_create_failed: {reason}")),
        PrObservation::Closed { .. } => PrDecision::Failed("pr_closed"),
        PrObservation::Open { head } => PrDecision::Wait {
            note: (!head.is_empty() && *head != i.merge_sha).then(|| "pr_head_changed".to_owned()),
            target_moved: i.target_moved,
        },
        PrObservation::Missing => decide_missing(i),
        PrObservation::Merged { merge_commit, .. } => {
            if merge_commit.is_none() {
                return wait("merge_commit_unknown");
            }
            decide_merged(i)
        }
    }
}

fn decide_missing(i: &PrInput) -> PrDecision {
    if !i.endpoint_matches {
        return wait("endpoint_changed");
    }
    match (i.reachable, i.branch_present) {
        (true, Some(false)) => PrDecision::Failed("pr_closed"),
        (true, Some(true)) => wait("pr_not_opened"),
        _ => wait("remote_unreachable"),
    }
}

fn decide_merged(i: &PrInput) -> PrDecision {
    if !i.endpoint_matches {
        return wait("endpoint_changed");
    }
    if !i.reachable {
        return wait("remote_unreachable");
    }
    match (i.merge_in_target, i.tree_same) {
        (Some(true), Some(true)) => PrDecision::Landed(None),
        (Some(true), Some(false)) => PrDecision::Landed(Some("tree_differs_from_verified")),
        (Some(true), None) => PrDecision::Landed(Some("tree_unchecked")),
        (Some(false), _) => wait("merge_commit_not_on_target"),
        (None, _) => wait("ancestry_unknown"),
    }
}

/// Reads what the daemon can read for [`decide_pr`] from the remote and its
/// own repository, only as much as the report needs. Blocking (git).
#[must_use]
pub fn gather_pr(git: &Git, f: &TrainFacts<'_>, observation: &PrObservation) -> PrInput {
    let mut input = PrInput {
        observation: observation.clone(),
        merge_sha: f.merge_sha.to_owned(),
        endpoint_matches: true,
        reachable: false,
        merge_in_target: None,
        tree_same: None,
        branch_present: None,
        target_moved: None,
    };
    let needs_remote = matches!(
        observation,
        PrObservation::Open { .. } | PrObservation::Missing | PrObservation::Merged { .. }
    );
    if !needs_remote {
        return input;
    }
    let Some(url) = bound(git, f) else {
        input.endpoint_matches = false;
        return input;
    };
    match observation {
        PrObservation::Merged {
            merge_commit: Some(commit),
            ..
        } => gather_merged(git, f, &url, commit, &mut input),
        PrObservation::Missing => {
            let reference = format!("refs/heads/{}", f.branch);
            if let Ok(refs) = git.ls_remote(f.repo, &url, &[&reference]) {
                input.reachable = true;
                input.branch_present = Some(refs.contains_key(&reference));
            }
        }
        PrObservation::Open { .. } => {
            input.target_moved = target_moved(git, f, &url);
            input.reachable = input.target_moved.is_some();
        }
        _ => {}
    }
    input
}

/// The local ref Sluice keeps the fetched target in.
fn tracking_ref(f: &TrainFacts<'_>) -> String {
    format!("refs/sluice/base/{}/{}", f.remote, f.target)
}

/// Fetches the target into Sluice's own ref; its tip.
fn fetch_target(git: &Git, f: &TrainFacts<'_>, url: &str) -> Option<String> {
    let spec = format!("+refs/heads/{}:{}", f.target, tracking_ref(f));
    git.fetch(f.repo, url, &spec).ok()?;
    git.ref_sha(f.repo, &tracking_ref(f)).ok().flatten()
}

fn gather_merged(git: &Git, f: &TrainFacts<'_>, url: &str, commit: &str, input: &mut PrInput) {
    let Some(tip) = fetch_target(git, f, url) else {
        return;
    };
    input.reachable = true;
    // A commit the fetched target does not bring cannot be on the target.
    let known = git.resolve_commit(f.repo, commit).ok().flatten();
    input.merge_in_target = match known {
        None => Some(false),
        Some(sha) => git.is_ancestor(f.repo, &sha, &tip).ok().flatten(),
    };
    if input.merge_in_target == Some(true) {
        input.tree_same = f
            .merge_tree
            .zip(git.tree_of(f.repo, commit).ok().flatten())
            .map(|(verified, merged)| verified == merged);
    }
}

/// Commits the target gained since the train's base (`0` = unmoved).
fn target_moved(git: &Git, f: &TrainFacts<'_>, url: &str) -> Option<u64> {
    let head = format!("refs/heads/{}", f.target);
    let refs = git.ls_remote(f.repo, url, &[&head]).ok()?;
    let current = refs.get(&head)?;
    if current == f.base_sha {
        return Some(0);
    }
    let tip = fetch_target(git, f, url)?;
    git.count_commits(f.repo, f.base_sha, &tip).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch() -> BranchInput {
        BranchInput {
            push_alive: false,
            endpoint_matches: true,
            branch: RemoteBranch::At("S".into()),
            merge_sha: "S".into(),
            cancel_requested: false,
        }
    }

    #[test]
    fn branch_row_1_push_in_progress() {
        let i = BranchInput {
            push_alive: true,
            ..branch()
        };
        assert_eq!(
            decide_branch(&i),
            BranchDecision::Refuse("push_in_progress")
        );
    }

    #[test]
    fn branch_row_2_endpoint_changed() {
        let i = BranchInput {
            endpoint_matches: false,
            ..branch()
        };
        assert_eq!(
            decide_branch(&i),
            BranchDecision::Refuse("endpoint_changed")
        );
    }

    #[test]
    fn branch_row_3_unreachable() {
        let i = BranchInput {
            branch: RemoteBranch::Unreachable,
            ..branch()
        };
        assert_eq!(
            decide_branch(&i),
            BranchDecision::Refuse("remote_unreachable")
        );
    }

    #[test]
    fn branch_row_4_the_branch_is_at_s() {
        assert_eq!(decide_branch(&branch()), BranchDecision::AwaitMerge);
    }

    #[test]
    fn branch_row_5_nothing_was_pushed() {
        let none = BranchInput {
            branch: RemoteBranch::Absent,
            ..branch()
        };
        assert_eq!(decide_branch(&none), BranchDecision::Ready);
        let cancel = BranchInput {
            cancel_requested: true,
            ..none
        };
        assert_eq!(decide_branch(&cancel), BranchDecision::Cancelled);
    }

    #[test]
    fn branch_row_6_the_branch_is_somewhere_else() {
        let i = BranchInput {
            branch: RemoteBranch::At("X".into()),
            ..branch()
        };
        assert_eq!(decide_branch(&i), BranchDecision::Refuse("branch_diverged"));
    }

    fn input(observation: PrObservation) -> PrInput {
        PrInput {
            observation,
            merge_sha: "S".into(),
            endpoint_matches: true,
            reachable: true,
            merge_in_target: None,
            tree_same: None,
            branch_present: None,
            target_moved: None,
        }
    }

    fn merged(commit: Option<&str>) -> PrObservation {
        PrObservation::Merged {
            merge_commit: commit.map(str::to_owned),
            head: Some("S".into()),
        }
    }

    fn waiting(note: &str) -> PrDecision {
        PrDecision::Wait {
            note: Some(note.to_owned()),
            target_moved: None,
        }
    }

    #[test]
    fn pr_row_1_forge_unreachable() {
        let i = input(PrObservation::Unreachable {
            reason: "gh is not installed or not in PATH".into(),
        });
        assert_eq!(
            decide_pr(&i),
            waiting("forge_unreachable: gh is not installed or not in PATH")
        );
    }

    #[test]
    fn pr_row_2_creation_failed() {
        let i = input(PrObservation::CreateFailed {
            reason: "HTTP 422".into(),
        });
        assert_eq!(decide_pr(&i), waiting("pr_create_failed: HTTP 422"));
    }

    #[test]
    fn pr_row_3_closed_unmerged() {
        let i = input(PrObservation::Closed { head: None });
        assert_eq!(decide_pr(&i), PrDecision::Failed("pr_closed"));
    }

    #[test]
    fn pr_row_4_the_head_changed() {
        let i = PrInput {
            target_moved: Some(3),
            ..input(PrObservation::Open { head: "X".into() })
        };
        assert_eq!(
            decide_pr(&i),
            PrDecision::Wait {
                note: Some("pr_head_changed".into()),
                target_moved: Some(3)
            }
        );
    }

    #[test]
    fn pr_row_5_open_at_s_waits_and_reports_the_drift_without_requeueing() {
        let moved = PrInput {
            target_moved: Some(4),
            ..input(PrObservation::Open { head: "S".into() })
        };
        assert_eq!(
            decide_pr(&moved),
            PrDecision::Wait {
                note: None,
                target_moved: Some(4)
            }
        );
        let still = input(PrObservation::Open { head: "S".into() });
        assert_eq!(
            decide_pr(&still),
            PrDecision::Wait {
                note: None,
                target_moved: None
            }
        );
    }

    #[test]
    fn pr_rows_6_to_9_no_pull_request() {
        let missing = || input(PrObservation::Missing);
        let i = PrInput {
            endpoint_matches: false,
            ..missing()
        };
        assert_eq!(decide_pr(&i), waiting("endpoint_changed"));
        let i = PrInput {
            reachable: false,
            ..missing()
        };
        assert_eq!(decide_pr(&i), waiting("remote_unreachable"));
        let i = PrInput {
            branch_present: None,
            ..missing()
        };
        assert_eq!(decide_pr(&i), waiting("remote_unreachable"));
        let i = PrInput {
            branch_present: Some(false),
            ..missing()
        };
        assert_eq!(decide_pr(&i), PrDecision::Failed("pr_closed"));
        let i = PrInput {
            branch_present: Some(true),
            ..missing()
        };
        assert_eq!(decide_pr(&i), waiting("pr_not_opened"));
    }

    #[test]
    fn pr_rows_10_to_14_merged_but_not_provably_landed() {
        let i = input(merged(None));
        assert_eq!(decide_pr(&i), waiting("merge_commit_unknown"));
        let i = PrInput {
            endpoint_matches: false,
            ..input(merged(Some("M")))
        };
        assert_eq!(decide_pr(&i), waiting("endpoint_changed"));
        let i = PrInput {
            reachable: false,
            ..input(merged(Some("M")))
        };
        assert_eq!(decide_pr(&i), waiting("remote_unreachable"));
        let i = PrInput {
            merge_in_target: Some(false),
            ..input(merged(Some("M")))
        };
        assert_eq!(decide_pr(&i), waiting("merge_commit_not_on_target"));
        let i = input(merged(Some("M")));
        assert_eq!(decide_pr(&i), waiting("ancestry_unknown"));
    }

    #[test]
    fn pr_rows_15_to_17_merged_and_on_the_target() {
        let on_target = |tree| PrInput {
            merge_in_target: Some(true),
            tree_same: tree,
            ..input(merged(Some("M")))
        };
        assert_eq!(decide_pr(&on_target(Some(true))), PrDecision::Landed(None));
        assert_eq!(
            decide_pr(&on_target(Some(false))),
            PrDecision::Landed(Some("tree_differs_from_verified"))
        );
        assert_eq!(
            decide_pr(&on_target(None)),
            PrDecision::Landed(Some("tree_unchecked"))
        );
    }
}
