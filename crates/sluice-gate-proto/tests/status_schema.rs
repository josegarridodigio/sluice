//! Freezes the `status` reply (and `sluice status --json`) schema.
//!
//! A fully populated [`StatusReport`] is serialised and compared with the
//! checked-in fixture `fixtures/status-schema-1.json`: renaming, removing or
//! re-nesting a field fails this test. Adding a field also fails it until the
//! fixture is updated deliberately (additive fields keep schema 1; a change of
//! meaning bumps [`STATUS_SCHEMA_VERSION`] and gets a new fixture).

use sluice_gate_proto::{
    BudgetInfo, Caller, DaemonInfo, PsiInfo, QueueSummary, QueuedJob, RunningJob,
    STATUS_SCHEMA_VERSION, StatusReport, Totals, WaitReason,
};

const FIXTURE: &str = include_str!("fixtures/status-schema-1.json");

fn report() -> StatusReport {
    StatusReport {
        schema: STATUS_SCHEMA_VERSION,
        daemon: DaemonInfo {
            instance: "4242-1760000000000".into(),
            pid: 4242,
            started_at: "2026-10-22T10:00:00Z".into(),
            version: "0.1.0".into(),
            socket: "/run/user/1000/sluice/sluice.sock".into(),
            log_level: None,
        },
        budget: BudgetInfo {
            total: 8,
            used: 5,
            psi: PsiInfo {
                enabled: true,
                cpu_some_avg10: Some(12.5),
                memory_some_avg10: Some(0.5),
                blocking: false,
            },
        },
        running: vec![RunningJob {
            job_id: "j-run".into(),
            repo: "/abs/repo".into(),
            gate: "test".into(),
            weight: 4,
            started_at: "2026-10-22T10:01:00Z".into(),
            elapsed_ms: 1200,
            subscribers: 2,
            caller: Caller::Agent,
        }],
        queued: vec![QueuedJob {
            job_id: "j-queued".into(),
            repo: "/abs/repo".into(),
            gate: "lint".into(),
            weight: 1,
            queued_at: "2026-10-22T10:01:30Z".into(),
            waiting_ms: 300,
            subscribers: 1,
            reason: WaitReason::Psi,
        }],
        totals: Totals {
            since: "2026-10-01T00:00:00Z".into(),
            runs: 120,
            passes: 110,
            failures: 8,
            timeouts: 2,
            hits: 430,
            coalesced: 37,
            wait_ms_total: 5000,
            run_ms_total: 900_000,
            time_saved_ms: 3_600_000,
            psi_blocked_ms_total: 42_000,
        },
        queue: None,
    }
}

#[test]
fn schema_version_is_frozen() {
    assert_eq!(STATUS_SCHEMA_VERSION, 1);
}

#[test]
fn status_report_matches_fixture() {
    let actual = serde_json::to_value(report()).unwrap();
    let expected: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(
        actual,
        expected,
        "the status schema changed; if deliberate, update \
         tests/fixtures/status-schema-1.json. Actual:\n{}",
        serde_json::to_string_pretty(&actual).unwrap()
    );
}

#[test]
fn fixture_round_trips() {
    let parsed: StatusReport = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(parsed, report());
}

#[test]
fn queue_summary_is_an_additive_optional_field() {
    let mut with_queue = report();
    with_queue.queue = Some(QueueSummary {
        active: 4,
        ready: 1,
        needs_reconcile: 2,
    });
    let mut actual = serde_json::to_value(&with_queue).unwrap();
    let queue = actual.as_object_mut().unwrap().remove("queue").unwrap();
    assert_eq!(
        queue,
        serde_json::json!({"active": 4, "ready": 1, "needs_reconcile": 2})
    );
    let expected: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(actual, expected);
    let back: StatusReport =
        serde_json::from_value(serde_json::to_value(&with_queue).unwrap()).unwrap();
    assert_eq!(back, with_queue);
}

#[test]
fn log_level_is_an_additive_optional_field() {
    let mut with_level = report();
    with_level.daemon.log_level = Some("debug".into());
    let mut actual = serde_json::to_value(&with_level).unwrap();
    let level = actual["daemon"]
        .as_object_mut()
        .unwrap()
        .remove("log_level")
        .unwrap();
    assert_eq!(level, serde_json::json!("debug"));
    let expected: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(actual, expected);
    let back: StatusReport =
        serde_json::from_value(serde_json::to_value(&with_level).unwrap()).unwrap();
    assert_eq!(back, with_level);
}
