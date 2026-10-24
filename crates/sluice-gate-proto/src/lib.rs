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

/// Feature announced by a daemon that serves `exec` and `signal`
/// (`sluice exec`, ADR-0010). Clients send `exec` only to a daemon listing it.
pub const FEATURE_EXEC: &str = "exec";

/// Feature announced by a daemon that honours [`RunRequest::no_marker`]
/// (queue-only gates). Clients set `no_marker` only for a daemon listing it.
pub const FEATURE_QUEUE_ONLY: &str = "queue-only";

/// Value of [`RunningJob::kind`] / [`QueuedJob::kind`] for `sluice exec` jobs.
pub const JOB_KIND_EXEC: &str = "exec";

/// [`ClientMessage::Cancel::reason`] of an MCP `verify` that ended early
/// because a sibling gate failed (ADR-0011 M4); also the `killed_by` value
/// of a job cancelled that way.
pub const CANCEL_EARLY_VERDICT: &str = "early-verdict";

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
        /// Why the client detaches, recorded as the job's `killed_by` when
        /// this was its last subscriber (for example
        /// [`CANCEL_EARLY_VERDICT`]); omitted from the wire when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Run an arbitrary command under the budget, without cache (`sluice
    /// exec`). Only sent to a daemon announcing [`FEATURE_EXEC`].
    Exec {
        /// Client-chosen request id, echoed in every reply.
        id: u64,
        /// The exec request payload.
        #[serde(flatten)]
        request: ExecRequest,
    },
    /// Forward a signal to the job of an `exec` connection (valid on such a
    /// connection only; `bad-request` elsewhere).
    Signal {
        /// Id of the `exec` request.
        id: u64,
        /// Signal received by the client.
        signal: ExecSignal,
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
        /// Surface the request came from (audit trail); `None` = CLI.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<ApproveVia>,
    },
    /// Merge queue: cancel a job. Reply: `queue-job`.
    QueueCancel {
        /// Client-chosen request id.
        #[serde(default)]
        id: u64,
        /// Job to cancel.
        job_id: String,
        /// Surface the request came from (audit trail); `None` = CLI.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<ApproveVia>,
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
    /// Surface the request came from (audit trail); `None` = CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<ApproveVia>,
}

/// Surface a queue request came from: `queue-approve` (required) and,
/// optionally, `queue-enqueue`, `queue-retry`, `queue-cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApproveVia {
    /// The `sluice queue` CLI.
    Cli,
    /// The MCP tools (`queue_approve`, `queue_enqueue`, `queue_retry`,
    /// `queue_cancel`).
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
    /// Queue-only gate: skip the marker double-check and never write a
    /// marker or a miss record. Only set for a daemon announcing
    /// [`FEATURE_QUEUE_ONLY`]; omitted from the wire when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_marker: bool,
    /// When the client sent the request (unix milliseconds), the first
    /// point of the job timeline; omitted from the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at: Option<u64>,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if signature"
)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Payload of an `exec` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Command and arguments; `argv[0]` is already resolved to an absolute
    /// path by the client. No shell is involved.
    pub argv: Vec<String>,
    /// Working directory (the caller's).
    pub cwd: PathBuf,
    /// Complete environment of the command (the caller's, verbatim).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Short label shown in `status` and messages (`cargo build`).
    #[serde(default)]
    pub label: String,
    /// Budget units (clamped by the daemon as for gates).
    #[serde(default)]
    pub weight: u32,
    /// Timeout in milliseconds; 0 means none.
    #[serde(default)]
    pub timeout_ms: u64,
    /// Coalescing key (`--coalesce-key`); `None` never coalesces. Requests
    /// coalesce only when key, `cwd` and `argv` are all equal.
    #[serde(default)]
    pub coalesce: Option<String>,
    /// Repository identity (main worktree or common dir) for the
    /// per-repository cap; `None` outside a repository.
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// Who asked.
    #[serde(default)]
    pub caller: Caller,
    /// Agent session identifier, if any.
    #[serde(default)]
    pub session: Option<String>,
    /// Client process id.
    #[serde(default)]
    pub client_pid: u32,
}

/// A signal forwarded by an `exec` client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ExecSignal {
    /// SIGINT (Ctrl-C).
    Int,
    /// SIGTERM.
    Term,
    /// SIGHUP.
    Hup,
}

impl ExecSignal {
    /// The Linux signal number.
    pub fn number(self) -> i32 {
        match self {
            Self::Hup => 1,
            Self::Int => 2,
            Self::Term => 15,
        }
    }
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
    /// Scheduler weight (budget units). With `weight_auto` it is the
    /// ceiling the client computed from its own budget; daemons that
    /// predate `weight_auto` use it as a declared weight.
    #[serde(default)]
    pub weight: u32,
    /// `weight = "auto"` in the gate: the daemon replaces `weight` with
    /// `ceil(budget / 2)` of its own budget. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub weight_auto: bool,
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
    /// A value this peer does not know (sent by a newer release).
    #[serde(other)]
    Unknown,
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
        /// Weight of the job (effective: learned or configured).
        #[serde(default)]
        weight: u32,
        /// Configured weight after the budget and repository-cap clamp;
        /// omitted when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weight_cfg: Option<u32>,
        /// Expected run time from the gate's history, in milliseconds;
        /// omitted when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_ms: Option<u64>,
    },
    /// The gate process started.
    Started {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Start time (RFC 3339).
        started_at: String,
        /// `exec` only: absolute path of the framed output spool the client
        /// replays (format in `kb/sluice/design/protocol.md`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spool: Option<PathBuf>,
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
    /// Number of the signal that killed the process (`exec`); omitted when
    /// absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Why the job log could not be written (the output was kept in memory
    /// for `tail` only); omitted when the log was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_error: Option<String>,
    /// Time this subscriber waited for the job to start, in milliseconds
    /// (`started_at - attached_at`, 0 when it joined a running job); omitted
    /// when the job never started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_wait_ms: Option<u64>,
    /// Dominant reason the job waited (`budget`, `psi`, `repo-cap`,
    /// `head-reserved` or `order`); omitted when it did not wait.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<String>,
    /// How the job was admitted: `nominal`, `over` (over-admission) or
    /// `guard` (PSI starvation guard); omitted when it never started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admission: Option<String>,
    /// Who killed the job: `last-subscriber`, `early-verdict`, `stop` or
    /// `superseded`; omitted unless it was killed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub killed_by: Option<String>,
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
    /// A value this peer does not know (sent by a newer release).
    #[serde(other)]
    Unknown,
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
    /// A value this peer does not know (sent by a newer release).
    #[serde(other)]
    Unknown,
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
    /// Recent interventions of the kernel or `systemd-oomd` on Sluice's
    /// cgroups (containment). Optional and additive: absent from the JSON
    /// when there is none or the daemon does not record them, and ignored by
    /// older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interventions: Option<Vec<Intervention>>,
    /// Containment state of the machine (`full`, `partial`, `off`) and the
    /// agents' CPU outside Sluice jobs. Absent from daemons without
    /// containment support or started without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment: Option<ContainmentInfo>,
    /// Usage of the live agent sessions (from cgroupfs). Absent when
    /// containment is not on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionUsage>>,
}

/// Containment state in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContainmentInfo {
    /// `full`, `partial` or `off`.
    pub level: String,
    /// What is missing (`partial`) or why it is off (`off`), as in
    /// `containment: off (<detail>)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// CPU the agents' sessions used over the last sampling window, as a
    /// percentage of all CPUs: CPU not spent in Sluice jobs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outside_jobs_cpu_percent: Option<f64>,
    /// Length of that window in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_ms: Option<u64>,
}

/// Usage of one live agent session cgroup (`s-<id>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionUsage {
    /// Session id (the directory name without `s-`).
    pub id: String,
    /// CPU time used by the session since its cgroup was created, seconds.
    pub cpu_seconds: f64,
    /// `memory.current`, bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// `memory.peak`, bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_peak_bytes: Option<u64>,
    /// `pids.current`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub procs: Option<u64>,
}

impl StatusReport {
    /// The containment part of the text status: the `containment:` line,
    /// the per-session lines, `outside jobs:` and `interventions:`. Empty
    /// when the daemon reports none of them. Shared by `sluice status` and
    /// the MCP `status` tool.
    #[must_use]
    pub fn containment_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        if let Some(c) = &self.containment {
            let _ = match c.detail.as_deref().filter(|d| !d.is_empty()) {
                Some(d) => writeln!(out, "containment: {} ({d})", c.level),
                None => writeln!(out, "containment: {}", c.level),
            };
        }
        for s in self.sessions.iter().flatten() {
            let mem = s
                .memory_bytes
                .map_or_else(|| "n/a".to_owned(), |b| format!("{} MB", b / (1024 * 1024)));
            let procs = s.procs.map_or_else(|| "n/a".to_owned(), |p| p.to_string());
            let _ = writeln!(
                out,
                "  s-{}  cpu {:.1}s  mem {mem}  procs {procs}",
                s.id, s.cpu_seconds
            );
        }
        if let Some(p) = self
            .containment
            .as_ref()
            .and_then(|c| c.outside_jobs_cpu_percent)
        {
            let _ = writeln!(out, "outside jobs: {p:.1}% CPU");
        }
        if let Some(rows) = self.interventions.as_ref().filter(|r| !r.is_empty()) {
            let _ = writeln!(out, "interventions:");
            for i in rows {
                let session = i
                    .session
                    .as_deref()
                    .map_or_else(|| "-".to_owned(), |s| format!("s-{s}"));
                let detail = i
                    .detail
                    .as_deref()
                    .map_or_else(String::new, |d| format!(" ({d})"));
                let _ = writeln!(
                    out,
                    "  {}  {session}  {}  {}{detail}",
                    i.at, i.leaf, i.cause
                );
            }
        }
        out
    }
}

/// One kill of a process group of an agent session or a job by the kernel
/// OOM killer or by `systemd-oomd`, recorded by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Intervention {
    /// When it happened (RFC 3339).
    pub at: String,
    /// Agent session id (the `s-<id>` directory without its prefix) when the
    /// cgroup belongs to a session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Cgroup path inside the Sluice scope (`s-abc/tools`, `jobs/<job-id>`).
    pub leaf: String,
    /// `oom_kill`, `oom_group_kill`, `oomd_memory_pressure` or `oomd_swap`.
    pub cause: String,
    /// Human-readable detail, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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
    /// Active log filter of the daemon (for example `info` or `debug`).
    /// Optional and additive: absent from the JSON when the daemon does not
    /// report it, and ignored by older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
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
    /// Over-admission cap (`floor(overcommit * total)`); `used` may exceed
    /// `total` up to it. Omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap: Option<u32>,
    /// CPUs available to the daemon (affinity mask and cgroup limit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    /// Mean idle CPUs over the last two utilisation samples.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_cpus: Option<f64>,
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
    /// `"exec"` for `sluice exec` jobs; omitted for gates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Label of an `exec` job (its `gate` is `exec:<label>`); omitted for gates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Configured weight (after the clamp); `weight` is the effective one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_cfg: Option<u32>,
    /// The effective weight (same value as `weight`, named explicitly for
    /// consumers that show configured and effective side by side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_effective: Option<u32>,
    /// True when the gate is configured with `weight = "auto"`; omitted
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_auto: Option<bool>,
    /// Milliseconds the job waited in the queue before it started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waited_ms: Option<u64>,
    /// `nominal`, `over` (over-admission) or `guard` (PSI starvation guard).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admission: Option<String>,
    /// Measured demand so far (CPU time plus run-queue wait over wall time,
    /// in CPUs); gates and `exec` jobs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub demand: Option<f64>,
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
    /// `"exec"` for `sluice exec` jobs; omitted for gates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Label of an `exec` job (its `gate` is `exec:<label>`); omitted for gates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Fairness lane (the worktree path the request came from).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<PathBuf>,
    /// 1-based position in the priority order (the next admission pass).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<usize>,
    /// Expected run time from the gate's history, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_ms: Option<u64>,
    /// Configured weight (after the clamp); `weight` is the effective one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_cfg: Option<u32>,
    /// The effective weight (same value as `weight`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_effective: Option<u32>,
    /// True when the gate is configured with `weight = "auto"`; omitted
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight_auto: Option<bool>,
    /// Detail of `reason` when the job would fit but higher-ranked jobs
    /// took the room: `outranked`, `lane` or `human-first`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_detail: Option<String>,
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
    /// A value this peer does not know (sent by a newer release).
    #[serde(other)]
    Unknown,
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
    /// Integral of the idle fraction of the daemon's CPUs while at least one
    /// job was queued, in milliseconds (current daemon instance only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_while_waiting_ms: Option<u64>,
    /// Wall time with at least one job queued, in milliseconds (current
    /// daemon instance only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_wall_ms: Option<u64>,
    /// Jobs started by over-admission (current daemon instance only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub over_admissions: Option<u64>,
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
    /// `[queue] push_timeout` of the base, in milliseconds (bounds a
    /// client's wait for the landing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_timeout_ms: Option<u64>,
    /// `[queue] network_timeout` of the base, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_timeout_ms: Option<u64>,
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

    #[test]
    fn wire_enums_round_trip_and_fall_back_to_unknown() {
        fn check<T>(known: &[(T, &str)], unknown: &T)
        where
            T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
        {
            for (value, text) in known {
                assert_eq!(&serde_json::to_string(value).unwrap(), text);
                assert_eq!(&serde_json::from_str::<T>(text).unwrap(), value);
            }
            let future: T = serde_json::from_str("\"from-the-future\"").unwrap();
            assert_eq!(&future, unknown);
        }
        check(
            &[(Caller::Agent, "\"agent\""), (Caller::Human, "\"human\"")],
            &Caller::Unknown,
        );
        check(
            &[
                (OutcomeStatus::Pass, "\"pass\""),
                (OutcomeStatus::Cached, "\"cached\""),
                (OutcomeStatus::Error, "\"error\""),
            ],
            &OutcomeStatus::Unknown,
        );
        check(
            &[
                (ErrorCode::BadRequest, "\"bad-request\""),
                (ErrorCode::NotSupported, "\"not-supported\""),
            ],
            &ErrorCode::Unknown,
        );
        check(
            &[
                (WaitReason::RepoCap, "\"repo-cap\""),
                (WaitReason::HeadReserved, "\"head-reserved\""),
            ],
            &WaitReason::Unknown,
        );
    }

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
                weight_auto: false,
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
            no_marker: false,
            requested_at: None,
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
            signal: None,
            log_error: None,
            ..Outcome::default()
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
                log_level: None,
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
                ..BudgetInfo::default()
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
                kind: None,
                label: None,
                ..RunningJob::default()
            }],
            queued: vec![
                QueuedJob {
                    job_id: "k".into(),
                    repo: "/abs".into(),
                    gate: "lint".into(),
                    weight: 1,
                    queued_at: "t".into(),
                    waiting_ms: 300,
                    subscribers: 1,
                    reason: WaitReason::HeadReserved,
                    kind: None,
                    label: None,
                    ..QueuedJob::default()
                },
                QueuedJob {
                    job_id: "x".into(),
                    gate: "exec:cargo build".into(),
                    kind: Some(JOB_KIND_EXEC.into()),
                    label: Some("cargo build".into()),
                    ..QueuedJob::default()
                },
            ],
            totals: Totals {
                runs: 120,
                ..Totals::default()
            },
            queue: Some(QueueSummary {
                active: 3,
                ready: 1,
                needs_reconcile: 0,
            }),
            interventions: Some(vec![Intervention {
                at: "2026-10-24T10:00:00Z".into(),
                session: Some("abc".into()),
                leaf: "s-abc/tools".into(),
                cause: "oom_kill".into(),
                detail: None,
            }]),
            containment: Some(ContainmentInfo {
                level: "partial".into(),
                detail: Some("memory, pids; cpu by nice".into()),
                outside_jobs_cpu_percent: Some(3.25),
                window_ms: Some(10_000),
            }),
            sessions: Some(vec![SessionUsage {
                id: "abc".into(),
                cpu_seconds: 12.5,
                memory_bytes: Some(432_013_312),
                memory_peak_bytes: Some(500_000_000),
                procs: Some(18),
            }]),
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
            push_timeout_ms: Some(600_000),
            network_timeout_ms: Some(60_000),
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
            ClientMessage::Cancel {
                id: 1,
                reason: None,
            },
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
                    via: Some(ApproveVia::Mcp),
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
                via: None,
            },
            ClientMessage::QueueRetry {
                id: 8,
                job_id: "j".into(),
                head_sha: TREE.into(),
                via: Some(ApproveVia::Mcp),
            },
            ClientMessage::QueueCancel {
                id: 9,
                job_id: "j".into(),
                via: None,
            },
            ClientMessage::QueueCancel {
                id: 9,
                job_id: "j".into(),
                via: Some(ApproveVia::Mcp),
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
                weight_cfg: None,
                expected_ms: None,
            },
            ServerMessage::Started {
                id: 1,
                job_id: "j".into(),
                started_at: "t".into(),
                spool: None,
            },
            ServerMessage::Started {
                id: 1,
                job_id: "j".into(),
                started_at: "t".into(),
                spool: Some("/s/logs/j.exec".into()),
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
                weight: 4,
                weight_cfg: None,
                expected_ms: None
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

    fn exec_request() -> ExecRequest {
        ExecRequest {
            argv: vec!["/usr/bin/cargo".into(), "build".into(), "--release".into()],
            cwd: "/abs/worktree".into(),
            env: BTreeMap::from([("CLAUDECODE".into(), "1".into())]),
            label: "cargo build".into(),
            weight: 4,
            timeout_ms: 0,
            coalesce: None,
            repo: Some("/abs/main".into()),
            caller: Caller::Agent,
            session: None,
            client_pid: 4242,
        }
    }

    #[test]
    fn exec_messages_round_trip_and_match_the_spec() {
        let msgs = [
            ClientMessage::Exec {
                id: 1,
                request: exec_request(),
            },
            ClientMessage::Exec {
                id: 1,
                request: ExecRequest {
                    coalesce: Some("k".into()),
                    repo: None,
                    ..exec_request()
                },
            },
            ClientMessage::Signal {
                id: 1,
                signal: ExecSignal::Int,
            },
        ];
        for m in msgs {
            assert_eq!(decode_client(&encode(&m).unwrap()).unwrap(), m);
        }
        let line = r#"{"type":"exec","id":1,"argv":["/usr/bin/cargo","build","--release"],
 "cwd":"/abs/worktree","env":{"CLAUDECODE":"1"},
 "label":"cargo build","weight":4,"timeout_ms":0,"coalesce":null,
 "repo":"/abs/main","caller":"agent","session":null,"client_pid":4242}"#;
        assert_eq!(
            decode_client(line).unwrap(),
            ClientMessage::Exec {
                id: 1,
                request: exec_request()
            }
        );
        for (text, sig, n) in [
            ("INT", ExecSignal::Int, 2),
            ("TERM", ExecSignal::Term, 15),
            ("HUP", ExecSignal::Hup, 1),
        ] {
            let m =
                decode_client(&format!(r#"{{"type":"signal","id":1,"signal":"{text}"}}"#)).unwrap();
            assert_eq!(m, ClientMessage::Signal { id: 1, signal: sig });
            assert_eq!(sig.number(), n);
        }
    }

    #[test]
    fn additive_exec_fields_stay_off_the_wire_when_unset() {
        // Golden lines of v0.2: `run`, `started`, `done` and `status` entries
        // serialise exactly as before when the new optional fields are unset.
        let run = encode(&ClientMessage::Run {
            id: 1,
            request: run_request(),
        })
        .unwrap();
        assert!(!run.contains("no_marker"), "{run}");
        let mut req = run_request();
        req.no_marker = true;
        let run = encode(&ClientMessage::Run {
            id: 1,
            request: req,
        })
        .unwrap();
        assert!(run.contains(r#""no_marker":true"#), "{run}");
        let started = encode(&ServerMessage::Started {
            id: 1,
            job_id: "j".into(),
            started_at: "t".into(),
            spool: None,
        })
        .unwrap();
        assert_eq!(
            started,
            "{\"type\":\"started\",\"id\":1,\"job_id\":\"j\",\"started_at\":\"t\"}\n"
        );
        let done = encode(&outcome()).unwrap();
        assert!(!done.contains("signal"), "{done}");
        let running = serde_json::to_string(&RunningJob::default()).unwrap();
        assert!(
            !running.contains("kind") && !running.contains("label"),
            "{running}"
        );
        let queued = serde_json::to_string(&QueuedJob::default()).unwrap();
        assert!(
            !queued.contains("kind") && !queued.contains("label"),
            "{queued}"
        );
    }

    /// ADR-0011: `requested_at`, the cancel reason, the `queued` scheduler
    /// fields and the outcome timeline fields stay off the wire when unset,
    /// and old lines still decode.
    #[test]
    fn scheduler_wire_fields_are_additive() {
        let run = encode(&ClientMessage::Run {
            id: 1,
            request: run_request(),
        })
        .unwrap();
        assert!(!run.contains("requested_at"), "{run}");
        let mut req = run_request();
        req.requested_at = Some(1_700_000_000_000);
        let line = encode(&ClientMessage::Run {
            id: 1,
            request: req.clone(),
        })
        .unwrap();
        assert!(line.contains(r#""requested_at":1700000000000"#), "{line}");
        assert_eq!(
            decode_client(&line).unwrap(),
            ClientMessage::Run {
                id: 1,
                request: req
            }
        );

        let cancel = encode(&ClientMessage::Cancel {
            id: 1,
            reason: None,
        })
        .unwrap();
        assert_eq!(cancel, "{\"type\":\"cancel\",\"id\":1}\n");
        let early = encode(&ClientMessage::Cancel {
            id: 1,
            reason: Some(CANCEL_EARLY_VERDICT.into()),
        })
        .unwrap();
        assert!(early.contains(r#""reason":"early-verdict""#), "{early}");
        assert_eq!(
            decode_client(r#"{"type":"cancel","id":1}"#).unwrap(),
            ClientMessage::Cancel {
                id: 1,
                reason: None
            }
        );

        let queued = ServerMessage::Queued {
            id: 1,
            job_id: "j".into(),
            position: 1,
            coalesced: false,
            weight: 6,
            weight_cfg: None,
            expected_ms: None,
        };
        assert_eq!(
            encode(&queued).unwrap(),
            "{\"type\":\"queued\",\"id\":1,\"job_id\":\"j\",\"position\":1,\"coalesced\":false,\"weight\":6}\n"
        );

        let done = encode(&outcome()).unwrap();
        for k in ["queue_wait_ms", "wait_reason", "admission", "killed_by"] {
            assert!(!done.contains(k), "{done}");
        }
        let full = Outcome {
            queue_wait_ms: Some(31_000),
            wait_reason: Some("budget".into()),
            admission: Some("over".into()),
            killed_by: Some(CANCEL_EARLY_VERDICT.into()),
            ..outcome()
        };
        let back: Outcome = serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert_eq!(back, full);
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
        // `via` is optional (old clients never send it) and omitted when unset.
        assert_eq!(request.via, None);
        let line = encode(&ClientMessage::QueueEnqueue { id: 1, request }).unwrap();
        assert!(!line.contains("via"), "{line}");
        let m = decode_client(r#"{"type":"queue-cancel","job_id":"j","via":"mcp"}"#).unwrap();
        assert!(matches!(
            m,
            ClientMessage::QueueCancel {
                via: Some(ApproveVia::Mcp),
                ..
            }
        ));
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
