//! Extension point of the daemon: the surface the merge queue
//! (`sluice-gate-queue`) builds on without the daemon depending on it.
//!
//! An [`Extension`] is passed in [`DaemonConfig::extensions`]. The daemon
//! calls [`Extension::on_start`] once (lease held, before the socket is
//! bound), spawns [`Extension::run`] as a long-lived task, routes the
//! `queue-*` client messages to [`Extension::handle`], merges
//! [`Extension::status_summary`] into `status` and announces
//! [`Extension::features`] in `welcome`. Everything an extension needs from
//! the daemon goes through [`DaemonHandle`].
//!
//! Design: `kb/sluice/design/merge-queue.md` section 10 and
//! `kb/sluice/design/architecture.md` section 5.5.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use rusqlite::{Transaction, TransactionBehavior};
use sluice_gate_core::config::UserConfig;
use sluice_gate_proto::{ClientMessage, ErrorCode, QueueSummary, RunRequest, ServerMessage};
use tokio::sync::{broadcast, watch};
use tokio::time::{Instant, Interval, interval_at};

use super::{
    DaemonConfig, JobEvent, LEASE_NAME, PROGRESS_EVERY, Registered, Shared, Subscriber,
    close_request, detach, lock, register, rfc3339,
};

/// A boxed, `Send` future: the return type of the [`Extension`] methods,
/// which keeps the trait object-safe. Implementations return
/// `Box::pin(async move { ... })`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A component plugged into the daemon (the merge queue).
///
/// Object-safe; the daemon holds extensions as `Arc<dyn Extension>`.
pub trait Extension: Send + Sync + std::fmt::Debug {
    /// Features announced in `welcome.features` (e.g. `["queue"]`).
    fn features(&self) -> &[&'static str];

    /// Called once at daemon start, after the lease is acquired and orphaned
    /// gate jobs are reconciled, before the socket accepts connections (for
    /// schema creation and parking interrupted work). An error is logged and
    /// disables this extension for the daemon's life: its features are not
    /// announced and the messages it would handle get `error{internal}`.
    /// Keeping a clone of `handle` inside the extension is allowed (it
    /// forms a reference cycle, harmless for a daemon that runs once per
    /// process).
    fn on_start<'a>(&'a self, handle: &'a DaemonHandle) -> BoxFuture<'a, anyhow::Result<()>>;

    /// Long-lived task, spawned once after a successful
    /// [`on_start`](Extension::on_start). It should return soon after
    /// [`DaemonHandle::shutdown`] turns `true`; the daemon waits up to 10 s
    /// for it (after its running gate jobs drained) and then aborts it,
    /// before releasing the lease.
    fn run(self: Arc<Self>, handle: DaemonHandle) -> BoxFuture<'static, ()>;

    /// Handles one `queue-*` request. `None` means "not mine": the daemon
    /// tries the next extension. The returned message is the single reply
    /// sent on the connection (echo the request `id`); the daemon then
    /// closes it.
    fn handle<'a>(
        &'a self,
        msg: ClientMessage,
        ctx: &'a ExtensionCtx,
    ) -> BoxFuture<'a, Option<ServerMessage>>;

    /// Summary merged into the `status` reply (`queue` field); the first
    /// extension returning `Some` wins. Must be quick (it delays `status`).
    fn status_summary(&self) -> BoxFuture<'_, Option<QueueSummary>> {
        Box::pin(async { None })
    }
}

/// Context of one request handed to [`Extension::handle`].
#[derive(Debug, Clone)]
pub struct ExtensionCtx {
    pub(super) conn_id: u64,
    pub(super) client: String,
    pub(super) peer_pid: Option<i32>,
}

impl ExtensionCtx {
    /// Daemon-unique id of the connection (increasing from 1).
    pub fn conn_id(&self) -> u64 {
        self.conn_id
    }

    /// Client name and version sent in `hello`.
    pub fn client(&self) -> &str {
        &self.client
    }

    /// Process id of the peer from `SO_PEERCRED`, when available.
    pub fn peer_pid(&self) -> Option<i32> {
        self.peer_pid
    }
}

/// Fencing token of the daemon's current lease (row `lease.name =
/// 'daemon'`). Queue writes compare-and-swap on it; see [`LeaseFence::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseFence {
    /// Daemon instance id (`<pid>-<start unix ms>`, as in `welcome.instance`).
    pub instance: String,
    /// Random lease token.
    pub token: String,
}

impl LeaseFence {
    /// True when the lease row still carries this token. Call it inside
    /// [`DaemonHandle::store_tx`] to fence a write on the lease.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn check(&self, tx: &Transaction<'_>) -> rusqlite::Result<bool> {
        let n: i64 = tx.query_row(
            "SELECT count(*) FROM lease WHERE name = ?1 AND token = ?2",
            [LEASE_NAME, self.token.as_str()],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

/// Failure of [`DaemonHandle::store_tx`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database stayed locked past the busy timeout (2 s). Nothing was
    /// written; the caller may retry later.
    #[error("database busy, retry later")]
    Busy,
    /// Any other SQLite failure (the transaction was rolled back).
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl StoreError {
    /// True for [`StoreError::Busy`].
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Busy)
    }

    fn classify(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(f, _) = &e
            && matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
        {
            return Self::Busy;
        }
        Self::Sqlite(e)
    }
}

/// Cloneable handle on the running daemon, given to extensions.
#[derive(Clone)]
pub struct DaemonHandle {
    pub(super) shared: Arc<Shared>,
}

impl std::fmt::Debug for DaemonHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonHandle")
            .field("instance", &self.shared.instance)
            .finish_non_exhaustive()
    }
}

impl DaemonHandle {
    /// Submits a gate run exactly like a client `run` request (scheduler,
    /// budget, coalescing, marker double-check, marker on pass) and returns
    /// the subscription that receives its events. The events carry `id: 0`.
    /// When the daemon refuses the request (stopping, lease lost) the
    /// subscription yields one `error{internal}` and ends.
    pub fn submit_run(&self, request: RunRequest) -> RunSubscription {
        let shared = Arc::clone(&self.shared);
        match register(&shared, request, "queue") {
            Ok(Registered {
                job_id,
                coalesced,
                position,
                weight,
                weight_cfg,
                expected_ms,
                attached_at,
                request_row,
                started_at,
                events,
                ..
            }) => {
                tracing::info!(job = %job_id, coalesced, position, "extension run queued");
                if !coalesced {
                    shared.wake.notify_one();
                }
                let mut pending = VecDeque::from([ServerMessage::Queued {
                    id: 0,
                    job_id: job_id.clone(),
                    position,
                    coalesced,
                    weight,
                    weight_cfg: Some(weight_cfg),
                    expected_ms,
                }]);
                if let Some(started_at) = started_at {
                    pending.push_back(ServerMessage::Started {
                        id: 0,
                        job_id: job_id.clone(),
                        started_at: rfc3339(started_at),
                        spool: None,
                    });
                }
                RunSubscription {
                    shared,
                    job_id,
                    coalesced,
                    attached_at,
                    request_row,
                    pending,
                    events: Some(events),
                    tick: interval_at(Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY),
                    finished: false,
                }
            }
            Err(message) => RunSubscription {
                shared,
                job_id: String::new(),
                coalesced: false,
                attached_at: 0,
                request_row: None,
                pending: VecDeque::from([ServerMessage::Error {
                    id: 0,
                    code: ErrorCode::Internal,
                    message,
                }]),
                events: None,
                tick: interval_at(Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY),
                finished: true,
            },
        }
    }

    /// The fencing token of the lease this daemon acquired at start.
    pub fn fence(&self) -> LeaseFence {
        LeaseFence {
            instance: self.shared.instance.clone(),
            token: self.shared.token.clone(),
        }
    }

    /// False once a heartbeat found the lease taken by someone else (the
    /// daemon then accepts no new work). Writes must still be fenced with
    /// [`LeaseFence::check`]: this flag is only refreshed every 15 s.
    pub fn lease_held(&self) -> bool {
        self.shared.lease_held.load(Ordering::SeqCst)
    }

    /// Receiver that turns `true` when the daemon starts stopping (`stop`
    /// request or signal).
    pub fn shutdown(&self) -> watch::Receiver<bool> {
        self.shared.shutting_down.subscribe()
    }

    /// Runs `f` in one `BEGIN IMMEDIATE` transaction on the daemon's SQLite
    /// connection, serialised with every other store access of the daemon
    /// (one mutex). Commits when `f` returns `Ok`, rolls back otherwise.
    /// Synchronous and holds the store lock: keep `f` short (no git, no
    /// I/O besides SQL).
    ///
    /// # Errors
    ///
    /// [`StoreError::Busy`] when the database stays locked past the busy
    /// timeout (retryable, nothing written); [`StoreError::Sqlite`] for any
    /// other error, including the ones returned by `f`.
    pub fn store_tx<R>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
    ) -> Result<R, StoreError> {
        let store = lock(&self.shared.store);
        let tx = Transaction::new_unchecked(store.connection(), TransactionBehavior::Immediate)
            .map_err(StoreError::classify)?;
        let r = f(&tx).map_err(StoreError::classify)?;
        tx.commit().map_err(StoreError::classify)?;
        Ok(r)
    }

    /// Like [`DaemonHandle::store_tx`], but the commit is made with `PRAGMA
    /// synchronous = FULL` (fsync of the WAL at commit), restored to the
    /// daemon's `NORMAL` afterwards. For write-ahead records that must
    /// survive a power loss (the merge queue's push marker). SQLite refuses
    /// to change the setting inside a transaction, hence a separate method.
    ///
    /// # Errors
    ///
    /// As [`DaemonHandle::store_tx`].
    pub fn store_tx_durable<R>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
    ) -> Result<R, StoreError> {
        let store = lock(&self.shared.store);
        let conn = store.connection();
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(StoreError::classify)?;
        let result = (|| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
                .map_err(StoreError::classify)?;
            let r = f(&tx).map_err(StoreError::classify)?;
            tx.commit().map_err(StoreError::classify)?;
            Ok(r)
        })();
        if let Err(e) = conn.pragma_update(None, "synchronous", "NORMAL") {
            tracing::warn!("cannot restore synchronous = NORMAL: {e}");
        }
        result
    }

    /// The state directory: the directory of the database (`<state>`).
    pub fn state_dir(&self) -> &Path {
        self.shared
            .config
            .db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
    }

    /// Directory of job logs (`<state>/logs`).
    pub fn log_dir(&self) -> &Path {
        &self.shared.config.logs_dir
    }

    /// The user config the daemon was started with (`DaemonConfig::user`).
    pub fn config(&self) -> &UserConfig {
        &self.shared.config.user
    }

    /// The full daemon configuration.
    pub fn daemon_config(&self) -> &DaemonConfig {
        &self.shared.config
    }

    /// The daemon's own environment sanitised for gate processes
    /// (`sluice_gate_core::git::child_env`: no `git rev-parse
    /// --local-env-vars` variables, no `CLAUDE*`), computed once at start.
    pub fn sanitised_env(&self) -> BTreeMap<String, String> {
        self.shared.sanitised_env.clone()
    }
}

/// Events of one run submitted with [`DaemonHandle::submit_run`]: the
/// `ServerMessage`s a client would receive (`queued`, `started`,
/// `progress`, `done`; or a single `error`). Dropping it before `done`
/// unsubscribes, exactly like a client disconnecting (the
/// `last_subscriber_gone` policy applies).
pub struct RunSubscription {
    shared: Arc<Shared>,
    job_id: String,
    coalesced: bool,
    attached_at: u64,
    request_row: Option<i64>,
    pending: VecDeque<ServerMessage>,
    events: Option<broadcast::Receiver<JobEvent>>,
    tick: Interval,
    finished: bool,
}

impl std::fmt::Debug for RunSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunSubscription")
            .field("job_id", &self.job_id)
            .field("coalesced", &self.coalesced)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl RunSubscription {
    /// Job id (the shared job when coalesced); empty when refused.
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    /// True when the run joined an existing job.
    pub fn coalesced(&self) -> bool {
        self.coalesced
    }

    /// Next event; `None` after `done` (or the refusal `error`).
    /// Cancel-safe.
    pub async fn recv(&mut self) -> Option<ServerMessage> {
        if let Some(m) = self.pending.pop_front() {
            return Some(m);
        }
        if self.finished {
            return None;
        }
        let events = self.events.as_mut()?;
        let sub = Subscriber {
            shared: &self.shared,
            id: 0,
            job_id: self.job_id.clone(),
            coalesced: self.coalesced,
            attached_at: self.attached_at,
        };
        loop {
            tokio::select! {
                ev = events.recv() => {
                    if let Some((msg, last)) = sub.message(ev) {
                        self.finished = last;
                        if last {
                            close_request(&self.shared, self.request_row.take(), false);
                        }
                        return Some(msg);
                    }
                }
                _ = self.tick.tick() => {
                    if let Some(msg) = sub.queued_progress() {
                        return Some(msg);
                    }
                }
            }
        }
    }
}

impl Drop for RunSubscription {
    fn drop(&mut self) {
        if !self.finished {
            tracing::debug!(job = %self.job_id, "extension subscription dropped");
            detach(&self.shared, &self.job_id);
            close_request(&self.shared, self.request_row.take(), true);
        }
    }
}
