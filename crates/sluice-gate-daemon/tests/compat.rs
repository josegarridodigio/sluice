//! Compatibility golden files (ADR-0013) for the daemon: requests recorded
//! from v0.1.0 and v0.2.0 clients are replayed against the current daemon,
//! and the databases the releases wrote must open.
//!
//! Fixtures: `sluice-gate-proto/tests/fixtures/compat/<release>/transcripts/`
//! (normalised placeholders: `{{REPO}}`, `{{HOME}}`, `{{SCRATCH}}`, `{{RUN}}`)
//! and `tests/fixtures/compat/<release>/db/sluice.db`. They are never edited
//! to make a test pass. Tests tagged `#[ignore = "gap Gn: ..."]` state a
//! promise the code does not keep yet; task H3b removes the attribute once
//! the code is fixed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use sluice_gate_core::git;
use sluice_gate_core::tree;
use sluice_gate_daemon::Error;
use sluice_gate_daemon::server::{self, AdmissionConfig, DaemonConfig, LastSubscriberGone};
use sluice_gate_daemon::store::Store;
use sluice_gate_proto::{ErrorCode, ServerMessage, decode_server};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const RELEASES: [&str; 2] = ["v0.1.0", "v0.2.0"];
const WAIT: Duration = Duration::from_secs(20);

fn own_fixtures(release: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compat")
        .join(release)
}

fn proto_fixtures(release: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sluice-gate-proto/tests/fixtures/compat")
        .join(release)
}

// ---------------------------------------------------------------------------
// Recorded transcripts
// ---------------------------------------------------------------------------

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
    let path = proto_fixtures(release)
        .join("transcripts")
        .join(format!("{name}.ndjson"));
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            Line {
                conn: v["conn"].as_u64().unwrap(),
                c2s: v["dir"] == "c2s",
                msg: v["msg"].clone(),
            }
        })
        .collect()
}

/// Connection ids in order of first appearance.
fn connections(lines: &[Line]) -> Vec<u64> {
    let mut out = Vec::new();
    for l in lines {
        if !out.contains(&l.conn) {
            out.push(l.conn);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// A throwaway repository and daemon
// ---------------------------------------------------------------------------

struct Repo {
    _dir: TempDir,
    root: PathBuf,
    tree: String,
}

/// The fixture repository of the recordings: `a.txt` and the recorded
/// `.sluice.toml`, staged, no commit (the tree depends on content only).
fn make_repo(release: &str) -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let files = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../sluice-gate-core/tests/fixtures/compat")
        .join(release)
        .join("repo-files");
    fs::copy(files.join("a.txt"), root.join("a.txt")).unwrap();
    fs::copy(files.join("sluice.toml"), root.join(".sluice.toml")).unwrap();
    for args in [&["init", "-q"][..], &["add", "-A"][..]] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let paths = git::discover(&root, &env).unwrap();
    let probe_env = git::probe_env(&env, Some(&paths));
    let scratch = tree::scratch_root(&env).unwrap();
    let tree = tree::snapshot(&paths, &probe_env, &scratch)
        .unwrap()
        .tree()
        .expect("known tree")
        .as_str()
        .to_owned();
    Repo {
        _dir: dir,
        root,
        tree,
    }
}

fn config(dir: &Path) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("d.sock"),
        lock_path: dir.join("daemon.lock"),
        db_path: dir.join("sluice.db"),
        logs_dir: dir.join("logs"),
        budget: 4,
        head_reserve_after: Duration::from_secs(30),
        repo_caps: BTreeMap::new(),
        nice: 0,
        ionice: None,
        last_subscriber_gone: LastSubscriberGone::Cancel,
        log_retention: Duration::from_hours(7 * 24),
        tail_lines: 20,
        marker_ttl: Duration::from_hours(7 * 24),
        version: "0.0.0-test".to_owned(),
        admission: AdmissionConfig {
            psi_enabled: false,
            ..AdmissionConfig::default()
        },
        psi_root: dir.join("pressure"),
        user: server::UserConfig::default(),
        log: server::LogSettings::default(),
        extensions: Vec::new(),
    }
}

struct Daemon {
    dir: TempDir,
    socket: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<sluice_gate_daemon::Result<()>>,
}

async fn start(dir: TempDir) -> Daemon {
    let cfg = config(dir.path());
    let socket = cfg.socket_path.clone();
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(server::run(cfg, async move {
        let _ = rx.await;
    }));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !socket.exists() {
        assert!(tokio::time::Instant::now() < deadline, "no socket");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Daemon {
        dir,
        socket,
        stop: Some(tx),
        task,
    }
}

impl Daemon {
    async fn shutdown(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        tokio::time::timeout(WAIT, self.task)
            .await
            .expect("daemon exits")
            .unwrap()
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

fn placeholders(text: &str, repo: &Repo, scratch: &Path) -> String {
    text.replace("{{REPO}}", repo.root.to_str().unwrap())
        .replace("{{HOOK}}", repo.root.to_str().unwrap())
        .replace("{{HOME}}", scratch.join("home").to_str().unwrap())
        .replace("{{RUN}}", scratch.join("run").to_str().unwrap())
        .replace("{{SCRATCH}}", scratch.to_str().unwrap())
}

/// Replays every recorded client connection of `lines` against the daemon and
/// returns, per connection, the decoded server replies (without `progress`).
async fn replay(d: &Daemon, lines: &[Line], repo: &Repo) -> Vec<(u64, Vec<ServerMessage>)> {
    let scratch = d.dir.path().to_path_buf();
    let mut out = Vec::new();
    for conn in connections(lines) {
        let wanted = lines
            .iter()
            .filter(|l| !l.c2s && l.conn == conn && l.kind() != "progress")
            .count();
        let stream = UnixStream::connect(&d.socket).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        for l in lines.iter().filter(|l| l.c2s && l.conn == conn) {
            let text = placeholders(&serde_json::to_string(&l.msg).unwrap(), repo, &scratch);
            wr.write_all(format!("{text}\n").as_bytes()).await.unwrap();
        }
        let mut reader = BufReader::new(rd).lines();
        let mut got = Vec::new();
        while got.len() < wanted {
            let next = tokio::time::timeout(WAIT, reader.next_line())
                .await
                .unwrap_or_else(|_| panic!("conn {conn}: no reply in time, got {got:?}"))
                .unwrap();
            let Some(line) = next else { break };
            let msg = decode_server(&line)
                .unwrap_or_else(|e| panic!("conn {conn}: undecodable reply {line}: {e}"));
            if !matches!(msg, ServerMessage::Progress { .. }) {
                got.push(msg);
            }
        }
        out.push((conn, got));
    }
    out
}

fn type_of(msg: &ServerMessage) -> String {
    serde_json::to_value(msg).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Checks the replies of one replayed connection against the recording: same
/// type sequence and the stable fields (never ids, times, paths, pids).
fn assert_matches_recording(
    release: &str,
    name: &str,
    lines: &[Line],
    replies: &[(u64, Vec<ServerMessage>)],
) {
    for (conn, got) in replies {
        let recorded: Vec<&Line> = lines
            .iter()
            .filter(|l| !l.c2s && l.conn == *conn && l.kind() != "progress")
            .collect();
        let want: Vec<&str> = recorded.iter().map(|l| l.kind()).collect();
        let have: Vec<String> = got.iter().map(type_of).collect();
        // `logs` of a fresh daemon with no job history is a `not-found` error
        // instead of the recorded log lines; both are valid answers.
        if name.starts_with("logs") && have == ["welcome", "error"] {
            continue;
        }
        assert_eq!(have, want, "{release}/{name} conn {conn}: reply sequence");
        for (msg, rec) in got.iter().zip(&recorded) {
            let now = serde_json::to_value(msg).unwrap();
            match rec.kind() {
                "welcome" => assert_eq!(now["protocol"], rec.msg["protocol"]),
                "done" => {
                    for field in ["status", "exit_code"] {
                        assert_eq!(
                            now["outcome"][field], rec.msg["outcome"][field],
                            "{release}/{name} conn {conn}: outcome.{field}"
                        );
                    }
                }
                "status" => assert_eq!(now["schema"], rec.msg["schema"]),
                "queued" => assert_eq!(now["coalesced"], rec.msg["coalesced"]),
                _ => {}
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fixture_repo_has_the_recorded_tree() {
    for release in RELEASES {
        let repo = make_repo(release);
        let lines = transcript(release, "run-pass");
        let run = lines.iter().find(|l| l.c2s && l.kind() == "run").unwrap();
        assert_eq!(
            run.msg["tree"].as_str().unwrap(),
            repo.tree,
            "{release}: the tree hash procedure changed, which changes every cache key"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn old_client_replay() {
    for release in RELEASES {
        for name in [
            "hello-welcome",
            "run-pass",
            "run-hit-event",
            "run-fail",
            "run-timeout",
            "run-modified",
            "run-keyed",
            "status",
            "logs",
            "logs-job",
        ] {
            let repo = make_repo(release);
            let d = start(tempfile::tempdir().unwrap()).await;
            let lines = transcript(release, name);
            let replies = replay(&d, &lines, &repo).await;
            assert_matches_recording(release, name, &lines, &replies);
            d.shutdown().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn old_client_stop_is_honoured() {
    for release in RELEASES {
        let repo = make_repo(release);
        let d = start(tempfile::tempdir().unwrap()).await;
        let lines = transcript(release, "stop");
        let replies = replay(&d, &lines, &repo).await;
        assert_matches_recording(release, "stop", &lines, &replies);
        let mut d = d;
        let _ = d.stop.take();
        tokio::time::timeout(WAIT, d.task)
            .await
            .expect("the daemon exits after `stop`")
            .unwrap()
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_request_type_gets_bad_request_and_the_daemon_keeps_serving() {
    let d = start(tempfile::tempdir().unwrap()).await;
    let stream = UnixStream::connect(&d.socket).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let hello = json!({"type":"hello","protocol":1,"client":"compat","pid":1});
    let unknown = json!({"type":"from-the-future","id":9,"extra":[1,2]});
    wr.write_all(format!("{hello}\n{unknown}\n").as_bytes())
        .await
        .unwrap();
    let mut lines = BufReader::new(rd).lines();
    let welcome = lines.next_line().await.unwrap().unwrap();
    assert!(matches!(
        decode_server(&welcome).unwrap(),
        ServerMessage::Welcome { .. }
    ));
    let reply = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("an answer to the unknown request");
    assert!(
        matches!(
            decode_server(&reply).unwrap(),
            ServerMessage::Error {
                code: ErrorCode::BadRequest,
                ..
            }
        ),
        "{reply}"
    );
    // The daemon is still alive for the next client.
    let repo = make_repo("v0.2.0");
    let replies = replay(&d, &transcript("v0.2.0", "status"), &repo).await;
    assert_eq!(replies.len(), 1);
    d.shutdown().await;
}

// ---------------------------------------------------------------------------
// Databases
// ---------------------------------------------------------------------------

fn copy_db(release: &str) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("sluice.db");
    fs::copy(own_fixtures(release).join("db/sluice.db"), &db).unwrap();
    (dir, db)
}

#[test]
fn old_databases_open() {
    for release in RELEASES {
        let (_dir, db) = copy_db(release);
        let ro =
            Store::open_read_only(&db).unwrap_or_else(|e| panic!("{release}: read-only open: {e}"));
        drop(ro);
        let store = Store::open(&db).unwrap_or_else(|e| panic!("{release}: open: {e}"));
        let newest = store.newest_job().unwrap().expect("job history survives");
        assert_eq!(
            newest.gate, "test",
            "{release}: the hook run was the last job"
        );
        let totals = store.totals(None).unwrap();
        assert!(totals.runs >= 7, "{release}: runs {}", totals.runs);
        assert!(totals.passes >= 4, "{release}: passes {}", totals.passes);
        assert!(
            totals.failures >= 1,
            "{release}: failures {}",
            totals.failures
        );
        assert!(
            totals.timeouts >= 1,
            "{release}: timeouts {}",
            totals.timeouts
        );
        assert!(totals.hits >= 1, "{release}: hits {}", totals.hits);
        let job = store.job(&newest.id).unwrap().unwrap();
        assert_eq!(job, newest);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_runs_on_an_old_database() {
    for release in RELEASES {
        let (dir, db) = copy_db(release);
        let before = Store::open(&db).unwrap().totals(None).unwrap().runs;
        let repo = make_repo(release);
        let d = start(dir).await;
        let lines = transcript(release, "run-fail");
        let replies = replay(&d, &lines, &repo).await;
        assert_matches_recording(release, "run-fail", &lines, &replies);
        let report = replay(&d, &transcript(release, "status"), &repo).await;
        let ServerMessage::Status { status, .. } = &report[0].1[1] else {
            panic!("{release}: {:?}", report[0].1)
        };
        assert!(
            status.totals.runs > before,
            "{release}: history was kept and extended"
        );
        d.shutdown().await;
    }
}

#[test]
fn a_refused_newer_database_names_the_release_that_wrote_it() {
    for release in RELEASES {
        let (_dir, db) = copy_db(release);
        // The current binary writes the database, then a "future" bump.
        drop(Store::open(&db).unwrap());
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("UPDATE schema SET version = 99", []).unwrap();
        drop(conn);
        let err = Store::open(&db).expect_err("a newer schema is refused");
        assert!(matches!(err, Error::Schema { found: 99, .. }), "{err}");
        let message = err.to_string();
        assert!(
            message.contains(sluice_gate_core::SLUICE_VERSION),
            "the refusal must say which release wrote the database: {message}"
        );
    }
}
