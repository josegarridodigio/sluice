//! States, the transition tables of `kb/sluice/design/merge-queue.md`
//! section 3, and the train hash (section 2).
//!
//! The states are the wire enums of `sluice-gate-proto`
//! ([`QueueJobState`], [`QueueTrainState`]); this module adds their
//! database spelling and the rules on which the store's compare-and-swap
//! updates rely.

use sha2::{Digest, Sha256};
use sluice_gate_proto::{QueueJobState as J, QueueTrainState as T};

pub use sluice_gate_proto::{QueueJobState, QueueTrainState};

/// Database (and wire) spelling of a job state.
#[must_use]
pub fn job_state_str(s: QueueJobState) -> &'static str {
    match s {
        J::Queued => "queued",
        J::Merging => "merging",
        J::Verifying => "verifying",
        J::Ready => "ready",
        J::Approved => "approved",
        J::Pushing => "pushing",
        J::Landed => "landed",
        J::Conflict => "conflict",
        J::Failed => "failed",
        J::Cancelled => "cancelled",
        J::NeedsReconcile => "needs_reconcile",
        J::AwaitingMerge => "awaiting_merge",
        J::Unknown => "unknown",
    }
}

/// Parses [`job_state_str`]; unknown spellings give [`QueueJobState::Unknown`].
#[must_use]
pub fn parse_job_state(s: &str) -> QueueJobState {
    ALL_JOB_STATES
        .iter()
        .copied()
        .find(|st| job_state_str(*st) == s)
        .unwrap_or(J::Unknown)
}

/// Database (and wire) spelling of a train state.
#[must_use]
pub fn train_state_str(s: QueueTrainState) -> &'static str {
    match s {
        T::Building => "building",
        T::Verifying => "verifying",
        T::Ready => "ready",
        T::Approved => "approved",
        T::Pushing => "pushing",
        T::Landed => "landed",
        T::Dissolved => "dissolved",
        T::NeedsReconcile => "needs_reconcile",
        T::AwaitingMerge => "awaiting_merge",
        T::Unknown => "unknown",
    }
}

/// Parses [`train_state_str`]; unknown spellings give [`QueueTrainState::Unknown`].
#[must_use]
pub fn parse_train_state(s: &str) -> QueueTrainState {
    ALL_TRAIN_STATES
        .iter()
        .copied()
        .find(|st| train_state_str(*st) == s)
        .unwrap_or(T::Unknown)
}

/// Every known job state.
pub const ALL_JOB_STATES: [QueueJobState; 12] = [
    J::Queued,
    J::Merging,
    J::Verifying,
    J::Ready,
    J::Approved,
    J::Pushing,
    J::Landed,
    J::Conflict,
    J::Failed,
    J::Cancelled,
    J::NeedsReconcile,
    J::AwaitingMerge,
];

/// Every known train state.
pub const ALL_TRAIN_STATES: [QueueTrainState; 9] = [
    T::Building,
    T::Verifying,
    T::Ready,
    T::Approved,
    T::Pushing,
    T::Landed,
    T::Dissolved,
    T::NeedsReconcile,
    T::AwaitingMerge,
];

/// The job transition table (design section 3): `(from, to, trigger)`.
/// The store refuses any other transition.
pub const JOB_TRANSITIONS: &[(QueueJobState, QueueJobState, &str)] = &[
    (J::Queued, J::Merging, "claimed into a train"),
    (J::Merging, J::Verifying, "merge applied cleanly"),
    (J::Merging, J::Conflict, "merge conflicted"),
    (
        J::Merging,
        J::Failed,
        "train failed before verification (tree_mismatch, internal)",
    ),
    (
        J::Merging,
        J::Queued,
        "train dissolved (member cancelled, daemon start)",
    ),
    (J::Verifying, J::Ready, "every gate passed or hit"),
    (
        J::Verifying,
        J::Failed,
        "gate failed or modified the tree, or prepare failed (gate_failed, gate_modified_tree, prepare_failed, prepare_dirty) in a single-member train, or its branch's configuration asks the pre-push hook for gates the train does not verify, or does not parse (hook_gate_unverified, member_config_invalid)",
    ),
    (
        J::Verifying,
        J::Queued,
        "multi-member train failed (split), dissolved, or its key inputs or the prepare of its base failed (key_input_failed, prepare_failed), or another member's configuration asks the pre-push hook for gates the train does not verify (hook_gate_unverified)",
    ),
    (
        J::Ready,
        J::Approved,
        "approved with the matching train hash",
    ),
    (
        J::Ready,
        J::Queued,
        "base moved, member cancelled; approval voided",
    ),
    (
        J::Approved,
        J::Queued,
        "base moved, member cancelled; approval voided",
    ),
    (J::Approved, J::Pushing, "marker and pin ref written"),
    (
        J::Approved,
        J::Failed,
        "local target checked out at landing time",
    ),
    (J::Pushing, J::Landed, "push exit 0 / ref updated"),
    (J::Pushing, J::Queued, "definitive rejection: target moved"),
    (
        J::Pushing,
        J::Failed,
        "definitive rejection: policy, permission, pre-push hook",
    ),
    (
        J::Pushing,
        J::NeedsReconcile,
        "ambiguous push end, or daemon start finds a marker",
    ),
    (
        J::Pushing,
        J::Cancelled,
        "rejected push with a pending cancel",
    ),
    (
        J::Pushing,
        J::AwaitingMerge,
        "pull-request landing: the branch was pushed",
    ),
    (
        J::AwaitingMerge,
        J::Landed,
        "the pull request was merged and its merge commit is on the target",
    ),
    (
        J::AwaitingMerge,
        J::Failed,
        "the pull request was closed without merging, or its branch is gone (pr_closed)",
    ),
    (J::AwaitingMerge, J::Cancelled, "queue cancel"),
    (
        J::AwaitingMerge,
        J::Queued,
        "train dissolved because another member was cancelled",
    ),
    (
        J::NeedsReconcile,
        J::AwaitingMerge,
        "reconcile: the branch of a pull-request landing is on the remote",
    ),
    (J::NeedsReconcile, J::Landed, "reconcile rows 4-6"),
    (J::NeedsReconcile, J::Ready, "reconcile row 8"),
    (J::NeedsReconcile, J::Queued, "reconcile row 9"),
    (J::NeedsReconcile, J::Cancelled, "reconcile row 7"),
    (J::Conflict, J::Queued, "queue retry"),
    (J::Failed, J::Queued, "queue retry"),
    (
        J::Queued,
        J::Failed,
        "no base after 3 attempts (base_unavailable, base_config_invalid, key_input_failed, prepare_failed), or at once a trusted job whose branch config is invalid (member_config_invalid)",
    ),
    (J::Queued, J::Cancelled, "queue cancel"),
    (J::Merging, J::Cancelled, "queue cancel"),
    (J::Verifying, J::Cancelled, "queue cancel"),
    (J::Ready, J::Cancelled, "queue cancel"),
    (J::Approved, J::Cancelled, "queue cancel"),
    (J::Conflict, J::Cancelled, "queue cancel"),
    (J::Failed, J::Cancelled, "queue cancel"),
];

/// The train transition table: `(from, to)`.
pub const TRAIN_TRANSITIONS: &[(QueueTrainState, QueueTrainState)] = &[
    (T::Building, T::Verifying),
    (T::Building, T::Dissolved),
    (T::Verifying, T::Ready),
    (T::Verifying, T::Dissolved),
    (T::Ready, T::Approved),
    (T::Ready, T::Dissolved),
    (T::Approved, T::Pushing),
    (T::Approved, T::Dissolved),
    (T::Pushing, T::Landed),
    (T::Pushing, T::Dissolved),
    (T::Pushing, T::NeedsReconcile),
    (T::Pushing, T::AwaitingMerge),
    (T::AwaitingMerge, T::Landed),
    (T::AwaitingMerge, T::Dissolved),
    (T::NeedsReconcile, T::AwaitingMerge),
    (T::NeedsReconcile, T::Landed),
    (T::NeedsReconcile, T::Ready),
    (T::NeedsReconcile, T::Dissolved),
];

/// True when the job table allows `from → to`.
#[must_use]
pub fn job_transition_allowed(from: QueueJobState, to: QueueJobState) -> bool {
    JOB_TRANSITIONS
        .iter()
        .any(|(f, t, _)| *f == from && *t == to)
}

/// True when the train table allows `from → to`.
#[must_use]
pub fn train_transition_allowed(from: QueueTrainState, to: QueueTrainState) -> bool {
    TRAIN_TRANSITIONS
        .iter()
        .any(|(f, t)| *f == from && *t == to)
}

/// Job states that end the job (`landed`, `cancelled`); `conflict` and
/// `failed` stay until `retry`.
#[must_use]
pub fn job_is_terminal(s: QueueJobState) -> bool {
    matches!(s, J::Landed | J::Cancelled)
}

/// Job states counted as "active" in `status.queue` (not landed,
/// cancelled, conflict or failed).
#[must_use]
pub fn job_is_active(s: QueueJobState) -> bool {
    !matches!(
        s,
        J::Landed | J::Cancelled | J::Conflict | J::Failed | J::Unknown
    )
}

/// Job states before `pushing` in which `cancel` takes effect at once.
#[must_use]
pub fn job_cancellable_now(s: QueueJobState) -> bool {
    job_transition_allowed(s, J::Cancelled) && !matches!(s, J::Pushing | J::NeedsReconcile)
}

/// Train states that occupy their (repo, remote, target): no other train of
/// the same target is built meanwhile. An `awaiting_merge` train occupies it
/// too: what the next train would be built on does not hold the pull request
/// still to merge.
#[must_use]
pub fn train_is_active(s: QueueTrainState) -> bool {
    matches!(
        s,
        T::Building
            | T::Verifying
            | T::Ready
            | T::Approved
            | T::Pushing
            | T::NeedsReconcile
            | T::AwaitingMerge
    )
}

/// Everything the train hash binds (design section 2).
#[derive(Debug, Clone)]
pub struct TrainHashInput<'a> {
    /// Git common dir of the repository.
    pub repo: &'a str,
    /// Remote (`""` for a local target).
    pub remote: &'a str,
    /// Endpoint binding of the remote.
    pub endpoint_hash: &'a str,
    /// Target branch.
    pub target: &'a str,
    /// Base the members were merged onto.
    pub base_sha: &'a str,
    /// `(job id, head sha)` in merge order.
    pub members: &'a [(String, String)],
    /// Landing commit `S`.
    pub merge_sha: &'a str,
    /// Its tree `T`.
    pub merge_tree: &'a str,
    /// `gate=key` entries (any order; sorted here). An uncached gate uses
    /// `gate=-`.
    pub gate_keys: &'a [String],
    /// Hash of the member's `.sluice.toml` blob (or `absent`) for a train built
    /// with `--trust-member-config`; `None` for every ordinary train, whose
    /// hash is then exactly what earlier releases computed.
    pub config_hash: Option<&'a str>,
}

/// `sha256` of the fields of [`TrainHashInput`], each followed by a NUL,
/// with a version prefix; gate keys are sorted first. A train built from its
/// member's own configuration appends the two fields `config` and its config
/// hash after the gate keys; no other train does. Lowercase hex.
#[must_use]
pub fn train_hash(input: &TrainHashInput<'_>) -> String {
    let mut h = Sha256::new();
    let mut field = |s: &str| {
        h.update(s.as_bytes());
        h.update([0u8]);
    };
    field("sluice-train-v1");
    field(input.repo);
    field(input.remote);
    field(input.endpoint_hash);
    field(input.target);
    field(input.base_sha);
    for (job, sha) in input.members {
        field(job);
        field(sha);
    }
    field("|");
    field(input.merge_sha);
    field(input.merge_tree);
    let mut keys = input.gate_keys.to_vec();
    keys.sort();
    for k in &keys {
        field(k);
    }
    if let Some(config) = input.config_hash {
        field("config");
        field(config);
    }
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_spellings_round_trip_and_match_serde() {
        for s in ALL_JOB_STATES {
            assert_eq!(parse_job_state(job_state_str(s)), s);
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", job_state_str(s))
            );
        }
        for s in ALL_TRAIN_STATES {
            assert_eq!(parse_train_state(train_state_str(s)), s);
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", train_state_str(s))
            );
        }
        assert_eq!(parse_job_state("bogus"), J::Unknown);
        assert_eq!(parse_train_state("bogus"), T::Unknown);
    }

    #[test]
    fn job_transition_table() {
        // The happy path.
        for (f, t) in [
            (J::Queued, J::Merging),
            (J::Merging, J::Verifying),
            (J::Verifying, J::Ready),
            (J::Ready, J::Approved),
            (J::Approved, J::Pushing),
            (J::Pushing, J::Landed),
        ] {
            assert!(job_transition_allowed(f, t), "{f:?} -> {t:?}");
        }
        // Side exits and recoveries of section 3.
        for (f, t) in [
            (J::Merging, J::Conflict),
            (J::Verifying, J::Failed),
            (J::Verifying, J::Queued),
            (J::Ready, J::Queued),
            (J::Approved, J::Queued),
            (J::Pushing, J::Queued),
            (J::Pushing, J::Failed),
            (J::Pushing, J::NeedsReconcile),
            (J::NeedsReconcile, J::Landed),
            (J::NeedsReconcile, J::Ready),
            (J::NeedsReconcile, J::Queued),
            (J::NeedsReconcile, J::Cancelled),
            (J::Conflict, J::Queued),
            (J::Failed, J::Queued),
            (J::Merging, J::Queued),
            (J::Pushing, J::AwaitingMerge),
            (J::NeedsReconcile, J::AwaitingMerge),
            (J::AwaitingMerge, J::Landed),
            (J::AwaitingMerge, J::Failed),
            (J::AwaitingMerge, J::Cancelled),
            (J::AwaitingMerge, J::Queued),
        ] {
            assert!(job_transition_allowed(f, t), "{f:?} -> {t:?}");
        }
        // Forbidden: pushing cannot be cancelled directly, terminal states
        // never move, nothing skips verification.
        for (f, t) in [
            (J::Landed, J::Queued),
            (J::Cancelled, J::Queued),
            (J::Queued, J::Ready),
            (J::Merging, J::Ready),
            (J::Ready, J::Pushing),
            (J::Queued, J::Landed),
            (J::Approved, J::Landed),
            (J::NeedsReconcile, J::Pushing),
            (J::Conflict, J::Ready),
            (J::Approved, J::AwaitingMerge),
            (J::AwaitingMerge, J::Pushing),
            (J::AwaitingMerge, J::Ready),
        ] {
            assert!(!job_transition_allowed(f, t), "{f:?} -> {t:?}");
        }
        assert!(job_cancellable_now(J::AwaitingMerge));
        assert!(train_is_active(T::AwaitingMerge));
        assert!(train_transition_allowed(T::Pushing, T::AwaitingMerge));
        assert!(train_transition_allowed(T::AwaitingMerge, T::Landed));
        assert!(train_transition_allowed(T::AwaitingMerge, T::Dissolved));
        assert!(!train_transition_allowed(T::AwaitingMerge, T::Ready));
        assert!(!job_cancellable_now(J::Pushing));
        assert!(!job_cancellable_now(J::NeedsReconcile));
        assert!(!job_cancellable_now(J::Landed));
        for s in [
            J::Queued,
            J::Merging,
            J::Verifying,
            J::Ready,
            J::Approved,
            J::Conflict,
            J::Failed,
        ] {
            assert!(job_cancellable_now(s), "{s:?}");
        }
        // Every pre-pushing state reaches cancelled (section 3, last rows).
        for s in ALL_JOB_STATES {
            if job_is_terminal(s) {
                assert!(
                    ALL_JOB_STATES
                        .iter()
                        .all(|t| !job_transition_allowed(s, *t))
                );
            }
        }
    }

    #[test]
    fn train_transition_table() {
        assert!(train_transition_allowed(T::Building, T::Verifying));
        assert!(train_transition_allowed(T::Approved, T::Pushing));
        assert!(train_transition_allowed(T::NeedsReconcile, T::Ready));
        assert!(!train_transition_allowed(T::Landed, T::Dissolved));
        assert!(!train_transition_allowed(T::Dissolved, T::Ready));
        assert!(!train_transition_allowed(T::Ready, T::Pushing));
        assert!(!train_transition_allowed(T::NeedsReconcile, T::Pushing));
        assert!(train_is_active(T::NeedsReconcile));
        assert!(!train_is_active(T::Dissolved));
        assert!(!train_is_active(T::Landed));
    }

    fn sample<'a>(members: &'a [(String, String)], keys: &'a [String]) -> TrainHashInput<'a> {
        TrainHashInput {
            repo: "/r/.git",
            remote: "origin",
            endpoint_hash: "e",
            target: "main",
            base_sha: "b",
            members,
            merge_sha: "s",
            merge_tree: "t",
            gate_keys: keys,
            config_hash: None,
        }
    }

    #[test]
    fn an_ordinary_train_hash_is_the_encoding_of_earlier_releases() {
        // Computed by hand from the encoding of design section 2: the fields
        // `sluice-train-v1`, repo, remote, endpoint, target, base, each member's
        // job and head, `|`, S, T and the sorted `gate=key` entries, each
        // followed by a NUL byte, through sha256.
        let members = vec![
            ("j1".to_owned(), "h1".to_owned()),
            ("j2".to_owned(), "h2".to_owned()),
        ];
        let keys = vec!["test=k1".to_owned(), "lint=k2".to_owned()];
        assert_eq!(
            train_hash(&sample(&members, &keys)),
            "ae9a22c5a36cd1a8b9c3307bf73c2b2b9b4001d460515a612ff6969339245ff5"
        );
        // A trusted train appends `config` and the config hash.
        let mut trusted = sample(&members, &keys);
        trusted.config_hash = Some("cfg1");
        assert_eq!(
            train_hash(&trusted),
            "5576ff07c65e8f3b4ed39e98065bfb3b2d8fa980c4cec02661de06e564ace8c9"
        );
        trusted.config_hash = Some("cfg2");
        assert_ne!(
            train_hash(&trusted),
            "5576ff07c65e8f3b4ed39e98065bfb3b2d8fa980c4cec02661de06e564ace8c9"
        );
        trusted.config_hash = Some("absent");
        assert_ne!(train_hash(&trusted), train_hash(&sample(&members, &keys)));
    }

    #[test]
    fn train_hash_is_deterministic_and_sensitive() {
        let members = vec![
            ("j1".to_owned(), "h1".to_owned()),
            ("j2".to_owned(), "h2".to_owned()),
        ];
        let keys = vec!["test=k1".to_owned(), "lint=k2".to_owned()];
        let base = train_hash(&sample(&members, &keys));
        assert_eq!(base.len(), 64);
        assert_eq!(base, train_hash(&sample(&members, &keys)));
        // Gate key order does not matter.
        let rev: Vec<String> = keys.iter().rev().cloned().collect();
        assert_eq!(base, train_hash(&sample(&members, &rev)));
        // Every field matters.
        let mut variants = Vec::new();
        let mut s = sample(&members, &keys);
        s.repo = "/other";
        variants.push(train_hash(&s));
        let mut s = sample(&members, &keys);
        s.remote = "upstream";
        variants.push(train_hash(&s));
        let mut s = sample(&members, &keys);
        s.endpoint_hash = "e2";
        variants.push(train_hash(&s));
        let mut s = sample(&members, &keys);
        s.target = "release";
        variants.push(train_hash(&s));
        let mut s = sample(&members, &keys);
        s.base_sha = "b2";
        variants.push(train_hash(&s));
        let mut s = sample(&members, &keys);
        s.merge_sha = "s2";
        variants.push(train_hash(&s));
        let mut s = sample(&members, &keys);
        s.merge_tree = "t2";
        variants.push(train_hash(&s));
        let swapped = vec![members[1].clone(), members[0].clone()];
        variants.push(train_hash(&sample(&swapped, &keys)));
        let moved = vec![("j1".to_owned(), "h1x".to_owned()), members[1].clone()];
        variants.push(train_hash(&sample(&moved, &keys)));
        let other_keys = vec!["test=k1".to_owned(), "lint=k3".to_owned()];
        variants.push(train_hash(&sample(&members, &other_keys)));
        for v in &variants {
            assert_ne!(*v, base);
        }
        let mut uniq = variants.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), variants.len());
    }
}
