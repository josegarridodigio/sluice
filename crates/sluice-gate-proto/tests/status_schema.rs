//! Freezes the `status` reply (and `sluice status --json`) schema.
//!
//! A fully populated [`StatusReport`] is serialised and compared with the
//! checked-in fixture `fixtures/status-schema-1.json`: renaming, removing or
//! re-nesting a field fails this test. Adding a field also fails it until the
//! fixture is updated deliberately (additive fields keep schema 1; a change of
//! meaning bumps [`STATUS_SCHEMA_VERSION`] and gets a new fixture).

use sluice_gate_proto::{
    BudgetInfo, Caller, DaemonInfo, PsiInfo, QueueSummary, QueueToolEnv, QueuedJob, RunningJob,
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
            ..BudgetInfo::default()
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
            kind: None,
            label: None,
            ..RunningJob::default()
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
            kind: None,
            label: None,
            ..QueuedJob::default()
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
            ..Totals::default()
        },
        queue: None,
        interventions: None,
        containment: None,
        sessions: None,
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
        tool_env: None,
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
fn queue_tool_env_is_an_additive_optional_field() {
    let mut with_env = report();
    with_env.queue = Some(QueueSummary {
        active: 1,
        tool_env: Some(QueueToolEnv {
            source: "daemon".into(),
            reason: Some("zsh exited with exit status: 1".into()),
            path: Some("/usr/bin:/bin".into()),
        }),
        ..QueueSummary::default()
    });
    let mut actual = serde_json::to_value(&with_env).unwrap();
    let queue = actual.as_object_mut().unwrap().remove("queue").unwrap();
    assert_eq!(
        queue["tool_env"],
        serde_json::json!({
            "source": "daemon",
            "reason": "zsh exited with exit status: 1",
            "path": "/usr/bin:/bin"
        })
    );
    let back: StatusReport =
        serde_json::from_value(serde_json::to_value(&with_env).unwrap()).unwrap();
    assert_eq!(back, with_env);
    // Absent fields stay absent, and an older daemon's summary still parses.
    let plain = serde_json::to_value(QueueSummary::default()).unwrap();
    assert!(plain.get("tool_env").is_none(), "{plain}");
    let old: QueueSummary =
        serde_json::from_str(r#"{"active": 1, "ready": 0, "needs_reconcile": 0}"#).unwrap();
    assert_eq!(old.tool_env, None);
}

#[test]
fn exec_kind_and_label_are_additive_optional_fields() {
    let mut with_exec = report();
    with_exec.running[0].kind = Some(sluice_gate_proto::JOB_KIND_EXEC.into());
    with_exec.running[0].label = Some("cargo build".into());
    with_exec.queued[0].kind = Some(sluice_gate_proto::JOB_KIND_EXEC.into());
    with_exec.queued[0].label = Some("make".into());
    let mut actual = serde_json::to_value(&with_exec).unwrap();
    for (list, label) in [("running", "cargo build"), ("queued", "make")] {
        let entry = actual[list][0].as_object_mut().unwrap();
        assert_eq!(entry.remove("kind").unwrap(), serde_json::json!("exec"));
        assert_eq!(entry.remove("label").unwrap(), serde_json::json!(label));
    }
    let expected: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(actual, expected);
    let back: StatusReport =
        serde_json::from_value(serde_json::to_value(&with_exec).unwrap()).unwrap();
    assert_eq!(back, with_exec);
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

/// ADR-0011 (B1, L1): every scheduler field is optional and additive.
#[test]
fn scheduler_fields_are_additive_optional_fields() {
    let mut with = report();
    with.budget.cap = Some(12);
    with.budget.cpus = Some(8);
    with.budget.idle_cpus = Some(2.5);
    with.running[0].weight_cfg = Some(8);
    with.running[0].waited_ms = Some(31_000);
    with.running[0].admission = Some("over".into());
    with.running[0].demand = Some(6.1);
    with.queued[0].lane = Some("/abs/wt".into());
    with.queued[0].rank = Some(1);
    with.queued[0].expected_ms = Some(12_000);
    with.queued[0].weight_cfg = Some(1);
    with.queued[0].wait_detail = Some("outranked".into());
    with.totals.idle_while_waiting_ms = Some(900);
    with.totals.waiting_wall_ms = Some(10_000);
    with.totals.over_admissions = Some(3);
    let mut actual = serde_json::to_value(&with).unwrap();
    for (obj, keys) in [
        ("budget", &["cap", "cpus", "idle_cpus"][..]),
        (
            "totals",
            &[
                "idle_while_waiting_ms",
                "waiting_wall_ms",
                "over_admissions",
            ][..],
        ),
    ] {
        let o = actual[obj].as_object_mut().unwrap();
        for k in keys {
            assert!(o.remove(*k).is_some(), "{obj}.{k}");
        }
    }
    for (list, keys) in [
        (
            "running",
            &["weight_cfg", "waited_ms", "admission", "demand"][..],
        ),
        (
            "queued",
            &["lane", "rank", "expected_ms", "weight_cfg", "wait_detail"][..],
        ),
    ] {
        let o = actual[list][0].as_object_mut().unwrap();
        for k in keys {
            assert!(o.remove(*k).is_some(), "{list}.{k}");
        }
    }
    let expected: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(actual, expected);
    let back: StatusReport = serde_json::from_value(serde_json::to_value(&with).unwrap()).unwrap();
    assert_eq!(back, with);
}

/// ADR-0015: the configured and effective weight fields are optional and
/// additive; schema 1 is unchanged and old reports still parse.
#[test]
fn weight_effective_and_auto_are_additive_optional_fields() {
    let mut with = report();
    with.running[0].weight_effective = Some(4);
    with.running[0].weight_auto = Some(true);
    with.queued[0].weight_effective = Some(1);
    with.queued[0].weight_auto = Some(true);
    let mut actual = serde_json::to_value(&with).unwrap();
    for list in ["running", "queued"] {
        let o = actual[list][0].as_object_mut().unwrap();
        assert_eq!(o.remove("weight_auto"), Some(serde_json::json!(true)));
        assert!(o.remove("weight_effective").is_some(), "{list}");
    }
    let expected: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(actual, expected);
    let back: StatusReport = serde_json::from_value(serde_json::to_value(&with).unwrap()).unwrap();
    assert_eq!(back, with);
    let old: StatusReport = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(old.running[0].weight_auto, None);
    assert_eq!(STATUS_SCHEMA_VERSION, 1);
}

/// Containment (C5): `containment`, `sessions` and `interventions` are
/// optional and additive; schema 1 is unchanged and old reports still parse.
#[test]
fn containment_fields_are_additive_optional_fields() {
    use sluice_gate_proto::{ContainmentInfo, Intervention, OomPolicyInfo, SessionUsage};
    let mut with = report();
    with.containment = Some(ContainmentInfo {
        level: "partial".into(),
        detail: Some("memory, pids; cpu by nice".into()),
        outside_jobs_cpu_percent: Some(3.5),
        window_ms: Some(10_000),
        gates_oom_policy: Some(OomPolicyInfo {
            limit_percent: 30,
            duration_secs: 3,
        }),
    });
    with.sessions = Some(vec![SessionUsage {
        id: "abc".into(),
        cpu_seconds: 12.5,
        memory_bytes: Some(432_013_312),
        memory_peak_bytes: None,
        procs: Some(18),
    }]);
    with.interventions = Some(vec![Intervention {
        at: "2026-10-24T10:00:00Z".into(),
        session: Some("abc".into()),
        leaf: "s-abc/tools".into(),
        cause: "oom_kill".into(),
        detail: None,
    }]);
    let mut actual = serde_json::to_value(&with).unwrap();
    let o = actual.as_object_mut().unwrap();
    assert_eq!(
        o.remove("containment").unwrap(),
        serde_json::json!({
            "level": "partial",
            "detail": "memory, pids; cpu by nice",
            "outside_jobs_cpu_percent": 3.5,
            "window_ms": 10000,
            "gates_oom_policy": {"limit_percent": 30, "duration_secs": 3}
        })
    );
    assert_eq!(
        o.remove("sessions").unwrap(),
        serde_json::json!([{
            "id": "abc", "cpu_seconds": 12.5, "memory_bytes": 432_013_312_u64, "procs": 18
        }])
    );
    assert_eq!(
        o.remove("interventions").unwrap(),
        serde_json::json!([{
            "at": "2026-10-24T10:00:00Z", "session": "abc",
            "leaf": "s-abc/tools", "cause": "oom_kill"
        }])
    );
    let expected: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(actual, expected);
    let back: StatusReport = serde_json::from_value(serde_json::to_value(&with).unwrap()).unwrap();
    assert_eq!(back, with);
    let old: StatusReport = serde_json::from_str(FIXTURE).unwrap();
    assert!(old.containment.is_none() && old.sessions.is_none());
    assert_eq!(STATUS_SCHEMA_VERSION, 1);
}

#[test]
fn containment_text_lists_state_sessions_outside_cpu_and_interventions() {
    use sluice_gate_proto::{ContainmentInfo, Intervention, SessionUsage};
    assert_eq!(report().containment_text(), "");
    let mut r = report();
    r.containment = Some(ContainmentInfo {
        level: "partial".into(),
        detail: Some("memory, pids; cpu by nice".into()),
        outside_jobs_cpu_percent: Some(3.24),
        window_ms: None,
        gates_oom_policy: None,
    });
    r.sessions = Some(vec![SessionUsage {
        id: "abc".into(),
        cpu_seconds: 12.34,
        memory_bytes: Some(412 * 1024 * 1024),
        procs: Some(18),
        ..SessionUsage::default()
    }]);
    r.interventions = Some(vec![Intervention {
        at: "2026-10-24T10:00:00Z".into(),
        session: Some("abc".into()),
        leaf: "s-abc/tools".into(),
        cause: "oom_kill".into(),
        detail: None,
    }]);
    assert_eq!(
        r.containment_text(),
        "containment: partial (memory, pids; cpu by nice)\n\
         \x20 s-abc  cpu 12.3s  mem 412 MB  procs 18\n\
         outside jobs: 3.2% CPU\n\
         interventions:\n\
         \x20 2026-10-24T10:00:00Z  s-abc  s-abc/tools  oom_kill\n"
    );
    r.containment = Some(ContainmentInfo {
        level: "off".into(),
        detail: Some("user manager not reachable".into()),
        ..ContainmentInfo::default()
    });
    assert!(
        r.containment_text()
            .starts_with("containment: off (user manager not reachable)\n")
    );
}

/// The gates' oomd policy (W6c) is an optional addition to `containment`:
/// printed when present, absent from the JSON otherwise.
#[test]
fn the_gates_oom_policy_is_optional_and_printed() {
    use sluice_gate_proto::{ContainmentInfo, OomPolicyInfo};
    let mut r = report();
    r.containment = Some(ContainmentInfo {
        level: "full".into(),
        ..ContainmentInfo::default()
    });
    let json = serde_json::to_value(&r).unwrap();
    assert!(json["containment"].get("gates_oom_policy").is_none());
    assert_eq!(r.containment_text(), "containment: full\n");
    r.containment.as_mut().unwrap().gates_oom_policy = Some(OomPolicyInfo {
        limit_percent: 30,
        duration_secs: 3,
    });
    assert_eq!(
        r.containment_text(),
        "containment: full\ngates memory policy: systemd-oomd at 30% for 3s\n"
    );
}
