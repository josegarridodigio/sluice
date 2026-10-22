//! The current blocking client against recorded daemons of v0.1.0 and v0.2.0
//! (ADR-0013): a fake daemon replays the server half of a recorded
//! connection, the real `client::Client` talks to it.
//!
//! Fixtures: `tests/fixtures/compat/<release>/transcripts/`, never edited to
//! make a test pass.

#![cfg(feature = "client")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};

use serde_json::Value;
use sluice_gate_proto::client::Client;
use sluice_gate_proto::{
    ClientMessage, OutcomeStatus, ServerMessage, decode_client, decode_server,
};

const RELEASES: [&str; 2] = ["v0.1.0", "v0.2.0"];

struct Line {
    conn: u64,
    c2s: bool,
    raw: String,
    kind: String,
}

fn transcript(release: &str, name: &str) -> Vec<Line> {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compat")
        .join(release)
        .join("transcripts")
        .join(format!("{name}.ndjson"));
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            Line {
                conn: v["conn"].as_u64().unwrap(),
                c2s: v["dir"] == "c2s",
                raw: serde_json::to_string(&v["msg"]).unwrap(),
                kind: v["msg"]["type"].as_str().unwrap_or("").to_owned(),
            }
        })
        .collect()
}

/// The recorded connection that carried a request of `kind`.
fn connection_of(lines: &[Line], kind: &str) -> u64 {
    lines.iter().find(|l| l.c2s && l.kind == kind).unwrap().conn
}

/// Fake daemon: answers the handshake with the recorded `welcome`, reads one
/// request line and replays the remaining recorded server lines.
fn fake_daemon(dir: &Path, lines: &[Line], conn: u64) -> (PathBuf, JoinHandle<String>) {
    let socket = dir.join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let replies: Vec<String> = lines
        .iter()
        .filter(|l| !l.c2s && l.conn == conn)
        .map(|l| l.raw.clone())
        .collect();
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut out = stream;
        let mut hello = String::new();
        reader.read_line(&mut hello).unwrap();
        writeln!(out, "{}", replies[0]).unwrap();
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        for line in &replies[1..] {
            writeln!(out, "{line}").unwrap();
        }
        request
    });
    (socket, handle)
}

fn run_request(lines: &[Line], conn: u64) -> sluice_gate_proto::RunRequest {
    let l = lines
        .iter()
        .find(|l| l.c2s && l.conn == conn && l.kind == "run")
        .unwrap();
    match decode_client(&l.raw).unwrap() {
        ClientMessage::Run { request, .. } => request,
        other => panic!("{other:?}"),
    }
}

#[test]
fn hello_and_features() {
    for (release, queue) in [("v0.1.0", false), ("v0.2.0", true)] {
        let lines = transcript(release, "hello-welcome");
        let tmp = tempfile::tempdir().unwrap();
        let (socket, daemon) = fake_daemon(tmp.path(), &lines, 1);
        let client = Client::connect(&socket).unwrap();
        assert_eq!(client.supports("queue"), queue, "{release}");
        assert_eq!(client.features().is_empty(), !queue, "{release}");
        drop(client);
        let _ = daemon.join();
    }
}

#[test]
fn run_against_recorded_daemons() {
    for release in RELEASES {
        for (name, status) in [
            ("run-pass", OutcomeStatus::Pass),
            ("run-fail", OutcomeStatus::Fail),
            ("run-timeout", OutcomeStatus::Timeout),
        ] {
            let lines = transcript(release, name);
            let conn = connection_of(&lines, "run");
            let tmp = tempfile::tempdir().unwrap();
            let (socket, daemon) = fake_daemon(tmp.path(), &lines, conn);
            let mut seen = Vec::new();
            let done = Client::connect(&socket)
                .unwrap()
                .run(run_request(&lines, conn), |m| {
                    seen.push(
                        format!("{m:?}")
                            .split([' ', '{'])
                            .next()
                            .unwrap()
                            .to_owned(),
                    );
                })
                .unwrap_or_else(|e| panic!("{release}/{name}: {e}"));
            assert_eq!(done.outcome.status, status, "{release}/{name}");
            assert_eq!(seen.first().map(String::as_str), Some("Queued"));
            assert_eq!(seen.last().map(String::as_str), Some("Done"));
            let sent = daemon.join().unwrap();
            assert!(matches!(
                decode_client(&sent).unwrap(),
                ClientMessage::Run { .. }
            ));
        }
    }
}

#[test]
fn status_against_recorded_daemons() {
    for release in RELEASES {
        let lines = transcript(release, "status");
        let conn = connection_of(&lines, "status");
        let tmp = tempfile::tempdir().unwrap();
        let (socket, daemon) = fake_daemon(tmp.path(), &lines, conn);
        let report = Client::connect(&socket).unwrap().status().unwrap();
        assert_eq!(report.schema, 1);
        assert_eq!(report.daemon.version, release.trim_start_matches('v'));
        assert_eq!(
            report.running.len(),
            1,
            "{release}: the slow job was running"
        );
        let _ = daemon.join();
    }
}

#[test]
fn logs_against_recorded_daemons() {
    for release in RELEASES {
        let lines = transcript(release, "logs");
        let conn = connection_of(&lines, "logs");
        let tmp = tempfile::tempdir().unwrap();
        let (socket, daemon) = fake_daemon(tmp.path(), &lines, conn);
        let (path, tail) = Client::connect(&socket).unwrap().logs(None, 5).unwrap();
        assert!(path.to_string_lossy().ends_with(".log"), "{release}");
        assert!(!tail.is_empty(), "{release}");
        let _ = daemon.join();
    }
}

#[test]
fn stop_against_recorded_daemons() {
    for release in RELEASES {
        let lines = transcript(release, "stop");
        let conn = connection_of(&lines, "stop");
        let tmp = tempfile::tempdir().unwrap();
        let (socket, daemon) = fake_daemon(tmp.path(), &lines, conn);
        let running = Client::connect(&socket).unwrap().stop(false).unwrap();
        assert_eq!(running, 0, "{release}");
        let _ = daemon.join();
    }
}

#[test]
fn queue_replies_of_a_v0_2_0_daemon() {
    for (name, kind) in [
        ("queue-status", "queue-status"),
        ("queue-enqueue", "queue-enqueue"),
    ] {
        let lines = transcript("v0.2.0", name);
        let conn = connection_of(&lines, kind);
        let request = lines
            .iter()
            .find(|l| l.c2s && l.conn == conn && l.kind == kind)
            .unwrap();
        let msg = decode_client(&request.raw).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let (socket, daemon) = fake_daemon(tmp.path(), &lines, conn);
        let client = Client::connect(&socket).unwrap();
        assert!(client.supports("queue"));
        let reply = client.request(&msg).unwrap();
        match (kind, &reply) {
            ("queue-status", ServerMessage::QueueStatus { status, .. }) => {
                assert_eq!(status.schema, 1);
                assert_eq!(status.trains.len(), 1);
            }
            ("queue-enqueue", ServerMessage::QueueJob { job, .. }) => {
                assert_eq!(job.branch, "feature");
            }
            other => panic!("unexpected {other:?}"),
        }
        let sent = daemon.join().unwrap();
        assert_eq!(decode_client(&sent).unwrap(), msg);
        decode_server(&serde_json::to_string(&reply).unwrap()).unwrap();
    }
}
