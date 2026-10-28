//! Freezes the `queue-status` reply (and `sluice queue status --json`)
//! schema 1, closing gap G8 of the compatibility design.
//!
//! The reference is the real output of the v0.2.0 release binary
//! (`tests/fixtures/compat/v0.2.0/queue-status/`). Parsing it and writing it
//! back must reproduce it exactly: a renamed, removed or re-nested field fails
//! this test, and so does a field that is suddenly always emitted (additive
//! fields stay optional and omitted when absent, like `queue` in the status
//! schema). A change of meaning bumps [`QUEUE_STATUS_SCHEMA_VERSION`] and gets
//! a new fixture directory.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use sluice_gate_proto::{
    QUEUE_STATUS_SCHEMA_VERSION, QueueJobState, QueueStatusReport, QueueTrainState,
    ReconcileDecision,
};

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compat/v0.2.0/queue-status")
        .join(format!("{name}.json"));
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn schema_version_is_frozen() {
    assert_eq!(QUEUE_STATUS_SCHEMA_VERSION, 1);
}

#[test]
fn queue_status_round_trips_exactly() {
    for name in ["empty", "populated"] {
        let old = fixture(name);
        let parsed: QueueStatusReport = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            old,
            "the queue status schema changed ({name}); if deliberate, bump the \
             schema and add a new fixture directory"
        );
    }
}

#[test]
fn state_spellings_are_frozen() {
    let jobs = [
        (QueueJobState::Queued, "queued"),
        (QueueJobState::Merging, "merging"),
        (QueueJobState::Verifying, "verifying"),
        (QueueJobState::Ready, "ready"),
        (QueueJobState::Approved, "approved"),
        (QueueJobState::Pushing, "pushing"),
        (QueueJobState::Landed, "landed"),
        (QueueJobState::Conflict, "conflict"),
        (QueueJobState::Failed, "failed"),
        (QueueJobState::Cancelled, "cancelled"),
        (QueueJobState::NeedsReconcile, "needs_reconcile"),
        (QueueJobState::AwaitingMerge, "awaiting_merge"),
    ];
    for (state, text) in jobs {
        assert_eq!(serde_json::to_value(state).unwrap(), json!(text));
        assert_eq!(
            serde_json::from_value::<QueueJobState>(json!(text)).unwrap(),
            state
        );
    }
    let trains = [
        (QueueTrainState::Building, "building"),
        (QueueTrainState::Verifying, "verifying"),
        (QueueTrainState::Ready, "ready"),
        (QueueTrainState::Approved, "approved"),
        (QueueTrainState::Pushing, "pushing"),
        (QueueTrainState::Landed, "landed"),
        (QueueTrainState::Dissolved, "dissolved"),
        (QueueTrainState::NeedsReconcile, "needs_reconcile"),
        (QueueTrainState::AwaitingMerge, "awaiting_merge"),
    ];
    for (state, text) in trains {
        assert_eq!(serde_json::to_value(state).unwrap(), json!(text));
        assert_eq!(
            serde_json::from_value::<QueueTrainState>(json!(text)).unwrap(),
            state
        );
    }
    let decisions = [
        (ReconcileDecision::Landed, "landed"),
        (ReconcileDecision::Ready, "ready"),
        (ReconcileDecision::Queued, "queued"),
        (ReconcileDecision::Cancelled, "cancelled"),
        (ReconcileDecision::Refused, "refused"),
        (ReconcileDecision::AwaitingMerge, "awaiting_merge"),
        (ReconcileDecision::Failed, "failed"),
    ];
    for (decision, text) in decisions {
        assert_eq!(serde_json::to_value(decision).unwrap(), json!(text));
    }
}

#[test]
fn unknown_states_from_a_newer_daemon_parse_as_unknown() {
    assert_eq!(
        serde_json::from_value::<QueueJobState>(json!("from-the-future")).unwrap(),
        QueueJobState::Unknown
    );
    assert_eq!(
        serde_json::from_value::<QueueTrainState>(json!("from-the-future")).unwrap(),
        QueueTrainState::Unknown
    );
    assert_eq!(
        serde_json::from_value::<ReconcileDecision>(json!("from-the-future")).unwrap(),
        ReconcileDecision::Unknown
    );
}

/// ADR-0025: a daemon that lands trains through pull requests adds optional
/// fields; the views a client of this version sends back and forth stay
/// readable without them (older daemons) and with them (older clients ignore
/// them).
#[test]
fn pull_request_fields_are_additive() {
    use sluice_gate_proto::{PullRequestView, QueueTrainView};
    let old: QueueTrainView = serde_json::from_value(json!({
        "train_id": "t-1", "state": "landed", "members": []
    }))
    .unwrap();
    assert_eq!((old.land, old.pr), (None, None));
    let view = QueueTrainView {
        train_id: "t-2".into(),
        state: QueueTrainState::AwaitingMerge,
        land: Some("pull-request".into()),
        pr: Some(PullRequestView {
            branch: "sluice/train/t-2".into(),
            number: Some(7),
            ..PullRequestView::default()
        }),
        ..QueueTrainView::default()
    };
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(wire["state"], "awaiting_merge");
    assert_eq!(wire["pr"]["branch"], "sluice/train/t-2");
    assert!(wire["pr"].get("url").is_none(), "{wire}");
    assert_eq!(
        serde_json::from_value::<QueueTrainView>(wire).unwrap(),
        view
    );
    // A train without a pull request does not grow the JSON.
    let plain = serde_json::to_value(QueueTrainView::default()).unwrap();
    assert!(
        plain.get("pr").is_none() && plain.get("land").is_none(),
        "{plain}"
    );
}
