//! Socket listener, protocol handling, subscribers, scheduling loop and
//! daemon lifecycle.
//!
//! Design: `kb/sluice/design/protocol.md` (wire protocol),
//! `kb/sluice/design/scheduler.md` (coalescing, admission, subscribers) and
//! `kb/sluice/design/executor.md` (lifecycle, executor, persistence) and
//! `kb/sluice/design/psi-admission.md` (load-aware admission).
//!
//! Concurrency model: one task per connection, one scheduling task, one
//! task per started job, one heartbeat task and (when PSI admission is on)
//! one PSI sampling task. Shared state lives in [`Shared`] behind
//! `std::sync::Mutex`es (scheduler state, then store; always taken in that
//! order and never held across an `.await`; the PSI admission mutex is
//! never held together with another one).
//! Each job has a `broadcast` channel of [`JobEvent`]s; every subscriber
//! rewrites them into [`ServerMessage`]s carrying its own request id.
//!
//! Extensions ([`Extension`], e.g. the merge queue) get one long-lived task
//! each and a [`DaemonHandle`]; `queue-*` requests are routed to them.

mod extension;

pub use extension::{
    BoxFuture, DaemonHandle, Extension, ExtensionCtx, LeaseFence, RunSubscription, StoreError,
};

use std::collections::{BTreeMap, HashMap};
use std::fs::{DirBuilder, File, OpenOptions};
use std::future::Future;
use std::hash::BuildHasher;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};
pub use sluice_gate_core::config::{AdmissionConfig, LastSubscriberGone, UserConfig};
use sluice_gate_core::dirs::{job_id as new_job_id, now_ms};
use sluice_gate_core::git::{self, RepoPaths};
use sluice_gate_core::key::{CacheKey, command_with_args, normalize_cmd};
use sluice_gate_core::marker::{Lookup, Marker, MarkerStore, MissRecord};
use sluice_gate_core::{CACHE_FORMAT_VERSION, JOB_ENV_VAR, tree};
use sluice_gate_proto::{
    self as proto, BudgetInfo, Caller, ClientEvent, ClientMessage, DaemonInfo, ErrorCode,
    ExecRequest, ExecSignal, FEATURE_EXEC, FEATURE_QUEUE_ONLY, JOB_KIND_EXEC, MAX_LINE_BYTES,
    Outcome, OutcomeStatus, PROTOCOL_VERSION, QueuedJob, RunRequest, RunningJob,
    STATUS_SCHEMA_VERSION, ServerMessage, StatusReport,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, interval_at, sleep, timeout};

use crate::executor::{
    self, ExecSpec, ExecStatus, Finished, IoPriority, Launch, ProcessInfo, RunningHandle, Sink,
};
use crate::psi::{self, Admission};
use crate::scheduler::{Enqueued, JobSpec, Scheduler};
use crate::store::{JobRow, JobState, Store};
use crate::{Error, Result};

/// Name of the lease row held by the running daemon.
pub const LEASE_NAME: &str = "daemon";
/// Lease validity, renewed by the heartbeat.
const LEASE_TTL_MS: u64 = 90_000;
/// Heartbeat period.
const HEARTBEAT: Duration = Duration::from_secs(15);
/// Period of `progress` events.
const PROGRESS_EVERY: Duration = Duration::from_secs(5);
/// Maximum time a client may take to send `hello` and its request.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Capacity of each job's event channel.
const CHANNEL_CAPACITY: usize = 64;
/// Fallback admission tick (head reservation depends on elapsed time).
const ADMIT_TICK: Duration = Duration::from_secs(1);
/// How often old logs are pruned while the daemon runs.
const PRUNE_EVERY_MS: u64 = 24 * 60 * 60 * 1000;
/// Time given to open connections to flush their last message on exit.
const CONNECTION_DRAIN: Duration = Duration::from_secs(2);
/// How long the daemon waits for extension tasks to return once stopping.
const EXTENSION_GRACE: Duration = Duration::from_secs(10);
/// Timeout used when a gate has `timeout_ms = 0` (no timeout): ten years.
const NO_TIMEOUT: Duration = Duration::from_hours(10 * 365 * 24);

/// Everything the daemon needs to run; built by the CLI from the user config.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Unix socket to listen on.
    pub socket_path: PathBuf,
    /// Lock file held (`flock`) for the daemon's whole life.
    pub lock_path: PathBuf,
    /// SQLite database.
    pub db_path: PathBuf,
    /// Directory of job logs (`<job-id>.log`).
    pub logs_dir: PathBuf,
    /// Budget units.
    pub budget: u32,
    /// Head-of-line reservation threshold.
    pub head_reserve_after: Duration,
    /// Per-repository caps (`budget_share_max`), keyed by toplevel.
    pub repo_caps: BTreeMap<PathBuf, u32>,
    /// CPU niceness of gate processes.
    pub nice: u8,
    /// I/O priority of gate processes.
    pub ionice: Option<IoPriority>,
    /// What to do when the last subscriber of a job disconnects.
    pub last_subscriber_gone: LastSubscriberGone,
    /// Logs of finished jobs older than this are deleted.
    pub log_retention: Duration,
    /// Lines of log tail sent in `done` and by default in `logs`.
    pub tail_lines: usize,
    /// Marker time to live (double-check before start, prune after write).
    pub marker_ttl: Duration,
    /// Sluice version (welcome, status, markers).
    pub version: String,
    /// PSI admission settings (`[admission]`).
    pub admission: AdmissionConfig,
    /// Directory holding the PSI files `cpu` and `memory` (normally
    /// [`psi::DEFAULT_ROOT`]; tests point it at a temporary directory).
    pub psi_root: PathBuf,
    /// The loaded user config, exposed to extensions through
    /// [`DaemonHandle::config`] (the fields above are derived from it).
    pub user: UserConfig,
    /// Log level reporting and `daemon.log` rotation.
    pub log: LogSettings,
    /// Extensions plugged into the daemon (the merge queue); empty for a
    /// plain daemon.
    pub extensions: Vec<Arc<dyn Extension>>,
}

/// How many rotated generations of `daemon.log` are kept (`.1` to `.3`).
pub const DAEMON_LOG_KEEP: usize = 3;

/// Logging facts the daemon reports and acts on. The default (used by
/// tests and foreground runs) reports nothing and never rotates.
#[derive(Debug, Clone, Default)]
pub struct LogSettings {
    /// Active log filter, shown by `sluice status`.
    pub level: Option<String>,
    /// The `daemon.log` file that stdout and stderr were redirected to when
    /// the daemon detached; `None` when it logs to a terminal.
    pub file: Option<PathBuf>,
    /// Rotate `file` when it grows beyond this many bytes (0 = never).
    pub max_bytes: u64,
}

/// Extra time, after [`executor::KILL_GRACE`], that a job gets to exit
/// after a forwarded signal before its whole group is terminated.
const SIGNAL_ESCALATE_AFTER: Duration = Duration::from_secs(5);

/// Event of one job, broadcast to all its subscribers.
#[derive(Debug, Clone)]
enum JobEvent {
    Started {
        started_at: u64,
        spool: Option<PathBuf>,
    },
    Progress {
        elapsed_ms: u64,
        log_bytes: u64,
    },
    Done(Box<Outcome>),
}

/// What a job runs: a gate (`run`) or an arbitrary command (`exec`).
#[derive(Debug, Clone)]
enum JobRequest {
    Gate(Arc<RunRequest>),
    Exec(Arc<ExecRequest>),
}

impl JobRequest {
    /// Gate name, or `exec:<label>` for exec jobs (what v0.2 clients show).
    fn name(&self) -> String {
        match self {
            Self::Gate(r) => r.gate.name.clone(),
            Self::Exec(r) => format!("exec:{}", r.label),
        }
    }

    fn caller(&self) -> Caller {
        match self {
            Self::Gate(r) => r.caller,
            Self::Exec(r) => r.caller,
        }
    }

    fn is_exec(&self) -> bool {
        matches!(self, Self::Exec(_))
    }

    /// `(kind, label)` for `status`: `None` for gates.
    fn kind_label(&self) -> (Option<String>, Option<String>) {
        match self {
            Self::Gate(_) => (None, None),
            Self::Exec(r) => (Some(JOB_KIND_EXEC.to_owned()), Some(r.label.clone())),
        }
    }
}

/// A job known to the daemon (queued or running).
#[derive(Debug)]
struct JobEntry {
    request: JobRequest,
    queued_at: u64,
    events: broadcast::Sender<JobEvent>,
    coalesced: u32,
    cancelled: bool,
    handle: Option<RunningHandle>,
    /// Start time (unix ms) once `started` was broadcast; late coalescers get
    /// a `started` of their own from it.
    started_at: Option<u64>,
    /// Spool of an exec job once started (sent to late coalescers too).
    spool: Option<PathBuf>,
    /// The spawned leader, once started (signal forwarding).
    process: Option<ProcessInfo>,
    /// Exec coalescing identity, removed from [`State::exec_inflight`] on finish.
    exec_identity: Option<String>,
}

/// What [`register`] hands to the connection handler.
struct Registered {
    job_id: String,
    coalesced: bool,
    position: usize,
    weight: u32,
    /// `Some` when the request joined a job that has already started.
    started_at: Option<u64>,
    /// Spool of an exec job that has already started.
    spool: Option<PathBuf>,
    events: broadcast::Receiver<JobEvent>,
}

/// Mutable scheduling state.
#[derive(Debug)]
struct State {
    sched: Scheduler,
    jobs: HashMap<String, JobEntry>,
    /// Exec coalescing identity (`--coalesce-key`, cwd, argv) to job id.
    exec_inflight: HashMap<String, String>,
    accepting: bool,
    refusal: &'static str,
}

/// State shared by every task of one daemon run.
#[derive(Debug)]
struct Shared {
    config: DaemonConfig,
    instance: String,
    pid: u32,
    started_at: String,
    state: Mutex<State>,
    store: Mutex<Store>,
    psi: Mutex<Admission>,
    wake: Notify,
    idle: Notify,
    stop: watch::Sender<Option<bool>>,
    /// Lease token (fencing value of [`LeaseFence`]).
    token: String,
    /// Cleared when a heartbeat finds the lease lost.
    lease_held: AtomicBool,
    /// Turns `true` when the daemon starts stopping.
    shutting_down: watch::Sender<bool>,
    /// The daemon's environment sanitised for gate processes.
    sanitised_env: BTreeMap<String, String>,
    /// Extensions whose `on_start` succeeded; set before the socket is bound.
    extensions: OnceLock<Extensions>,
    /// Connection id counter.
    next_conn: AtomicU64,
}

/// Extensions after start-up.
#[derive(Debug, Default)]
struct Extensions {
    active: Vec<Arc<dyn Extension>>,
    /// `on_start` errors of disabled extensions.
    failures: Vec<String>,
    /// Concatenated features of the active extensions.
    features: Vec<String>,
}

impl State {
    fn new(config: &DaemonConfig) -> Self {
        let head_ms = u64::try_from(config.head_reserve_after.as_millis()).unwrap_or(u64::MAX);
        Self {
            sched: Scheduler::new(config.budget, head_ms, config.repo_caps.clone()),
            jobs: HashMap::new(),
            exec_inflight: HashMap::new(),
            accepting: true,
            refusal: "",
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// Runs one synchronous store operation, logging failures.
    fn store<R>(&self, what: &str, f: impl FnOnce(&Store) -> Result<R>) -> Option<R> {
        match f(&lock(&self.store)) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!("store: {what}: {e}");
                None
            }
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn extensions(&self) -> &Extensions {
        static EMPTY: Extensions = Extensions {
            active: Vec::new(),
            failures: Vec::new(),
            features: Vec::new(),
        };
        self.extensions.get().unwrap_or(&EMPTY)
    }
}

/// The daemon's own environment sanitised like a client's `child_env`;
/// falls back to dropping `GIT_*` and `CLAUDE*` when git cannot list its
/// local variables.
fn sanitised_env() -> BTreeMap<String, String> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    git::child_env(&env).unwrap_or_else(|e| {
        tracing::warn!("cannot sanitise the environment through git: {e}");
        env.into_iter()
            .filter(|(k, _)| !k.starts_with("GIT_") && !k.starts_with("CLAUDE"))
            .collect()
    })
}

/// Runs every extension's `on_start`; failed ones are disabled.
async fn start_extensions(shared: &Arc<Shared>) {
    let handle = DaemonHandle {
        shared: Arc::clone(shared),
    };
    let mut ext = Extensions::default();
    for e in &shared.config.extensions {
        match e.on_start(&handle).await {
            Ok(()) => {
                ext.features
                    .extend(e.features().iter().map(|f| (*f).to_owned()));
                ext.active.push(Arc::clone(e));
            }
            Err(err) => {
                tracing::error!(extension = ?e, "extension failed to start: {err:#}");
                ext.failures.push(format!("{err:#}"));
            }
        }
    }
    let _ = shared.extensions.set(ext);
}

fn rfc3339(ms: u64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_millis(ms)).to_string()
}

fn io_err(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
    let context = context.into();
    move |source| Error::Io { context, source }
}

/// Takes the daemon lease, reclaiming first one left by a dead daemon.
fn take_lease(store: &Store, owner: &str, token: &str, now: u64) -> Result<bool> {
    reclaim_dead_lease(store, owner)?;
    store.acquire_lease(LEASE_NAME, owner, token, now, LEASE_TTL_MS)
}

/// Frees the lease of a previous daemon of this user that died without
/// releasing it (SIGKILL, OOM, power loss of the process): its `uid:pid`
/// owner no longer exists or is a zombie. Without this a restart after a
/// crash was refused for the whole lease TTL (90 s). The caller holds the
/// daemon `flock`, and a live owner (or one of another user, or an owner
/// string that does not parse) is never touched, so an unexpired lease of a
/// running daemon is still never stolen.
fn reclaim_dead_lease(store: &Store, owner: &str) -> Result<()> {
    let Some((cur_owner, cur_token, _, _)) = store.lease(LEASE_NAME)? else {
        return Ok(());
    };
    if cur_owner == owner {
        return Ok(());
    }
    let Some((uid, pid)) = cur_owner
        .split_once(':')
        .and_then(|(u, p)| Some((u.parse::<u32>().ok()?, p.parse::<u32>().ok()?)))
    else {
        return Ok(());
    };
    if uid == current_uid() && process_is_gone(pid) {
        tracing::warn!(
            previous = %cur_owner,
            "the previous daemon died without releasing its lease; reclaiming it"
        );
        store.release_lease(LEASE_NAME, &cur_token)?;
    }
    Ok(())
}

/// True when `pid` does not exist or is a zombie.
fn process_is_gone(pid: u32) -> bool {
    if pid <= 1 || pid == std::process::id() {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // "<pid> (<comm>) <state> ...": the state follows the last ')'.
        Ok(stat) => stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().next())
            .is_some_and(|state| state == "Z" || state == "X"),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

fn current_uid() -> u32 {
    std::fs::metadata("/proc/self").map_or(0, |m| m.uid())
}

fn random_token(pid: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let a = std::collections::hash_map::RandomState::new().hash_one((nanos, pid));
    let b = std::collections::hash_map::RandomState::new().hash_one(a);
    format!("{a:016x}{b:016x}")
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(io_err(format!("creating {}", parent.display())))?;
    }
    Ok(())
}

fn take_lock(path: &Path) -> Result<Flock<File>> {
    ensure_parent(path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(io_err(format!("opening {}", path.display())))?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, errno)| {
        if errno == Errno::EWOULDBLOCK {
            Error::AlreadyRunning(path.to_path_buf())
        } else {
            Error::Io {
                context: format!("locking {}", path.display()),
                source: errno.into(),
            }
        }
    })
}

/// Kills the process groups of running jobs left by a previous instance and
/// marks their rows `orphaned`.
async fn reconcile_orphans(store: &Mutex<Store>, instance: &str) -> Result<()> {
    let orphans = lock(store).orphans(instance)?;
    for row in &orphans {
        let Some(pgid) = row.pgid else { continue };
        let Ok(leader) = u32::try_from(pgid) else {
            continue;
        };
        if executor::process_matches(leader, row.proc_start_ticks) {
            tracing::info!(job = %row.id, pgid, "killing orphaned gate process group");
            executor::terminate_group(pgid, executor::KILL_GRACE).await;
        } else {
            tracing::info!(job = %row.id, pgid, "orphaned job is no longer running");
        }
    }
    let n = lock(store).mark_orphaned(instance, now_ms())?;
    if n > 0 {
        tracing::info!("marked {n} job(s) of previous daemons as orphaned");
    }
    Ok(())
}

/// Rotates `daemon.log` when it exceeds the cap, then points stdout and
/// stderr (which tracing writes to) at the fresh file with the safe
/// `dup2` wrappers, so nothing keeps appending to the renamed file.
fn rotate_daemon_log(settings: &LogSettings) {
    let Some(path) = &settings.file else { return };
    match sluice_gate_core::logfile::rotate_if_larger(path, settings.max_bytes, DAEMON_LOG_KEEP) {
        Ok(false) => {}
        Ok(true) => {
            let reopened = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|file| {
                    nix::unistd::dup2_stdout(&file)?;
                    nix::unistd::dup2_stderr(&file)?;
                    Ok(())
                });
            match reopened {
                Ok(()) => tracing::info!("rotated {}", path.display()),
                Err(e) => tracing::warn!("rotated {} but cannot reopen it: {e}", path.display()),
            }
        }
        Err(e) => tracing::warn!("cannot rotate {}: {e}", path.display()),
    }
}

fn prune_logs(shared: &Shared) {
    rotate_daemon_log(&shared.config.log);
    let cutoff = now_ms()
        .saturating_sub(u64::try_from(shared.config.log_retention.as_millis()).unwrap_or(u64::MAX));
    if let Some(n) = shared.store("pruning logs", |s| {
        s.prune_logs(&shared.config.logs_dir, cutoff)
    }) && n > 0
    {
        tracing::info!("deleted {n} old log file(s)");
    }
}

/// Runs the daemon until `shutdown` resolves or a `stop` request arrives.
///
/// Order of operations: exclusive `flock` on `lock_path`, store and lease,
/// orphan reconciliation, socket bind (mode `0600`), then the accept loop
/// with a scheduling task and a heartbeat task. On `stop` (or `shutdown`)
/// the socket file is removed, queued jobs are dropped with outcome
/// `killed`, running jobs are left to finish (`stop` without `force`) or
/// killed (`stop --force` and `shutdown`), and the lease is released.
///
/// # Errors
///
/// [`Error::AlreadyRunning`] when another daemon holds the lock or the
/// lease; store, schema and I/O errors during start-up.
pub async fn run(config: DaemonConfig, shutdown: impl Future<Output = ()> + Send) -> Result<()> {
    let _lock = take_lock(&config.lock_path)?;
    let pid = std::process::id();
    let started_ms = now_ms();
    let instance = format!("{pid}-{started_ms}");
    let store = Store::open(&config.db_path)?;
    let token = random_token(pid);
    let owner = format!("{}:{pid}", current_uid());
    if !take_lease(&store, &owner, &token, started_ms)? {
        return Err(Error::AlreadyRunning(config.lock_path.clone()));
    }
    let store = Mutex::new(store);
    if let Err(e) = reconcile_orphans(&store, &instance).await {
        tracing::warn!("orphan reconciliation failed: {e}");
    }

    let (stop, _) = watch::channel(None);
    let (shutting_down, _) = watch::channel(false);
    let sanitised_env = tokio::task::spawn_blocking(sanitised_env)
        .await
        .unwrap_or_default();
    let shared = Arc::new(Shared {
        state: Mutex::new(State::new(&config)),
        store,
        psi: Mutex::new(Admission::new(&config.admission)),
        instance,
        pid,
        started_at: rfc3339(started_ms),
        wake: Notify::new(),
        idle: Notify::new(),
        stop,
        token: token.clone(),
        lease_held: AtomicBool::new(true),
        shutting_down,
        sanitised_env,
        extensions: OnceLock::new(),
        next_conn: AtomicU64::new(1),
        config,
    });
    prune_logs(&shared);
    start_extensions(&shared).await;

    let listener = match bind(&shared.config.socket_path) {
        Ok(l) => l,
        Err(e) => {
            release_lease(&shared, &token);
            return Err(e);
        }
    };
    tracing::info!(
        instance = %shared.instance,
        socket = %shared.config.socket_path.display(),
        budget = shared.config.budget,
        "daemon started"
    );

    let scheduling = tokio::spawn(scheduling_loop(Arc::clone(&shared)));
    let heartbeat = tokio::spawn(heartbeat_loop(Arc::clone(&shared), token.clone()));
    let sampler = lock(&shared.psi)
        .enabled
        .then(|| tokio::spawn(psi_loop(Arc::clone(&shared))));
    let mut runners = JoinSet::new();
    for e in &shared.extensions().active {
        let handle = DaemonHandle {
            shared: Arc::clone(&shared),
        };
        runners.spawn(Arc::clone(e).run(handle));
    }

    let mut conns = JoinSet::new();
    let force = accept_loop(&shared, &listener, &mut conns, shutdown).await;
    shared.shutting_down.send_replace(true);
    drop(listener);
    remove_socket(&shared.config.socket_path);

    drain(&shared, force).await;
    if timeout(EXTENSION_GRACE, async {
        while runners.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tracing::warn!("extension tasks did not stop in time; aborting them");
    }
    runners.abort_all();
    scheduling.abort();
    heartbeat.abort();
    if let Some(sampler) = sampler {
        sampler.abort();
    }
    release_lease(&shared, &token);
    let _ = timeout(CONNECTION_DRAIN, async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    conns.abort_all();
    tracing::info!(instance = %shared.instance, "daemon stopped");
    Ok(())
}

fn bind(socket: &Path) -> Result<UnixListener> {
    ensure_parent(socket)?;
    // We hold the lock, so any socket file left behind is stale.
    remove_socket(socket);
    let listener =
        UnixListener::bind(socket).map_err(io_err(format!("binding {}", socket.display())))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .map_err(io_err(format!("chmod {}", socket.display())))?;
    Ok(listener)
}

fn remove_socket(socket: &Path) {
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("cannot remove {}: {e}", socket.display()),
    }
}

fn release_lease(shared: &Shared, token: &str) {
    shared.store("releasing the lease", |s| {
        s.release_lease(LEASE_NAME, token)
    });
}

/// Accepts connections until shutdown or `stop`; returns whether running
/// jobs must be killed.
async fn accept_loop(
    shared: &Arc<Shared>,
    listener: &UnixListener,
    conns: &mut JoinSet<()>,
    shutdown: impl Future<Output = ()> + Send,
) -> bool {
    let mut stop_rx = shared.stop.subscribe();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => {
                tracing::info!("shutdown requested");
                return true;
            }
            changed = stop_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
                let requested = *stop_rx.borrow_and_update();
                if let Some(force) = requested {
                    return force;
                }
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    conns.spawn(handle_connection(Arc::clone(shared), stream));
                }
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

/// Stops accepting work, drops queued jobs, optionally kills running ones
/// and waits until no job is left.
async fn drain(shared: &Arc<Shared>, force: bool) {
    let queued: Vec<String> = {
        let mut st = shared.state();
        st.accepting = false;
        st.refusal = "the daemon is stopping";
        let ids: Vec<String> = st.jobs.keys().cloned().collect();
        let mut queued = Vec::new();
        for id in ids {
            if st.sched.is_queued(&id) {
                queued.push(id);
            } else if force && let Some(entry) = st.jobs.get_mut(&id) {
                entry.cancelled = true;
                if let Some(h) = &entry.handle {
                    h.cancel();
                }
            }
        }
        queued
    };
    for id in queued {
        let gate = gate_name(shared, &id);
        finish_job(
            shared,
            &id,
            Outcome {
                status: OutcomeStatus::Killed,
                message: format!("{gate}: dropped from the queue, the daemon is stopping"),
                ..Outcome::default()
            },
            None,
        );
    }
    tracing::info!(force, "waiting for running jobs");
    loop {
        let notified = shared.idle.notified();
        if shared.state().jobs.is_empty() {
            break;
        }
        tokio::select! {
            () = notified => {}
            () = sleep(Duration::from_millis(100)) => {}
        }
    }
}

fn gate_name(shared: &Shared, id: &str) -> String {
    shared
        .state()
        .jobs
        .get(id)
        .map(|e| e.request.name())
        .unwrap_or_default()
}

async fn heartbeat_loop(shared: Arc<Shared>, token: String) {
    let mut last_prune = now_ms();
    loop {
        sleep(HEARTBEAT).await;
        let now = now_ms();
        let held = shared.store("heartbeat", |s| {
            s.heartbeat(LEASE_NAME, &token, now, LEASE_TTL_MS)
        });
        if held == Some(false) {
            tracing::error!("the daemon lease was lost; no new work is accepted");
            shared.lease_held.store(false, Ordering::SeqCst);
            let mut st = shared.state();
            st.accepting = false;
            st.refusal = "the daemon lost its lease; run `sluice daemon stop`";
            return;
        }
        if now.saturating_sub(last_prune) >= PRUNE_EVERY_MS {
            last_prune = now;
            prune_logs(&shared);
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduling and job execution
// ---------------------------------------------------------------------------

/// Samples PSI every `psi_poll_interval` and wakes the scheduling loop when
/// the blocking state changes; exits once PSI turns out to be unavailable.
async fn psi_loop(shared: Arc<Shared>) {
    let period = shared.config.admission.psi_poll_interval;
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let sample = psi::read_sample(&shared.config.psi_root);
        let (changed, enabled) = {
            let mut adm = lock(&shared.psi);
            let before = adm.blocking();
            let after = adm.observe(sample, now_ms());
            (before != after, adm.enabled)
        };
        if changed {
            shared.wake.notify_one();
        }
        if !enabled {
            return;
        }
    }
}

/// One admission pass: the budget scheduler gated by PSI, plus the PSI
/// starvation guard (admit the queue head alone when it has waited longer
/// than `psi_max_wait` while PSI blocks).
fn admit(shared: &Shared, now: u64) -> Vec<String> {
    let blocking = lock(&shared.psi).blocking();
    let mut started = shared.state().sched.admit(now, blocking);
    if !blocking {
        return started;
    }
    let Some(head_queued_at) = shared.state().sched.head_queued_at() else {
        return started;
    };
    if !lock(&shared.psi).starvation_override(head_queued_at, now) {
        return started;
    }
    if let Some(id) = shared.state().sched.admit_head(now) {
        lock(&shared.psi).record_override(now);
        tracing::info!(
            job = %id,
            waited_ms = now.saturating_sub(head_queued_at),
            "PSI starvation guard: starting the queue head despite system pressure"
        );
        started.push(id);
    }
    started
}

async fn scheduling_loop(shared: Arc<Shared>) {
    loop {
        tokio::select! {
            () = shared.wake.notified() => {}
            () = sleep(ADMIT_TICK) => {}
        }
        let started = admit(&shared, now_ms());
        for id in started {
            tracing::debug!(job = %id, "admitted");
            tokio::spawn(start_job(Arc::clone(&shared), id));
        }
    }
}

/// Completes a job: records it, removes it from the scheduler and the job
/// map, broadcasts `done` and wakes the scheduling loop.
fn finish_job(shared: &Shared, id: &str, outcome: Outcome, exit_code: Option<i32>) {
    let label = match outcome.status {
        OutcomeStatus::Pass => "pass",
        OutcomeStatus::Fail => "fail",
        OutcomeStatus::Timeout => "timeout",
        OutcomeStatus::Killed => "killed",
        OutcomeStatus::Cached => "cached",
        OutcomeStatus::Error => "error",
    };
    let events = {
        let mut st = shared.state();
        let max = st.sched.subscribers_max(id).unwrap_or(1);
        if st.sched.finish(id).is_none() {
            st.sched.remove_queued(id);
        }
        let entry = st.jobs.remove(id);
        if let Some(identity) = entry.as_ref().and_then(|e| e.exec_identity.as_ref())
            && st.exec_inflight.get(identity).is_some_and(|j| j == id)
        {
            st.exec_inflight.remove(identity);
        }
        let coalesced = entry.as_ref().map_or(0, |e| e.coalesced);
        shared.store("recording the outcome", |s| {
            s.set_subscribers(id, u32::try_from(max).unwrap_or(u32::MAX), coalesced)?;
            s.mark_done(
                id,
                &shared.instance,
                now_ms(),
                label,
                exit_code,
                outcome.marker_written,
            )
        });
        entry.map(|e| e.events)
    };
    tracing::info!(job = %id, outcome = label, "job done");
    if let Some(events) = events {
        let _ = events.send(JobEvent::Done(Box::new(outcome)));
    }
    shared.wake.notify_one();
    shared.idle.notify_waiters();
}

fn repo_paths(request: &RunRequest) -> RepoPaths {
    RepoPaths {
        toplevel: request.repo.toplevel.clone(),
        git_dir: request.repo.git_dir.clone(),
        common_dir: request.repo.common_dir.clone(),
        index_file: request.repo.git_dir.join("index"),
        objects_dir: request.repo.common_dir.join("objects"),
    }
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Re-checks the marker store right before a start; `Some` on a fresh hit.
async fn marker_hit(shared: &Shared, request: &RunRequest) -> Option<Marker> {
    let key = request.key.clone()?;
    let common = request.repo.common_dir.clone();
    let ttl = shared.config.marker_ttl;
    match tokio::task::spawn_blocking(move || MarkerStore::new(&common).lookup(&key, ttl)).await {
        Ok(Ok(Lookup::Hit(marker))) => Some(marker),
        _ => None,
    }
}

fn timeout_of(ms: u64) -> Duration {
    if ms == 0 {
        NO_TIMEOUT
    } else {
        Duration::from_millis(ms)
    }
}

fn exec_spec(shared: &Shared, request: &RunRequest, id: &str, log_path: &Path) -> ExecSpec {
    let mut env = request.child_env.clone();
    env.extend(request.gate.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.insert(JOB_ENV_VAR.to_owned(), id.to_owned());
    ExecSpec {
        job_id: id.to_owned(),
        gate: request.gate.name.clone(),
        launch: Launch::Shell {
            cmd: request.cmd.clone(),
            extra: request.extra.clone(),
        },
        sink: Sink::Log,
        cwd: request.cwd.clone(),
        env,
        timeout: timeout_of(request.gate.timeout_ms),
        nice: shared.config.nice,
        ionice: shared.config.ionice,
        log_path: log_path.to_path_buf(),
    }
}

async fn start_job(shared: Arc<Shared>, id: String) {
    let Some(request) = shared.state().jobs.get(&id).map(|e| e.request.clone()) else {
        return;
    };
    match request {
        JobRequest::Gate(request) => start_gate(shared, id, request).await,
        JobRequest::Exec(request) => start_exec(shared, id, request).await,
    }
}

/// Spawns a job, records its start, broadcasts `started` and waits for it
/// with periodic `progress`. `None` when it could not start (the job is
/// then already finished with an `error` outcome).
async fn launch(shared: &Shared, id: &str, spec: ExecSpec) -> Option<Finished> {
    let name = spec.gate.clone();
    let log_path = spec.log_path.clone();
    let spool = (spec.sink == Sink::Spool).then(|| log_path.clone());
    let running = match executor::spawn(spec).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(job = %id, "cannot start the job: {e}");
            let outcome = Outcome {
                status: OutcomeStatus::Error,
                log_path: Some(log_path),
                message: format!("{name}: cannot start: {e}"),
                ..Outcome::default()
            };
            finish_job(shared, id, outcome, None);
            return None;
        }
    };
    let info = running.info().clone();
    let handle = running.handle();
    shared.store("recording the start", |s| {
        s.mark_started(
            id,
            &shared.instance,
            info.started_at,
            Some(info.pgid),
            info.proc_start_ticks,
            &log_path,
        )
    });
    let events = {
        let mut st = shared.state();
        let Some(entry) = st.jobs.get_mut(id) else {
            handle.cancel();
            return None;
        };
        entry.handle = Some(handle.clone());
        if entry.cancelled {
            handle.cancel();
        }
        entry.started_at = Some(info.started_at);
        entry.spool.clone_from(&spool);
        entry.process = Some(info.clone());
        let _ = entry.events.send(JobEvent::Started {
            started_at: info.started_at,
            spool,
        });
        entry.events.clone()
    };
    tracing::info!(job = %id, name = %name, pgid = info.pgid, "job started");

    let mut wait = std::pin::pin!(running.wait());
    let mut tick = interval_at(Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY);
    Some(loop {
        tokio::select! {
            f = &mut wait => break f,
            _ = tick.tick() => {
                let _ = events.send(JobEvent::Progress {
                    elapsed_ms: now_ms().saturating_sub(info.started_at),
                    log_bytes: handle.log_bytes(),
                });
            }
        }
    })
}

/// Runs an `exec` job: its argv with the caller's environment plus
/// `SLUICE_JOB`, output framed into `<logs>/<job>.exec`.
async fn start_exec(shared: Arc<Shared>, id: String, request: Arc<ExecRequest>) {
    let name = format!("exec '{}'", request.label);
    if shared.state().jobs.get(&id).is_none_or(|e| e.cancelled) {
        finish_job(&shared, &id, killed_outcome(&name), None);
        return;
    }
    let Some((program, args)) = request.argv.split_first() else {
        let outcome = Outcome {
            status: OutcomeStatus::Error,
            message: format!("{name}: empty argv"),
            ..Outcome::default()
        };
        finish_job(&shared, &id, outcome, None);
        return;
    };
    let mut env = request.env.clone();
    env.insert(JOB_ENV_VAR.to_owned(), id.clone());
    let spec = ExecSpec {
        job_id: id.clone(),
        gate: name.clone(),
        launch: Launch::Argv {
            program: program.clone(),
            args: args.to_vec(),
        },
        sink: Sink::Spool,
        cwd: request.cwd.clone(),
        env,
        timeout: timeout_of(request.timeout_ms),
        nice: shared.config.nice,
        ionice: shared.config.ionice,
        log_path: shared.config.logs_dir.join(format!("{id}.exec")),
    };
    let Some(finished) = launch(&shared, &id, spec).await else {
        return;
    };
    let secs = finished.duration.as_secs_f64();
    let mut outcome = Outcome {
        duration_ms: duration_ms(finished.duration),
        log_path: Some(finished.log_path.clone()),
        ..Outcome::default()
    };
    (outcome.status, outcome.message) = match finished.status {
        ExecStatus::Exited(0) => (
            OutcomeStatus::Pass,
            format!("{name} exited 0 in {secs:.1}s"),
        ),
        ExecStatus::Exited(n) => (
            OutcomeStatus::Fail,
            format!("{name} exited {n} after {secs:.1}s"),
        ),
        ExecStatus::Signaled(s) => (
            OutcomeStatus::Fail,
            format!("{name} was killed by signal {s} after {secs:.1}s"),
        ),
        ExecStatus::Timeout => (
            OutcomeStatus::Timeout,
            format!(
                "{name} timed out after {}",
                humantime::format_duration(Duration::from_millis(request.timeout_ms))
            ),
        ),
        ExecStatus::Killed => (
            OutcomeStatus::Killed,
            format!("{name} was killed after {secs:.1}s"),
        ),
    };
    match finished.status {
        ExecStatus::Exited(n) => outcome.exit_code = Some(n),
        ExecStatus::Signaled(s) => outcome.signal = Some(s),
        ExecStatus::Timeout | ExecStatus::Killed => {}
    }
    let tail = shared.config.tail_lines;
    let log = finished.log_path;
    outcome.tail = tokio::task::spawn_blocking(move || executor::tail_lines(&log, tail))
        .await
        .unwrap_or_default();
    let exit_code = outcome.exit_code;
    finish_job(&shared, &id, outcome, exit_code);
}

async fn start_gate(shared: Arc<Shared>, id: String, request: Arc<RunRequest>) {
    let gate = request.gate.name.clone();

    let marker = if request.no_marker {
        None
    } else {
        marker_hit(&shared, &request).await
    };
    if let Some(marker) = marker {
        let outcome = Outcome {
            status: OutcomeStatus::Cached,
            exit_code: Some(0),
            duration_ms: marker.duration_ms,
            message: format!(
                "{gate}: a pass for this tree appeared while queued (job {})",
                marker.job_id
            ),
            ..Outcome::default()
        };
        finish_job(&shared, &id, outcome, Some(0));
        return;
    }
    if shared.state().jobs.get(&id).is_none_or(|e| e.cancelled) {
        finish_job(&shared, &id, killed_outcome(&gate), None);
        return;
    }

    let log_path = shared.config.logs_dir.join(format!("{id}.log"));
    let Some(finished) = launch(&shared, &id, exec_spec(&shared, &request, &id, &log_path)).await
    else {
        return;
    };
    let (outcome, exit_code) = conclude(&shared, &request, &id, finished).await;
    finish_job(&shared, &id, outcome, exit_code);
}

fn killed_outcome(gate: &str) -> Outcome {
    Outcome {
        status: OutcomeStatus::Killed,
        message: format!("{gate} was killed"),
        ..Outcome::default()
    }
}

/// Status, exit code, message and miss reason of a finished run.
fn classify(
    request: &RunRequest,
    finished: &Finished,
) -> (OutcomeStatus, Option<i32>, String, Option<&'static str>) {
    let gate = &request.gate.name;
    let secs = finished.duration.as_secs_f64();
    match finished.status {
        ExecStatus::Exited(0) => (
            OutcomeStatus::Pass,
            Some(0),
            format!("{gate} passed in {secs:.1}s"),
            None,
        ),
        ExecStatus::Exited(n) => (
            OutcomeStatus::Fail,
            Some(n),
            format!("{gate} failed with exit code {n} after {secs:.1}s"),
            Some("gate failed"),
        ),
        ExecStatus::Signaled(_) => (
            OutcomeStatus::Fail,
            None,
            format!("{gate} was terminated by a signal after {secs:.1}s"),
            Some("gate failed"),
        ),
        ExecStatus::Timeout => (
            OutcomeStatus::Timeout,
            None,
            format!(
                "{gate} timed out after {}",
                humantime::format_duration(Duration::from_millis(request.gate.timeout_ms))
            ),
            Some("timed out"),
        ),
        ExecStatus::Killed => (
            OutcomeStatus::Killed,
            None,
            format!("{gate} was killed after {secs:.1}s"),
            Some("killed"),
        ),
    }
}

/// Inputs of the post-run cache bookkeeping (runs on a blocking thread).
struct CacheWrite {
    request: Arc<RunRequest>,
    key: CacheKey,
    job_id: String,
    version: String,
    ttl: Duration,
    log_path: PathBuf,
    duration_ms: u64,
    exit_code: Option<i32>,
    miss_reason: Option<&'static str>,
}

impl CacheWrite {
    fn miss(&self, store: &MarkerStore, exit_code: Option<i32>, reason: &str) {
        let record = MissRecord {
            format: CACHE_FORMAT_VERSION,
            key: self.key.clone(),
            gate: self.request.gate.name.clone(),
            exit_code,
            reason: reason.to_owned(),
            job_id: self.job_id.clone(),
            log_path: Some(self.log_path.clone()),
            recorded_at: sluice_gate_core::dirs::now_rfc3339(),
        };
        if let Err(e) = store.write_miss(&record) {
            tracing::warn!("cannot record the miss: {e}");
        }
    }

    /// Writes the marker (pass on an unchanged tree) or a miss record;
    /// returns `(marker_written, not_cached_reason)`.
    fn apply(self) -> (bool, Option<String>) {
        let store = MarkerStore::new(&self.request.repo.common_dir);
        if let Some(reason) = self.miss_reason {
            self.miss(&store, self.exit_code, reason);
            return (false, None);
        }
        let Some(tree_before) = self.request.tree.clone() else {
            return (false, Some("the tree is unknown".to_owned()));
        };
        let env: BTreeMap<String, String> = std::env::vars().collect();
        let after = tree::scratch_root(&env).and_then(|root| {
            tree::snapshot(&repo_paths(&self.request), &self.request.probe_env, &root)
        });
        match after {
            Ok(snap) if snap.tree() == Some(&tree_before) => {
                let marker = Marker {
                    format: CACHE_FORMAT_VERSION,
                    key: self.key.clone(),
                    tree: tree_before,
                    gate: self.request.gate.name.clone(),
                    cmd: command_with_args(
                        &normalize_cmd(&self.request.gate.cmd),
                        &self.request.extra,
                    ),
                    passed_at: sluice_gate_core::dirs::now_rfc3339(),
                    duration_ms: self.duration_ms,
                    exit_code: 0,
                    job_id: self.job_id.clone(),
                    sluice_version: self.version.clone(),
                    committed_tree: self.request.committed_tree.clone(),
                };
                match store.write(&marker) {
                    Ok(()) => {
                        let _ = store.prune(self.ttl);
                        (true, None)
                    }
                    Err(e) => (false, Some(format!("cannot write the marker: {e}"))),
                }
            }
            Ok(_) => {
                self.miss(&store, Some(0), "the gate modified the tree");
                (
                    false,
                    Some("the gate modified the tree; verify the new tree again".to_owned()),
                )
            }
            Err(e) => (false, Some(format!("cannot snapshot the tree: {e}"))),
        }
    }
}

/// Maps a finished run to its outcome and writes the marker or miss record.
async fn conclude(
    shared: &Arc<Shared>,
    request: &Arc<RunRequest>,
    id: &str,
    finished: Finished,
) -> (Outcome, Option<i32>) {
    let (status, exit_code, message, miss_reason) = classify(request, &finished);
    let mut outcome = Outcome {
        status,
        exit_code,
        duration_ms: duration_ms(finished.duration),
        log_path: Some(finished.log_path.clone()),
        message,
        ..Outcome::default()
    };
    // Queue-only gates (`no_marker`) never write a marker or a miss record.
    if let Some(key) = request.key.clone().filter(|_| !request.no_marker) {
        let write = CacheWrite {
            request: Arc::clone(request),
            key,
            job_id: id.to_owned(),
            version: shared.config.version.clone(),
            ttl: shared.config.marker_ttl,
            log_path: finished.log_path.clone(),
            duration_ms: outcome.duration_ms,
            exit_code,
            miss_reason,
        };
        match tokio::task::spawn_blocking(move || write.apply()).await {
            Ok((written, reason)) => {
                outcome.marker_written = written;
                outcome.not_cached_reason = reason;
            }
            Err(e) => outcome.not_cached_reason = Some(format!("marker task failed: {e}")),
        }
    }
    let tail = shared.config.tail_lines;
    let log = finished.log_path;
    outcome.tail = tokio::task::spawn_blocking(move || executor::tail_lines(&log, tail))
        .await
        .unwrap_or_default();
    (outcome, exit_code)
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// Why a line could not be read.
#[derive(Debug)]
enum LineError {
    TooLong,
    NotUtf8,
    Io,
}

/// Reads one line of at most [`MAX_LINE_BYTES`] bytes; `Ok(None)` on EOF.
async fn read_line(
    rd: &mut BufReader<OwnedReadHalf>,
) -> std::result::Result<Option<String>, LineError> {
    let mut buf = Vec::new();
    let limit = u64::try_from(MAX_LINE_BYTES).unwrap_or(u64::MAX) + 2;
    let n = (&mut *rd)
        .take(limit)
        .read_until(b'\n', &mut buf)
        .await
        .map_err(|_| LineError::Io)?;
    if n == 0 {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    if buf.len() > MAX_LINE_BYTES {
        return Err(LineError::TooLong);
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| LineError::NotUtf8)
}

async fn send(wr: &mut OwnedWriteHalf, msg: &ServerMessage) -> bool {
    tracing::debug!(?msg, "send");
    let Ok(line) = proto::encode(msg) else {
        return false;
    };
    wr.write_all(line.as_bytes()).await.is_ok()
}

async fn send_error(wr: &mut OwnedWriteHalf, id: u64, code: ErrorCode, message: impl Into<String>) {
    let _ = send(
        wr,
        &ServerMessage::Error {
            id,
            code,
            message: message.into(),
        },
    )
    .await;
}

fn line_error_message(e: &LineError) -> &'static str {
    match e {
        LineError::TooLong => "line longer than 1 MiB",
        LineError::NotUtf8 => "line is not valid UTF-8",
        LineError::Io => "read error",
    }
}

/// Reads one request line with the handshake timeout. Replies with an
/// error and returns `None` when the line is unusable.
async fn read_message(
    rd: &mut BufReader<OwnedReadHalf>,
    wr: &mut OwnedWriteHalf,
) -> Option<std::result::Result<ClientMessage, serde_json::Error>> {
    match timeout(HANDSHAKE_TIMEOUT, read_line(rd)).await {
        Ok(Ok(Some(line))) => {
            tracing::debug!(line = %line, "recv");
            Some(proto::decode_client(&line))
        }
        Ok(Err(e @ (LineError::TooLong | LineError::NotUtf8))) => {
            send_error(wr, 0, ErrorCode::BadRequest, line_error_message(&e)).await;
            None
        }
        Ok(Ok(None) | Err(LineError::Io)) | Err(_) => None,
    }
}

/// Reads `hello` and answers `welcome`; returns the client name, `None`
/// when the connection must close.
async fn handshake(
    shared: &Shared,
    rd: &mut BufReader<OwnedReadHalf>,
    wr: &mut OwnedWriteHalf,
) -> Option<String> {
    let client = match read_message(rd, wr).await {
        None => return None,
        Some(Ok(ClientMessage::Hello {
            protocol, client, ..
        })) if protocol == PROTOCOL_VERSION => {
            tracing::debug!(client = %client, "hello");
            client
        }
        Some(Ok(ClientMessage::Hello { protocol, .. })) => {
            let msg = format!(
                "this daemon speaks protocol {PROTOCOL_VERSION}, the client speaks {protocol}; run `sluice daemon stop`"
            );
            send_error(wr, 0, ErrorCode::Protocol, msg).await;
            return None;
        }
        Some(Ok(_)) => {
            send_error(wr, 0, ErrorCode::Protocol, "expected hello").await;
            return None;
        }
        Some(Err(e)) => {
            send_error(
                wr,
                0,
                ErrorCode::BadRequest,
                format!("malformed hello: {e}"),
            )
            .await;
            return None;
        }
    };
    let mut features = vec![FEATURE_EXEC.to_owned(), FEATURE_QUEUE_ONLY.to_owned()];
    features.extend(shared.extensions().features.iter().cloned());
    let welcome = ServerMessage::Welcome {
        protocol: PROTOCOL_VERSION,
        daemon: format!("sluice-gate {}", shared.config.version),
        instance: shared.instance.clone(),
        pid: shared.pid,
        started_at: shared.started_at.clone(),
        features,
    };
    send(wr, &welcome).await.then_some(client)
}

/// Message of the `not-supported` error for `queue-*` requests.
const QUEUE_NOT_SUPPORTED: &str =
    "the running daemon does not support the merge queue; run 'sluice daemon stop' and retry";

/// Routes a `queue-*` request to the first extension that answers.
async fn dispatch_extension(
    shared: &Shared,
    wr: &mut OwnedWriteHalf,
    id: u64,
    msg: ClientMessage,
    ctx: ExtensionCtx,
) {
    let ext = shared.extensions();
    for e in &ext.active {
        if let Some(reply) = e.handle(msg.clone(), &ctx).await {
            let _ = send(wr, &reply).await;
            return;
        }
    }
    if let Some(failure) = ext.failures.first() {
        let message = format!("the merge queue failed to start: {failure}");
        send_error(wr, id, ErrorCode::Internal, message).await;
    } else {
        send_error(wr, id, ErrorCode::NotSupported, QUEUE_NOT_SUPPORTED).await;
    }
}

async fn handle_connection(shared: Arc<Shared>, stream: UnixStream) {
    let conn_id = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let peer_pid = stream.peer_cred().ok().and_then(|c| c.pid());
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    let Some(client) = handshake(&shared, &mut rd, &mut wr).await else {
        return;
    };

    let request = match read_message(&mut rd, &mut wr).await {
        None => return,
        Some(Ok(m)) => m,
        Some(Err(e)) => {
            send_error(
                &mut wr,
                0,
                ErrorCode::BadRequest,
                format!("malformed request: {e}"),
            )
            .await;
            return;
        }
    };
    match request {
        ClientMessage::Run { id, request } => handle_run(&shared, rd, wr, id, request).await,
        ClientMessage::Exec { id, request } => handle_exec(&shared, rd, wr, id, request).await,
        ClientMessage::Status { id } => {
            let mut status = status_report(&shared);
            for e in &shared.extensions().active {
                if let Some(summary) = e.status_summary().await {
                    status.queue = Some(summary);
                    break;
                }
            }
            let _ = send(&mut wr, &ServerMessage::Status { id, status }).await;
        }
        msg @ (ClientMessage::QueueEnqueue { id, .. }
        | ClientMessage::QueueStatus { id, .. }
        | ClientMessage::QueueApprove { id, .. }
        | ClientMessage::QueueRetry { id, .. }
        | ClientMessage::QueueCancel { id, .. }
        | ClientMessage::QueueReconcile { id, .. }) => {
            let ctx = ExtensionCtx {
                conn_id,
                client,
                peer_pid,
            };
            dispatch_extension(&shared, &mut wr, id, msg, ctx).await;
        }
        ClientMessage::Logs { id, job_id, tail } => {
            handle_logs(&shared, &mut wr, id, job_id, tail).await;
        }
        ClientMessage::Event {
            event:
                ClientEvent::Hit {
                    repo,
                    gate,
                    saved_ms,
                },
            ..
        } => {
            shared.store("recording a hit", |s| {
                s.record_hit(now_ms(), &repo, &gate, saved_ms)
            });
        }
        ClientMessage::Stop { id, force } => {
            let running = {
                let st = shared.state();
                st.jobs.keys().filter(|j| st.sched.is_running(j)).count()
            };
            tracing::info!(force, running, "stop requested");
            let _ = send(&mut wr, &ServerMessage::Stopping { id, running }).await;
            remove_socket(&shared.config.socket_path);
            shared.stop.send_replace(Some(force));
        }
        ClientMessage::Cancel { id } => {
            send_error(
                &mut wr,
                id,
                ErrorCode::BadRequest,
                "cancel outside a run connection",
            )
            .await;
        }
        ClientMessage::Signal { id, .. } => {
            send_error(
                &mut wr,
                id,
                ErrorCode::BadRequest,
                "signal outside an exec connection",
            )
            .await;
        }
        ClientMessage::Hello { .. } => {
            send_error(&mut wr, 0, ErrorCode::Protocol, "duplicate hello").await;
        }
    }
}

fn status_report(shared: &Shared) -> StatusReport {
    let now = now_ms();
    let (psi, psi_blocked_ms_total, psi_blocking) = {
        let adm = lock(&shared.psi);
        (adm.info(), adm.blocked_ms_total(), adm.blocking())
    };
    let (running, queued, total, used) = {
        let st = shared.state();
        let (running, queued) = st.sched.snapshot(now, psi_blocking);
        let caller = |id: &str| {
            st.jobs
                .get(id)
                .map_or(Caller::default(), |e| e.request.caller())
        };
        let kind_label = |id: &str| {
            st.jobs
                .get(id)
                .map_or((None, None), |e| e.request.kind_label())
        };
        let running: Vec<RunningJob> = running
            .into_iter()
            .map(|v| {
                let (kind, label) = kind_label(&v.id);
                RunningJob {
                    caller: caller(&v.id),
                    kind,
                    label,
                    job_id: v.id,
                    repo: v.repo,
                    gate: v.gate,
                    weight: v.weight,
                    started_at: rfc3339(v.started_at),
                    elapsed_ms: now.saturating_sub(v.started_at),
                    subscribers: v.subscribers,
                }
            })
            .collect();
        let queued: Vec<QueuedJob> = queued
            .into_iter()
            .map(|v| {
                let (kind, label) = kind_label(&v.id);
                QueuedJob {
                    kind,
                    label,
                    job_id: v.id,
                    repo: v.repo,
                    gate: v.gate,
                    weight: v.weight,
                    queued_at: rfc3339(v.queued_at),
                    waiting_ms: now.saturating_sub(v.queued_at),
                    subscribers: v.subscribers,
                    reason: v.reason,
                }
            })
            .collect();
        (running, queued, st.sched.budget(), st.sched.used())
    };
    let mut totals = shared
        .store("reading totals", |s| s.totals(None))
        .unwrap_or_default();
    // Kept in memory: covers this daemon instance only.
    totals.psi_blocked_ms_total = psi_blocked_ms_total;
    StatusReport {
        schema: STATUS_SCHEMA_VERSION,
        daemon: DaemonInfo {
            instance: shared.instance.clone(),
            pid: shared.pid,
            started_at: shared.started_at.clone(),
            version: shared.config.version.clone(),
            socket: shared.config.socket_path.clone(),
            log_level: shared.config.log.level.clone(),
        },
        budget: BudgetInfo { total, used, psi },
        running,
        queued,
        totals,
        queue: None,
    }
}

async fn handle_logs(
    shared: &Shared,
    wr: &mut OwnedWriteHalf,
    id: u64,
    job_id: Option<String>,
    tail: usize,
) {
    let row = shared.store("reading a job", |s| match &job_id {
        Some(j) => s.job(j),
        None => s.newest_job(),
    });
    let Some(Some(row)) = row else {
        send_error(wr, id, ErrorCode::NotFound, "no such job").await;
        return;
    };
    let Some(path) = row.log_path else {
        send_error(
            wr,
            id,
            ErrorCode::NotFound,
            format!("job {} has no log yet", row.id),
        )
        .await;
        return;
    };
    let n = if tail == 0 {
        shared.config.tail_lines
    } else {
        tail
    };
    let p = path.clone();
    let lines = tokio::task::spawn_blocking(move || executor::tail_lines(&p, n))
        .await
        .unwrap_or_default();
    let _ = send(
        wr,
        &ServerMessage::Log {
            id,
            job_id: row.id,
            path,
            lines,
        },
    )
    .await;
}

fn caller_str(caller: Caller) -> &'static str {
    match caller {
        Caller::Agent => "agent",
        Caller::Human => "human",
    }
}

/// Repository identity used for the per-repository cap and `status`: the
/// parent of the git common dir when it is named `.git` (the main worktree),
/// else the common dir itself (bare repositories), so every worktree of one
/// repository shares it.
fn repo_identity(repo: &proto::RepoInfo) -> PathBuf {
    let common = &repo.common_dir;
    if common.file_name().is_some_and(|n| n == ".git")
        && let Some(parent) = common.parent()
    {
        return parent.to_path_buf();
    }
    common.clone()
}

/// A job about to be registered, gate or exec.
struct NewJob {
    request: JobRequest,
    /// Scheduler coalescing key (gates only).
    key: Option<CacheKey>,
    /// Exec coalescing identity (exec with `--coalesce-key` only).
    exec_identity: Option<String>,
    /// Repository identity for the per-repository cap and `status`.
    repo: PathBuf,
    weight: u32,
    /// Database columns.
    row_key: Option<String>,
    row_tree: Option<String>,
    row_repo: PathBuf,
    cmd: String,
    session: Option<String>,
}

/// Registers a `run` request (new job or coalesced), or returns an error message.
fn register(shared: &Shared, request: RunRequest) -> std::result::Result<Registered, String> {
    register_job(
        shared,
        NewJob {
            key: request.key.clone(),
            exec_identity: None,
            repo: repo_identity(&request.repo),
            weight: request.gate.weight,
            row_key: request.key.as_ref().map(|k| k.as_str().to_owned()),
            row_tree: request.tree.as_ref().map(|t| t.as_str().to_owned()),
            row_repo: request.repo.toplevel.clone(),
            cmd: request.cmd.clone(),
            session: request.session.clone(),
            request: JobRequest::Gate(Arc::new(request)),
        },
    )
}

/// Identity of an exec request with `--coalesce-key K`: SHA-256 of
/// `"sluice-exec-v1\0" K "\0" cwd "\0" argv joined by NUL`. The environment
/// is deliberately not part of it.
fn exec_identity(request: &ExecRequest) -> Option<String> {
    let key = request.coalesce.as_ref()?;
    let mut h = Sha256::new();
    h.update(b"sluice-exec-v1\0");
    h.update(key.as_bytes());
    h.update(b"\0");
    h.update(request.cwd.as_os_str().as_encoded_bytes());
    h.update(b"\0");
    h.update(request.argv.join("\0").as_bytes());
    Some(
        h.finalize()
            .iter()
            .fold(String::with_capacity(64), |mut s, b| {
                use std::fmt::Write as _;
                let _ = write!(s, "{b:02x}");
                s
            }),
    )
}

/// `argv` quoted for display (the `cmd` column of exec jobs).
fn shell_quote(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            let plain = !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
            if plain {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Registers an `exec` request (new job or coalesced), or returns an error message.
fn register_exec(shared: &Shared, request: ExecRequest) -> std::result::Result<Registered, String> {
    let repo = request.repo.clone().unwrap_or_else(|| request.cwd.clone());
    register_job(
        shared,
        NewJob {
            key: None,
            exec_identity: exec_identity(&request),
            repo: repo.clone(),
            weight: request.weight,
            row_key: None,
            row_tree: None,
            row_repo: repo,
            cmd: shell_quote(&request.argv),
            session: request.session.clone(),
            request: JobRequest::Exec(Arc::new(request)),
        },
    )
}

/// Attaches one more subscriber (already counted by the scheduler) to `job_id`.
fn coalesce_into(
    shared: &Shared,
    st: &mut State,
    job_id: String,
    position: Option<usize>,
) -> std::result::Result<Registered, String> {
    let weight = st.sched.spec(&job_id).map_or(1, |s| s.weight);
    let max = st.sched.subscribers_max(&job_id).unwrap_or(1);
    let Some(entry) = st.jobs.get_mut(&job_id) else {
        st.sched.unsubscribe(&job_id);
        return Err("internal error: coalesced into an unknown job".to_owned());
    };
    entry.coalesced += 1;
    let coalesced = entry.coalesced;
    let started_at = entry.started_at;
    let spool = entry.spool.clone();
    let rx = entry.events.subscribe();
    shared.store("updating subscribers", |s| {
        s.set_subscribers(&job_id, u32::try_from(max).unwrap_or(u32::MAX), coalesced)
    });
    Ok(Registered {
        job_id,
        coalesced: true,
        position: position.unwrap_or(0),
        weight,
        started_at,
        spool,
        events: rx,
    })
}

/// Registers a new job or attaches the request to an identical one.
fn register_job(shared: &Shared, job: NewJob) -> std::result::Result<Registered, String> {
    let now = now_ms();
    let mut st = shared.state();
    if !st.accepting {
        return Err(st.refusal.to_owned());
    }
    if let Some(identity) = &job.exec_identity
        && let Some(existing) = st.exec_inflight.get(identity).cloned()
        && st.sched.subscribe(&existing).is_some()
    {
        let position = st.sched.position(&existing);
        return coalesce_into(shared, &mut st, existing, position);
    }
    let id = new_job_id();
    let spec = JobSpec {
        id: id.clone(),
        key: job.key.clone(),
        repo: job.repo.clone(),
        gate: job.request.name(),
        weight: job.weight,
        queued_at: now,
    };
    match st.sched.enqueue(spec) {
        Enqueued::Created { position } => {
            let weight = st.sched.spec(&id).map_or(1, |s| s.weight);
            let row = JobRow {
                id: id.clone(),
                key: job.row_key,
                tree: job.row_tree,
                repo: job.row_repo,
                gate: job.request.name(),
                cmd: job.cmd,
                weight,
                caller: caller_str(job.request.caller()).to_owned(),
                session: job.session,
                state: JobState::Queued,
                outcome: None,
                exit_code: None,
                queued_at: now,
                started_at: None,
                finished_at: None,
                log_path: None,
                pgid: None,
                proc_start_ticks: None,
                daemon_instance: shared.instance.clone(),
                subscribers_max: 1,
                coalesced: 0,
                marker_written: false,
            };
            shared.store("inserting a job", |s| s.insert_job(&row));
            let (events, rx) = broadcast::channel(CHANNEL_CAPACITY);
            if let Some(identity) = &job.exec_identity {
                st.exec_inflight.insert(identity.clone(), id.clone());
            }
            st.jobs.insert(
                id.clone(),
                JobEntry {
                    request: job.request,
                    queued_at: now,
                    events,
                    coalesced: 0,
                    cancelled: false,
                    handle: None,
                    started_at: None,
                    spool: None,
                    process: None,
                    exec_identity: job.exec_identity,
                },
            );
            Ok(Registered {
                job_id: id,
                coalesced: false,
                position,
                weight,
                started_at: None,
                spool: None,
                events: rx,
            })
        }
        Enqueued::Coalesced { job_id, position } => {
            coalesce_into(shared, &mut st, job_id, position)
        }
    }
}

/// Removes one subscriber and applies `last_subscriber_gone` when it was the last.
fn detach(shared: &Shared, job_id: &str) {
    let drop_queued = {
        let mut st = shared.state();
        let Some(remaining) = st.sched.unsubscribe(job_id) else {
            return;
        };
        tracing::debug!(job = %job_id, remaining, "subscriber detached");
        // `finish` keeps a gate running to cache its result; an exec job
        // has nothing to cache, so losing its last subscriber always kills it.
        let is_exec = st.jobs.get(job_id).is_some_and(|e| e.request.is_exec());
        if remaining > 0
            || (shared.config.last_subscriber_gone == LastSubscriberGone::Finish && !is_exec)
        {
            return;
        }
        if st.sched.is_queued(job_id) {
            true
        } else {
            if let Some(entry) = st.jobs.get_mut(job_id) {
                tracing::info!(job = %job_id, "last subscriber gone, cancelling");
                entry.cancelled = true;
                if let Some(h) = &entry.handle {
                    h.cancel();
                }
            }
            false
        }
    };
    if drop_queued {
        let gate = gate_name(shared, job_id);
        tracing::info!(job = %job_id, "last subscriber gone, dropping the queued job");
        finish_job(shared, job_id, killed_outcome(&gate), None);
    }
}

/// Reads the client side of a `run` connection in its own task (`read_line`
/// is not cancel-safe); `None` on the channel means EOF or a read error.
fn spawn_reader(
    mut rd: BufReader<OwnedReadHalf>,
) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<Option<String>>) {
    let (tx, rx) = mpsc::channel::<Option<String>>(4);
    let task = tokio::spawn(async move {
        loop {
            let line = read_line(&mut rd).await.ok().flatten();
            let eof = line.is_none();
            if tx.send(line).await.is_err() || eof {
                return;
            }
        }
    });
    (task, rx)
}

/// One subscriber of a job: its request id and how it joined.
struct Subscriber<'a> {
    shared: &'a Shared,
    id: u64,
    job_id: String,
    coalesced: bool,
}

impl Subscriber<'_> {
    /// Translates a job event; the bool is true for the final message.
    fn message(
        &self,
        event: std::result::Result<JobEvent, broadcast::error::RecvError>,
    ) -> Option<(ServerMessage, bool)> {
        let (id, job_id) = (self.id, self.job_id.clone());
        match event {
            Ok(JobEvent::Started { started_at, spool }) => Some((
                ServerMessage::Started {
                    id,
                    job_id,
                    started_at: rfc3339(started_at),
                    spool,
                },
                false,
            )),
            Ok(JobEvent::Progress {
                elapsed_ms,
                log_bytes,
            }) => Some((
                ServerMessage::Progress {
                    id,
                    job_id,
                    elapsed_ms,
                    log_bytes,
                    position: None,
                },
                false,
            )),
            Ok(JobEvent::Done(outcome)) => {
                let mut outcome = *outcome;
                if self.coalesced {
                    outcome.coalesced_with = Some(job_id.clone());
                }
                Some((
                    ServerMessage::Done {
                        id,
                        job_id,
                        outcome,
                    },
                    true,
                ))
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!(job = %job_id, "subscriber lagged by {n} events");
                None
            }
            Err(broadcast::error::RecvError::Closed) => Some((
                ServerMessage::Done {
                    id,
                    job_id,
                    outcome: Outcome {
                        status: OutcomeStatus::Error,
                        message: "the job vanished without an outcome".to_owned(),
                        ..Outcome::default()
                    },
                },
                true,
            )),
        }
    }

    /// `progress` with the queue position while the job is still queued.
    fn queued_progress(&self) -> Option<ServerMessage> {
        let st = self.shared.state();
        let position = st.sched.position(&self.job_id)?;
        let queued_at = st.jobs.get(&self.job_id)?.queued_at;
        Some(ServerMessage::Progress {
            id: self.id,
            job_id: self.job_id.clone(),
            elapsed_ms: now_ms().saturating_sub(queued_at),
            log_bytes: 0,
            position: Some(position),
        })
    }
}

async fn handle_run(
    shared: &Arc<Shared>,
    rd: BufReader<OwnedReadHalf>,
    mut wr: OwnedWriteHalf,
    id: u64,
    request: RunRequest,
) {
    let Registered {
        job_id,
        coalesced,
        position,
        weight,
        started_at,
        mut events,
        ..
    } = match register(shared, request) {
        Ok(r) => r,
        Err(message) => {
            send_error(&mut wr, id, ErrorCode::Internal, message).await;
            return;
        }
    };
    tracing::info!(job = %job_id, coalesced, position, "run request queued");
    if !coalesced {
        shared.wake.notify_one();
    }
    let sub = Subscriber {
        shared,
        id,
        job_id,
        coalesced,
    };
    let queued = ServerMessage::Queued {
        id,
        job_id: sub.job_id.clone(),
        position,
        coalesced,
        weight,
    };
    if !send(&mut wr, &queued).await {
        detach(shared, &sub.job_id);
        return;
    }
    // A request that joins a running job never sees the broadcast `started`.
    if let Some(started_at) = started_at {
        let started = ServerMessage::Started {
            id,
            job_id: sub.job_id.clone(),
            started_at: rfc3339(started_at),
            spool: None,
        };
        if !send(&mut wr, &started).await {
            detach(shared, &sub.job_id);
            return;
        }
    }

    let (reader, mut lines) = spawn_reader(rd);
    let mut tick = interval_at(Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY);
    loop {
        let (msg, last) = tokio::select! {
            ev = events.recv() => match sub.message(ev) {
                Some(m) => m,
                None => continue,
            },
            _ = tick.tick() => match sub.queued_progress() {
                Some(m) => (m, false),
                None => continue,
            },
            line = lines.recv() => match line.flatten().map(|l| proto::decode_client(&l)) {
                None | Some(Ok(ClientMessage::Cancel { .. })) => {
                    tracing::debug!(job = %sub.job_id, "subscriber left");
                    detach(shared, &sub.job_id);
                    break;
                }
                Some(Ok(_)) => {
                    send_error(&mut wr, id, ErrorCode::BadRequest, "only cancel is accepted during a run").await;
                    continue;
                }
                Some(Err(e)) => {
                    send_error(&mut wr, id, ErrorCode::BadRequest, format!("malformed message: {e}")).await;
                    continue;
                }
            },
        };
        if !send(&mut wr, &msg).await && !last {
            detach(shared, &sub.job_id);
            break;
        }
        if last {
            break;
        }
    }
    reader.abort();
}

/// What a forwarded signal did.
enum SignalAction {
    /// Sent to the job's process group; keep serving the connection.
    Forwarded,
    /// The subscriber was detached (queued job, or other subscribers left
    /// running); close the connection.
    Detached,
}

/// Forwards `signal` from an exec client: `killpg(sig)` when the job is
/// running and this is its only subscriber (then, once the leader is gone,
/// SIGKILL to what is left of the group; after `KILL_GRACE` + 5 s with the
/// leader still alive, the cancel sequence). Otherwise the subscriber is
/// detached, which drops a queued job left without subscribers.
fn forward_signal(shared: &Arc<Shared>, job_id: &str, signal: ExecSignal) -> SignalAction {
    let target = {
        let st = shared.state();
        let alone = st.sched.subscribers(job_id) == Some(1);
        st.jobs
            .get(job_id)
            .and_then(|e| e.process.clone())
            .filter(|_| alone && st.sched.is_running(job_id))
    };
    let Some(info) = target else {
        detach(shared, job_id);
        return SignalAction::Detached;
    };
    let Ok(sig) = nix::sys::signal::Signal::try_from(signal.number()) else {
        detach(shared, job_id);
        return SignalAction::Detached;
    };
    tracing::info!(job = %job_id, ?signal, pgid = info.pgid, "forwarding a signal");
    executor::kill_group(info.pgid, sig);
    let shared = Arc::clone(shared);
    let job_id = job_id.to_owned();
    tokio::spawn(async move {
        let deadline = Instant::now() + executor::KILL_GRACE + SIGNAL_ESCALATE_AFTER;
        loop {
            sleep(Duration::from_millis(50)).await;
            let handle = {
                let st = shared.state();
                match st.jobs.get(&job_id) {
                    Some(e) if e.process.as_ref().is_some_and(|p| p.pid == info.pid) => {
                        e.handle.clone()
                    }
                    _ => return,
                }
            };
            let reused = info.proc_start_ticks.is_some()
                && !executor::process_matches(info.pid, info.proc_start_ticks);
            if reused || process_is_gone(info.pid) {
                // The leader is gone: kill whatever it left behind (background
                // children that ignore SIGINT and hold the output pipes).
                executor::kill_group(info.pgid, nix::sys::signal::Signal::SIGKILL);
                return;
            }
            if Instant::now() >= deadline {
                tracing::info!(job = %job_id, "still running after a forwarded signal; terminating");
                if let Some(h) = handle {
                    h.cancel();
                }
                return;
            }
        }
    });
    SignalAction::Forwarded
}

async fn handle_exec(
    shared: &Arc<Shared>,
    rd: BufReader<OwnedReadHalf>,
    mut wr: OwnedWriteHalf,
    id: u64,
    request: ExecRequest,
) {
    if request.argv.is_empty() || request.argv[0].is_empty() {
        send_error(&mut wr, id, ErrorCode::BadRequest, "exec needs a command").await;
        return;
    }
    let Registered {
        job_id,
        coalesced,
        position,
        weight,
        started_at,
        spool,
        mut events,
    } = match register_exec(shared, request) {
        Ok(r) => r,
        Err(message) => {
            send_error(&mut wr, id, ErrorCode::Internal, message).await;
            return;
        }
    };
    tracing::info!(job = %job_id, coalesced, position, "exec request queued");
    if !coalesced {
        shared.wake.notify_one();
    }
    let sub = Subscriber {
        shared,
        id,
        job_id,
        coalesced,
    };
    let queued = ServerMessage::Queued {
        id,
        job_id: sub.job_id.clone(),
        position,
        coalesced,
        weight,
    };
    if !send(&mut wr, &queued).await {
        detach(shared, &sub.job_id);
        return;
    }
    if let Some(started_at) = started_at {
        let started = ServerMessage::Started {
            id,
            job_id: sub.job_id.clone(),
            started_at: rfc3339(started_at),
            spool,
        };
        if !send(&mut wr, &started).await {
            detach(shared, &sub.job_id);
            return;
        }
    }

    let (reader, mut lines) = spawn_reader(rd);
    let mut tick = interval_at(Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY);
    loop {
        let (msg, last) = tokio::select! {
            ev = events.recv() => match sub.message(ev) {
                Some(m) => m,
                None => continue,
            },
            _ = tick.tick() => match sub.queued_progress() {
                Some(m) => (m, false),
                None => continue,
            },
            line = lines.recv() => match line.flatten().map(|l| proto::decode_client(&l)) {
                None | Some(Ok(ClientMessage::Cancel { .. })) => {
                    tracing::debug!(job = %sub.job_id, "exec subscriber left");
                    detach(shared, &sub.job_id);
                    break;
                }
                Some(Ok(ClientMessage::Signal { signal, .. })) => {
                    match forward_signal(shared, &sub.job_id, signal) {
                        SignalAction::Forwarded => continue,
                        SignalAction::Detached => break,
                    }
                }
                Some(Ok(_)) => {
                    send_error(&mut wr, id, ErrorCode::BadRequest, "only signal and cancel are accepted during an exec").await;
                    continue;
                }
                Some(Err(e)) => {
                    send_error(&mut wr, id, ErrorCode::BadRequest, format!("malformed message: {e}")).await;
                    continue;
                }
            },
        };
        if !send(&mut wr, &msg).await && !last {
            detach(shared, &sub.job_id);
            break;
        }
        if last {
            break;
        }
    }
    reader.abort();
}

// ---------------------------------------------------------------------------
// Client helper
// ---------------------------------------------------------------------------

/// Connects to the daemon, performs the handshake and sends `request`.
///
/// Returns the write half (to send `cancel` or close) and the reply stream
/// as an async line reader (decode each line with
/// [`sluice_gate_proto::decode_server`]). Small on purpose: the CLI has its
/// own client.
///
/// # Errors
///
/// [`Error::Io`] when the socket cannot be reached or written, and
/// [`Error::Protocol`] when the daemon does not answer with a matching
/// `welcome`.
pub async fn connect(
    socket: &Path,
    request: &ClientMessage,
) -> Result<(OwnedWriteHalf, Lines<BufReader<OwnedReadHalf>>)> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(io_err(format!("connecting to {}", socket.display())))?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let hello = ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        client: format!("sluice-gate {}", env!("CARGO_PKG_VERSION")),
        pid: std::process::id(),
    };
    let write =
        |msg: &ClientMessage| proto::encode(msg).map_err(|e| Error::Protocol(e.to_string()));
    wr.write_all(write(&hello)?.as_bytes())
        .await
        .map_err(io_err("sending hello"))?;
    let line = lines
        .next_line()
        .await
        .map_err(io_err("reading welcome"))?
        .ok_or_else(|| Error::Protocol("the daemon closed the connection".to_owned()))?;
    match proto::decode_server(&line) {
        Ok(ServerMessage::Welcome { protocol, .. }) if protocol == PROTOCOL_VERSION => {}
        Ok(other) => {
            return Err(Error::Protocol(format!(
                "unexpected handshake reply: {other:?}"
            )));
        }
        Err(e) => return Err(Error::Protocol(format!("malformed handshake reply: {e}"))),
    }
    wr.write_all(write(request)?.as_bytes())
        .await
        .map_err(io_err("sending the request"))?;
    Ok((wr, lines))
}
