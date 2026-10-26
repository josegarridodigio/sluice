//! The daemon extension point (`server::Extension`, `DaemonHandle`) with a
//! fake merge-queue extension on a real socket.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use sluice_gate_daemon::server::{
    self, AdmissionConfig, BoxFuture, DaemonConfig, DaemonHandle, Extension, ExtensionCtx,
    LastSubscriberGone, UserConfig, Vouch,
};
use sluice_gate_proto::{
    Caller, ClientMessage, ErrorCode, FEATURE_EXEC, FEATURE_QUEUE, FEATURE_QUEUE_ONLY, GateSpec,
    OutcomeStatus, PROTOCOL_VERSION, QueueStatusReport, QueueSummary, RepoInfo, RunRequest,
    ServerMessage, StatusReport, decode_server, encode,
};

/// Features every daemon announces, before those of its extensions.
const BUILTIN: [&str; 2] = [FEATURE_EXEC, FEATURE_QUEUE_ONLY];
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(20);

#[derive(Debug, Default)]
struct FakeQueue {
    fail_start: bool,
    handle: OnceLock<DaemonHandle>,
    stopped: AtomicBool,
    /// Peer pids `vouch_run` was asked with.
    vouched: Mutex<Vec<Option<u32>>>,
}

impl Extension for FakeQueue {
    fn features(&self) -> &[&'static str] {
        &[FEATURE_QUEUE]
    }

    fn on_start<'a>(&'a self, handle: &'a DaemonHandle) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if self.fail_start {
                anyhow::bail!("queue_schema 2 is newer than this build");
            }
            handle.store_tx(|tx| {
                tx.execute_batch("CREATE TABLE IF NOT EXISTS fake_queue (n INTEGER NOT NULL)")
            })?;
            let _ = self.handle.set(handle.clone());
            Ok(())
        })
    }

    fn run(self: Arc<Self>, handle: DaemonHandle) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let mut shutdown = handle.shutdown();
            let _ = shutdown.wait_for(|s| *s).await;
            self.stopped.store(true, Ordering::SeqCst);
        })
    }

    fn handle<'a>(
        &'a self,
        msg: ClientMessage,
        ctx: &'a ExtensionCtx,
    ) -> BoxFuture<'a, Option<ServerMessage>> {
        Box::pin(async move {
            match msg {
                ClientMessage::QueueStatus { id, .. } => Some(ServerMessage::QueueStatus {
                    id,
                    status: QueueStatusReport {
                        schema: 1,
                        ..QueueStatusReport::default()
                    },
                }),
                ClientMessage::QueueCancel { id, job_id, .. } => Some(ServerMessage::Error {
                    id,
                    code: ErrorCode::ConflictState,
                    message: format!(
                        "{job_id} via conn {} from {} pid {:?}",
                        ctx.conn_id(),
                        ctx.client(),
                        ctx.peer_pid()
                    ),
                }),
                _ => None,
            }
        })
    }

    fn vouch_run<'a>(
        &'a self,
        request: &'a RunRequest,
        peer_pid: Option<u32>,
    ) -> BoxFuture<'a, Option<Vouch>> {
        Box::pin(async move {
            self.vouched.lock().unwrap().push(peer_pid);
            match request.gate.name.as_str() {
                "vouched" => Some(Vouch::Verified {
                    train_id: "t-1".into(),
                    job_id: Some("j-9".into()),
                }),
                "refused" => Some(Vouch::Refused {
                    message: "not verified here".into(),
                }),
                _ => None,
            }
        })
    }

    fn status_summary(&self) -> BoxFuture<'_, Option<QueueSummary>> {
        Box::pin(async {
            Some(QueueSummary {
                active: 2,
                ready: 1,
                needs_reconcile: 0,
            })
        })
    }
}

struct Daemon {
    dir: TempDir,
    socket: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<sluice_gate_daemon::Result<()>>,
}

fn config(dir: &Path, extensions: Vec<Arc<dyn Extension>>) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("d.sock"),
        lock_path: dir.join("daemon.lock"),
        db_path: dir.join("sluice.db"),
        logs_dir: dir.join("logs"),
        budget: 2,
        head_reserve_after: Duration::from_secs(30),
        repo_caps: BTreeMap::new(),
        nice: 0,
        ionice: None,
        last_subscriber_gone: LastSubscriberGone::Cancel,
        log_retention: Duration::from_hours(24),
        tail_lines: 20,
        marker_ttl: Duration::from_hours(24),
        version: "0.0.0-test".to_owned(),
        admission: AdmissionConfig {
            psi_enabled: false,
            ..AdmissionConfig::default()
        },
        psi_root: dir.join("pressure"),
        proc_root: std::path::PathBuf::from("/proc"),
        user: UserConfig::default(),
        log: sluice_gate_daemon::server::LogSettings::default(),
        extensions,
        reaper: None,
        containment: None,
    }
}

async fn start(extensions: Vec<Arc<dyn Extension>>) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), extensions);
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

    /// Sends `msg` and returns the single reply.
    async fn ask(&self, msg: &ClientMessage) -> ServerMessage {
        let (_w, mut lines) = server::connect(&self.socket, msg).await.unwrap();
        let line = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .expect("a reply in time")
            .unwrap()
            .expect("a reply");
        decode_server(&line).unwrap()
    }

    async fn status(&self) -> StatusReport {
        match self.ask(&ClientMessage::Status { id: 2 }).await {
            ServerMessage::Status { status, .. } => status,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Raw handshake; returns the `welcome` features.
    async fn welcome_features(&self) -> Vec<String> {
        let stream = UnixStream::connect(&self.socket).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let hello = encode(&ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client: "t".into(),
            pid: 1,
        })
        .unwrap();
        wr.write_all(hello.as_bytes()).await.unwrap();
        let line = BufReader::new(rd)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        match decode_server(&line).unwrap() {
            ServerMessage::Welcome { features, .. } => features,
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn run_request(dir: &Path, cmd: &str) -> RunRequest {
    RunRequest {
        repo: RepoInfo {
            toplevel: dir.to_path_buf(),
            common_dir: dir.join(".git"),
            git_dir: dir.join(".git"),
        },
        gate: GateSpec {
            name: "g".to_owned(),
            cmd: cmd.to_owned(),
            weight: 1,
            weight_auto: false,
            timeout_ms: 60_000,
            env: BTreeMap::new(),
            cache: false,
        },
        key: None,
        tree: None,
        committed_tree: None,
        cmd: cmd.to_owned(),
        extra: Vec::new(),
        cwd: dir.to_path_buf(),
        child_env: std::env::vars().collect(),
        probe_env: BTreeMap::new(),
        caller: Caller::Human,
        session: Some("queue:t-test".to_owned()),
        client_pid: std::process::id(),
        hook: None,
        no_marker: false,
        requested_at: None,
    }
}

fn queue_status() -> ClientMessage {
    ClientMessage::QueueStatus { id: 11, repo: None }
}

#[tokio::test(flavor = "multi_thread")]
async fn without_extensions_queue_requests_are_not_supported() {
    let d = start(Vec::new()).await;
    assert_eq!(d.welcome_features().await, BUILTIN);
    match d.ask(&queue_status()).await {
        ServerMessage::Error { id, code, message } => {
            assert_eq!((id, code), (11, ErrorCode::NotSupported));
            assert_eq!(
                message,
                "the running daemon does not support the merge queue; run 'sluice daemon stop' and retry"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(d.status().await.queue, None);
    d.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_requests_are_routed_to_the_extension() {
    let fake = Arc::new(FakeQueue::default());
    let d = start(vec![fake.clone()]).await;
    assert_eq!(
        d.welcome_features().await,
        [FEATURE_EXEC, FEATURE_QUEUE_ONLY, FEATURE_QUEUE]
    );

    match d.ask(&queue_status()).await {
        ServerMessage::QueueStatus { id, status } => {
            assert_eq!(id, 11);
            assert_eq!(status.schema, 1);
        }
        other => panic!("unexpected {other:?}"),
    }
    let cancel = ClientMessage::QueueCancel {
        id: 12,
        job_id: "j1".into(),
        via: None,
    };
    match d.ask(&cancel).await {
        ServerMessage::Error { id, code, message } => {
            assert_eq!((id, code), (12, ErrorCode::ConflictState));
            assert!(message.starts_with("j1 via conn "), "{message}");
            let pid = std::process::id();
            assert!(message.ends_with(&format!("pid Some({pid})")), "{message}");
        }
        other => panic!("unexpected {other:?}"),
    }
    // A queue message no extension answers is `not-supported`.
    let reconcile = ClientMessage::QueueReconcile { id: 13, repo: None };
    assert!(matches!(
        d.ask(&reconcile).await,
        ServerMessage::Error {
            id: 13,
            code: ErrorCode::NotSupported,
            ..
        }
    ));
    assert_eq!(
        d.status().await.queue,
        Some(QueueSummary {
            active: 2,
            ready: 1,
            needs_reconcile: 0
        })
    );
    assert!(!fake.stopped.load(Ordering::SeqCst));
    d.shutdown().await;
    assert!(
        fake.stopped.load(Ordering::SeqCst),
        "run() saw the shutdown"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_on_start_disables_the_extension() {
    let fake = Arc::new(FakeQueue {
        fail_start: true,
        ..FakeQueue::default()
    });
    let d = start(vec![fake.clone()]).await;
    assert_eq!(d.welcome_features().await, BUILTIN);
    match d.ask(&queue_status()).await {
        ServerMessage::Error { code, message, .. } => {
            assert_eq!(code, ErrorCode::Internal);
            assert!(message.contains("queue_schema 2 is newer"), "{message}");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(d.status().await.queue, None);
    d.shutdown().await;
    assert!(!fake.stopped.load(Ordering::SeqCst), "run() never spawned");
}

#[tokio::test(flavor = "multi_thread")]
async fn handle_runs_gates_and_transactions() {
    let fake = Arc::new(FakeQueue::default());
    let d = start(vec![fake.clone()]).await;
    let handle = fake
        .handle
        .get()
        .expect("on_start stored the handle")
        .clone();

    // Accessors.
    assert!(handle.lease_held());
    assert!(!*handle.shutdown().borrow());
    assert_eq!(handle.state_dir(), d.dir.path());
    assert_eq!(handle.log_dir(), d.dir.path().join("logs"));
    assert_eq!(handle.config().version, UserConfig::default().version);
    assert!(
        handle
            .sanitised_env()
            .keys()
            .all(|k| !k.starts_with("CLAUDE"))
    );
    let fence = handle.fence();
    assert_ne!(fence.token, "");
    assert!(
        fence
            .instance
            .starts_with(&format!("{}-", std::process::id()))
    );

    // `store_tx`: commit, rollback on error, lease fencing.
    let n = handle
        .store_tx(|tx| {
            tx.execute("INSERT INTO fake_queue (n) VALUES (1)", [])?;
            tx.query_row("SELECT count(*) FROM fake_queue", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .unwrap();
    assert_eq!(n, 1);
    let err = handle
        .store_tx(|tx| {
            tx.execute("INSERT INTO fake_queue (n) VALUES (2)", [])?;
            tx.execute("INSERT INTO no_such_table (n) VALUES (3)", [])
        })
        .unwrap_err();
    assert!(!err.is_retryable());
    let n = handle
        .store_tx(|tx| {
            tx.query_row("SELECT count(*) FROM fake_queue", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .unwrap();
    assert_eq!(n, 1, "the failed transaction was rolled back");
    assert!(handle.store_tx(|tx| fence.check(tx)).unwrap());
    let stale = server::LeaseFence {
        token: "stale".into(),
        ..fence.clone()
    };
    assert!(!handle.store_tx(|tx| stale.check(tx)).unwrap());

    // `submit_run`: the events a client gets.
    let mut sub = handle.submit_run(run_request(d.dir.path(), "true"));
    let mut kinds = Vec::new();
    while let Some(msg) = tokio::time::timeout(WAIT, sub.recv()).await.unwrap() {
        match &msg {
            ServerMessage::Queued { job_id, .. } | ServerMessage::Started { job_id, .. } => {
                assert_eq!(job_id, sub.job_id());
            }
            ServerMessage::Done { outcome, .. } => {
                assert_eq!(outcome.status, OutcomeStatus::Pass);
            }
            ServerMessage::Progress { .. } => continue,
            other => panic!("unexpected {other:?}"),
        }
        kinds.push(match msg {
            ServerMessage::Queued { .. } => "queued",
            ServerMessage::Started { .. } => "started",
            _ => "done",
        });
    }
    assert_eq!(kinds, ["queued", "started", "done"]);
    assert!(sub.recv().await.is_none());

    // Dropping a subscription unsubscribes: the last subscriber is gone,
    // so the `cancel` policy kills the running job.
    let mut sub = handle.submit_run(run_request(d.dir.path(), "sleep 30"));
    loop {
        match tokio::time::timeout(WAIT, sub.recv()).await.unwrap() {
            Some(ServerMessage::Started { .. }) => break,
            Some(ServerMessage::Queued { .. } | ServerMessage::Progress { .. }) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(d.status().await.running.len(), 1);
    drop(sub);
    let deadline = tokio::time::Instant::now() + WAIT;
    while !d.status().await.running.is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the job was not killed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    d.shutdown().await;
    assert!(*handle.shutdown().borrow(), "shutdown turned true");
}

/// An extension may answer a hook `run` itself: the reply is one `done`
/// (no `queued`), `cached` with exit 0 for a verified gate and `error` for a
/// refused one. Requests without a hook never reach it; hook requests it
/// does not claim run normally.
#[tokio::test(flavor = "multi_thread")]
async fn an_extension_can_vouch_for_a_hook_run() {
    let fake = Arc::new(FakeQueue::default());
    let d = start(vec![fake.clone()]).await;
    let run = |gate: &str, hook: Option<&str>| {
        let mut r = run_request(d.dir.path(), "true");
        r.gate.name = gate.to_owned();
        r.hook = hook.map(str::to_owned);
        ClientMessage::Run { id: 5, request: r }
    };

    match d.ask(&run("vouched", Some("pre-push"))).await {
        ServerMessage::Done {
            id,
            job_id,
            outcome,
        } => {
            assert_eq!((id, job_id.as_str()), (5, "j-9"));
            assert_eq!(outcome.status, OutcomeStatus::Cached);
            assert_eq!(outcome.exit_code, Some(0));
            assert!(!outcome.marker_written);
            assert_eq!(
                outcome.message,
                "vouched: verified by merge train t-1 (job j-9)"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    match d.ask(&run("refused", Some("pre-push"))).await {
        ServerMessage::Done { outcome, .. } => {
            assert_eq!(outcome.status, OutcomeStatus::Error);
            assert_eq!(outcome.message, "not verified here");
        }
        other => panic!("unexpected {other:?}"),
    }
    // Asked with the peer's pid (this process, on the same socket).
    assert_eq!(
        *fake.vouched.lock().unwrap(),
        vec![Some(std::process::id()); 2]
    );

    // A hook run the extension does not claim goes the normal way.
    assert!(matches!(
        d.ask(&run("other", Some("pre-push"))).await,
        ServerMessage::Queued { id: 5, .. }
    ));
    // Without a hook the extension is not even asked, whatever the gate.
    assert!(matches!(
        d.ask(&run("vouched", None)).await,
        ServerMessage::Queued { id: 5, .. }
    ));
    assert_eq!(fake.vouched.lock().unwrap().len(), 3);
    d.shutdown().await;
}
