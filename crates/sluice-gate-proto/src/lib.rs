//! IPC protocol types between the Sluice daemon and its clients.
//!
//! # Framing
//!
//! Messages travel over a Unix stream socket as newline-delimited JSON: one
//! JSON object per line, UTF-8, compact (no pretty printing). Use [`encode`]
//! to produce a line (it appends the trailing `\n`) and [`decode_client`] /
//! [`decode_server`] to parse one. Lines longer than [`MAX_LINE_BYTES`] must
//! be rejected; [`line_too_long`] performs that check. This crate does no I/O.
//!
//! # Versioning
//!
//! The integer [`PROTOCOL_VERSION`] is exchanged in the `hello` / `welcome`
//! handshake; any incompatible change bumps it. Within a version, new optional
//! fields may be added to any message: every wire type tolerates unknown
//! fields (no `deny_unknown_fields`) and gives optional fields a
//! `#[serde(default)]`. [`STATUS_SCHEMA_VERSION`] versions the `status` reply
//! payload separately and is bumped when a field changes meaning.
//!
//! Both top-level enums are internally tagged with `type` and use kebab-case
//! spellings, for example `{"type":"run","id":1,...}`.
//!
//! # Client
//!
//! With the `client` feature, [`client`] provides the blocking daemon client
//! shared by the CLI and the MCP server (ADR-0008).

#[cfg(feature = "client")]
pub mod client;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sluice_gate_core::key::CacheKey;
use sluice_gate_core::tree::TreeHash;

/// Version of the daemon <-> client protocol.
pub const PROTOCOL_VERSION: u32 = 1;

/// Version of the `status` reply schema ([`StatusReport::schema`]).
pub const STATUS_SCHEMA_VERSION: u32 = 1;

/// Version of the `queue-status` reply schema ([`QueueStatusReport::schema`]).
pub const QUEUE_STATUS_SCHEMA_VERSION: u32 = 1;

/// Feature announced in [`ServerMessage::Welcome::features`] by a daemon
/// that serves the merge-queue messages (`queue-*`).
pub const FEATURE_QUEUE: &str = "queue";

/// Maximum accepted length of one protocol line, in bytes (1 MiB).
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// Message sent from a client to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[allow(
    clippy::large_enum_variant,
    reason = "short-lived wire value; keeps the documented unboxed API"
)]
pub enum ClientMessage {
    /// Handshake, always the first line from the client.
    Hello {
        /// Protocol version the client speaks.
        protocol: u32,
        /// Client name and version, for diagnostics.
        client: String,
        /// Client process id.
        pid: u32,
    },
    /// Request to run (or coalesce into) a gate job.
    Run {
        /// Client-chosen request id, echoed in every reply.
        id: u64,
        /// The run request payload.
        #[serde(flatten)]
        request: RunRequest,
    },
    /// Request a status report.
    Status {
        /// Client-chosen request id.
        id: u64,
    },
    /// Request the tail of a job log.
    Logs {
        /// Client-chosen request id.
        id: u64,
        /// Job to read; `None` selects the newest job.
        #[serde(default)]
        job_id: Option<String>,
        /// Number of trailing lines wanted.
        #[serde(default)]
        tail: usize,
    },
    /// Fire-and-forget telemetry from a client that did not need the daemon.
    Event {
        /// Client-chosen request id (conventionally 0).
        id: u64,
        /// The event payload.
        #[serde(flatten)]
        event: ClientEvent,
    },
    /// Ask the daemon to stop.
    Stop {
        /// Client-chosen request id.
        id: u64,
        /// Kill running jobs instead of letting them finish.
        #[serde(default)]
        force: bool,
    },
    /// Detach this subscriber from a running `run` request.
    Cancel {
        /// Id of the `run` request to detach from.
        id: u64,
    },
    /// Merge queue: enqueue a branch. Reply: `queue-job`.
    QueueEnqueue {
        /// Client-chosen request id.
        #[serde(default)]
        id: u64,
        /// The enqueue payload.
        #[serde(flatten)]
        request: EnqueueRequest,
    },
    /// Merge queue: list trains and jobs. Reply: `queue-status`.
    QueueStatus {
        /// Client-chosen request id.
        #[serde(default)]
        id: u64,
        /// Restrict to one repository (its git common dir); `None` lists all.
        #[serde(default)]
        repo: Option<PathBuf>,
    },
    /// Merge queue: approve a `ready` train. Reply: `queue-approved`.
    QueueApprove {
        /// Client-chosen request id.
        #[serde(default)]
        id: u64,
        /// Train to approve.
        train_id: String,
        /// Train hash the client displayed; `stale-plan` when it no longer matches.
        train_hash: String,
        /// Surface the approval came from.
        via: ApproveVia,
        /// Allowlisted environment of the approving client, used for the push.
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// Merge queue: re-queue a `conflict` or `failed` job. Reply: `queue-job`.
    QueueRetry {
        /// Client-chosen request id.
        #[serde(default)]
        id: u64,
        /// Job to retry.
        job_id: String,
        /// Branch head re-read by the client.
        head_sha: String,
    },
    /// Merge queue: cancel a job. Reply: `queue-job`.
    QueueCancel {
        /// Client-chosen request id.
        #[serde(default)]
        id: u64,
        /// Job to cancel.
        job_id: String,
    },
    /// Merge queue: evaluate `needs_reconcile` trains. Reply: `queue-reconcile`.
    QueueReconcile {
        /// Client-chosen request id.
        #[serde(default)]
        id: u64,
        /// Restrict to one repository (its git common dir); `None` means all.
        #[serde(default)]
        repo: Option<PathBuf>,
    },
}

/// Payload of a `queue-enqueue` request. The client resolves the branch,
/// its SHA and the worktree's cleanliness itself and sends facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnqueueRequest {
    /// Absolute path of the repository's git common dir.
    pub repo: PathBuf,
    /// Absolute path of the root of the worktree the request comes from.
    pub worktree: PathBuf,
    /// Branch to queue (short name, without `refs/heads/`).
    pub branch: String,
    /// Commit of the branch captured by the client.
    pub head_sha: String,
    /// Landing target as typed by the user (`<remote>/<branch>` or a local
    /// branch); `None` selects the configured default.
    #[serde(default)]
    pub base: Option<String>,
    /// Who enqueued.
    pub caller: Caller,
    /// Agent session identifier, if any.
    #[serde(default)]
    pub session: Option<String>,
    /// Allowlisted environment of the enqueuing client (credentials for fetches).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Surface an approval came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApproveVia {
    /// `sluice queue approve`.
    Cli,
    /// The MCP tool `queue_approve`.
    Mcp,
}

/// Payload of a `run` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRequest {
    /// Repository location.
    pub repo: RepoInfo,
    /// Gate definition as resolved by the client.
    pub gate: GateSpec,
    /// Cache key, `None` when the tree is unknown or caching is off.
    #[serde(default)]
    pub key: Option<CacheKey>,
    /// Working tree hash, `None` when unknown.
    #[serde(default)]
    pub tree: Option<TreeHash>,
    /// Committed tree hash, `None` when unknown.
    #[serde(default)]
    pub committed_tree: Option<TreeHash>,
    /// Command line to execute.
    pub cmd: String,
    /// Extra arguments appended to the command.
    #[serde(default)]
    pub extra: Vec<String>,
    /// Working directory for the gate process.
    pub cwd: PathBuf,
    /// Sanitised environment for the gate process.
    #[serde(default)]
    pub child_env: BTreeMap<String, String>,
    /// Environment used by the daemon to probe git state.
    #[serde(default)]
    pub probe_env: BTreeMap<String, String>,
    /// Who triggered the run.
    pub caller: Caller,
    /// Agent session identifier, if any.
    #[serde(default)]
    pub session: Option<String>,
    /// Client process id.
    pub client_pid: u32,
    /// Git hook that triggered the run, if any.
    #[serde(default)]
    pub hook: Option<String>,
}

/// Repository paths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoInfo {
    /// Absolute path of the working tree root.
    pub toplevel: PathBuf,
    /// Absolute path of the common git directory.
    pub common_dir: PathBuf,
    /// Absolute path of the git directory of this worktree.
    pub git_dir: PathBuf,
}

/// Gate definition sent with a run request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateSpec {
    /// Gate name.
    pub name: String,
    /// Configured command.
    pub cmd: String,
    /// Scheduler weight (budget units).
    #[serde(default)]
    pub weight: u32,
    /// Timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: u64,
    /// Extra environment configured for the gate.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Whether results of this gate are cached.
    #[serde(default)]
    pub cache: bool,
}

/// Origin of a run request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Caller {
    /// An AI agent.
    Agent,
    /// A person at a terminal.
    #[default]
    Human,
}

/// Telemetry event payload carried by [`ClientMessage::Event`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum ClientEvent {
    /// The client served a cache hit without contacting the daemon.
    Hit {
        /// Repository the hit happened in.
        repo: PathBuf,
        /// Gate name.
        gate: String,
        /// Estimated time saved, in milliseconds.
        #[serde(default)]
        saved_ms: u64,
    },
}

/// Message sent from the daemon to a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerMessage {
    /// Handshake reply.
    Welcome {
        /// Protocol version the daemon speaks.
        protocol: u32,
        /// Daemon name and version.
        daemon: String,
        /// Unique id of this daemon instance.
        instance: String,
        /// Daemon process id.
        pid: u32,
        /// Daemon start time (RFC 3339).
        started_at: String,
        /// Optional features served by this daemon (for example
        /// [`FEATURE_QUEUE`]); omitted from the wire when empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        features: Vec<String>,
    },
    /// The request entered the scheduler; always the first `run` event.
    Queued {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Position in the queue.
        position: usize,
        /// True when attached to an existing job.
        #[serde(default)]
        coalesced: bool,
        /// Weight of the job.
        #[serde(default)]
        weight: u32,
    },
    /// The gate process started.
    Started {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Start time (RFC 3339).
        started_at: String,
    },
    /// Periodic progress while queued or running.
    Progress {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Milliseconds since the job was queued or started.
        #[serde(default)]
        elapsed_ms: u64,
        /// Bytes written to the log so far.
        #[serde(default)]
        log_bytes: u64,
        /// Queue position while queued.
        #[serde(default)]
        position: Option<usize>,
    },
    /// The job finished; the server closes afterwards.
    Done {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Final outcome.
        outcome: Outcome,
    },
    /// Reply to a status request.
    Status {
        /// Request id.
        id: u64,
        /// The report.
        #[serde(flatten)]
        status: StatusReport,
    },
    /// Reply to a logs request.
    Log {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Path of the log file.
        path: PathBuf,
        /// Trailing log lines.
        #[serde(default)]
        lines: Vec<String>,
    },
    /// Reply to a stop request.
    Stopping {
        /// Request id.
        id: u64,
        /// Number of jobs still running.
        #[serde(default)]
        running: usize,
    },
    /// Request failed.
    Error {
        /// Request id (0 when unknown).
        #[serde(default)]
        id: u64,
        /// Machine-readable code.
        code: ErrorCode,
        /// Human-readable description.
        #[serde(default)]
        message: String,
    },
    /// Reply to `queue-enqueue`, `queue-retry` and `queue-cancel`.
    QueueJob {
        /// Request id.
        #[serde(default)]
        id: u64,
        /// The job after the request was applied.
        job: QueueJobView,
    },
    /// Reply to `queue-status`.
    QueueStatus {
        /// Request id.
        #[serde(default)]
        id: u64,
        /// The report.
        #[serde(flatten)]
        status: QueueStatusReport,
    },
    /// Reply to `queue-approve`.
    QueueApproved {
        /// Request id.
        #[serde(default)]
        id: u64,
        /// The train after approval.
        train: QueueTrainView,
    },
    /// Reply to `queue-reconcile`.
    QueueReconcile {
        /// Request id.
        #[serde(default)]
        id: u64,
        /// One entry per evaluated train.
        #[serde(default)]
        results: Vec<ReconcileResult>,
    },
}

/// Final result of a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Outcome {
    /// Result category.
    pub status: OutcomeStatus,
    /// Process exit code, when the process exited normally.
    pub exit_code: Option<i32>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Path of the job log.
    pub log_path: Option<PathBuf>,
    /// Last lines of output.
    pub tail: Vec<String>,
    /// Whether a cache marker was written.
    pub marker_written: bool,
    /// Why no marker was written, if relevant.
    pub not_cached_reason: Option<String>,
    /// Job id this request was coalesced with.
    pub coalesced_with: Option<String>,
    /// Human-readable one-liner.
    pub message: String,
}

/// Category of an [`Outcome`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutcomeStatus {
    /// Exit code 0.
    Pass,
    /// Non-zero exit code.
    Fail,
    /// Timeout exceeded.
    Timeout,
    /// Killed (forced stop or last subscriber gone).
    Killed,
    /// A marker appeared while queued; nothing ran.
    Cached,
    /// Sluice itself failed.
    #[default]
    Error,
}

/// Code carried by [`ServerMessage::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// Handshake or protocol version problem.
    Protocol,
    /// Malformed or unknown request.
    BadRequest,
    /// Requested job or resource does not exist.
    NotFound,
    /// Unexpected daemon failure.
    Internal,
    /// Merge queue: the job or train is not in a state that allows the request.
    ConflictState,
    /// Merge queue: the request is refused by policy (for example agent approval).
    NotAllowed,
    /// Merge queue: the train changed since the client displayed it.
    StalePlan,
    /// Merge queue: the branch already has an active job.
    DuplicateBranch,
    /// The daemon does not serve this request (no extension handles it).
    NotSupported,
}

/// Payload of the `status` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StatusReport {
    /// Schema version ([`STATUS_SCHEMA_VERSION`]).
    pub schema: u32,
    /// Daemon identity.
    pub daemon: DaemonInfo,
    /// Resource budget.
    pub budget: BudgetInfo,
    /// Running jobs.
    pub running: Vec<RunningJob>,
    /// Queued jobs.
    pub queued: Vec<QueuedJob>,
    /// Cumulative counters.
    pub totals: Totals,
    /// Merge-queue summary; absent when the daemon serves no queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<QueueSummary>,
}

/// Merge-queue counters in a status report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueueSummary {
    /// Jobs not in a terminal state (`landed`, `cancelled`, `conflict`, `failed`).
    pub active: usize,
    /// Trains waiting for approval.
    pub ready: usize,
    /// Trains waiting for `sluice queue reconcile`.
    pub needs_reconcile: usize,
}

/// Daemon identity in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonInfo {
    /// Unique instance id.
    pub instance: String,
    /// Process id.
    pub pid: u32,
    /// Start time (RFC 3339).
    pub started_at: String,
    /// Daemon version.
    pub version: String,
    /// Socket path.
    pub socket: PathBuf,
}

/// Budget usage in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BudgetInfo {
    /// Total budget units.
    pub total: u32,
    /// Units in use.
    pub used: u32,
    /// Pressure stall information.
    pub psi: PsiInfo,
}

/// Linux pressure stall information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PsiInfo {
    /// Whether PSI gating is enabled.
    pub enabled: bool,
    /// CPU "some" 10 s average, if available.
    pub cpu_some_avg10: Option<f64>,
    /// Memory "some" 10 s average, if available.
    pub memory_some_avg10: Option<f64>,
    /// Whether PSI currently blocks new jobs.
    pub blocking: bool,
}

/// A running job in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RunningJob {
    /// Job id.
    pub job_id: String,
    /// Repository path.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Weight.
    pub weight: u32,
    /// Start time (RFC 3339).
    pub started_at: String,
    /// Milliseconds since start.
    pub elapsed_ms: u64,
    /// Attached subscribers.
    pub subscribers: usize,
    /// Who requested it.
    pub caller: Caller,
}

/// A queued job in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueuedJob {
    /// Job id.
    pub job_id: String,
    /// Repository path.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Weight.
    pub weight: u32,
    /// Queue time (RFC 3339).
    pub queued_at: String,
    /// Milliseconds waiting so far.
    pub waiting_ms: u64,
    /// Attached subscribers.
    pub subscribers: usize,
    /// Why it is still waiting.
    pub reason: WaitReason,
}

/// Why a queued job is not running yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WaitReason {
    /// Not enough budget units free.
    #[default]
    Budget,
    /// Blocked by system pressure.
    Psi,
    /// Per-repository concurrency cap reached.
    RepoCap,
    /// Capacity reserved for an older queued job.
    HeadReserved,
}

/// Cumulative counters in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Totals {
    /// Start of the counting period (RFC 3339).
    pub since: String,
    /// Jobs run.
    pub runs: u64,
    /// Jobs that passed.
    pub passes: u64,
    /// Jobs that failed.
    pub failures: u64,
    /// Jobs that timed out.
    pub timeouts: u64,
    /// Cache hits.
    pub hits: u64,
    /// Requests coalesced into existing jobs.
    pub coalesced: u64,
    /// Total queue wait, in milliseconds.
    pub wait_ms_total: u64,
    /// Total run time, in milliseconds.
    pub run_ms_total: u64,
    /// Estimated time saved by caching and coalescing, in milliseconds.
    pub time_saved_ms: u64,
    /// Time PSI admission spent blocking new jobs, in milliseconds (current
    /// daemon instance only, accumulated per sample; keeps what accrued if PSI
    /// later becomes unavailable).
    pub psi_blocked_ms_total: u64,
}

/// Payload of the `queue-status` reply (also `sluice queue status --json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueueStatusReport {
    /// Schema version ([`QUEUE_STATUS_SCHEMA_VERSION`]).
    pub schema: u32,
    /// Trains, oldest first.
    pub trains: Vec<QueueTrainView>,
    /// Jobs, in queue order.
    pub jobs: Vec<QueueJobView>,
}

/// State of a merge-queue job (`queue_jobs.state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueJobState {
    /// Waiting to be claimed into a train.
    #[default]
    Queued,
    /// Being merged into a train.
    Merging,
    /// Gates running on the train.
    Verifying,
    /// Train verified; waiting for approval.
    Ready,
    /// Approved; waiting for the push.
    Approved,
    /// Push in progress.
    Pushing,
    /// Landed on the target (terminal).
    Landed,
    /// Merge conflict; terminal until `queue-retry`.
    Conflict,
    /// Gate or push failure; terminal until `queue-retry`.
    Failed,
    /// Cancelled (terminal).
    Cancelled,
    /// Push outcome unknown; waiting for `queue-reconcile`.
    NeedsReconcile,
    /// A state this client does not know (sent by a newer daemon).
    #[serde(other)]
    Unknown,
}

/// State of a merge-queue train (`queue_trains.state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueTrainState {
    /// Members being merged.
    #[default]
    Building,
    /// Gates running.
    Verifying,
    /// Verified; waiting for approval.
    Ready,
    /// Approved; waiting for the push.
    Approved,
    /// Push in progress.
    Pushing,
    /// Landed on the target.
    Landed,
    /// Dissolved; its members went back to the queue or ended.
    Dissolved,
    /// Push outcome unknown; waiting for `queue-reconcile`.
    NeedsReconcile,
    /// A state this client does not know (sent by a newer daemon).
    #[serde(other)]
    Unknown,
}

/// One merge-queue job as shown to clients (columns of `queue_jobs`).
/// Timestamps are RFC 3339.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueueJobView {
    /// Job id.
    pub job_id: String,
    /// Git common dir of the repository.
    pub repo: PathBuf,
    /// Worktree the job was enqueued from.
    pub worktree: PathBuf,
    /// Queued branch.
    pub branch: String,
    /// Commit captured at enqueue (or retry).
    pub head_sha: String,
    /// Remote of the target; empty for a local target.
    pub remote: String,
    /// Target branch.
    pub target: String,
    /// Current state.
    pub state: QueueJobState,
    /// Train the job belongs to, if claimed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub train_id: Option<String>,
    /// 1-based position among the `queued` jobs of its target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    /// Who enqueued.
    pub caller: Caller,
    /// Agent session identifier, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Machine-readable reason of the last side exit (e.g. `conflict`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Structured detail (conflicting paths, gate and sluice job id, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// A cancel was requested while the job was `pushing`.
    pub cancel_requested: bool,
    /// Number of attempts (1 on enqueue).
    pub attempts: u32,
    /// Enqueue time.
    pub enqueued_at: String,
    /// Last state change.
    pub updated_at: String,
    /// Time the job reached a terminal state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

/// One gate of a train (`queue_trains.gates`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TrainGate {
    /// Gate name.
    pub gate: String,
    /// Cache key of the gate on the train's tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Sluice job id of the run, when one was submitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// Gate state: `pending`, `running`, `pass`, `cached`, `fail`, `timeout`,
    /// `killed` or `error`.
    pub status: String,
    /// Run time in milliseconds, once finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// One merge-queue train as shown to clients (columns of `queue_trains`).
/// Timestamps are RFC 3339.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueueTrainView {
    /// Train id (`t-<unix-ms>-<6 hex>`).
    pub train_id: String,
    /// Git common dir of the repository.
    pub repo: PathBuf,
    /// Remote of the target; empty for a local target.
    pub remote: String,
    /// Target branch.
    pub target: String,
    /// Commit the members were merged onto.
    pub base_sha: String,
    /// Current state.
    pub state: QueueTrainState,
    /// Member job ids, in merge order.
    pub members: Vec<String>,
    /// Landing commit `S`, once built.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_sha: Option<String>,
    /// Gates of the train.
    pub gates: Vec<TrainGate>,
    /// Train hash to send back in `queue-approve`, once `ready`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub train_hash: Option<String>,
    /// Surface of the approval, once approved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_via: Option<ApproveVia>,
    /// Approval time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
    /// Creation time.
    pub created_at: String,
    /// Last state change.
    pub updated_at: String,
    /// Landing time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landed_at: Option<String>,
    /// Free-form note (e.g. `rewritten_after_landing`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Decision taken by `queue-reconcile` for one train.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileDecision {
    /// The train landed.
    Landed,
    /// Nothing landed; back to `ready` (approval voided).
    Ready,
    /// Nothing landed and the target moved; members re-queued.
    Queued,
    /// Nothing landed and a cancel was pending; members cancelled.
    Cancelled,
    /// No decision possible yet; the train is unchanged (see `reason`).
    Refused,
    /// A decision this client does not know (sent by a newer daemon).
    #[serde(other)]
    Unknown,
}

/// Result of `queue-reconcile` for one train.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReconcileResult {
    /// Train evaluated.
    pub train_id: String,
    /// Decision taken.
    pub decision: ReconcileDecision,
    /// Why (e.g. `push_in_progress`, `endpoint_changed`, `rewritten_after_landing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Serialises a message as compact JSON followed by `\n`.
///
/// # Errors
///
/// Returns an error if serialisation fails.
pub fn encode<T: Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

/// Parses one client line (trailing newline optional).
///
/// # Errors
///
/// Returns an error for malformed JSON or an unknown `type`.
pub fn decode_client(line: &str) -> Result<ClientMessage, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
}

/// Parses one server line (trailing newline optional).
///
/// # Errors
///
/// Returns an error for malformed JSON or an unknown `type`.
pub fn decode_server(line: &str) -> Result<ServerMessage, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
}

/// Returns true when `line` exceeds [`MAX_LINE_BYTES`].
#[must_use]
pub fn line_too_long(line: &str) -> bool {
    line.len() > MAX_LINE_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TREE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn run_request() -> RunRequest {
        let key: CacheKey = serde_json::from_str(&format!("\"{KEY}\"")).unwrap();
        let tree: TreeHash = serde_json::from_str(&format!("\"{TREE}\"")).unwrap();
        RunRequest {
            repo: RepoInfo {
                toplevel: "/abs".into(),
                common_dir: "/abs/.git".into(),
                git_dir: "/abs/.git".into(),
            },
            gate: GateSpec {
                name: "test".into(),
                cmd: "npm test".into(),
                weight: 4,
                timeout_ms: 1_200_000,
                env: BTreeMap::from([("NODE_ENV".into(), "test".into())]),
                cache: true,
            },
            key: Some(key),
            tree: Some(tree.clone()),
            committed_tree: Some(tree),
            cmd: "npm test".into(),
            extra: vec!["--grep".into(), "x".into()],
            cwd: "/abs/worktree".into(),
            child_env: BTreeMap::from([("HOME".into(), "/h".into())]),
            probe_env: BTreeMap::from([("GIT_DIR".into(), "/abs/.git".into())]),
            caller: Caller::Agent,
            session: Some("s1".into()),
            client_pid: 4242,
            hook: Some("pre-commit".into()),
        }
    }

    fn outcome() -> Outcome {
        Outcome {
            status: OutcomeStatus::Pass,
            exit_code: Some(0),
            duration_ms: 32100,
            log_path: Some("/l/j.log".into()),
            tail: vec!["a".into()],
            marker_written: true,
            not_cached_reason: None,
            coalesced_with: Some("j0".into()),
            message: "ok".into(),
        }
    }

    fn status_report() -> StatusReport {
        StatusReport {
            schema: STATUS_SCHEMA_VERSION,
            daemon: DaemonInfo {
                instance: "i".into(),
                pid: 1,
                started_at: "t".into(),
                version: "0.1.0".into(),
                socket: "/s".into(),
            },
            budget: BudgetInfo {
                total: 8,
                used: 5,
                psi: PsiInfo {
                    enabled: true,
                    cpu_some_avg10: Some(12.3),
                    memory_some_avg10: None,
                    blocking: false,
                },
            },
            running: vec![RunningJob {
                job_id: "j".into(),
                repo: "/abs".into(),
                gate: "test".into(),
                weight: 4,
                started_at: "t".into(),
                elapsed_ms: 1200,
                subscribers: 2,
                caller: Caller::Human,
            }],
            queued: vec![QueuedJob {
                job_id: "k".into(),
                repo: "/abs".into(),
                gate: "lint".into(),
                weight: 1,
                queued_at: "t".into(),
                waiting_ms: 300,
                subscribers: 1,
                reason: WaitReason::HeadReserved,
            }],
            totals: Totals {
                runs: 120,
                ..Totals::default()
            },
            queue: Some(QueueSummary {
                active: 3,
                ready: 1,
                needs_reconcile: 0,
            }),
        }
    }

    fn job_view() -> QueueJobView {
        QueueJobView {
            job_id: "1761-1-a1b2c3".into(),
            repo: "/abs/.git".into(),
            worktree: "/abs/wt".into(),
            branch: "feat/login".into(),
            head_sha: TREE.into(),
            remote: "origin".into(),
            target: "main".into(),
            state: QueueJobState::Conflict,
            train_id: Some("t-1761-9f2e01".into()),
            position: Some(2),
            caller: Caller::Agent,
            session: Some("s1".into()),
            reason: Some("conflict".into()),
            detail: Some(serde_json::json!({"paths": ["src/app.ts"]})),
            cancel_requested: false,
            attempts: 1,
            enqueued_at: "t".into(),
            updated_at: "t".into(),
            finished_at: None,
        }
    }

    fn train_view() -> QueueTrainView {
        QueueTrainView {
            train_id: "t-1761-9f2e01".into(),
            repo: "/abs/.git".into(),
            remote: "origin".into(),
            target: "main".into(),
            base_sha: TREE.into(),
            state: QueueTrainState::NeedsReconcile,
            members: vec!["1761-1-a1b2c3".into()],
            merge_sha: Some(TREE.into()),
            gates: vec![TrainGate {
                gate: "test".into(),
                key: Some(KEY.into()),
                job_id: Some("j".into()),
                status: "pass".into(),
                duration_ms: Some(12_400),
            }],
            train_hash: Some(KEY.into()),
            approved_via: Some(ApproveVia::Cli),
            approved_at: Some("t".into()),
            created_at: "t".into(),
            updated_at: "t".into(),
            landed_at: None,
            note: None,
        }
    }

    #[test]
    fn client_round_trip() {
        let msgs = vec![
            ClientMessage::Hello {
                protocol: 1,
                client: "c".into(),
                pid: 7,
            },
            ClientMessage::Run {
                id: 1,
                request: run_request(),
            },
            ClientMessage::Status { id: 2 },
            ClientMessage::Logs {
                id: 3,
                job_id: None,
                tail: 40,
            },
            ClientMessage::Logs {
                id: 3,
                job_id: Some("j".into()),
                tail: 5,
            },
            ClientMessage::Event {
                id: 0,
                event: ClientEvent::Hit {
                    repo: "/abs".into(),
                    gate: "test".into(),
                    saved_ms: 9,
                },
            },
            ClientMessage::Stop { id: 4, force: true },
            ClientMessage::Cancel { id: 1 },
            ClientMessage::QueueEnqueue {
                id: 5,
                request: EnqueueRequest {
                    repo: "/abs/.git".into(),
                    worktree: "/abs/wt".into(),
                    branch: "feat/login".into(),
                    head_sha: TREE.into(),
                    base: Some("origin/main".into()),
                    caller: Caller::Agent,
                    session: Some("s1".into()),
                    env: BTreeMap::from([("SSH_AUTH_SOCK".into(), "/a".into())]),
                },
            },
            ClientMessage::QueueStatus {
                id: 6,
                repo: Some("/abs/.git".into()),
            },
            ClientMessage::QueueStatus { id: 6, repo: None },
            ClientMessage::QueueApprove {
                id: 7,
                train_id: "t-1".into(),
                train_hash: KEY.into(),
                via: ApproveVia::Mcp,
                env: BTreeMap::from([("HOME".into(), "/h".into())]),
            },
            ClientMessage::QueueRetry {
                id: 8,
                job_id: "j".into(),
                head_sha: TREE.into(),
            },
            ClientMessage::QueueCancel {
                id: 9,
                job_id: "j".into(),
            },
            ClientMessage::QueueReconcile { id: 10, repo: None },
        ];
        for m in msgs {
            let line = encode(&m).unwrap();
            assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
            assert_eq!(decode_client(&line).unwrap(), m);
        }
    }

    #[test]
    fn server_round_trip() {
        let msgs = vec![
            ServerMessage::Welcome {
                protocol: 1,
                daemon: "d".into(),
                instance: "i".into(),
                pid: 1,
                started_at: "t".into(),
                features: vec![FEATURE_QUEUE.into()],
            },
            ServerMessage::Queued {
                id: 1,
                job_id: "j".into(),
                position: 0,
                coalesced: true,
                weight: 4,
            },
            ServerMessage::Started {
                id: 1,
                job_id: "j".into(),
                started_at: "t".into(),
            },
            ServerMessage::Progress {
                id: 1,
                job_id: "j".into(),
                elapsed_ms: 5000,
                log_bytes: 10,
                position: Some(2),
            },
            ServerMessage::Done {
                id: 1,
                job_id: "j".into(),
                outcome: outcome(),
            },
            ServerMessage::Status {
                id: 2,
                status: status_report(),
            },
            ServerMessage::Log {
                id: 3,
                job_id: "j".into(),
                path: "/l".into(),
                lines: vec!["x".into()],
            },
            ServerMessage::Stopping { id: 4, running: 2 },
            ServerMessage::Error {
                id: 5,
                code: ErrorCode::BadRequest,
                message: "m".into(),
            },
        ];
        for m in msgs {
            assert_eq!(decode_server(&encode(&m).unwrap()).unwrap(), m);
        }
        for code in [
            ErrorCode::Protocol,
            ErrorCode::BadRequest,
            ErrorCode::NotFound,
            ErrorCode::Internal,
            ErrorCode::ConflictState,
            ErrorCode::NotAllowed,
            ErrorCode::StalePlan,
            ErrorCode::DuplicateBranch,
            ErrorCode::NotSupported,
        ] {
            let m = ServerMessage::Error {
                id: 0,
                code,
                message: String::new(),
            };
            assert_eq!(decode_server(&encode(&m).unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn queue_server_round_trip() {
        let msgs = vec![
            ServerMessage::QueueJob {
                id: 6,
                job: job_view(),
            },
            ServerMessage::QueueJob {
                id: 6,
                job: QueueJobView::default(),
            },
            ServerMessage::QueueStatus {
                id: 7,
                status: QueueStatusReport {
                    schema: QUEUE_STATUS_SCHEMA_VERSION,
                    trains: vec![train_view(), QueueTrainView::default()],
                    jobs: vec![job_view()],
                },
            },
            ServerMessage::QueueApproved {
                id: 8,
                train: train_view(),
            },
            ServerMessage::QueueReconcile {
                id: 9,
                results: vec![
                    ReconcileResult {
                        train_id: "t-1".into(),
                        decision: ReconcileDecision::Landed,
                        reason: Some("rewritten_after_landing".into()),
                    },
                    ReconcileResult {
                        train_id: "t-2".into(),
                        decision: ReconcileDecision::Refused,
                        reason: None,
                    },
                ],
            },
        ];
        for m in msgs {
            assert_eq!(decode_server(&encode(&m).unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn spec_hello_welcome() {
        let c = decode_client(
            r#"{"type":"hello","protocol":1,"client":"sluice-gate 0.1.0","pid":4242}"#,
        )
        .unwrap();
        assert_eq!(
            c,
            ClientMessage::Hello {
                protocol: 1,
                client: "sluice-gate 0.1.0".into(),
                pid: 4242
            }
        );
        let s = decode_server(
            r#"{"type":"welcome","protocol":1,"daemon":"sluice-gate 0.1.0","instance":"u","pid":1234,"started_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert!(matches!(s, ServerMessage::Welcome { pid: 1234, .. }));
    }

    #[test]
    fn spec_queued_done() {
        let q = decode_server(
            r#"{"type":"queued","id":1,"job_id":"j","position":0,"coalesced":false,"weight":4}"#,
        )
        .unwrap();
        assert_eq!(
            q,
            ServerMessage::Queued {
                id: 1,
                job_id: "j".into(),
                position: 0,
                coalesced: false,
                weight: 4
            }
        );
        let d = decode_server(
            r#"{"type":"done","id":1,"job_id":"j","outcome":{"status":"pass","exit_code":0,"duration_ms":32100,"log_path":"/x/j.log","tail":["last","lines"],"marker_written":true,"not_cached_reason":null,"coalesced_with":null,"message":"ok"}}"#,
        )
        .unwrap();
        let ServerMessage::Done { outcome, .. } = d else {
            panic!("not done");
        };
        assert_eq!(outcome.status, OutcomeStatus::Pass);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.duration_ms, 32100);
        assert_eq!(outcome.tail, ["last", "lines"]);
        assert!(outcome.marker_written);
        assert_eq!(outcome.coalesced_with, None);
    }

    #[test]
    fn spec_status() {
        let line = r#"{"type":"status","id":2,"schema":1,
 "daemon":{"instance":"i","pid":1,"started_at":"t","version":"0.1.0","socket":"/s"},
 "budget":{"total":8,"used":5,"psi":{"enabled":true,"cpu_some_avg10":12.3,"memory_some_avg10":0.4,"blocking":false}},
 "running":[{"job_id":"j","repo":"/abs","gate":"test","weight":4,"started_at":"t","elapsed_ms":1200,"subscribers":2,"caller":"agent"}],
 "queued":[{"job_id":"k","repo":"/abs","gate":"lint","weight":1,"queued_at":"t","waiting_ms":300,"subscribers":1,"reason":"repo-cap"}],
 "totals":{"since":"t","runs":120,"passes":110,"failures":8,"timeouts":2,"hits":430,"coalesced":37,
           "wait_ms_total":1,"run_ms_total":2,"time_saved_ms":3}}"#;
        let ServerMessage::Status { id, status } = decode_server(line).unwrap() else {
            panic!("not status");
        };
        assert_eq!(id, 2);
        assert_eq!(status.schema, 1);
        assert_eq!(status.budget.used, 5);
        assert_eq!(status.budget.psi.cpu_some_avg10, Some(12.3));
        assert_eq!(status.running[0].caller, Caller::Agent);
        assert_eq!(status.queued[0].reason, WaitReason::RepoCap);
        assert_eq!(status.totals.hits, 430);
        assert_eq!(status.totals.time_saved_ms, 3);
    }

    #[test]
    fn spec_run_with_nulls() {
        let line = r#"{"type":"run","id":1,
 "repo":{"toplevel":"/abs","common_dir":"/abs/.git","git_dir":"/abs/.git"},
 "gate":{"name":"test","cmd":"npm test","weight":4,"timeout_ms":1200000,"env":{"NODE_ENV":"test"},"cache":true},
 "key":null,"tree":null,"committed_tree":null,
 "cmd":"npm test","extra":["--grep","x"],
 "cwd":"/abs/worktree","child_env":{"PATH":"p"},"probe_env":{"GIT_DIR":"d"},
 "caller":"human","session":null,"client_pid":4242,"hook":null}"#;
        let ClientMessage::Run { id, request } = decode_client(line).unwrap() else {
            panic!("not run");
        };
        assert_eq!(id, 1);
        assert!(
            request.key.is_none() && request.tree.is_none() && request.committed_tree.is_none()
        );
        assert_eq!(request.caller, Caller::Human);
        assert_eq!(request.gate.timeout_ms, 1_200_000);
    }

    #[test]
    fn spec_event() {
        let m = decode_client(
            r#"{"type":"event","id":0,"event":"hit","repo":"/abs","gate":"test","saved_ms":32100}"#,
        )
        .unwrap();
        assert_eq!(
            m,
            ClientMessage::Event {
                id: 0,
                event: ClientEvent::Hit {
                    repo: "/abs".into(),
                    gate: "test".into(),
                    saved_ms: 32100
                }
            }
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let m = decode_client(r#"{"type":"status","id":2,"future":{"a":1}}"#).unwrap();
        assert_eq!(m, ClientMessage::Status { id: 2 });
        let o: Outcome = serde_json::from_str(r#"{"status":"fail","new_field":true}"#).unwrap();
        assert_eq!(o.status, OutcomeStatus::Fail);
        let s = decode_server(r#"{"type":"stopping","id":4,"running":2,"extra":1}"#).unwrap();
        assert_eq!(s, ServerMessage::Stopping { id: 4, running: 2 });
    }

    #[test]
    fn unknown_type_is_an_error() {
        assert!(decode_client(r#"{"type":"bogus","id":1}"#).is_err());
        assert!(decode_server(r#"{"type":"bogus"}"#).is_err());
        assert!(decode_client("not json").is_err());
    }

    #[test]
    fn spellings() {
        let run = encode(&ClientMessage::Run {
            id: 1,
            request: run_request(),
        })
        .unwrap();
        assert!(run.contains(r#""type":"run""#));
        assert!(run.contains(r#""caller":"agent""#));
        let done = encode(&ServerMessage::Done {
            id: 1,
            job_id: "j".into(),
            outcome: outcome(),
        })
        .unwrap();
        assert!(done.contains(r#""type":"done""#));
        assert!(done.contains(r#""status":"pass""#));
        let err = encode(&ServerMessage::Error {
            id: 1,
            code: ErrorCode::BadRequest,
            message: String::new(),
        })
        .unwrap();
        assert!(err.contains(r#""code":"bad-request""#));
        let st = encode(&OutcomeStatus::Cached).unwrap();
        assert_eq!(st, "\"cached\"\n");
        assert!(encode(&WaitReason::RepoCap).unwrap().contains("repo-cap"));
    }

    #[test]
    fn welcome_without_features_parses_and_empty_features_are_omitted() {
        let w = decode_server(
            r#"{"type":"welcome","protocol":1,"daemon":"d","instance":"i","pid":1,"started_at":"t"}"#,
        )
        .unwrap();
        let ServerMessage::Welcome { features, .. } = &w else {
            panic!("not welcome");
        };
        assert_eq!(*features, Vec::<String>::new());
        assert!(!encode(&w).unwrap().contains("features"));
        let q = decode_server(
            r#"{"type":"welcome","protocol":1,"daemon":"d","instance":"i","pid":1,"started_at":"t","features":["queue"]}"#,
        )
        .unwrap();
        assert!(
            matches!(q, ServerMessage::Welcome { features, .. } if features == [FEATURE_QUEUE])
        );
    }

    #[test]
    fn status_queue_summary_is_optional() {
        let mut report = status_report();
        report.queue = None;
        let line = encode(&ServerMessage::Status {
            id: 2,
            status: report,
        })
        .unwrap();
        assert!(!line.contains("\"queue\""));
        let ServerMessage::Status { status, .. } = decode_server(&line).unwrap() else {
            panic!("not status");
        };
        assert_eq!(status.queue, None);
        let line = encode(&ServerMessage::Status {
            id: 2,
            status: status_report(),
        })
        .unwrap();
        assert!(line.contains(r#""queue":{"active":3,"ready":1,"needs_reconcile":0}"#));
    }

    #[test]
    fn queue_spellings() {
        let m = decode_client(
            r#"{"type":"queue-enqueue","id":1,"repo":"/r/.git","worktree":"/r","branch":"b","head_sha":"abc","caller":"human"}"#,
        )
        .unwrap();
        let ClientMessage::QueueEnqueue { request, .. } = m else {
            panic!("not queue-enqueue");
        };
        assert_eq!(request.base, None);
        assert!(request.env.is_empty());
        let m = decode_client(
            r#"{"type":"queue-approve","train_id":"t","train_hash":"h","via":"cli"}"#,
        )
        .unwrap();
        assert!(matches!(
            m,
            ClientMessage::QueueApprove {
                id: 0,
                via: ApproveVia::Cli,
                ..
            }
        ));
        let job = encode(&ServerMessage::QueueJob {
            id: 1,
            job: job_view(),
        })
        .unwrap();
        assert!(job.contains(r#""type":"queue-job""#));
        assert!(job.contains(r#""state":"conflict""#));
        let st = encode(&QueueTrainState::NeedsReconcile).unwrap();
        assert_eq!(st, "\"needs_reconcile\"\n");
        let unknown: QueueJobState = serde_json::from_str(r#""bisecting""#).unwrap();
        assert_eq!(unknown, QueueJobState::Unknown);
        let err = encode(&ServerMessage::Error {
            id: 1,
            code: ErrorCode::NotSupported,
            message: String::new(),
        })
        .unwrap();
        assert!(err.contains(r#""code":"not-supported""#));
        assert!(
            encode(&ErrorCode::ConflictState)
                .unwrap()
                .contains("conflict-state")
        );
        let status = encode(&ServerMessage::QueueStatus {
            id: 3,
            status: QueueStatusReport {
                schema: 1,
                ..QueueStatusReport::default()
            },
        })
        .unwrap();
        assert_eq!(
            status,
            "{\"type\":\"queue-status\",\"id\":3,\"schema\":1,\"trains\":[],\"jobs\":[]}\n"
        );
    }

    #[test]
    fn line_limit() {
        assert!(!line_too_long("short"));
        assert!(!line_too_long(&"a".repeat(MAX_LINE_BYTES)));
        assert!(line_too_long(&"a".repeat(MAX_LINE_BYTES + 1)));
    }
}
