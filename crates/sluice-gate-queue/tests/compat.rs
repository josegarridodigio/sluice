//! Compatibility golden files (ADR-0013) for the merge queue: the database
//! written by the v0.1.0 and v0.2.0 binaries, and an old client's queue
//! requests against the current queue.
//!
//! Fixtures: `tests/fixtures/compat/<release>/db/sluice.db` and
//! `queue-status/*.json` (v0.2.0), recorded by `scripts/compat/generate.sh`
//! and never edited to make a test pass.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Daemon, Fixture, user_config};
use rusqlite::Connection;
use serde_json::Value;
use sluice_gate_proto::{QueueJobState, QueueStatusReport, QueueTrainState};
use sluice_gate_queue::store::{self, QUEUE_SCHEMA_VERSION};
use tempfile::TempDir;

fn fixtures(release: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compat")
        .join(release)
}

fn copy_db(release: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sluice.db");
    fs::copy(fixtures(release).join("db/sluice.db"), &db).unwrap();
    (dir, db)
}

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn populated() -> QueueStatusReport {
    let path = fixtures("v0.2.0").join("queue-status/populated.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn queue_tables_added_to_v0_1_0_db() {
    let (_dir, db) = copy_db("v0.1.0");
    let mut conn = Connection::open(&db).unwrap();
    assert!(!has_table(&conn, "queue_jobs"), "v0.1.0 knows no queue");
    let jobs_before = count(&conn, "jobs");
    assert!(jobs_before >= 7);
    let tx = conn.transaction().unwrap();
    store::init(&tx).unwrap();
    assert_eq!(store::jobs(&tx, None).unwrap().len(), 0);
    assert_eq!(store::trains(&tx, None).unwrap().len(), 0);
    tx.commit().unwrap();
    for table in [
        "queue_schema",
        "queue_jobs",
        "queue_trains",
        "queue_push_markers",
        "queue_events",
    ] {
        assert!(has_table(&conn, table), "{table}");
    }
    let version: i64 = conn
        .query_row("SELECT max(version) FROM queue_schema", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, QUEUE_SCHEMA_VERSION);
    // The daemon's own history is untouched.
    assert_eq!(count(&conn, "jobs"), jobs_before);
}

#[test]
fn v0_2_0_db_keeps_rows() {
    let (_dir, db) = copy_db("v0.2.0");
    let mut conn = Connection::open(&db).unwrap();
    let want = populated();
    assert_eq!(count(&conn, "queue_jobs"), 1);
    assert_eq!(count(&conn, "queue_trains"), 1);
    let tx = conn.transaction().unwrap();
    store::init(&tx).unwrap(); // opening an older-or-equal version changes nothing
    let jobs = store::jobs(&tx, None).unwrap();
    let trains = store::trains(&tx, None).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(trains.len(), 1);
    assert_eq!(jobs[0].id, want.jobs[0].job_id);
    assert_eq!(jobs[0].branch, "feature");
    assert_eq!(jobs[0].state, QueueJobState::Ready);
    assert_eq!(trains[0].id, want.trains[0].train_id);
    assert_eq!(trains[0].state, QueueTrainState::Ready);
    assert_eq!(trains[0].train_hash, want.trains[0].train_hash);
    assert_eq!(trains[0].members, vec![jobs[0].id.clone()]);
    tx.commit().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queue_daemon_serves_an_old_database() {
    const GATE_TRUE: &str = "version = 1\n[gates.test]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n";
    for release in ["v0.1.0", "v0.2.0"] {
        let fx = Fixture::new(GATE_TRUE);
        let state = fx.root.join("state");
        fs::create_dir_all(&state).unwrap();
        fs::copy(
            fixtures(release).join("db/sluice.db"),
            state.join("sluice.db"),
        )
        .unwrap();
        let d = Daemon::start(&fx, user_config(false)).await;
        let st = d.status().await;
        assert_eq!(st.schema, 1);
        if release == "v0.1.0" {
            assert!(st.jobs.is_empty() && st.trains.is_empty(), "{release}");
        } else {
            let want = populated();
            assert!(
                st.jobs.iter().any(|j| j.job_id == want.jobs[0].job_id),
                "{release}: the queued job survived the restart: {st:#?}"
            );
            assert!(
                st.trains
                    .iter()
                    .any(|t| t.train_id == want.trains[0].train_id),
                "{release}"
            );
        }
        d.shutdown().await;
    }
}

#[test]
fn the_recorded_queue_status_is_the_current_schema() {
    for name in ["empty", "populated"] {
        let path = fixtures("v0.2.0").join(format!("queue-status/{name}.json"));
        let v: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(v["schema"], 1);
        let parsed: QueueStatusReport = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.schema, 1);
    }
}

/// The first recorded client line of `kind` in a v0.2.0 transcript.
fn recorded_request(transcript: &str, kind: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sluice-gate-proto/tests/fixtures/compat/v0.2.0/transcripts")
        .join(format!("{transcript}.ndjson"));
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["dir"] == "c2s" && v["msg"]["type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} in {transcript}"))["msg"]
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v0_2_0_queue_requests_are_served_by_the_current_queue() {
    use sluice_gate_proto::{ServerMessage, decode_client};

    const GATE_TRUE: &str = "version = 1\n[gates.test]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n";
    let fx = Fixture::new(GATE_TRUE);
    let (wt, sha) = fx.branch("feature", "main", &[("f.txt", "feature\n")]);
    let d = Daemon::start(&fx, user_config(false)).await;

    for transcript in ["queue-status-empty", "queue-status"] {
        let msg = decode_client(&recorded_request(transcript, "queue-status").to_string()).unwrap();
        match d.ask(&msg).await {
            ServerMessage::QueueStatus { status, .. } => {
                assert_eq!(status.schema, 1, "{transcript}");
                assert!(
                    status.jobs.is_empty() && status.trains.is_empty(),
                    "{transcript}"
                );
            }
            other => panic!("{transcript}: {other:?}"),
        }
    }

    // The recorded enqueue, pointed at this repository and branch.
    let mut request = recorded_request("queue-enqueue", "queue-enqueue");
    request["repo"] = fx.common.to_str().unwrap().into();
    request["worktree"] = wt.to_str().unwrap().into();
    request["head_sha"] = sha.into();
    request["env"] = serde_json::to_value(fx.client_env()).unwrap();
    let msg = decode_client(&request.to_string()).unwrap();
    match d.ask(&msg).await {
        ServerMessage::QueueJob { job, .. } => {
            assert_eq!(job.branch, "feature");
            assert_eq!(job.state, QueueJobState::Queued);
        }
        other => panic!("enqueue: {other:?}"),
    }
    d.shutdown().await;
}
