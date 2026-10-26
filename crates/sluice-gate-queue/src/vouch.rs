//! The queue's answer to its own push's `pre-push` hook
//! (`Extension::vouch_run`; ADR-0018 section 3, design section 6).
//!
//! While the daemon pushes an approved train, the repository's `pre-push`
//! hook may run `sluice run --hook pre-push <gate>`. Re-deriving the cache
//! key in the push's environment is fragile (`PATH`-dependent `key_inputs`,
//! `CI`), so the daemon states a fact instead: the request comes from the
//! push it spawned, in the worktree it locked, which still holds the tree it
//! verified; the train's own record says whether it ran the gate.
//!
//! Every precondition that fails other than the gate check sends the request
//! the normal way (`None`): this module never makes a gate pass anywhere but
//! in the train's push.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sluice_gate_daemon::executor::process_matches;
use sluice_gate_daemon::server::Vouch;
use sluice_gate_proto::{QueueTrainState as T, RunRequest, TrainGate};

use crate::runner::Core;
use crate::store::{self, MarkerRow, TrainRow};

/// Longest ancestry walk (pids between the peer and the push).
const MAX_DEPTH: usize = 64;

/// How long to wait for the push's pid to be recorded in the marker (it is
/// written right after the spawn, a hook can ask before that).
const PID_WAIT: Duration = Duration::from_secs(2);

/// Parent pid from the content of `/proc/<pid>/stat`: field 4, counted after
/// the last `)` because the command name may hold spaces and parentheses.
pub(crate) fn parse_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Parent of `pid` according to `/proc`; `None` when it cannot be read.
pub(crate) fn parent_pid(pid: u32) -> Option<u32> {
    parse_ppid(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// True when `ancestor` is a strict ancestor of `pid`, following `parent_of`
/// for at most [`MAX_DEPTH`] steps and stopping at pid 1.
pub(crate) fn descends_from(
    pid: u32,
    ancestor: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
) -> bool {
    let mut cur = pid;
    for _ in 0..MAX_DEPTH {
        if cur <= 1 {
            return false;
        }
        let Some(parent) = parent_of(cur) else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        cur = parent;
    }
    false
}

/// A `pushing` train for the request's worktree and repository.
struct Found {
    train: TrainRow,
    marker: Option<MarkerRow>,
    /// A member changes `.sluice.toml`.
    config_changed: bool,
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn find(core: &Core, request: &RunRequest) -> Result<Option<Found>, String> {
    let trains = core
        .read(|tx| {
            let mut out = Vec::new();
            for t in store::live_trains(tx, None)? {
                if t.state != T::Pushing {
                    continue;
                }
                let marker = store::marker(tx, &t.id)?;
                let config_changed = store::train_jobs(tx, &t)?.iter().any(|j| {
                    j.detail
                        .as_ref()
                        .and_then(|d| d.get("config_changed"))
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                });
                // A trusted train ran its member's own config, not the base's.
                let config_changed = config_changed && !crate::runner::is_trusted(&t);
                out.push(Found {
                    train: t,
                    marker,
                    config_changed,
                });
            }
            Ok(out)
        })
        .map_err(|e| e.to_string())?;
    if trains.is_empty() {
        return Ok(None);
    }
    let (top, common) = (
        canonical(&request.repo.toplevel),
        canonical(&request.repo.common_dir),
    );
    Ok(trains
        .into_iter()
        .find(|f| canonical(&f.train.worktree) == top && canonical(&f.train.repo) == common))
}

/// What the daemon's own look at the merge worktree found.
enum Worktree {
    /// `HEAD == S` and no tracked file differs from it.
    Verified,
    /// It holds something else; the text says what.
    Differs(String),
    /// Could not be decided; the request goes the normal way.
    Unknown(String),
}

/// `HEAD == S` and no tracked change, the same rule as the pre-push checks:
/// untracked or ignored output a gate left (W2 records it as `not cached`)
/// does not change the commit that is pushed, so it must not refuse the push.
fn check_worktree(core: &Core, t: &TrainRow) -> Worktree {
    let Some(s) = &t.merge_sha else {
        return Worktree::Unknown("the train has no merge commit".into());
    };
    let head = match core.git.wt(&t.worktree, &["rev-parse", "HEAD"]) {
        Ok(h) => h,
        Err(e) => return Worktree::Unknown(format!("{e:#}")),
    };
    if head != *s {
        return Worktree::Differs(format!("HEAD {head} is not the merge commit {s}"));
    }
    match core.git.wt(
        &t.worktree,
        &["status", "--porcelain", "--untracked-files=no"],
    ) {
        Ok(st) if st.is_empty() => Worktree::Verified,
        Ok(st) => Worktree::Differs(format!(
            "tracked files differ from the merge commit: {}",
            st.lines().take(5).collect::<Vec<_>>().join(", ")
        )),
        Err(e) => Worktree::Unknown(format!("{e:#}")),
    }
}

/// The verdict on `gate` from the train's own record.
pub(crate) fn judge(
    train_id: &str,
    gates: &[TrainGate],
    gate: &str,
    config_changed: bool,
) -> Vouch {
    if let Some(g) = gates
        .iter()
        .find(|g| g.gate == gate && matches!(g.status.as_str(), "pass" | "cached"))
    {
        return Vouch::Verified {
            train_id: train_id.to_owned(),
            job_id: g.job_id.clone(),
        };
    }
    let why = if config_changed {
        "a member of the train changes .sluice.toml and the train runs the gates of the \
         base's .sluice.toml; land the configuration change first"
    } else {
        "add it to `[queue] gates` in .sluice.toml (or give it hooks = [\"pre-push\"]) so \
         the train verifies it"
    };
    Vouch::Refused {
        message: format!(
            "gate '{gate}' runs in the pre-push hook but merge train {train_id} did not verify it: {why}"
        ),
    }
}

/// Answers a hook `run` request that comes from a train's push, or `None`.
pub(crate) async fn vouch(
    core: &Arc<Core>,
    request: &RunRequest,
    peer_pid: Option<u32>,
) -> Option<Vouch> {
    let peer = peer_pid?;
    let deadline = tokio::time::Instant::now() + PID_WAIT;
    let found = loop {
        let (c2, r2) = (Arc::clone(core), request.clone());
        let found = match tokio::task::spawn_blocking(move || find(&c2, &r2))
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        {
            Ok(Some(f)) => f,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!("push hook not vouched, cannot read the queue: {e}");
                return None;
            }
        };
        let recorded = found.marker.as_ref().is_some_and(|m| m.push_pid.is_some());
        if recorded || tokio::time::Instant::now() >= deadline {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let marker = found.marker.as_ref()?;
    let push = marker.push_pid?;
    let ticks = marker.push_start_ticks;
    let from_push = tokio::task::spawn_blocking(move || {
        process_matches(push, ticks) && descends_from(peer, push, parent_pid)
    })
    .await
    .unwrap_or(false);
    if !from_push {
        return None;
    }
    let (c2, t2) = (Arc::clone(core), found.train.clone());
    let state = match tokio::task::spawn_blocking(move || check_worktree(&c2, &t2)).await {
        Ok(s) => s,
        Err(e) => Worktree::Unknown(e.to_string()),
    };
    let id = &found.train.id;
    match state {
        Worktree::Verified => Some(judge(
            id,
            &found.train.gates,
            &request.gate.name,
            found.config_changed,
        )),
        Worktree::Differs(what) => Some(Vouch::Refused {
            message: format!(
                "the merge worktree of train {id} no longer holds the verified tree ({what}); \
                 the push is refused"
            ),
        }),
        Worktree::Unknown(why) => {
            tracing::warn!(train = %id, "push hook not vouched, worktree check inconclusive: {why}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::process::Command;

    #[test]
    fn ppid_is_read_after_the_last_parenthesis() {
        assert_eq!(parse_ppid("42 (bash) S 7 42 42 0 -1"), Some(7));
        assert_eq!(parse_ppid("42 (a) b (c)) S 9 1 1"), Some(9));
        assert_eq!(parse_ppid("garbage"), None);
        assert_eq!(parse_ppid("42 (x) S"), None);
    }

    #[test]
    fn the_walk_follows_parents_stops_at_init_and_is_bounded() {
        let parents: HashMap<u32, u32> = HashMap::from([(30, 20), (20, 10), (10, 1)]);
        let parent = |p: u32| parents.get(&p).copied();
        assert!(descends_from(30, 20, parent));
        assert!(descends_from(30, 10, parent));
        assert!(!descends_from(30, 30, parent), "strict ancestors only");
        assert!(!descends_from(30, 99, parent));
        assert!(!descends_from(1, 5, parent), "stops at pid 1");
        assert!(!descends_from(30, 20, |_| None), "unreadable parent");
        // A cycle never loops forever.
        assert!(!descends_from(5, 99, |p| Some(if p == 5 { 6 } else { 5 })));
        // Deeper than the bound.
        assert!(!descends_from(1000, 5, |p| Some(p - 1)));
        let bound = u32::try_from(MAX_DEPTH).unwrap();
        assert!(descends_from(5 + bound - 1, 5, |p| Some(p - 1)));
    }

    #[test]
    fn the_real_process_tree_is_walked() {
        let me = std::process::id();
        let parent = std::os::unix::process::parent_id();
        assert!(descends_from(me, parent, parent_pid));
        let mut child = Command::new("sleep").arg("5").spawn().expect("sleep");
        let kid = child.id();
        assert!(
            descends_from(kid, me, parent_pid),
            "a child descends from us"
        );
        assert!(
            !descends_from(me, kid, parent_pid),
            "a child is not our ancestor"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    fn gate(name: &str, status: &str, job: Option<&str>) -> TrainGate {
        TrainGate {
            gate: name.into(),
            status: status.into(),
            job_id: job.map(str::to_owned),
            ..TrainGate::default()
        }
    }

    #[test]
    fn a_gate_the_train_verified_is_vouched_with_its_job() {
        let gates = [
            gate("a", "pass", Some("j1")),
            gate("b", "cached", None),
            gate("c", "fail", Some("j3")),
        ];
        assert_eq!(
            judge("t1", &gates, "a", false),
            Vouch::Verified {
                train_id: "t1".into(),
                job_id: Some("j1".into())
            }
        );
        assert!(matches!(
            judge("t1", &gates, "b", true),
            Vouch::Verified { job_id: None, .. }
        ));
    }

    #[test]
    fn an_unverified_gate_is_refused_with_an_actionable_message() {
        let gates = [gate("a", "pass", Some("j1")), gate("c", "fail", None)];
        for name in ["c", "missing"] {
            let Vouch::Refused { message } = judge("t1", &gates, name, false) else {
                panic!("not refused");
            };
            assert!(message.contains(&format!("gate '{name}' runs in the pre-push hook")));
            assert!(message.contains("merge train t1 did not verify it"));
            assert!(message.contains("[queue] gates"), "{message}");
            assert!(!message.contains("changes .sluice.toml"), "{message}");
        }
        let Vouch::Refused { message } = judge("t1", &gates, "missing", true) else {
            panic!("not refused");
        };
        assert!(message.contains("changes .sluice.toml"), "{message}");
        assert!(message.contains("base's"), "{message}");
        assert!(!message.contains("[queue] gates"), "{message}");
    }
}
