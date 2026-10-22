//! Compatibility golden files (ADR-0013) for the wire formats: status JSON,
//! queue status JSON and recorded protocol transcripts of v0.1.0 and v0.2.0.
//!
//! The fixtures under `tests/fixtures/compat/<release>/` come from the release
//! binaries (`scripts/compat/generate.sh`) and are never edited to make a test
//! pass. Tests tagged `#[ignore = "gap Gn: ..."]` state a promise the code does
//! not keep yet; task H3b removes the attribute once the code is fixed.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sluice_gate_proto::{
    ClientMessage, OutcomeStatus, QueueStatusReport, STATUS_SCHEMA_VERSION, ServerMessage,
    StatusReport, decode_client, decode_server,
};

const RELEASES: [&str; 2] = ["v0.1.0", "v0.2.0"];

fn fixtures(release: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compat")
        .join(release)
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// One recorded protocol line.
#[derive(Debug, Clone)]
struct Line {
    conn: u64,
    c2s: bool,
    msg: Value,
}

impl Line {
    fn kind(&self) -> &str {
        self.msg["type"].as_str().unwrap_or("")
    }
}

fn transcript(release: &str, name: &str) -> Vec<Line> {
    let path = fixtures(release)
        .join("transcripts")
        .join(format!("{name}.ndjson"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            Line {
                conn: v["conn"].as_u64().unwrap(),
                c2s: match v["dir"].as_str().unwrap() {
                    "c2s" => true,
                    "s2c" => false,
                    other => panic!("bad dir {other}"),
                },
                msg: v["msg"].clone(),
            }
        })
        .collect()
}

fn transcript_names(release: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(fixtures(release).join("transcripts"))
        .unwrap()
        .map(|e| {
            e.unwrap()
                .file_name()
                .into_string()
                .unwrap()
                .trim_end_matches(".ndjson")
                .to_owned()
        })
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Status JSON
// ---------------------------------------------------------------------------

/// Every path of `old` exists in `new` with the same JSON type. An old `null`
/// may be absent from `new` (an omitted optional field reads the same).
fn assert_superset(old: &Value, new: &Value, path: &str) {
    match (old, new) {
        (Value::Object(o), Value::Object(n)) => {
            for (k, v) in o {
                match n.get(k) {
                    Some(nv) => assert_superset(v, nv, &format!("{path}.{k}")),
                    None => assert!(v.is_null(), "{path}.{k} disappeared from the output"),
                }
            }
        }
        (Value::Array(o), Value::Array(n)) => {
            assert!(n.len() >= o.len(), "{path}: array shrank");
            for (i, (a, b)) in o.iter().zip(n).enumerate() {
                assert_superset(a, b, &format!("{path}[{i}]"));
            }
        }
        (Value::Null, _) => {}
        (a, b) => assert_eq!(
            std::mem::discriminant(a),
            std::mem::discriminant(b),
            "{path}: type changed from {a} to {b}"
        ),
    }
}

#[test]
fn status_json_superset() {
    for release in RELEASES {
        let path = fixtures(release).join("status/status.json");
        let old = read_json(&path);
        assert_eq!(old["schema"], STATUS_SCHEMA_VERSION, "{release}");
        assert!(
            old["running"].as_array().is_some_and(|r| !r.is_empty()),
            "{release}: a running job was recorded"
        );
        let parsed: StatusReport = serde_json::from_value(old.clone())
            .unwrap_or_else(|e| panic!("{release}: status.json no longer parses: {e}"));
        assert_eq!(parsed.schema, 1);
        let now = serde_json::to_value(&parsed).unwrap();
        assert_superset(&old, &now, &format!("{release}/status"));
    }
}

#[test]
fn status_without_daemon_json_is_frozen() {
    for release in RELEASES {
        let old = read_json(&fixtures(release).join("status/status-no-daemon.json"));
        assert_eq!(old, json!({"schema": 1, "daemon": null}), "{release}");
    }
}

#[test]
fn status_replies_in_transcripts_are_status_reports() {
    for release in RELEASES {
        let lines = transcript(release, "status");
        let reply = lines
            .iter()
            .find(|l| !l.c2s && l.kind() == "status")
            .unwrap();
        let report: StatusReport = serde_json::from_value(reply.msg.clone()).unwrap();
        assert_eq!(report.schema, 1);
        assert!(report.daemon.version.starts_with("0."));
        assert_superset(
            &reply.msg,
            &{
                let mut v = serde_json::to_value(&report).unwrap();
                v["type"] = json!("status");
                v["id"] = reply.msg["id"].clone();
                v
            },
            "status reply",
        );
    }
}

// ---------------------------------------------------------------------------
// Queue status JSON (since v0.2.0)
// ---------------------------------------------------------------------------

#[test]
fn queue_status_json_superset() {
    for file in ["empty", "populated"] {
        let path = fixtures("v0.2.0").join(format!("queue-status/{file}.json"));
        let old = read_json(&path);
        assert_eq!(old["schema"], 1);
        let parsed: QueueStatusReport = serde_json::from_value(old.clone())
            .unwrap_or_else(|e| panic!("queue-status/{file}.json no longer parses: {e}"));
        let now = serde_json::to_value(&parsed).unwrap();
        assert_superset(&old, &now, &format!("queue-status/{file}"));
    }
}

#[test]
fn queue_status_populated_has_a_ready_train() {
    let old = read_json(&fixtures("v0.2.0").join("queue-status/populated.json"));
    let parsed: QueueStatusReport = serde_json::from_value(old).unwrap();
    assert_eq!(parsed.trains.len(), 1);
    assert_eq!(parsed.jobs.len(), 1);
    assert_eq!(format!("{:?}", parsed.trains[0].state), "Ready");
    assert_eq!(parsed.trains[0].gates[0].status, "pass");
}

// ---------------------------------------------------------------------------
// Transcripts
// ---------------------------------------------------------------------------

#[test]
fn transcripts_decode() {
    for release in RELEASES {
        let names = transcript_names(release);
        for expected in [
            "hello-welcome",
            "run-pass",
            "run-fail",
            "run-hit-event",
            "status",
            "logs",
            "stop",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "{release}: missing transcript {expected}"
            );
        }
        for name in names {
            for l in transcript(release, &name) {
                let line = serde_json::to_string(&l.msg).unwrap();
                if l.c2s {
                    decode_client(&line)
                        .unwrap_or_else(|e| panic!("{release}/{name}: client line {line}: {e}"));
                } else {
                    decode_server(&line)
                        .unwrap_or_else(|e| panic!("{release}/{name}: server line {line}: {e}"));
                }
            }
        }
    }
}

#[test]
fn welcome_features_follow_the_release() {
    for (release, features) in [("v0.1.0", None), ("v0.2.0", Some(json!(["queue"])))] {
        let lines = transcript(release, "hello-welcome");
        let welcome = lines.iter().find(|l| !l.c2s).unwrap();
        assert_eq!(welcome.kind(), "welcome");
        assert_eq!(welcome.msg["protocol"], 1);
        assert_eq!(welcome.msg.get("features"), features.as_ref(), "{release}");
        let ServerMessage::Welcome { features: f, .. } =
            decode_server(&serde_json::to_string(&welcome.msg).unwrap()).unwrap()
        else {
            panic!()
        };
        assert_eq!(f.is_empty(), features.is_none());
    }
}

#[test]
fn recorded_run_outcomes_decode_with_the_documented_statuses() {
    let expected = [
        ("run-pass", OutcomeStatus::Pass),
        ("run-fail", OutcomeStatus::Fail),
        ("run-timeout", OutcomeStatus::Timeout),
        ("run-keyed", OutcomeStatus::Pass),
    ];
    for release in RELEASES {
        for (name, status) in expected {
            let lines = transcript(release, name);
            let done = lines.iter().find(|l| !l.c2s && l.kind() == "done").unwrap();
            let ServerMessage::Done { outcome, .. } =
                decode_server(&serde_json::to_string(&done.msg).unwrap()).unwrap()
            else {
                panic!()
            };
            assert_eq!(outcome.status, status, "{release}/{name}");
            let kinds: Vec<_> = lines
                .iter()
                .filter(|l| !l.c2s && l.conn == done.conn)
                .map(Line::kind)
                .filter(|k| *k != "progress")
                .collect();
            assert_eq!(
                kinds,
                ["welcome", "queued", "started", "done"],
                "{release}/{name}"
            );
        }
    }
}

#[test]
fn run_requests_of_old_clients_decode_to_runs() {
    for release in RELEASES {
        let lines = transcript(release, "run-pass");
        let run = lines.iter().find(|l| l.c2s && l.kind() == "run").unwrap();
        let ClientMessage::Run { request, .. } =
            decode_client(&serde_json::to_string(&run.msg).unwrap()).unwrap()
        else {
            panic!()
        };
        assert_eq!(request.gate.name, "test");
        assert!(request.key.is_some() && request.tree.is_some());
    }
}

// ---------------------------------------------------------------------------
// Unknown values from a newer daemon
// ---------------------------------------------------------------------------

#[test]
#[ignore = "gap G5: fixed by task H3b"]
fn wire_enums_have_an_unknown_fallback() {
    // A newer daemon may add a variant; an older client must still decode
    // the line (the queue enums already do).
    let done = r#"{"type":"done","id":1,"job_id":"j","outcome":{"status":"from-the-future","exit_code":0,"duration_ms":1,"log_path":null,"tail":[],"marker_written":false,"not_cached_reason":null,"coalesced_with":null,"message":""}}"#;
    let error = r#"{"type":"error","id":1,"code":"from-the-future","message":"x"}"#;
    let status = fs::read_to_string(fixtures("v0.2.0").join("status/status.json")).unwrap();
    let mut status: Value = serde_json::from_str(&status).unwrap();
    status["type"] = json!("status");
    status["id"] = json!(1);
    let mut queued = status.clone();
    queued["running"] = json!([]);
    queued["queued"] = json!([{"job_id":"j","repo":"/r","gate":"g","weight":1,"queued_at":"t","waiting_ms":1,"subscribers":1,"reason":"from-the-future"}]);
    let mut caller = status.clone();
    caller["running"][0]["caller"] = json!("from-the-future");
    let broken: Vec<String> = [
        ("OutcomeStatus", done.to_owned()),
        ("ErrorCode", error.to_owned()),
        ("WaitReason", queued.to_string()),
        ("Caller", caller.to_string()),
    ]
    .into_iter()
    .filter_map(|(what, line)| decode_server(&line).err().map(|e| format!("{what}: {e}")))
    .collect();
    assert!(
        broken.is_empty(),
        "unknown variants break decoding: {broken:#?}"
    );
}
