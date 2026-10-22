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
    Caller, ClientEvent, ClientMessage, ErrorCode, GateSpec, OutcomeStatus, PROTOCOL_VERSION,
    RepoInfo, RunRequest, STATUS_SCHEMA_VERSION, ServerMessage, decode_server, encode,
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
        user: server::UserConfig::default(),
        log: sluice_gate_daemon::server::LogSettings::default(),
        extensions: Vec::new(),
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
                timeout_ms,
                env: BTreeMap::new(),
                cache: key.is_some(),
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
        Some(ServerMessage::Status { id: 2, status }) => status,
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
    assert_eq!(row.outcome.as_deref(), Some("orphaned"));
    assert_eq!(row.state, JobState::Done);
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
