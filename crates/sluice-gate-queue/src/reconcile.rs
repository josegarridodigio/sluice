//! `queue reconcile`: the decision table of design section 6 as a pure
//! function ([`decide`]) over facts gathered by [`gather`]. Reconcile only
//! reads the remote (`ls-remote`, `fetch` into Sluice's own ref); it never
//! pushes.

use std::path::Path;

use crate::git::Git;
use crate::target::endpoint_hash;

/// Facts about one `needs_reconcile` train.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a table of independent facts"
)]
pub struct ReconcileInput {
    /// Local target (rows 1–3 do not apply).
    pub local: bool,
    /// The recorded push process is still alive.
    pub push_alive: bool,
    /// The remote resolves to the recorded endpoint.
    pub endpoint_matches: bool,
    /// `ls-remote` (or the local ref read) succeeded.
    pub reachable: bool,
    /// Current target SHA (`None` = the branch does not exist).
    pub target: Option<String>,
    /// Landing commit `S`.
    pub merge_sha: String,
    /// Base the train was built on.
    pub base_sha: String,
    /// The audit ref `refs/sluice/landed/<S>` exists.
    pub audit_ref: bool,
    /// `S` is an ancestor of the target; `None` = cannot be determined.
    pub s_in_target: Option<bool>,
    /// A cancel is pending on a member.
    pub cancel_requested: bool,
}

/// Outcome of [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Unchanged; the reason token.
    Refuse(&'static str),
    /// The train landed (optional note).
    Landed(Option<&'static str>),
    /// Nothing landed, target unchanged: back to `ready`, approval voided.
    Ready,
    /// Nothing landed, target moved: dissolved, members re-queued.
    Queued,
    /// Nothing landed and a cancel was pending.
    Cancelled,
}

/// The decision table; the first matching row wins.
#[must_use]
pub fn decide(i: &ReconcileInput) -> Decision {
    // Rows 1–3 (remote targets only).
    if !i.local {
        if i.push_alive {
            return Decision::Refuse("push_in_progress");
        }
        if !i.endpoint_matches {
            return Decision::Refuse("endpoint_changed");
        }
    }
    if !i.reachable {
        return Decision::Refuse("remote_unreachable");
    }
    let target = i.target.as_deref();
    // Row 4.
    if target == Some(i.merge_sha.as_str()) {
        return Decision::Landed(None);
    }
    // Row 5.
    if i.audit_ref {
        let note = (i.s_in_target == Some(false)).then_some("rewritten_after_landing");
        return Decision::Landed(note);
    }
    // Row 6.
    if i.s_in_target == Some(true) {
        return Decision::Landed(None);
    }
    // Rows 7 and 8.
    if target == Some(i.base_sha.as_str()) {
        return if i.cancel_requested {
            Decision::Cancelled
        } else {
            Decision::Ready
        };
    }
    // Row 9.
    if i.s_in_target == Some(false) {
        return Decision::Queued;
    }
    // Row 10.
    Decision::Refuse("ancestry_unknown")
}

/// What [`gather`] needs to know about the train.
#[derive(Debug, Clone)]
pub struct TrainFacts<'a> {
    /// Git common dir.
    pub repo: &'a Path,
    /// Remote (`""` = local).
    pub remote: &'a str,
    /// Target branch.
    pub target: &'a str,
    /// Recorded endpoint hash.
    pub endpoint_hash: &'a str,
    /// `S`.
    pub merge_sha: &'a str,
    /// Base.
    pub base_sha: &'a str,
    /// Recorded push pid and start ticks.
    pub push: Option<(u32, Option<u64>)>,
    /// A member has a pending cancel.
    pub cancel_requested: bool,
}

/// Queries the remote (or the local refs) and builds the input of [`decide`].
/// Blocking (git).
#[must_use]
pub fn gather(git: &Git, f: &TrainFacts<'_>) -> ReconcileInput {
    let local = f.remote.is_empty();
    let mut input = ReconcileInput {
        local,
        merge_sha: f.merge_sha.to_owned(),
        base_sha: f.base_sha.to_owned(),
        cancel_requested: f.cancel_requested,
        endpoint_matches: true,
        ..ReconcileInput::default()
    };
    let head = format!("refs/heads/{}", f.target);
    let audit = format!("refs/sluice/landed/{}", f.merge_sha);
    if local {
        input.reachable = true;
        input.target = git.ref_sha(f.repo, &head).ok().flatten();
        input.audit_ref = git.ref_sha(f.repo, &audit).ok().flatten().is_some();
    } else {
        input.push_alive = f
            .push
            .is_some_and(|(pid, ticks)| sluice_gate_daemon::executor::process_matches(pid, ticks));
        if input.push_alive {
            return input;
        }
        input.endpoint_matches =
            endpoint_hash(git, f.repo, f.remote).is_ok_and(|h| h == f.endpoint_hash);
        if !input.endpoint_matches {
            return input;
        }
        match git.ls_remote(f.repo, f.remote, &[&head, &audit]) {
            Ok(refs) => {
                input.reachable = true;
                input.target = refs.get(&head).cloned();
                input.audit_ref = refs.contains_key(&audit);
            }
            Err(_) => return input,
        }
    }
    input.s_in_target = match input.target.as_deref() {
        None => Some(false),
        Some(t) if t == f.merge_sha => Some(true),
        Some(t) if t == f.base_sha => {
            // S is a descendant of base; it can only be in the target if equal.
            Some(f.merge_sha == f.base_sha)
        }
        Some(t) => ancestry(git, f, t),
    };
    input
}

fn ancestry(git: &Git, f: &TrainFacts<'_>, target_sha: &str) -> Option<bool> {
    if !f.remote.is_empty() {
        let has = git
            .resolve_commit(f.repo, target_sha)
            .ok()
            .flatten()
            .is_some();
        if !has {
            let spec = format!(
                "+refs/heads/{t}:refs/sluice/base/{r}/{t}",
                t = f.target,
                r = f.remote
            );
            git.repo(f.repo, &["fetch", "--no-tags", "--quiet", f.remote, &spec])
                .ok()?;
        }
    }
    git.is_ancestor(f.repo, f.merge_sha, target_sha)
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> ReconcileInput {
        ReconcileInput {
            local: false,
            push_alive: false,
            endpoint_matches: true,
            reachable: true,
            target: Some("B".into()),
            merge_sha: "S".into(),
            base_sha: "B".into(),
            audit_ref: false,
            s_in_target: Some(false),
            cancel_requested: false,
        }
    }

    #[test]
    fn row_1_push_in_progress() {
        let i = ReconcileInput {
            push_alive: true,
            target: Some("S".into()),
            ..base()
        };
        assert_eq!(decide(&i), Decision::Refuse("push_in_progress"));
        // Not for local targets.
        let i = ReconcileInput { local: true, ..i };
        assert_eq!(decide(&i), Decision::Landed(None));
    }

    #[test]
    fn row_2_endpoint_changed() {
        let i = ReconcileInput {
            endpoint_matches: false,
            ..base()
        };
        assert_eq!(decide(&i), Decision::Refuse("endpoint_changed"));
    }

    #[test]
    fn row_3_unreachable() {
        let i = ReconcileInput {
            reachable: false,
            target: None,
            ..base()
        };
        assert_eq!(decide(&i), Decision::Refuse("remote_unreachable"));
    }

    #[test]
    fn row_4_target_is_s() {
        let i = ReconcileInput {
            target: Some("S".into()),
            s_in_target: Some(true),
            ..base()
        };
        assert_eq!(decide(&i), Decision::Landed(None));
    }

    #[test]
    fn row_5_audit_ref() {
        let i = ReconcileInput {
            target: Some("X".into()),
            audit_ref: true,
            s_in_target: Some(false),
            ..base()
        };
        assert_eq!(
            decide(&i),
            Decision::Landed(Some("rewritten_after_landing"))
        );
        let i = ReconcileInput {
            s_in_target: Some(true),
            ..i
        };
        assert_eq!(decide(&i), Decision::Landed(None));
        // Audit ref wins over a cancel and over a target back at base.
        let i = ReconcileInput {
            target: Some("B".into()),
            cancel_requested: true,
            s_in_target: Some(false),
            audit_ref: true,
            ..base()
        };
        assert_eq!(
            decide(&i),
            Decision::Landed(Some("rewritten_after_landing"))
        );
    }

    #[test]
    fn row_6_pushed_on_top() {
        let i = ReconcileInput {
            target: Some("X".into()),
            s_in_target: Some(true),
            ..base()
        };
        assert_eq!(decide(&i), Decision::Landed(None));
    }

    #[test]
    fn row_7_cancelled() {
        let i = ReconcileInput {
            cancel_requested: true,
            ..base()
        };
        assert_eq!(decide(&i), Decision::Cancelled);
    }

    #[test]
    fn row_8_ready() {
        assert_eq!(decide(&base()), Decision::Ready);
        let local = ReconcileInput {
            local: true,
            push_alive: true,
            endpoint_matches: false,
            ..base()
        };
        assert_eq!(decide(&local), Decision::Ready);
    }

    #[test]
    fn row_9_target_moved() {
        let i = ReconcileInput {
            target: Some("X".into()),
            ..base()
        };
        assert_eq!(decide(&i), Decision::Queued);
        let deleted = ReconcileInput {
            target: None,
            ..base()
        };
        assert_eq!(decide(&deleted), Decision::Queued);
    }

    #[test]
    fn row_10_ancestry_unknown() {
        let i = ReconcileInput {
            target: Some("X".into()),
            s_in_target: None,
            ..base()
        };
        assert_eq!(decide(&i), Decision::Refuse("ancestry_unknown"));
    }
}
