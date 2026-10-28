//! End-to-end tests of the daemon: a real socket, real gate processes and a
//! throwaway git repository.

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::sys::signal::killpg;
use nix::unistd::Pid;
use sluice_gate_core::git::{self, RepoPaths};
use sluice_gate_core::key::CacheKey;
use sluice_gate_core::marker::{Marker, MarkerStore};
use sluice_gate_core::tree::{self, TreeHash};
use sluice_gate_core::{CACHE_FORMAT_VERSION, dirs};
use sluice_gate_daemon::Error;
use sluice_gate_daemon::executor;
use sluice_gate_daemon::server::{self, AdmissionConfig, DaemonConfig, LastSubscriberGone};
use sluice_gate_daemon::store::{JobRow, JobState, Store};
use sluice_gate_proto::{
    Caller, ClientEvent, ClientMessage, ErrorCode, ExecRequest, ExecSignal, FEATURE_EXEC,
    FEATURE_QUEUE_ONLY, GateSpec, OutcomeStatus, PROTOCOL_VERSION, RepoInfo, RunRequest,
    STATUS_SCHEMA_VERSION, ServerMessage, decode_server, encode,
};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

type EnvMap = BTreeMap<String, String>;
type Reader = Lines<BufReader<OwnedReadHalf>>;

const WAIT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct Repo {
    _dir: TempDir,
    paths: RepoPaths,
    probe_env: EnvMap,
    tree: TreeHash,
}

fn host_env() -> EnvMap {
    std::env::vars().collect()
}

fn make_repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let env = host_env();
    let paths = git::discover(dir.path(), &env).unwrap();
    let probe_env = git::probe_env(&env, Some(&paths));
    let scratch = tree::scratch_root(&env).unwrap();
    let tree = tree::snapshot(&paths, &probe_env, &scratch)
        .unwrap()
        .tree()
        .cloned()
        .expect("known tree");
    Repo {
        _dir: dir,
        paths,
        probe_env,
        tree,
    }
}

fn key(n: u8) -> CacheKey {
    CacheKey::try_from(format!("{n:02x}").repeat(32)).unwrap()
}

struct Daemon {
    dir: TempDir,
    socket: PathBuf,
    config: DaemonConfig,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<sluice_gate_daemon::Result<()>>,
}

fn config(dir: &Path, budget: u32, policy: LastSubscriberGone) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("d.sock"),
        lock_path: dir.join("daemon.lock"),
        db_path: dir.join("sluice.db"),
        logs_dir: dir.join("logs"),
        budget,
        head_reserve_after: Duration::from_secs(30),
        repo_caps: BTreeMap::new(),
        nice: 0,
        ionice: None,
        last_subscriber_gone: policy,
        log_retention: Duration::from_hours(7 * 24),
        tail_lines: 20,
        marker_ttl: Duration::from_hours(7 * 24),
        version: "0.0.0-test".to_owned(),
        // PSI off by default so the host's real pressure (e.g. a parallel
        // build) cannot hold jobs back; the PSI tests turn it on.
        admission: AdmissionConfig {
            psi_enabled: false,
            ..AdmissionConfig::default()
        },
        psi_root: dir.join("pressure"),
        proc_root: std::path::PathBuf::from("/proc"),
        user: server::UserConfig::default(),
        log: sluice_gate_daemon::server::LogSettings::default(),
        extensions: Vec::new(),
        reaper: None,
        containment: None,
        stall: sluice_gate_daemon::stall::Tuning::default(),
    }
}

async fn wait_for(what: &str, mut cond: impl FnMut() -> bool, limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn start_in(dir: TempDir, config: DaemonConfig) -> Daemon {
    let (tx, rx) = oneshot::channel::<()>();
    let socket = config.socket_path.clone();
    let task = tokio::spawn(server::run(config.clone(), async move {
        let _ = rx.await;
    }));
    wait_for("the socket", || socket.exists(), Duration::from_secs(15)).await;
    Daemon {
        dir,
        socket,
        config,
        stop: Some(tx),
        task,
    }
}

async fn start(budget: u32, policy: LastSubscriberGone) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), budget, policy);
    start_in(dir, cfg).await
}

impl Daemon {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

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

    async fn request(&self, msg: &ClientMessage) -> (OwnedWriteHalf, Reader) {
        server::connect(&self.socket, msg).await.unwrap()
    }
}

fn run_msg(
    repo: &Repo,
    gate: &str,
    cmd: &str,
    key: Option<CacheKey>,
    tree: Option<TreeHash>,
    timeout_ms: u64,
) -> ClientMessage {
    ClientMessage::Run {
        id: 7,
        request: RunRequest {
            repo: RepoInfo {
                toplevel: repo.paths.toplevel.clone(),
                common_dir: repo.paths.common_dir.clone(),
                git_dir: repo.paths.git_dir.clone(),
            },
            gate: GateSpec {
                name: gate.to_owned(),
                cmd: cmd.to_owned(),
                weight: 1,
                weight_auto: false,
                timeout_ms,
                env: BTreeMap::new(),
                cache: key.is_some(),
                stall_timeout_ms: None,
            },
            key,
            tree,
            committed_tree: None,
            cmd: cmd.to_owned(),
            extra: Vec::new(),
            cwd: repo.paths.toplevel.clone(),
            child_env: host_env(),
            probe_env: repo.probe_env.clone(),
            caller: Caller::Agent,
            session: None,
            client_pid: std::process::id(),
            hook: None,
            no_marker: false,
            requested_at: None,
        },
    }
}

async fn next(lines: &mut Reader) -> Option<ServerMessage> {
    let line = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .expect("a reply in time")
        .unwrap()?;
    Some(decode_server(&line).unwrap())
}

/// Reads until `done` (inclusive) or EOF.
async fn until_done(lines: &mut Reader) -> Vec<ServerMessage> {
    let mut out = Vec::new();
    while let Some(msg) = next(lines).await {
        let done = matches!(msg, ServerMessage::Done { .. });
        out.push(msg);
        if done {
            break;
        }
    }
    out
}

async fn until_started(lines: &mut Reader) -> String {
    loop {
        match next(lines).await.expect("started") {
            ServerMessage::Started { job_id, .. } => return job_id,
            ServerMessage::Queued { .. } | ServerMessage::Progress { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn done_status(msgs: &[ServerMessage]) -> OutcomeStatus {
    match msgs.last() {
        Some(ServerMessage::Done { outcome, .. }) => outcome.status,
        other => panic!("no done: {other:?}"),
    }
}

fn group_alive(pgid: i32) -> bool {
    killpg(Pid::from_raw(pgid), None).is_ok()
}

async fn read_pid(path: &Path) -> i32 {
    let mut pid = None;
    wait_for(
        "the pid file",
        || {
            pid = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse().ok());
            pid.is_some()
        },
        Duration::from_secs(10),
    )
    .await;
    pid.unwrap()
}

async fn status(d: &Daemon) -> sluice_gate_proto::StatusReport {
    let (_w, mut r) = d.request(&ClientMessage::Status { id: 2 }).await;
    match next(&mut r).await {
        Some(ServerMessage::Status { id: 2, status }) => *status,
        other => panic!("unexpected {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn handshake_checks_the_protocol() {
    let d = start(2, LastSubscriberGone::Cancel).await;
    for (line, expect_welcome, code) in [
        (
            encode(&ClientMessage::Hello {
                protocol: PROTOCOL_VERSION + 98,
                client: "t".into(),
                pid: 1,
            })
            .unwrap(),
            false,
            Some(ErrorCode::Protocol),
        ),
        ("{not json\n".to_owned(), false, Some(ErrorCode::BadRequest)),
        (
            encode(&ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                client: "t".into(),
                pid: 1,
            })
            .unwrap(),
            true,
            None,
        ),
    ] {
        let stream = UnixStream::connect(&d.socket).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        wr.write_all(line.as_bytes()).await.unwrap();
        let mut lines = BufReader::new(rd).lines();
        let reply = next(&mut lines).await.expect("a reply");
        if expect_welcome {
            assert!(
                matches!(reply, ServerMessage::Welcome { protocol, .. } if protocol == PROTOCOL_VERSION)
            );
        } else {
            assert!(
                matches!(reply, ServerMessage::Error { code: c, .. } if Some(c) == code),
                "{reply:?}"
            );
            assert!(next(&mut lines).await.is_none(), "connection closed");
        }
    }
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn welcome_announces_exec_and_queue_only() {
    let d = start(2, LastSubscriberGone::Cancel).await;
    let stream = UnixStream::connect(&d.socket).await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let hello = encode(&ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        client: "t".into(),
        pid: 1,
    })
    .unwrap();
    wr.write_all(hello.as_bytes()).await.unwrap();
    let mut lines = BufReader::new(rd).lines();
    let Some(ServerMessage::Welcome { features, .. }) = next(&mut lines).await else {
        panic!("no welcome");
    };
    assert!(features.iter().any(|f| f == FEATURE_EXEC), "{features:?}");
    assert!(
        features.iter().any(|f| f == FEATURE_QUEUE_ONLY),
        "{features:?}"
    );
    d.shutdown().await;
}

fn exec_msg(dir: &Path, script: &str) -> ClientMessage {
    ClientMessage::Exec {
        id: 3,
        request: ExecRequest {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: dir.to_path_buf(),
            env: host_env(),
            label: "sh -c".into(),
            weight: 1,
            timeout_ms: 0,
            coalesce: None,
            repo: None,
            caller: Caller::Agent,
            session: None,
            client_pid: std::process::id(),
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_runs_an_argv_into_a_spool_with_sluice_job_set() {
    let d = start(2, LastSubscriberGone::Cancel).await;
    let msg = exec_msg(
        d.dir.path(),
        "printf out; printf \"$SLUICE_JOB\" >&2; pwd > cwd.txt; exit 5",
    );
    let (_w, mut r) = d.request(&msg).await;
    let msgs = until_done(&mut r).await;
    let spool = msgs
        .iter()
        .find_map(|m| match m {
            ServerMessage::Started { spool, .. } => spool.clone(),
            _ => None,
        })
        .expect("started carries the spool");
    let Some(ServerMessage::Done {
        job_id, outcome, ..
    }) = msgs.last()
    else {
        panic!("no done");
    };
    assert_eq!(outcome.status, OutcomeStatus::Fail);
    assert_eq!(outcome.exit_code, Some(5));
    assert_eq!(outcome.signal, None);
    assert!(!outcome.marker_written);
    assert_eq!(spool, d.config.logs_dir.join(format!("{job_id}.exec")));
    let bytes = std::fs::read(&spool).unwrap();
    let body = bytes.strip_prefix(executor::spool::MAGIC).expect("magic");
    let (records, _) = executor::spool::parse(body);
    let stream = |id: u8| -> String {
        records
            .iter()
            .filter(|(s, _)| *s == id)
            .map(|(_, d)| String::from_utf8_lossy(d).into_owned())
            .collect()
    };
    assert_eq!(stream(executor::spool::STDOUT), "out");
    assert_eq!(&stream(executor::spool::STDERR), job_id);
    assert_eq!(
        std::fs::read_to_string(d.path("cwd.txt")).unwrap().trim(),
        d.dir.path().canonicalize().unwrap().display().to_string()
    );
    // `logs` decodes the spool.
    let (_w, mut r) = d
        .request(&ClientMessage::Logs {
            id: 4,
            job_id: Some(job_id.clone()),
            tail: 0,
        })
        .await;
    let Some(ServerMessage::Log { lines, .. }) = next(&mut r).await else {
        panic!("no log");
    };
    // stdout and stderr are separate streams: their relative arrival order
    // is not defined, only the content of each one.
    let joined = lines.concat();
    assert!(
        joined == format!("out{job_id}") || joined == format!("{job_id}out"),
        "{joined:?}"
    );
    // A signal outside an exec connection is refused.
    let (_w, mut r) = d
        .request(&ClientMessage::Signal {
            id: 5,
            signal: ExecSignal::Int,
        })
        .await;
    assert!(matches!(
        next(&mut r).await,
        Some(ServerMessage::Error {
            code: ErrorCode::BadRequest,
            ..
        })
    ));
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_marker_runs_write_nothing_to_the_cache_and_gates_see_sluice_job() {
    let repo = make_repo();
    let d = start(2, LastSubscriberGone::Cancel).await;
    let seen = d.path("job.txt");
    for (cmd, status) in [
        (
            format!("echo \"$SLUICE_JOB\" > {}", seen.display()),
            OutcomeStatus::Pass,
        ),
        ("exit 4".to_owned(), OutcomeStatus::Fail),
    ] {
        let mut msg = run_msg(
            &repo,
            "build",
            &cmd,
            Some(key(9)),
            Some(repo.tree.clone()),
            0,
        );
        if let ClientMessage::Run { request, .. } = &mut msg {
            request.no_marker = true;
        }
        let (_w, mut r) = d.request(&msg).await;
        let msgs = until_done(&mut r).await;
        assert_eq!(done_status(&msgs), status);
        let Some(ServerMessage::Done {
            job_id, outcome, ..
        }) = msgs.last()
        else {
            panic!("no done");
        };
        assert!(!outcome.marker_written);
        if status == OutcomeStatus::Pass {
            assert_eq!(std::fs::read_to_string(&seen).unwrap().trim(), job_id);
        }
    }
    let store = MarkerStore::new(&repo.paths.common_dir);
    let files = std::fs::read_dir(store.dir()).map_or(0, Iterator::count);
    assert_eq!(files, 0, "no marker and no miss record");
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_for_one_key_run_once() {
    let repo = make_repo();
    let d = start(4, LastSubscriberGone::Cancel).await;
    let counter = d.path("counter");
    let cmd = format!("echo run >> {}; sleep 1", counter.display());
    let msg = run_msg(
        &repo,
        "test",
        &cmd,
        Some(key(1)),
        Some(repo.tree.clone()),
        0,
    );
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let socket = d.socket.clone();
        let msg = msg.clone();
        tasks.push(tokio::spawn(async move {
            let (_w, mut r) = server::connect(&socket, &msg).await.unwrap();
            until_done(&mut r).await
        }));
    }
    let mut coalesced = 0;
    for t in tasks {
        let msgs = t.await.unwrap();
        match &msgs[0] {
            ServerMessage::Queued { coalesced: c, .. } => coalesced += usize::from(*c),
            other => panic!("first message {other:?}"),
        }
        assert_eq!(done_status(&msgs), OutcomeStatus::Pass);
    }
    assert_eq!(coalesced, 5);
    let runs = std::fs::read_to_string(&counter).unwrap();
    assert_eq!(runs.lines().count(), 1, "{runs}");
    let marker = MarkerStore::new(&repo.paths.common_dir)
        .dir()
        .join(format!("{}.json", key(1)));
    assert!(marker.exists(), "marker written");
    let totals = status(&d).await.totals;
    assert_eq!(totals.coalesced, 5);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_limits_concurrency() {
    let repo = make_repo();
    let d = start(2, LastSubscriberGone::Cancel).await;
    let log = d.path("times");
    let cmd = format!(
        "echo start $(date +%s%N) >> {f}; sleep 0.5; echo end $(date +%s%N) >> {f}",
        f = log.display()
    );
    let mut tasks = Vec::new();
    for i in 0..3 {
        let socket = d.socket.clone();
        let msg = run_msg(&repo, &format!("g{i}"), &cmd, None, None, 0);
        tasks.push(tokio::spawn(async move {
            let (_w, mut r) = server::connect(&socket, &msg).await.unwrap();
            until_done(&mut r).await
        }));
    }
    for t in tasks {
        assert_eq!(done_status(&t.await.unwrap()), OutcomeStatus::Pass);
    }
    let mut events: Vec<(u128, i32)> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| {
            let (kind, t) = l.split_once(' ').unwrap();
            (t.parse().unwrap(), if kind == "start" { 1 } else { -1 })
        })
        .collect();
    assert_eq!(events.len(), 6);
    events.sort_by_key(|&(t, delta)| (t, delta));
    let (mut now, mut max) = (0, 0);
    for (_, delta) in events {
        now += delta;
        max = max.max(now);
    }
    assert!(max <= 2, "{max} gates overlapped");
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn marker_appearing_while_queued_is_cached() {
    let repo = make_repo();
    let d = start(1, LastSubscriberGone::Cancel).await;
    let flag = d.path("b-ran");
    let a = run_msg(&repo, "a", "sleep 1", None, None, 0);
    let (_wa, mut ra) = d.request(&a).await;
    until_started(&mut ra).await;

    let b_cmd = format!("echo ran > {}", flag.display());
    let b = run_msg(&repo, "b", &b_cmd, Some(key(2)), Some(repo.tree.clone()), 0);
    let (_wb, mut rb) = d.request(&b).await;
    assert!(matches!(
        next(&mut rb).await,
        Some(ServerMessage::Queued { .. })
    ));
    MarkerStore::new(&repo.paths.common_dir)
        .write(&Marker {
            format: CACHE_FORMAT_VERSION,
            key: key(2),
            tree: repo.tree.clone(),
            gate: "b".into(),
            cmd: b_cmd.clone(),
            passed_at: dirs::now_rfc3339(),
            duration_ms: 10,
            exit_code: 0,
            job_id: "by-hand".into(),
            sluice_version: "test".into(),
            committed_tree: None,
        })
        .unwrap();
    let msgs = until_done(&mut rb).await;
    assert_eq!(done_status(&msgs), OutcomeStatus::Cached);
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ServerMessage::Started { .. }))
    );
    assert_eq!(done_status(&until_done(&mut ra).await), OutcomeStatus::Pass);
    assert!(!flag.exists(), "b never ran");
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn last_subscriber_gone_cancel_kills_the_group() {
    let repo = make_repo();
    let d = start(2, LastSubscriberGone::Cancel).await;
    let pidf = d.path("pid");
    let cmd = format!("echo $$ > {}; sleep 30", pidf.display());
    let (w, mut r) = d
        .request(&run_msg(&repo, "slow", &cmd, None, None, 0))
        .await;
    until_started(&mut r).await;
    let pgid = read_pid(&pidf).await;
    assert!(group_alive(pgid));
    drop(w);
    drop(r);
    wait_for(
        "the group to die",
        || !group_alive(pgid),
        Duration::from_secs(7),
    )
    .await;
    wait_for(
        "the job to be recorded",
        || {
            Store::open_read_only(&d.config.db_path)
                .ok()
                .and_then(|s| s.newest_job().ok().flatten())
                .is_some_and(|j| j.outcome.as_deref() == Some("killed"))
        },
        Duration::from_secs(5),
    )
    .await;
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn last_subscriber_gone_finish_writes_the_marker() {
    let repo = make_repo();
    let d = start(2, LastSubscriberGone::Finish).await;
    let msg = run_msg(
        &repo,
        "short",
        "sleep 0.5",
        Some(key(3)),
        Some(repo.tree.clone()),
        0,
    );
    let (w, mut r) = d.request(&msg).await;
    assert!(matches!(
        next(&mut r).await,
        Some(ServerMessage::Queued { .. })
    ));
    drop(w);
    drop(r);
    let marker = MarkerStore::new(&repo.paths.common_dir)
        .dir()
        .join(format!("{}.json", key(3)));
    wait_for("the marker", || marker.exists(), Duration::from_secs(10)).await;
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_kills_the_group() {
    let repo = make_repo();
    let d = start(2, LastSubscriberGone::Cancel).await;
    let pidf = d.path("pid");
    let cmd = format!("echo $$ > {}; sleep 30", pidf.display());
    let (_w, mut r) = d
        .request(&run_msg(&repo, "slow", &cmd, None, None, 300))
        .await;
    let msgs = until_done(&mut r).await;
    assert_eq!(done_status(&msgs), OutcomeStatus::Timeout);
    let pgid = read_pid(&pidf).await;
    wait_for(
        "the group to die",
        || !group_alive(pgid),
        Duration::from_secs(7),
    )
    .await;
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_logs_and_events() {
    let repo = make_repo();
    let d = start(1, LastSubscriberGone::Cancel).await;
    let (_wx, mut rx) = d
        .request(&run_msg(
            &repo,
            "x",
            "echo hello-log; sleep 1.5",
            None,
            None,
            0,
        ))
        .await;
    let x = until_started(&mut rx).await;
    let (_wy, mut ry) = d.request(&run_msg(&repo, "y", "true", None, None, 0)).await;
    let Some(ServerMessage::Queued { job_id: y, .. }) = next(&mut ry).await else {
        panic!("queued");
    };

    let st = status(&d).await;
    assert_eq!(st.schema, STATUS_SCHEMA_VERSION);
    assert_eq!((st.budget.total, st.budget.used), (1, 1));
    assert_eq!(st.running.len(), 1);
    assert_eq!(st.running[0].job_id, x);
    assert_eq!(st.running[0].caller, Caller::Agent);
    assert_eq!(st.queued.len(), 1);
    assert_eq!(st.queued[0].job_id, y);
    assert_eq!(st.daemon.socket, d.socket);

    assert_eq!(done_status(&until_done(&mut rx).await), OutcomeStatus::Pass);
    assert_eq!(done_status(&until_done(&mut ry).await), OutcomeStatus::Pass);

    let logs = ClientMessage::Logs {
        id: 3,
        job_id: Some(x.clone()),
        tail: 0,
    };
    let (_w, mut r) = d.request(&logs).await;
    match next(&mut r).await {
        Some(ServerMessage::Log {
            id: 3,
            job_id,
            lines,
            ..
        }) => {
            assert_eq!(job_id, x);
            assert!(lines.iter().any(|l| l == "hello-log"), "{lines:?}");
        }
        other => panic!("unexpected {other:?}"),
    }
    let newest = ClientMessage::Logs {
        id: 4,
        job_id: None,
        tail: 5,
    };
    let (_w, mut r) = d.request(&newest).await;
    assert!(matches!(next(&mut r).await, Some(ServerMessage::Log { job_id, .. }) if job_id == y));
    let missing = ClientMessage::Logs {
        id: 5,
        job_id: Some("nope".into()),
        tail: 5,
    };
    let (_w, mut r) = d.request(&missing).await;
    assert!(matches!(
        next(&mut r).await,
        Some(ServerMessage::Error {
            code: ErrorCode::NotFound,
            ..
        })
    ));

    let event = ClientMessage::Event {
        id: 0,
        event: ClientEvent::Hit {
            repo: repo.paths.toplevel.clone(),
            gate: "x".into(),
            saved_ms: 1234,
        },
    };
    drop(d.request(&event).await);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let totals = status(&d).await.totals;
        if totals.hits == 1 {
            assert_eq!(totals.runs, 2);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "hit not recorded");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn second_daemon_is_refused() {
    let d = start(1, LastSubscriberGone::Cancel).await;
    let second = tokio::time::timeout(
        Duration::from_secs(5),
        server::run(d.config.clone(), std::future::pending()),
    )
    .await
    .expect("returns promptly");
    assert!(
        matches!(second, Err(Error::AlreadyRunning(_))),
        "{second:?}"
    );
    assert!(d.socket.exists(), "the first daemon keeps its socket");
    d.shutdown().await;
}

/// Regression (Phase 6 acceptance run): a daemon killed with SIGKILL leaves
/// an unexpired lease behind; the next daemon reclaims it when the owner
/// process is gone instead of refusing to start for the whole TTL, and still
/// refuses while the owner lives.
#[tokio::test(flavor = "multi_thread")]
async fn lease_of_a_dead_daemon_is_reclaimed_on_start() {
    use std::os::unix::fs::MetadataExt as _;
    let uid = std::fs::metadata("/proc/self").unwrap().uid();
    // A live owner of this user: the lease is not stolen.
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), 1, LastSubscriberGone::Cancel);
    let mut alive = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    {
        let store = Store::open(&cfg.db_path).unwrap();
        let owner = format!("{uid}:{}", alive.id());
        assert!(
            store
                .acquire_lease("daemon", &owner, "old-token", dirs::now_ms(), 90_000)
                .unwrap()
        );
    }
    let refused = tokio::time::timeout(
        Duration::from_secs(5),
        server::run(cfg.clone(), std::future::pending()),
    )
    .await
    .expect("returns promptly");
    assert!(
        matches!(refused, Err(Error::AlreadyRunning(_))),
        "{refused:?}"
    );
    // The owner dies (reaped): the next start reclaims the lease.
    alive.kill().unwrap();
    alive.wait().unwrap();
    let d = start_in(dir, cfg).await;
    assert!(d.socket.exists());
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn orphans_are_reconciled_on_start() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), 1, LastSubscriberGone::Cancel);
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let ticks = executor::proc_start_ticks(pid);
    assert!(ticks.is_some());
    {
        let store = Store::open(&cfg.db_path).unwrap();
        store
            .insert_job(&JobRow {
                id: "orphan-1".into(),
                key: None,
                tree: None,
                repo: "/r".into(),
                gate: "g".into(),
                cmd: "sleep 30".into(),
                weight: 1,
                caller: "human".into(),
                session: None,
                state: JobState::Running,
                outcome: None,
                exit_code: None,
                queued_at: dirs::now_ms(),
                started_at: Some(dirs::now_ms()),
                finished_at: None,
                log_path: None,
                pgid: Some(i32::try_from(pid).unwrap()),
                proc_start_ticks: ticks,
                daemon_instance: "previous-1".into(),
                subscribers_max: 1,
                coalesced: 0,
                marker_written: false,
            })
            .unwrap();
    }
    let d = start_in(dir, cfg).await;
    wait_for(
        "the orphan to die",
        || child.try_wait().unwrap().is_some(),
        Duration::from_secs(7),
    )
    .await;
    let row = Store::open_read_only(&d.config.db_path)
        .unwrap()
        .job("orphan-1")
        .unwrap()
        .unwrap();
    assert_eq!(row.outcome.as_deref(), Some("killed"));
    assert_eq!(row.state, JobState::Done);
    d.shutdown().await;
}

/// A running row of a previous instance whose leader already exited while
/// a background child of the gate lives on in its process group.
fn orphan_row(id: &str, pgid: i32, ticks: Option<u64>) -> JobRow {
    JobRow {
        id: id.into(),
        key: None,
        tree: None,
        repo: "/r".into(),
        gate: "g".into(),
        cmd: "sleep 300 &".into(),
        weight: 1,
        caller: "human".into(),
        session: None,
        state: JobState::Running,
        outcome: None,
        exit_code: None,
        queued_at: dirs::now_ms(),
        started_at: Some(dirs::now_ms()),
        finished_at: None,
        log_path: None,
        pgid: Some(pgid),
        proc_start_ticks: ticks,
        daemon_instance: "previous-2".into(),
        subscribers_max: 1,
        coalesced: 0,
        marker_written: false,
    }
}

/// Live members of process group `pgid` (zombies excluded).
fn members(pgid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(member) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(i) = stat.rfind(')') else { continue };
        let f: Vec<&str> = stat[i + 1..].split_whitespace().collect();
        if f.first() != Some(&"Z") && f.get(2) == Some(&pgid.to_string().as_str()) {
            out.push(member);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_surviving_group_whose_leader_exited_is_reconciled_too() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), 1, LastSubscriberGone::Cancel);
    // The leader forks a child and exits at once: only the child keeps the
    // group alive, as with a gate that left a background process behind.
    let mut leader = std::process::Command::new("sh")
        .args(["-c", "sleep 313.25 & echo started"])
        .stdout(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let leader_pid = leader.id();
    let ticks = executor::proc_start_ticks(leader_pid);
    let pgid = i32::try_from(leader_pid).unwrap();
    leader.wait().unwrap();
    wait_for(
        "the leader to be gone",
        || executor::proc_start_ticks(leader_pid).is_none(),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(members(pgid).len(), 1, "the background child lives on");
    // A row whose leader pid now belongs to an unrelated process (pid
    // reuse) must leave that process alone.
    let mut unrelated = std::process::Command::new("sleep")
        .arg("313.5")
        .process_group(0)
        .spawn()
        .unwrap();
    let other = i32::try_from(unrelated.id()).unwrap();
    let other_ticks = executor::proc_start_ticks(unrelated.id()).map(|t| t + 1);
    {
        let store = Store::open(&cfg.db_path).unwrap();
        store
            .insert_job(&orphan_row("orphan-2", pgid, ticks))
            .unwrap();
        store
            .insert_job(&orphan_row("reused-1", other, other_ticks))
            .unwrap();
    }
    let d = start_in(dir, cfg).await;
    wait_for(
        "the background child to die",
        || members(pgid).is_empty(),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        unrelated.try_wait().unwrap().is_none(),
        "a reused pid must never be killed"
    );
    let store = Store::open_read_only(&d.config.db_path).unwrap();
    for id in ["orphan-2", "reused-1"] {
        let row = store.job(id).unwrap().unwrap();
        assert_eq!(row.state, JobState::Done, "{id}");
        assert_eq!(row.outcome.as_deref(), Some("killed"), "{id}");
    }
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unwritable_logs_dir_still_runs_the_gate_with_the_tail_in_memory() {
    use std::os::unix::fs::PermissionsExt;
    let repo = make_repo();
    let d = start(2, LastSubscriberGone::Cancel).await;
    let logs = &d.config.logs_dir;
    std::fs::create_dir_all(logs).unwrap();
    std::fs::set_permissions(logs, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::write(logs.join("probe"), "").is_ok() {
        // Root ignores permissions: nothing to prove.
        std::fs::set_permissions(logs, std::fs::Permissions::from_mode(0o700)).unwrap();
        d.shutdown().await;
        return;
    }
    let msg = run_msg(
        &repo,
        "unlogged",
        "echo visible-output; echo on-stderr >&2",
        Some(key(9)),
        Some(repo.tree.clone()),
        0,
    );
    let (_w, mut r) = d.request(&msg).await;
    let msgs = until_done(&mut r).await;
    let Some(ServerMessage::Done { outcome, .. }) = msgs.last() else {
        panic!("no done: {msgs:?}");
    };
    let fail_msg = run_msg(&repo, "unlogged-fail", "echo boom; exit 4", None, None, 0);
    let (_w2, mut r2) = d.request(&fail_msg).await;
    let fail_msgs = until_done(&mut r2).await;
    std::fs::set_permissions(logs, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(outcome.status, OutcomeStatus::Pass, "{outcome:?}");
    assert!(outcome.marker_written, "a real pass is still cached");
    let err = outcome
        .log_error
        .as_deref()
        .expect("the log error is reported");
    assert!(err.contains("Permission denied"), "{err}");
    assert!(
        outcome.tail.iter().any(|l| l == "visible-output")
            && outcome.tail.iter().any(|l| l == "on-stderr"),
        "{:?}",
        outcome.tail
    );
    let Some(ServerMessage::Done {
        outcome: failed, ..
    }) = fail_msgs.last()
    else {
        panic!("no done: {fail_msgs:?}");
    };
    assert_eq!(failed.status, OutcomeStatus::Fail);
    assert_eq!(failed.exit_code, Some(4));
    assert!(!failed.marker_written);
    assert!(failed.tail.iter().any(|l| l == "boom"), "{:?}", failed.tail);
    d.shutdown().await;
}

async fn stop_with_running_job(force: bool) {
    let repo = make_repo();
    let mut d = start(1, LastSubscriberGone::Cancel).await;
    let cmd = if force { "sleep 30" } else { "sleep 1" };
    let (_w, mut r) = d.request(&run_msg(&repo, "g", cmd, None, None, 0)).await;
    until_started(&mut r).await;
    let (_ws, mut rs) = d.request(&ClientMessage::Stop { id: 9, force }).await;
    assert!(matches!(
        next(&mut rs).await,
        Some(ServerMessage::Stopping { id: 9, running: 1 })
    ));
    let expected = if force {
        OutcomeStatus::Killed
    } else {
        OutcomeStatus::Pass
    };
    assert_eq!(done_status(&until_done(&mut r).await), expected);
    let task = std::mem::replace(&mut d.task, tokio::spawn(async { Ok(()) }));
    tokio::time::timeout(WAIT, task)
        .await
        .expect("daemon exits")
        .unwrap()
        .unwrap();
    assert!(!d.socket.exists(), "socket removed");
    let lease = Store::open_read_only(&d.config.db_path)
        .unwrap()
        .lease("daemon")
        .unwrap();
    assert!(lease.is_none(), "lease released");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_waits_for_running_jobs() {
    stop_with_running_job(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_force_kills_running_jobs() {
    stop_with_running_job(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_late_coalescer_gets_started() {
    let repo = make_repo();
    let d = start(2, LastSubscriberGone::Cancel).await;
    let msg = run_msg(
        &repo,
        "test",
        "sleep 1",
        Some(key(4)),
        Some(repo.tree.clone()),
        0,
    );
    let (_w1, mut r1) = d.request(&msg).await;
    let first = until_started(&mut r1).await;
    let (_w2, mut r2) = d.request(&msg).await;
    match next(&mut r2).await {
        Some(ServerMessage::Queued {
            job_id,
            position: 0,
            coalesced: true,
            ..
        }) => assert_eq!(job_id, first),
        other => panic!("unexpected {other:?}"),
    }
    match next(&mut r2).await {
        Some(ServerMessage::Started { job_id, .. }) => assert_eq!(job_id, first),
        other => panic!("expected started, got {other:?}"),
    }
    let rest = until_done(&mut r2).await;
    assert!(
        !rest
            .iter()
            .any(|m| matches!(m, ServerMessage::Started { .. })),
        "started is sent once: {rest:?}"
    );
    assert_eq!(done_status(&rest), OutcomeStatus::Pass);
    assert_eq!(done_status(&until_done(&mut r1).await), OutcomeStatus::Pass);
    d.shutdown().await;
}

fn git_in(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn worktrees_of_one_repository_share_its_cap() {
    let repo = make_repo();
    let main = repo.paths.toplevel.clone();
    git_in(&main, &["add", "-A"]);
    git_in(
        &main,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "init"],
    );
    let wt_parent = tempfile::tempdir().unwrap();
    let wt = wt_parent.path().join("wt");
    git_in(
        &main,
        &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
    );
    let wt_paths = git::discover(&wt, &host_env()).unwrap();
    assert_ne!(wt_paths.toplevel, main);
    assert_eq!(wt_paths.common_dir, repo.paths.common_dir);

    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), 4, LastSubscriberGone::Cancel);
    cfg.repo_caps = BTreeMap::from([(main.clone(), 1)]);
    let d = start_in(dir, cfg).await;

    let (_wa, mut ra) = d
        .request(&run_msg(&repo, "a", "sleep 1", None, None, 0))
        .await;
    until_started(&mut ra).await;
    let mut b = run_msg(&repo, "b", "true", None, None, 0);
    if let ClientMessage::Run { request, .. } = &mut b {
        request.repo = RepoInfo {
            toplevel: wt_paths.toplevel.clone(),
            common_dir: wt_paths.common_dir.clone(),
            git_dir: wt_paths.git_dir.clone(),
        };
        request.cwd = wt_paths.toplevel.clone();
    }
    let (_wb, mut rb) = d.request(&b).await;
    assert!(matches!(
        next(&mut rb).await,
        Some(ServerMessage::Queued { .. })
    ));
    let st = status(&d).await;
    assert_eq!(st.running.len(), 1);
    assert_eq!(st.running[0].repo, main);
    assert_eq!(st.queued.len(), 1, "the worktree shares the cap");
    assert_eq!(st.queued[0].repo, main);
    assert_eq!(st.queued[0].reason, sluice_gate_proto::WaitReason::RepoCap);
    assert_eq!(done_status(&until_done(&mut ra).await), OutcomeStatus::Pass);
    assert_eq!(done_status(&until_done(&mut rb).await), OutcomeStatus::Pass);
    d.shutdown().await;
}

// ---------------------------------------------------------------------------
// PSI admission (Phase 4)
// ---------------------------------------------------------------------------

/// Daemon log lines captured by a process-wide tracing subscriber.
static LOG: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOG.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing_subscriber::filter::LevelFilter::INFO)
        .with_writer(|| Capture)
        .try_init();
}

fn captured() -> String {
    String::from_utf8_lossy(
        &LOG.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned()
}

/// Writes `<root>/cpu` atomically with the given `some avg10`.
fn write_cpu(root: &Path, avg10: f64) {
    std::fs::create_dir_all(root).unwrap();
    let tmp = root.join("cpu.tmp");
    std::fs::write(
        &tmp,
        format!("some avg10={avg10:.2} avg60=1.00 avg300=0.50 total=123456\n"),
    )
    .unwrap();
    std::fs::rename(&tmp, root.join("cpu")).unwrap();
}

fn psi_config(dir: &Path, max_wait: Duration) -> DaemonConfig {
    let mut cfg = config(dir, 2, LastSubscriberGone::Cancel);
    cfg.admission = AdmissionConfig {
        psi_enabled: true,
        psi_poll_interval: Duration::from_millis(50),
        psi_max_wait: max_wait,
        ..AdmissionConfig::default()
    };
    cfg
}

/// Polls `status` until `cond` holds.
async fn status_until(
    d: &Daemon,
    what: &str,
    cond: impl Fn(&sluice_gate_proto::StatusReport) -> bool,
) -> sluice_gate_proto::StatusReport {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let s = status(d).await;
        if cond(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn psi_pressure_holds_jobs_until_it_drops() {
    let repo = make_repo();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("pressure");
    write_cpu(&root, 99.0);
    let cfg = psi_config(dir.path(), Duration::from_mins(10));
    let d = start_in(dir, cfg).await;
    status_until(&d, "PSI to block", |s| s.budget.psi.blocking).await;

    let (_w, mut r) = d
        .request(&run_msg(&repo, "held", "true", None, None, 0))
        .await;
    assert!(matches!(
        next(&mut r).await,
        Some(ServerMessage::Queued { .. })
    ));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let s = status_until(&d, "the queued job", |s| !s.queued.is_empty()).await;
    assert!(s.running.is_empty(), "{s:?}");
    assert_eq!(s.queued[0].reason, sluice_gate_proto::WaitReason::Psi);
    assert!(s.budget.psi.enabled && s.budget.psi.blocking);
    assert_eq!(s.budget.psi.cpu_some_avg10, Some(99.0));
    assert_eq!(s.budget.psi.memory_some_avg10, None, "no memory file");

    write_cpu(&root, 0.0);
    until_started(&mut r).await;
    assert_eq!(done_status(&until_done(&mut r).await), OutcomeStatus::Pass);
    let s = status(&d).await;
    assert!(!s.budget.psi.blocking);
    assert_eq!(s.budget.psi.cpu_some_avg10, Some(0.0));
    assert!(s.totals.psi_blocked_ms_total >= 400, "{:?}", s.totals);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn psi_starvation_guard_admits_the_head() {
    capture_logs();
    let repo = make_repo();
    let dir = tempfile::tempdir().unwrap();
    write_cpu(&dir.path().join("pressure"), 99.0);
    let cfg = psi_config(dir.path(), Duration::from_millis(300));
    let d = start_in(dir, cfg).await;
    status_until(&d, "PSI to block", |s| s.budget.psi.blocking).await;

    let (_w, mut r) = d
        .request(&run_msg(&repo, "starved", "true", None, None, 0))
        .await;
    let job_id = until_started(&mut r).await;
    assert_eq!(done_status(&until_done(&mut r).await), OutcomeStatus::Pass);
    assert!(status(&d).await.budget.psi.blocking, "still blocking");
    let log = captured();
    assert!(
        log.lines()
            .any(|l| l.contains("starvation guard") && l.contains(&job_id)),
        "{log}"
    );
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn psi_unavailable_falls_back_to_the_budget() {
    let repo = make_repo();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = psi_config(dir.path(), Duration::from_mins(10));
    cfg.psi_root = dir.path().join("no-such-dir");
    let d = start_in(dir, cfg).await;
    // The first sample fails and disables PSI admission.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let s = status(&d).await;
    assert!(!s.budget.psi.enabled, "{:?}", s.budget.psi);
    assert_eq!(s.budget.psi.cpu_some_avg10, None);
    assert!(!s.budget.psi.blocking);
    let (_w, mut r) = d
        .request(&run_msg(&repo, "free", "true", None, None, 0))
        .await;
    assert_eq!(done_status(&until_done(&mut r).await), OutcomeStatus::Pass);
    d.shutdown().await;
}

// ---------------------------------------------------------------------------
// ADR-0011: timeline, utilisation sampler, early verdict
// ---------------------------------------------------------------------------

fn with_requested_at(mut msg: ClientMessage) -> ClientMessage {
    if let ClientMessage::Run { request, .. } = &mut msg {
        request.requested_at = Some(dirs::now_ms());
    }
    msg
}

fn done_outcome(msgs: &[ServerMessage]) -> (String, sluice_gate_proto::Outcome) {
    match msgs.last() {
        Some(ServerMessage::Done {
            job_id, outcome, ..
        }) => (job_id.clone(), outcome.clone()),
        other => panic!("no done: {other:?}"),
    }
}

/// L1: every job records its timeline; the points are present and ordered,
/// and a job that waited reports how long and why.
#[tokio::test(flavor = "multi_thread")]
async fn timeline_fields_are_present_and_monotonic() {
    let repo = make_repo();
    let d = start(1, LastSubscriberGone::Cancel).await;

    let (_wa, mut ra) = d
        .request(&with_requested_at(run_msg(
            &repo, "first", "sleep 1", None, None, 0,
        )))
        .await;
    let first = until_started(&mut ra).await;
    let (_wb, mut rb) = d
        .request(&with_requested_at(run_msg(
            &repo, "second", "true", None, None, 0,
        )))
        .await;
    let msgs_b = until_done(&mut rb).await;
    let msgs_a = until_done(&mut ra).await;
    assert!(
        matches!(
            &msgs_b[0],
            ServerMessage::Queued {
                weight: 1,
                weight_cfg: Some(1),
                ..
            }
        ),
        "{msgs_b:?}"
    );
    let (_, a) = done_outcome(&msgs_a);
    assert_eq!(a.admission.as_deref(), Some("nominal"));
    assert_eq!(a.killed_by, None);
    let (second, b) = done_outcome(&msgs_b);
    assert_eq!(b.status, OutcomeStatus::Pass);
    assert_eq!(b.wait_reason.as_deref(), Some("budget"), "{b:?}");
    assert!(b.queue_wait_ms.is_some_and(|w| w > 0), "{b:?}");

    let store = Store::open_read_only(&d.path("sluice.db")).unwrap();
    assert_eq!(store.sched_version(), 1);
    for id in [&first, &second] {
        let t = store.timeline(id).unwrap().expect("the job row");
        let requested = t.requested_at.expect("requested_at");
        let admitted = t.admitted_at.expect("admitted_at");
        let started = t.job.started_at.expect("started_at");
        let finished = t.job.finished_at.expect("finished_at");
        assert!(
            requested <= t.job.queued_at
                && t.job.queued_at <= admitted
                && admitted <= started
                && started <= finished,
            "{t:?}"
        );
        assert_eq!(t.weight_cfg, Some(1));
        assert_eq!(t.admission.as_deref(), Some("nominal"));
        assert_eq!(t.requests, 1);
    }
    let t = store.timeline(&second).unwrap().unwrap();
    assert!(t.waits[0].is_some_and(|w| w > 0), "budget wait: {t:?}");
    d.shutdown().await;
}

/// Writes a fake `/proc/stat` with two CPUs.
fn write_stat(proc_root: &Path, idle: u64, busy: u64) {
    let tmp = proc_root.join("stat.tmp");
    std::fs::write(
        &tmp,
        format!(
            "cpu  {b2} 0 0 {i2} 0 0 0 0 0 0\ncpu0 {busy} 0 0 {idle} 0 0 0 0 0 0\n\
             cpu1 {busy} 0 0 {idle} 0 0 0 0 0 0\n",
            b2 = busy * 2,
            i2 = idle * 2
        ),
    )
    .unwrap();
    std::fs::rename(&tmp, proc_root.join("stat")).unwrap();
}

/// L1: while a job is queued and the (fake) CPUs are idle, the daemon's
/// utilisation sampler integrates the idle fraction into
/// `idle_while_waiting_ms`.
#[tokio::test(flavor = "multi_thread")]
async fn idle_while_waiting_grows_when_the_budget_leaves_cpus_idle() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let repo = make_repo();
    let dir = tempfile::tempdir().unwrap();
    let proc_root = dir.path().join("proc");
    std::fs::create_dir_all(&proc_root).unwrap();
    write_stat(&proc_root, 0, 0);
    let mut cfg = config(dir.path(), 1, LastSubscriberGone::Cancel);
    cfg.proc_root.clone_from(&proc_root);
    cfg.admission.psi_poll_interval = Duration::from_millis(50);
    let d = start_in(dir, cfg).await;

    // Both fake CPUs stay fully idle: the idle counters grow, busy ones not.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = Arc::clone(&stop);
        let proc_root = proc_root.clone();
        std::thread::spawn(move || {
            let mut idle = 0;
            while !stop.load(Ordering::SeqCst) {
                idle += 5;
                write_stat(&proc_root, idle, 0);
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };

    let (wa, mut ra) = d
        .request(&run_msg(&repo, "holder", "sleep 30", None, None, 0))
        .await;
    until_started(&mut ra).await;
    let (wb, mut rb) = d
        .request(&run_msg(&repo, "waiter", "true", None, None, 0))
        .await;
    assert!(matches!(
        next(&mut rb).await,
        Some(ServerMessage::Queued { .. })
    ));
    let s = status_until(&d, "idle time while waiting", |s| {
        s.totals.idle_while_waiting_ms.is_some_and(|v| v >= 100)
    })
    .await;
    let idle = s.totals.idle_while_waiting_ms.unwrap();
    let wall = s.totals.waiting_wall_ms.unwrap();
    assert!(idle <= wall, "idle fraction <= 1: {:?}", s.totals);
    assert!(
        s.budget.idle_cpus.is_some_and(|c| c > 1.5 && c <= 2.0),
        "{:?}",
        s.budget
    );
    assert_eq!(s.queued.len(), 1);
    assert!(s.queued[0].rank == Some(1) && s.queued[0].weight_cfg == Some(1));
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    drop((wa, wb));
    d.shutdown().await;
}

const GIB: u64 = 1 << 30;

/// Writes a fake `/proc/meminfo` (sizes in GiB), atomically.
fn write_meminfo(proc_root: &Path, total_gib: u64, available_gib: u64) {
    let tmp = proc_root.join("meminfo.tmp");
    std::fs::write(
        &tmp,
        format!(
            "MemTotal:       {} kB\nMemFree:        1024 kB\nMemAvailable:   {} kB\n\
             SwapFree:       99999999 kB\n",
            total_gib * GIB / 1024,
            available_gib * GIB / 1024
        ),
    )
    .unwrap();
    std::fs::rename(&tmp, proc_root.join("meminfo")).unwrap();
}

/// A daemon of budget 2 reading a fake `/proc` (only `meminfo`) holding
/// `total` GiB of which `available` are available; two gates of weight 1
/// always fit its CPU budget.
async fn memory_daemon(total_gib: u64, available_gib: u64, aware: bool) -> (Daemon, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let proc_root = dir.path().join("proc");
    std::fs::create_dir_all(&proc_root).unwrap();
    write_meminfo(&proc_root, total_gib, available_gib);
    let mut cfg = config(dir.path(), 2, LastSubscriberGone::Cancel);
    cfg.proc_root.clone_from(&proc_root);
    cfg.admission.psi_poll_interval = Duration::from_millis(50);
    cfg.admission.memory_aware = aware;
    (start_in(dir, cfg).await, proc_root)
}

/// T-W6-5: the second gate waits for memory although the CPU budget has room
/// (`reason` stays `budget`, `wait_detail` says `memory`), and starts as soon
/// as `MemAvailable` recovers.
#[tokio::test(flavor = "multi_thread")]
async fn a_gate_waits_for_memory_and_starts_when_meminfo_recovers() {
    let repo = make_repo();
    // 20 GiB machine, 12 GiB available, reserve 2 GiB (auto): a gate of
    // weight 1 on a budget of 2 is guessed at half of the 18 GiB usable.
    let (d, proc_root) = memory_daemon(20, 12, true).await;
    let (wa, mut ra) = d
        .request(&run_msg(&repo, "holder", "sleep 30", None, None, 0))
        .await;
    until_started(&mut ra).await;
    let (wb, mut rb) = d
        .request(&run_msg(&repo, "waiter", "true", None, None, 0))
        .await;
    assert!(matches!(
        next(&mut rb).await,
        Some(ServerMessage::Queued { .. })
    ));
    let s = status_until(&d, "the memory wait", |s| {
        s.queued
            .first()
            .is_some_and(|q| q.wait_detail.as_deref() == Some("memory"))
    })
    .await;
    assert_eq!(s.queued[0].reason, sluice_gate_proto::WaitReason::Budget);
    assert_eq!(s.queued[0].memory_expected_bytes, Some(9 * GIB));
    assert_eq!(s.queued[0].memory_expected_learned, Some(false), "a guess");
    let run = &s.running[0];
    assert_eq!(run.memory_expected_bytes, Some(9 * GIB));
    assert_eq!(run.memory_expected_learned, Some(false));
    let mem = s.budget.memory.expect("the memory rule is on");
    assert_eq!(mem.available_bytes, 12 * GIB);
    assert_eq!(mem.reserve_bytes, 2 * GIB);
    assert_eq!(
        mem.headroom_bytes,
        9 * GIB - run.memory_bytes.unwrap_or(0),
        "what the holder is still expected to grow by"
    );
    // The queue did not move while memory stayed short.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(status(&d).await.queued.len(), 1);
    // MemAvailable recovers: the waiter starts and passes.
    write_meminfo(&proc_root, 20, 30);
    let msgs = tokio::time::timeout(WAIT, until_done(&mut rb))
        .await
        .expect("the waiter runs once memory is back");
    assert_eq!(done_status(&msgs), OutcomeStatus::Pass);
    // The wait was charged to the budget accumulator.
    let store = Store::open_read_only(&d.config.db_path).unwrap();
    let job = msgs
        .iter()
        .find_map(|m| match m {
            ServerMessage::Done { job_id, .. } => Some(job_id.clone()),
            _ => None,
        })
        .unwrap();
    let t = store.timeline(&job).unwrap().unwrap();
    assert!(t.waits[0].is_some_and(|w| w >= 300), "budget wait: {t:?}");
    drop((wa, wb));
    d.shutdown().await;
}

/// T-W6-5: a gate alone (nothing else runs) starts even when `MemAvailable` is
/// far below what it is expected to use (progress is always possible), a
/// second one then waits, and a daemon with
/// `memory_aware = false` ignores `meminfo` altogether.
#[tokio::test(flavor = "multi_thread")]
async fn a_gate_alone_runs_with_tiny_memory_and_the_switch_turns_the_rule_off() {
    let repo = make_repo();
    let (d, _proc) = memory_daemon(20, 1, true).await;
    let (_w, mut r) = d
        .request(&run_msg(&repo, "alone", "true", None, None, 0))
        .await;
    let msgs = until_done(&mut r).await;
    assert_eq!(done_status(&msgs), OutcomeStatus::Pass);
    // The exemption is for a machine with nothing else running: once a
    // gate runs, however light, a second one waits for memory.
    let (wa, mut ra) = d
        .request(&run_msg(&repo, "holder", "sleep 30", None, None, 0))
        .await;
    until_started(&mut ra).await;
    let (wb, mut rb) = d
        .request(&run_msg(&repo, "second", "true", None, None, 0))
        .await;
    assert!(matches!(
        next(&mut rb).await,
        Some(ServerMessage::Queued { .. })
    ));
    status_until(&d, "the memory wait", |s| {
        s.queued
            .first()
            .is_some_and(|q| q.wait_detail.as_deref() == Some("memory"))
    })
    .await;
    drop((wa, wb));
    d.shutdown().await;

    // Rule off: two gates run together on the same tiny memory, and the
    // status carries no memory section.
    let (d, _proc) = memory_daemon(20, 1, false).await;
    let (wa, mut ra) = d
        .request(&run_msg(&repo, "holder", "sleep 30", None, None, 0))
        .await;
    until_started(&mut ra).await;
    let (wb, mut rb) = d
        .request(&run_msg(&repo, "second", "sleep 30", None, None, 0))
        .await;
    until_started(&mut rb).await;
    let s = status(&d).await;
    assert_eq!((s.running.len(), s.queued.len()), (2, 0));
    assert_eq!(s.budget.memory, None);
    assert!(s.running.iter().all(|j| j.memory_expected_bytes.is_none()));
    drop((wa, wb));
    d.shutdown().await;
}

/// A finished gate teaches its working set: the daemon samples the processes of
/// the job's group (the real `/proc` here, tiny processes), stores the run in
/// `gate_memory` and the next run of the same command is expected to peak at
/// what was learned, not at a guess.
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_gate_teaches_its_working_set() {
    let repo = make_repo();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), 2, LastSubscriberGone::Cancel);
    cfg.admission.psi_poll_interval = Duration::from_millis(50);
    let d = start_in(dir, cfg).await;
    let (_w, mut r) = d
        .request(&run_msg(&repo, "learner", "sleep 1", None, None, 0))
        .await;
    assert_eq!(done_status(&until_done(&mut r).await), OutcomeStatus::Pass);
    let store = Store::open_read_only(&d.config.db_path).unwrap();
    let rows = store.gate_memory_recent(5).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].gate.as_str(), rows[0].outcome.as_str()),
        ("learner", "pass")
    );
    assert!(rows[0].anon_peak_bytes > 0, "{rows:?}");
    assert!(!rows[0].oom_killed);
    let learned = rows[0].anon_peak_bytes + rows[0].anon_peak_bytes / 5;
    // The same command again: its expectation is the learned peak.
    let (_w2, mut r2) = d
        .request(&run_msg(&repo, "learner", "sleep 1", None, None, 0))
        .await;
    until_started(&mut r2).await;
    let s = status(&d).await;
    let run = &s.running[0];
    assert_eq!(run.memory_expected_learned, Some(true), "{run:?}");
    assert_eq!(run.memory_expected_bytes, Some(learned));
    assert_eq!(done_status(&until_done(&mut r2).await), OutcomeStatus::Pass);
    d.shutdown().await;
}

/// M4, daemon side: a `cancel` with reason `early-verdict` detaches one
/// subscriber; the job keeps running while another one waits and is killed
/// (recorded as `killed_by = early-verdict`) when the last one leaves so.
#[tokio::test(flavor = "multi_thread")]
async fn an_early_verdict_cancel_kills_only_a_job_nobody_else_waits_for() {
    let repo = make_repo();
    let d = start(2, LastSubscriberGone::Cancel).await;
    let msg = run_msg(&repo, "slow", "sleep 30", Some(key(9)), None, 0);
    let (mut wa, mut ra) = d.request(&msg).await;
    let job = until_started(&mut ra).await;
    let (mut wb, mut rb) = d.request(&msg).await;
    assert!(matches!(
        next(&mut rb).await,
        Some(ServerMessage::Queued {
            coalesced: true,
            ..
        })
    ));
    let early = |id| {
        encode(&ClientMessage::Cancel {
            id,
            reason: Some(sluice_gate_proto::CANCEL_EARLY_VERDICT.into()),
        })
        .unwrap()
    };
    wa.write_all(early(7).as_bytes()).await.unwrap();
    // The other subscriber still waits: the job keeps running.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let s = status(&d).await;
    assert_eq!(s.running.len(), 1, "{s:?}");
    assert_eq!(s.running[0].subscribers, 1);
    wb.write_all(early(7).as_bytes()).await.unwrap();
    let db = d.path("sluice.db");
    let mut killed_by = None;
    wait_for(
        "the job to be killed",
        || {
            killed_by = Store::open_read_only(&db)
                .ok()
                .and_then(|s| s.timeline(&job).ok().flatten())
                .filter(|t| t.job.state == JobState::Done)
                .and_then(|t| t.killed_by);
            killed_by.is_some()
        },
        WAIT,
    )
    .await;
    assert_eq!(killed_by.as_deref(), Some("early-verdict"));
    d.shutdown().await;
}

/// Alpha review finding 7: a queued job whose last subscriber leaves is
/// dropped under the same lock as the "is it still queued" check, so it
/// is never admitted afterwards and never runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_job_dropped_by_its_last_subscriber_never_starts() {
    let repo = make_repo();
    let d = start(1, LastSubscriberGone::Cancel).await;
    let release = d.path("release");
    let holder_cmd = format!("while [ ! -e {} ]; do sleep 0.05; done", release.display());
    let (wa, mut ra) = d
        .request(&run_msg(&repo, "holder", &holder_cmd, None, None, 0))
        .await;
    until_started(&mut ra).await;
    let flag = d.path("waiter-ran");
    let (wb, mut rb) = d
        .request(&run_msg(
            &repo,
            "waiter",
            &format!("echo ran > {}", flag.display()),
            None,
            None,
            0,
        ))
        .await;
    let waiter = match next(&mut rb).await {
        Some(ServerMessage::Queued { job_id, .. }) => job_id,
        other => panic!("{other:?}"),
    };
    drop((wb, rb));
    let db = d.path("sluice.db");
    wait_for(
        "the queued job to be dropped",
        || {
            Store::open_read_only(&db)
                .ok()
                .and_then(|s| s.timeline(&waiter).ok().flatten())
                .is_some_and(|t| t.job.state == JobState::Done)
        },
        WAIT,
    )
    .await;
    let t = Store::open_read_only(&db)
        .unwrap()
        .timeline(&waiter)
        .unwrap()
        .unwrap();
    assert_eq!(t.job.outcome.as_deref(), Some("killed"));
    assert_eq!(t.job.started_at, None, "never admitted");
    // Free the budget: nothing is left to start.
    std::fs::write(&release, "").unwrap();
    assert_eq!(done_status(&until_done(&mut ra).await), OutcomeStatus::Pass);
    let s = status(&d).await;
    assert!(s.running.is_empty() && s.queued.is_empty(), "{s:?}");
    assert!(!flag.exists(), "the dropped job never ran");
    drop(wa);
    d.shutdown().await;
}

// ---------------------------------------------------------------------------
// The Sluice-side memory guard (ADR-0019 piece 7, W6e)
// ---------------------------------------------------------------------------

/// Writes the fake `/proc/<pid>` of a gate leader: its own process group and
/// a resident set of `rss_gib` GiB (the guard reads it like any job without a
/// cgroup leaf).
fn write_fake_process(proc_root: &Path, pid: i32, rss_gib: u64) {
    let dir = proc_root.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("stat"),
        format!("{pid} (sh) S 1 {pid} {pid} 0 0\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("status"),
        format!("Name:\tsh\nVmRSS:\t{} kB\n", rss_gib * GIB / 1024),
    )
    .unwrap();
}

/// Writes the fake system memory pressure file with the given `full avg10`.
fn write_memory_pressure(psi_root: &Path, full_avg10: f64) {
    std::fs::create_dir_all(psi_root).unwrap();
    std::fs::write(
        psi_root.join("memory"),
        format!(
            "some avg10=60.00 avg60=20.00 avg300=5.00 total=1\n\
             full avg10={full_avg10:.2} avg60=10.00 avg300=2.00 total=1\n"
        ),
    )
    .unwrap();
}

/// A daemon of budget 2 with a fake `/proc` (`meminfo` plus the leaders'
/// entries) and a fake pressure directory, the guard `on` or off, memory
/// admission off so both gates run whatever `meminfo` says.
async fn guard_daemon(guard: bool) -> (Daemon, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let proc_root = dir.path().join("proc");
    std::fs::create_dir_all(&proc_root).unwrap();
    // 20 GiB machine, reserve 2 GiB (auto), healthy.
    write_meminfo(&proc_root, 20, 15);
    let psi_root = dir.path().join("pressure");
    write_memory_pressure(&psi_root, 0.0);
    let mut cfg = config(dir.path(), 2, LastSubscriberGone::Cancel);
    cfg.proc_root.clone_from(&proc_root);
    cfg.psi_root.clone_from(&psi_root);
    cfg.admission.psi_poll_interval = Duration::from_millis(50);
    cfg.admission.memory_aware = false;
    cfg.admission.memory_guard = guard;
    (start_in(dir, cfg).await, proc_root, psi_root)
}

/// Starts two gates ("small" 3 GiB, "big" 6 GiB of fake resident set) and
/// returns their connections and pids. The machine is still healthy.
async fn two_fake_gates(
    d: &Daemon,
    repo: &Repo,
    proc_root: &Path,
) -> ((OwnedWriteHalf, Reader), (OwnedWriteHalf, Reader), i32) {
    let mut started = Vec::new();
    for (name, gib) in [("small", 3), ("big", 6)] {
        let pidfile = d.path(&format!("{name}.pid"));
        let cmd = format!("echo $$ > {}; sleep 30", pidfile.display());
        let (w, mut r) = d.request(&run_msg(repo, name, &cmd, None, None, 0)).await;
        until_started(&mut r).await;
        let pid = read_pid(&pidfile).await;
        write_fake_process(proc_root, pid, gib);
        started.push((w, r, pid));
    }
    let (wb, rb, big) = started.pop().unwrap();
    let (ws, rs, _) = started.pop().unwrap();
    ((ws, rs), (wb, rb), big)
}

/// W6e: with the guard on, three seconds of trouble (`MemAvailable`
/// below the reserve, the gates holding more than it, memory stalls) cancel the
/// gate with the largest working set: `killed` with `killed_by = memory` and the
/// guard's message, an `interventions` row, and the other gate keeps running
/// (one action, then the cooldown).
#[tokio::test(flavor = "multi_thread")]
async fn the_memory_guard_cancels_the_largest_gate_under_exhaustion() {
    let repo = make_repo();
    let (d, proc_root, psi_root) = guard_daemon(true).await;
    let (small, (_wb, mut rb), big_pid) = two_fake_gates(&d, &repo, &proc_root).await;
    let (ws, rs) = small;
    // Healthy so far: nothing happens however long it takes.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(status(&d).await.running.len(), 2);
    // The machine runs out of memory and stalls.
    write_meminfo(&proc_root, 20, 1);
    write_memory_pressure(&psi_root, 40.0);
    let msgs = tokio::time::timeout(WAIT, until_done(&mut rb))
        .await
        .expect("the guard cancels the biggest gate");
    let (job, outcome) = done_outcome(&msgs);
    assert_eq!(outcome.status, OutcomeStatus::Killed, "{outcome:?}");
    assert_eq!(outcome.killed_by.as_deref(), Some("memory"));
    assert!(outcome.killed_for_memory());
    assert!(
        outcome
            .message
            .contains("cancelled by Sluice for lack of memory: job working set 6.0 GiB, MemAvailable 1.0 GiB below the reserve 2.0 GiB"),
        "{}",
        outcome.message
    );
    assert!(!group_alive(big_pid), "the process group was killed");
    // The row of the intervention, with the guard's cause.
    let store = Store::open_read_only(&d.config.db_path).unwrap();
    let rows = store.recent_interventions(0, 10).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].cause, "sluice_memory_guard");
    assert_eq!(rows[0].leaf, format!("jobs/{job}"));
    assert_eq!(rows[0].session, None);
    // Cooldown: the other gate is not touched while it lasts.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = status(&d).await;
    assert_eq!(s.running.len(), 1, "{:?}", s.running);
    assert_eq!(s.running[0].gate, "small");
    drop((ws, rs));
    d.shutdown().await;
}

/// W6e: off by default. The same exhaustion changes nothing for a daemon
/// without `[admission] memory_guard`, and with the guard on a lone gate below
/// the reserve is never touched.
#[tokio::test(flavor = "multi_thread")]
async fn the_memory_guard_does_nothing_when_off_or_when_nothing_is_to_blame() {
    let repo = make_repo();
    assert!(!AdmissionConfig::default().memory_guard);
    let (d, proc_root, psi_root) = guard_daemon(false).await;
    let (_a, _b, _) = two_fake_gates(&d, &repo, &proc_root).await;
    write_meminfo(&proc_root, 20, 1);
    write_memory_pressure(&psi_root, 90.0);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(status(&d).await.running.len(), 2, "the guard is off");
    d.shutdown().await;

    // Guard on, but the only gate holds less than the reserve (the agents
    // cause the shortage, not the gates): no action.
    let (d, proc_root, psi_root) = guard_daemon(true).await;
    let pidfile = d.path("lone.pid");
    let cmd = format!("echo $$ > {}; sleep 30", pidfile.display());
    let (_w, mut r) = d
        .request(&run_msg(&repo, "lone", &cmd, None, None, 0))
        .await;
    until_started(&mut r).await;
    let pid = read_pid(&pidfile).await;
    write_fake_process(&proc_root, pid, 1);
    write_meminfo(&proc_root, 20, 1);
    write_memory_pressure(&psi_root, 90.0);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(status(&d).await.running.len(), 1, "nothing to blame");
    d.shutdown().await;
}

/// Progress of the job as `(stalled_ms)` values, in order.
fn stall_marks(msgs: &[ServerMessage]) -> Vec<Option<u64>> {
    msgs.iter()
        .filter_map(|m| match m {
            ServerMessage::Progress { stalled_ms, .. } => Some(*stalled_ms),
            _ => None,
        })
        .collect()
}

/// ADR-0024: a job that sleeps without output is reported stalled (to its
/// subscriber and in `status`), reported active again when it wakes up, and
/// is never ended.
#[tokio::test(flavor = "multi_thread")]
async fn a_silent_job_is_reported_stalled_then_active_and_never_killed() {
    let repo = make_repo();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), 2, LastSubscriberGone::Cancel);
    cfg.admission.psi_poll_interval = Duration::from_millis(200);
    cfg.stall = sluice_gate_daemon::stall::Tuning::with_test_overrides(Some("1000"), Some("200"));
    let d = start_in(dir, cfg).await;
    let mut msg = run_msg(
        &repo,
        "hang",
        "sleep 4; echo awake; sleep 1",
        None,
        None,
        60_000,
    );
    if let ClientMessage::Run { request, .. } = &mut msg {
        request.gate.stall_timeout_ms = Some(1_500);
    }
    let (_w, mut r) = d.request(&msg).await;
    let mut seen = Vec::new();
    loop {
        let m = next(&mut r).await.expect("a message");
        let stalled = matches!(
            m,
            ServerMessage::Progress {
                stalled_ms: Some(_),
                ..
            }
        );
        seen.push(m);
        if stalled {
            break;
        }
    }
    let report = status(&d).await;
    let job = &report.running[0];
    let stalled = job.stalled.as_ref().expect("status shows the stall");
    assert!(stalled.for_ms >= 1_500, "{stalled:?}");
    assert!(
        stalled.flat.contains(&"cpu".to_owned()) && stalled.flat.contains(&"output".to_owned())
    );
    assert_eq!(job.weight, 1, "the job still holds its weight");
    seen.extend(until_done(&mut r).await);
    assert_eq!(done_status(&seen), OutcomeStatus::Pass, "never killed");
    let marks = stall_marks(&seen);
    let first = marks.iter().position(Option::is_some).unwrap();
    assert!(
        marks[first..].iter().any(Option::is_none),
        "active again after the stall: {marks:?}"
    );
    d.shutdown().await;
}

/// `stall_timeout = "off"` (`Some(0)`) never reports a stall.
#[tokio::test(flavor = "multi_thread")]
async fn a_gate_with_the_stall_timeout_off_is_never_reported_stalled() {
    let repo = make_repo();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path(), 2, LastSubscriberGone::Cancel);
    cfg.admission.psi_poll_interval = Duration::from_millis(200);
    cfg.stall = sluice_gate_daemon::stall::Tuning::with_test_overrides(Some("500"), Some("200"));
    let d = start_in(dir, cfg).await;
    let mut msg = run_msg(&repo, "serve", "sleep 2", None, None, 60_000);
    if let ClientMessage::Run { request, .. } = &mut msg {
        request.gate.stall_timeout_ms = Some(0);
    }
    let (_w, mut r) = d.request(&msg).await;
    let seen = until_done(&mut r).await;
    assert_eq!(done_status(&seen), OutcomeStatus::Pass);
    assert!(stall_marks(&seen).iter().all(Option::is_none));
    d.shutdown().await;
}
