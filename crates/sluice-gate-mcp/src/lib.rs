//! MCP server for Sluice (`sluice mcp`, Phase 3).
//!
//! A thin client over stdio: `verify` performs the same cache-hit check as
//! `sluice run` (core library, no daemon) and forwards misses to the
//! daemon, autostarting it when needed. It never runs a gate in-process.
//! Design: `kb/sluice/design/mcp-tools.md`, ADR-0005.
//!
//! Logging goes to stderr only (`SLUICE_LOG`, default `info`); stdout is
//! the MCP transport.

mod daemon;
mod queue;
mod text;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::schema_for_output;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProgressNotificationParam, ProgressToken,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt, tool, tool_handler,
    tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sluice_gate_core::config::{RepoConfig, UserConfig};
use sluice_gate_core::git::{self, EnvMap};
use sluice_gate_core::hookgate::{HookExclusion, HookGateSource, HookGates};
use sluice_gate_core::lookup::{self, Lookup, RepoContext, Resolved, RunMaterial};
use sluice_gate_core::marker::{self, Marker};
use sluice_gate_core::mode::{CacheMode, cache_mode};
use sluice_gate_core::tree::{self, SnapshotFull};
use sluice_gate_core::{SLUICE_VERSION, dirs};
use sluice_gate_proto::client::{self, ClientError};
use sluice_gate_proto::{
    self as proto, ClientMessage, GateSpec, OutcomeStatus, QueueTrainState, QueueTrainView,
    RepoInfo, RunRequest, STATUS_SCHEMA_VERSION, ServerMessage,
};
use tokio_util::sync::CancellationToken;

use crate::daemon::{RunEnd, RunError};
use crate::queue::{ApproveReport, JobSchema, StatusSchema, TrainSchema};
use crate::text::{age, dur};

/// Default and maximum number of lines returned by `logs`.
const LOGS_DEFAULT_TAIL: usize = 100;
const LOGS_MAX_TAIL: usize = 2000;
/// Cap of the `logs` output, in bytes.
const LOGS_MAX_BYTES: usize = 64 * 1024;

/// Instructions sent to the model in `initialize`.
const INSTRUCTIONS: &str = "Sluice runs this machine's verification gates (tests, linters, type \
checks) through one shared queue and caches every pass per git tree. Before committing, call \
the `verify` tool with your absolute working directory. Do not run test, lint or type-check \
commands through the shell: they bypass the queue and the cache and slow down every other \
agent on the machine. When `verify` reports FAIL, read the log tail in the result, fix the \
code, and call `verify` again rather than re-running the commands yourself. A tree that \
already passed is answered from the cache in milliseconds. `status` shows queued and running \
jobs, `logs` shows a job's output, `gate_list` lists the gates of a repository: those of its `.sluice.toml` or, in a repository \
without one whose git hooks go through Sluice, the check units of its hooks (it also says which \
hooks and units are left out and why); `verify` runs them with the same cache entries as the \
commit's hook, so a pass here makes the commit's hook a cache hit. A unit that `gate_list` leaves \
out is run by the commit's hook: do not run its command by hand through the shell. \
Merge queue: to land a finished branch, commit it, call `verify` and wait for it to pass, then \
call `queue_enqueue` with your absolute working directory, and stop. Tell the human the job id \
(and the train id from `queue_status`): a human approves the train with `sluice queue approve`. \
Never approve a train yourself and never call `queue_approve` unless the user told you to; it \
is refused unless the user enabled agent approval. Never push to the target branch yourself. \
`queue_status` shows trains, conflicts and what is ready. When it reports a conflict (or a \
failed gate) for your branch, rebase or fix the branch, commit, run `verify` again, and call \
`queue_retry` with the job id: the job keeps the branch, so `queue_enqueue` would answer \
`duplicate-branch`. `queue_cancel` withdraws a job. Errors start with a code: \
`conflict-state`, `stale-plan`, `duplicate-branch`, `not-allowed`, `not-supported`. \
Reasons in `queue_status`: `prepare_failed` on a waiting job (`base: true`) is the target's or \
the environment's fault: wait or tell the human; with `base: false` the branch broke the \
prepare (often a lockfile out of sync): fix it, then `queue_retry`. `prepare_dirty` (the prepare \
leaves files the tree lacks: fix `.gitignore` or the prepare), `gate_modified_tree` (a gate edits \
tracked files: commit the result or fix the gate) and `member_config_invalid` (the branch's \
`.sluice.toml` does not parse) are yours to fix; `key_input_failed` is the environment's: tell the \
human; `pre_push_hook`: the repository's hook declined, read its output; `target_protected`: the forge \
refuses direct pushes to the target: tell the human, do not retry (a `queue_enqueue` result with \
a warning about the target is for the human too; the job is queued anyway). A job or train \
`awaiting_merge` means a human approved it and its pull request (not you) is the way it lands: \
wait, or tell the human; never merge, close or push to the pull request or its `sluice/train/*` \
branch, and never cancel it (`queue_cancel` is refused for it). `pr_closed` (the pull request \
was closed without merging, or its branch is gone): tell the human, do not retry. \
`hook_gate_unverified` and `config_changed` mean the branch changes `.sluice.toml`, which \
the target's configuration judges: tell the human to land it first or retry with \
`--trust-member-config`; never use that flag or ask for such a train to be approved. \
A gate result `KILLED for lack of memory` means the machine lacked the memory the gate needs: \
follow its message (run it again unchanged once memory is free, Sluice waits for it, or tell the \
human); other arguments or a lower concurrency rarely lower its memory and start a fresh \
estimate. A gate that `status` shows waiting for memory is only queued. Never run a gate through the shell to get around either. \
A `verify` progress message or a `status` that reports its job STALLED (no CPU, I/O or output \
for its quiet window) means the gate is probably hung, for example a test waiting forever: \
look at the test and `logs`, and cancel your own `verify` call (the daemon then ends the job \
if nobody else waits for it); never re-run the gate through the shell. Sluice only reports a \
stall, it never ends the job itself.";

/// Runs the MCP server on stdin/stdout until the client disconnects.
///
/// `env` is the process environment (it locates the daemon socket, the user
/// config and the state directory, and seeds the gate environment).
///
/// # Errors
///
/// When the transport fails to initialise or breaks.
pub async fn serve(env: EnvMap, hook_gates: HookGateSource) -> anyhow::Result<()> {
    init_tracing(&env);
    tracing::info!(version = SLUICE_VERSION, "sluice mcp starting");
    let service = Server::new(env, hook_gates)
        .serve(rmcp::transport::stdio())
        .await?;
    let reason = service.waiting().await?;
    tracing::info!(?reason, "sluice mcp stopped");
    Ok(())
}

/// Installs the tracing subscriber: stderr plus `<state>/logs/mcp-<pid>.log`
/// (stdout is the MCP channel and is never written). The filter is
/// `SLUICE_LOG`, then `[debug] level` of the user config (an unreadable
/// config is ignored here), then `info`. The file is best effort.
fn init_tracing(env: &EnvMap) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let configured = sluice_gate_core::config::UserConfig::load(env)
        .ok()
        .map(|user| user.debug.level);
    let level = sluice_gate_core::config::resolve_log_level(env, configured.as_deref());
    let filter = tracing_subscriber::EnvFilter::try_new(&level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let file = open_mcp_log(env).map(|f| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::sync::Mutex::new(f))
            .with_ansi(false)
    });
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        )
        .with(file)
        .try_init();
}

/// Opens `<state>/logs/mcp-<pid>.log` for appending.
fn open_mcp_log(env: &EnvMap) -> Option<std::fs::File> {
    let dir = dirs::logs_dir(env);
    sluice_gate_core::dirs::create_private_dir(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!(
            "{}{}.log",
            sluice_gate_core::logfile::MCP_LOG_PREFIX,
            std::process::id()
        )))
        .ok()
}

// ---------------------------------------------------------------- params

#[derive(Debug, Deserialize, JsonSchema)]
struct VerifyParams {
    /// Absolute path inside the repository (worktree) to verify.
    working_directory: String,
    /// Gate names, run in this order. Default: the gates whose `hooks`
    /// contain `pre-commit`; when none is tagged, every gate.
    #[serde(default)]
    gates: Option<Vec<String>>,
    /// Execution time limit per gate in seconds, overriding the gate's
    /// `timeout`. Time spent waiting in the queue does not count.
    #[serde(default)]
    timeout_seconds: Option<u64>,
    /// Arguments appended to every selected gate's command.
    #[serde(default)]
    extra_args: Option<Vec<String>>,
    /// Stop at the first failing gate (default true): the call returns as
    /// soon as one gate fails, times out or errors, and the gates still
    /// pending are cancelled (a job another client also waits for keeps
    /// running). `false` waits for every gate and reports every failure.
    #[serde(default)]
    fail_fast: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StatusParams {
    /// Absolute path inside a repository: adds the cache state of its
    /// current tree per gate and marks that repository's jobs.
    #[serde(default)]
    working_directory: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GateListParams {
    /// Absolute path inside the repository (worktree).
    working_directory: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogsParams {
    /// Job id (as printed by `verify` or `status`). Default: the newest job.
    #[serde(default)]
    job_id: Option<String>,
    /// Number of trailing lines (default 100, max 2000).
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QueueEnqueueParams {
    /// Absolute path inside the repository (worktree) whose branch is queued.
    working_directory: String,
    /// Branch to queue. Default: the current branch of the worktree
    /// (a detached HEAD is refused).
    #[serde(default)]
    branch: Option<String>,
    /// Landing target as `<remote>/<branch>` or a local branch. Default: the
    /// repository's configured target.
    #[serde(default)]
    base: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QueueStatusParams {
    /// Absolute path inside the repository (worktree).
    working_directory: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QueueJobParams {
    /// Absolute path inside the repository (worktree) the job belongs to.
    working_directory: String,
    /// Merge-queue job id, as returned by `queue_enqueue` or shown by
    /// `queue_status`.
    job_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QueueApproveParams {
    /// Absolute path inside the repository (worktree).
    working_directory: String,
    /// Train to approve, as shown by `queue_status`.
    train_id: String,
}

// ---------------------------------------------------------------- results

/// Final state of one gate in a `verify` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum GateStatus {
    /// Passed (or a cached pass).
    Pass,
    /// Non-zero exit or killed by a foreign signal.
    Fail,
    /// Exceeded its execution timeout.
    Timeout,
    /// Killed by Sluice (daemon stop, last subscriber gone).
    Killed,
    /// Sluice itself failed to run the gate.
    Error,
    /// The `verify` call was cancelled while this gate was pending.
    Cancelled,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GateResult {
    /// Gate name.
    gate: String,
    /// Outcome category.
    status: GateStatus,
    /// True when answered from the cache (nothing ran for this call).
    cached: bool,
    /// Wall-clock run time in milliseconds (absent for cached passes).
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    /// Job id (for a cached pass, the job that produced the marker).
    job_id: Option<String>,
    /// Path of the job log.
    log_path: Option<PathBuf>,
    /// Exit code, when the process exited normally.
    exit_code: Option<i32>,
    /// True when this call joined a job another client had already queued.
    coalesced: bool,
    /// Why a pass was not cached, if it was not.
    #[serde(skip_serializing_if = "Option::is_none")]
    not_cached_reason: Option<String>,
    /// Human-readable detail (errors, cancellation, a memory kill).
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// `memory` when the kernel or `systemd-oomd` killed the gate for lack of
    /// memory (`status` is `killed`, `message` says how much it used).
    #[serde(skip_serializing_if = "Option::is_none")]
    killed_by: Option<String>,
    /// Last lines of the log of a failing gate (absent when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tail: Vec<String>,
    /// Time this call waited in the queue before the gate started, in
    /// milliseconds (absent for cached passes and gates that never started).
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_wait_ms: Option<u64>,
    /// Dominant reason the job waited: `budget`, `psi`, `repo-cap`,
    /// `head-reserved` or `order` (absent when it did not wait).
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_reason: Option<String>,
}

impl GateResult {
    fn new(gate: &str, status: GateStatus) -> Self {
        Self {
            gate: gate.to_owned(),
            status,
            cached: false,
            duration_ms: None,
            job_id: None,
            log_path: None,
            exit_code: None,
            coalesced: false,
            not_cached_reason: None,
            message: None,
            killed_by: None,
            tail: Vec::new(),
            queue_wait_ms: None,
            wait_reason: None,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
struct VerifyReport {
    /// Repository root.
    repository: PathBuf,
    /// One entry per gate, in the requested order.
    results: Vec<GateResult>,
    /// True when every gate passed.
    all_passed: bool,
    /// True when the call was cancelled before every gate finished.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    cancelled: bool,
    /// Wall time of the whole call, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    wall_ms: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GateInfo {
    /// Gate name.
    name: String,
    /// Command, as configured.
    cmd: String,
    /// Budget units the gate occupies while running.
    weight: u32,
    /// Execution timeout in seconds (0 = none).
    timeout_seconds: u64,
    /// Git hooks the gate belongs to.
    hooks: Vec<String>,
    /// Whether passes are cached.
    cache: bool,
    /// `config` for a gate of `.sluice.toml`, `hook` for a unit of the
    /// repository's git hooks (a repository without `.sluice.toml`).
    source: &'static str,
    /// The hook manager of a `hook` gate (`husky`, `lefthook`, `pre-commit`,
    /// `plain`).
    #[serde(skip_serializing_if = "Option::is_none")]
    manager: Option<String>,
}

/// A hook or unit of the repository's hooks that is not a gate.
#[derive(Debug, Serialize, JsonSchema)]
struct ExcludedInfo {
    /// The hook.
    hook: String,
    /// The unit's name; absent when the whole hook is excluded.
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<String>,
    /// The unit's command.
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    /// Why it is not a gate.
    reason: String,
}

impl From<HookExclusion> for ExcludedInfo {
    fn from(e: HookExclusion) -> Self {
        Self {
            hook: e.hook,
            unit: e.unit,
            command: e.command,
            reason: e.reason,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
struct GateList {
    /// Repository root.
    repository: PathBuf,
    /// Gates of `.sluice.toml`, by name; without one, the check units of the
    /// repository's hooks.
    gates: Vec<GateInfo>,
    /// Hooks and units of the repository's hooks that are not gates, with
    /// the reason (only for a repository without `.sluice.toml`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    excluded: Vec<ExcludedInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct LogsReport {
    /// Job id.
    job_id: String,
    /// Log file (the full output stays there).
    path: PathBuf,
    /// Trailing lines, oldest first.
    lines: Vec<String>,
    /// Bytes dropped from the front to keep the output under 64 KiB.
    omitted_bytes: usize,
}

/// Cache state of one gate on the current tree (`status`).
#[derive(Debug, Serialize)]
struct GateCache {
    gate: String,
    /// `hit`, `miss`, `uncached` or `error`.
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    passed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_run: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Path scope of a scoped gate: `{files, total, hash}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<serde_json::Value>,
}

// ---------------------------------------------------------------- helpers

fn invalid(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

fn internal(message: impl Into<String>) -> McpError {
    McpError::internal_error(message.into(), None)
}

fn workdir(dir: &str) -> Result<PathBuf, McpError> {
    let path = PathBuf::from(dir);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(invalid(format!(
            "working_directory must be an absolute path, got '{dir}'"
        )))
    }
}

/// Runs blocking work off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, McpError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| internal(format!("internal task failed: {e}")))
}

/// [`blocking`], abandoned (`None`) when `ct` is cancelled first.
async fn blocking_or_cancel<T: Send + 'static>(
    ct: &CancellationToken,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<Option<T>, McpError> {
    tokio::select! {
        () = ct.cancelled() => Ok(None),
        r = blocking(f) => r.map(Some),
    }
}

/// A result with a text block and `structured_content`.
fn result<T: Serialize>(
    text: String,
    value: &T,
    is_error: bool,
) -> Result<CallToolResult, McpError> {
    let content = vec![ContentBlock::text(text)];
    let mut r = if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    r.structured_content = Some(serde_json::to_value(value).map_err(|e| internal(e.to_string()))?);
    Ok(r)
}

/// The line telling this session about interventions it has not heard of
/// yet (design section 5), from `events` (the daemon's recent interventions
/// or the log file). Marks them as told. Blocking: reads and writes small
/// files.
fn intervention_note(
    env: &EnvMap,
    events: &[sluice_gate_core::cgroup::explain::Event],
) -> Option<String> {
    let own = sluice_gate_core::cgroup::explain::own_cgroup();
    sluice_gate_core::cgroup::explain::take_note(env, events, own.as_deref(), None)
}

/// The recorded interventions from the daemon's log file.
fn logged_interventions(env: &EnvMap) -> Vec<sluice_gate_core::cgroup::explain::Event> {
    sluice_gate_core::cgroup::explain::read_events(&sluice_gate_core::cgroup::explain::log_path(
        env,
    ))
}

/// Converts the shared run material into the protocol request (caller `agent`).
fn to_request(m: RunMaterial) -> RunRequest {
    RunRequest {
        repo: RepoInfo {
            toplevel: m.paths.toplevel,
            common_dir: m.paths.common_dir,
            git_dir: m.paths.git_dir,
        },
        gate: GateSpec {
            name: m.gate_name,
            cmd: m.gate.cmd,
            weight: m
                .gate
                .weight
                .ceiling(sluice_gate_core::cpus::default_budget()),
            weight_auto: m.gate.weight.is_auto(),
            timeout_ms: m.timeout_ms,
            env: m.gate.env,
            cache: m.gate.cache,
            stall_timeout_ms: m.gate.stall_timeout.to_wire(),
        },
        key: m.key,
        tree: m.tree,
        committed_tree: m.committed_tree,
        cmd: m.cmd,
        extra: m.extra,
        cwd: m.cwd,
        child_env: m.child_env,
        probe_env: m.probe_env,
        caller: proto::Caller::Agent,
        session: m.session,
        client_pid: std::process::id(),
        hook: m.hook,
        no_marker: false,
        requested_at: Some(sluice_gate_core::dirs::now_ms()),
    }
}

/// Gates `verify` runs: the requested ones (validated), else the
/// `pre-commit` gates, else every gate.
fn select_gates(
    config: &RepoConfig,
    requested: Option<Vec<String>>,
) -> Result<Vec<String>, McpError> {
    let all: Vec<String> = config.gates.keys().cloned().collect();
    if all.is_empty() {
        return Err(invalid(
            "the .sluice.toml of this repository defines no gates",
        ));
    }
    match requested {
        Some(gates) if !gates.is_empty() => {
            for g in &gates {
                if config.gate(g).is_err() {
                    return Err(invalid(format!(
                        "unknown gate '{g}' (defined: {})",
                        all.join(", ")
                    )));
                }
            }
            Ok(gates)
        }
        _ => {
            let tagged: Vec<String> = config
                .gates_for_hook("pre-commit")
                .into_iter()
                .map(|(n, _)| n.to_owned())
                .collect();
            Ok(if tagged.is_empty() { all } else { tagged })
        }
    }
}

/// What the hit check decided for one gate.
#[allow(clippy::large_enum_variant)] // one short-lived value per gate
enum Prepared {
    Hit(Marker),
    Run {
        request: RunRequest,
        res: Resolved,
        off_reason: Option<String>,
    },
}

/// Hit check and request material for one gate (blocking: spawns git).
fn prepare(
    res: Resolved,
    mode: &CacheMode,
    extra: &[String],
    timeout_seconds: Option<u64>,
) -> Result<Prepared, String> {
    let state = lookup::lookup(&res, mode, extra);
    if let Lookup::Hit { marker, .. } = state {
        return Ok(Prepared::Hit(marker));
    }
    // A queue-only gate is never cached by design: no "not cached" note.
    let off_reason = match &state {
        Lookup::Off { reason, .. } if !res.gate.is_queue_only() => Some(reason.clone()),
        _ => None,
    };
    let mut material =
        lookup::run_material(&res, &state, extra, None).map_err(|e| e.to_string())?;
    if let Some(secs) = timeout_seconds {
        material.timeout_ms = secs.saturating_mul(1000);
    }
    Ok(Prepared::Run {
        request: to_request(material),
        res,
        off_reason,
    })
}

/// Where a running gate stands, for progress notifications.
#[derive(Debug, Default)]
struct JobState {
    job_id: Option<String>,
    position: Option<usize>,
    running: bool,
    /// Elapsed run time reported by the daemon and when it was reported.
    elapsed: Option<(u64, Instant)>,
    /// How long the running job has shown no progress, while it is stalled
    /// (ADR-0024), and when it was reported.
    stalled: Option<(u64, Instant)>,
}

impl JobState {
    fn update(&mut self, msg: &ServerMessage) {
        match msg {
            ServerMessage::Queued {
                job_id, position, ..
            } => {
                self.job_id = Some(job_id.clone());
                self.position = Some(*position);
            }
            ServerMessage::Started { job_id, .. } => {
                self.job_id = Some(job_id.clone());
                self.running = true;
                self.position = None;
                self.elapsed = Some((0, Instant::now()));
            }
            ServerMessage::Progress {
                elapsed_ms,
                position,
                stalled_ms,
                ..
            } => {
                if self.running {
                    self.elapsed = Some((*elapsed_ms, Instant::now()));
                    self.stalled = stalled_ms.map(|ms| (ms, Instant::now()));
                } else {
                    self.position = *position;
                }
            }
            _ => {}
        }
    }

    fn describe(&self, gate: &str) -> String {
        let job = self
            .job_id
            .as_deref()
            .map_or_else(String::new, |j| format!(" (job {j})"));
        if self.running {
            let ms = self.elapsed.map_or(0, |(ms, at)| {
                ms + u64::try_from(at.elapsed().as_millis()).unwrap_or(0)
            });
            let stalled = self.stalled.map_or_else(String::new, |(ms, at)| {
                let ms = ms + u64::try_from(at.elapsed().as_millis()).unwrap_or(0);
                format!(
                    "; looks STALLED: no CPU, I/O or output for {}, probably hung",
                    dur(ms)
                )
            });
            format!("{gate}: running for {}{job}{stalled}", dur(ms))
        } else if let Some(p) = self.position {
            format!("{gate}: queued, position {p}{job}")
        } else {
            format!("{gate}: submitting to the daemon")
        }
    }
}

/// Progress notification target of one `verify` call.
struct Progress {
    peer: Peer<RoleServer>,
    token: ProgressToken,
    interval: Duration,
    started: Instant,
}

impl Progress {
    async fn notify(&self, message: String) {
        let param = ProgressNotificationParam::new(
            self.token.clone(),
            self.started.elapsed().as_secs_f64(),
        )
        .with_message(message);
        if let Err(e) = self.peer.notify_progress(param).await {
            tracing::debug!("cannot send progress: {e}");
        }
    }
}

/// One gate's entry in the result: structured part and text line (without the name).
struct GateLine {
    result: GateResult,
    line: String,
    /// Cancelled because a sibling gate failed first (early verdict).
    early: bool,
}

fn cancelled_line(gate: &str, job_id: Option<String>) -> GateLine {
    let line = job_id.as_deref().map_or_else(
        || "CANCELLED".to_owned(),
        |j| format!("CANCELLED (job {j})"),
    );
    let mut result = GateResult::new(gate, GateStatus::Cancelled);
    result.job_id = job_id;
    result.message = Some("the verification was cancelled".to_owned());
    GateLine {
        result,
        line,
        early: false,
    }
}

/// A gate left unfinished because a sibling gate failed first; the message
/// names that gate once the call knows it ([`name_early_verdicts`]).
fn early_line(gate: &str, job_id: Option<String>) -> GateLine {
    let mut line = cancelled_line(gate, job_id);
    line.early = true;
    line
}

/// Words the lines of gates cancelled by the early verdict:
/// `not finished: <gate> failed first`.
fn name_early_verdicts(lines: &mut [GateLine], failed: &str) {
    for g in lines.iter_mut().filter(|g| g.early) {
        let why = format!("not finished: {failed} failed first");
        g.line = match &g.result.job_id {
            Some(j) => format!("CANCELLED ({why}; job {j})"),
            None => format!("CANCELLED ({why})"),
        };
        g.result.message = Some(why);
    }
}

/// A Sluice-side failure of one gate (daemon unavailable, request refused,
/// connection lost): that gate is an `error`, the remaining ones still run.
fn error_line(gate: &str, why: &str) -> GateLine {
    let mut result = GateResult::new(gate, GateStatus::Error);
    result.message = Some(why.to_owned());
    GateLine {
        result,
        line: format!("ERROR ({why})"),
        early: false,
    }
}

/// The first words of a gate's summary line (`PASS 12.3s (job …)`,
/// `FAIL exit=1 …`, …), before the queue-wait suffix.
fn status_text(
    outcome: &proto::Outcome,
    secs: f64,
    job_id: &str,
    log: &str,
    timeout: Duration,
    not_cached: Option<&str>,
) -> String {
    match outcome.status {
        OutcomeStatus::Cached => "PASS (cached)".to_owned(),
        OutcomeStatus::Pass => format!(
            "PASS {secs:.1}s (job {job_id}){}",
            not_cached.map_or_else(String::new, |r| format!(" (not cached: {r})"))
        ),
        OutcomeStatus::Fail => format!(
            "FAIL exit={} {secs:.1}s (job {job_id}) log={log}",
            outcome
                .exit_code
                .map_or_else(|| "signal".to_owned(), |c| c.to_string())
        ),
        OutcomeStatus::Timeout => format!(
            "TIMEOUT after {} (job {job_id}) log={log}",
            humantime::format_duration(timeout)
        ),
        // Killed by the kernel or `systemd-oomd`: the agent must read that
        // and change something, not retry a cancel.
        OutcomeStatus::Killed if outcome.killed_for_memory() => format!(
            "KILLED for lack of memory after {secs:.1}s (job {job_id}) log={log}: {}",
            outcome.message
        ),
        OutcomeStatus::Killed if outcome.log_path.is_none() => {
            format!("KILLED before it started (job {job_id}): the daemon stopped")
        }
        OutcomeStatus::Killed => format!("KILLED after {secs:.1}s (job {job_id}) log={log}"),
        OutcomeStatus::Error => format!("ERROR {}", outcome.message),
        // Never a pass: a status from a newer daemon is reported as an error.
        OutcomeStatus::Unknown => {
            format!("ERROR unknown outcome status from the daemon (job {job_id}); update sluice")
        }
    }
}

/// Maps a daemon outcome to the result entry and the Phase 1 summary line.
fn outcome_line(
    res: &Resolved,
    timeout: Duration,
    off_reason: Option<&str>,
    end: (String, proto::Outcome, bool),
) -> GateLine {
    let (done_job, outcome, coalesced) = end;
    let job_id = outcome
        .coalesced_with
        .clone()
        .or_else(|| {
            outcome
                .log_path
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or(done_job);
    let secs = Duration::from_millis(outcome.duration_ms).as_secs_f64();
    let log = outcome
        .log_path
        .as_deref()
        .unwrap_or(Path::new("-"))
        .display()
        .to_string();
    let status = match outcome.status {
        OutcomeStatus::Pass | OutcomeStatus::Cached => GateStatus::Pass,
        OutcomeStatus::Fail => GateStatus::Fail,
        OutcomeStatus::Timeout => GateStatus::Timeout,
        OutcomeStatus::Killed => GateStatus::Killed,
        OutcomeStatus::Error | OutcomeStatus::Unknown => GateStatus::Error,
    };
    let not_cached = off_reason
        .map(str::to_owned)
        .or_else(|| outcome.not_cached_reason.clone())
        .filter(|_| outcome.status == OutcomeStatus::Pass);
    let line = status_text(
        &outcome,
        secs,
        &job_id,
        &log,
        timeout,
        not_cached.as_deref(),
    );
    // `(queued 31.0s: budget)` when the job waited for a reason.
    let line = match (&outcome.wait_reason, outcome.queue_wait_ms) {
        (Some(reason), Some(ms)) if ms > 0 && outcome.status != OutcomeStatus::Cached => {
            format!("{line} (queued {}: {reason})", dur(ms))
        }
        _ => line,
    };
    let failed_with_log = matches!(
        status,
        GateStatus::Fail | GateStatus::Timeout | GateStatus::Killed
    ) && outcome.log_path.is_some();
    let tail = if failed_with_log {
        text::gate_tail(outcome.log_path.as_deref(), res.tail_lines(), &outcome.tail)
    } else {
        Vec::new()
    };
    let result = GateResult {
        gate: res.gate_name.clone(),
        status,
        cached: outcome.status == OutcomeStatus::Cached,
        duration_ms: (outcome.status != OutcomeStatus::Cached).then_some(outcome.duration_ms),
        job_id: Some(job_id),
        log_path: outcome.log_path.clone(),
        exit_code: outcome.exit_code,
        coalesced: coalesced || outcome.coalesced_with.is_some(),
        not_cached_reason: not_cached,
        message: (status == GateStatus::Error || outcome.killed_for_memory())
            .then(|| outcome.message.clone()),
        killed_by: outcome
            .killed_by
            .clone()
            .filter(|_| outcome.killed_for_memory()),
        tail,
        queue_wait_ms: outcome.queue_wait_ms,
        wait_reason: outcome.wait_reason.clone(),
    };
    GateLine {
        result,
        line,
        early: false,
    }
}

/// Text of a `verify` result: one line per gate, then the tail of the
/// first failing log.
fn verify_text(lines: &[GateLine], cancelled: bool) -> String {
    let width = lines.iter().map(|g| g.result.gate.len()).max().unwrap_or(0);
    let mut out = String::new();
    if cancelled {
        out.push_str(
            "sluice: the verification was cancelled; the daemon subscription was dropped.\n",
        );
    }
    for g in lines {
        let _ = writeln!(out, "{:<width$}  {}", g.result.gate, g.line);
    }
    if let Some(g) = lines.iter().find(|g| !g.result.tail.is_empty()) {
        let path = g
            .result
            .log_path
            .as_deref()
            .unwrap_or(Path::new("-"))
            .display()
            .to_string();
        let _ = writeln!(out, "--- last {} lines of {path} ---", g.result.tail.len());
        for l in &g.result.tail {
            let _ = writeln!(out, "{l}");
        }
    }
    out
}

/// Job ids are `<digits>-<digits>-<hex>`; anything else must not reach a path.
fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ---------------------------------------------------------------- server

#[derive(Clone)]
struct Server {
    env: Arc<EnvMap>,
    /// The gates a repository without `.sluice.toml` gets from its hooks.
    hook_gates: HookGateSource,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").finish_non_exhaustive()
    }
}

#[tool_router]
impl Server {
    fn new(env: EnvMap, hook_gates: HookGateSource) -> Self {
        Self {
            env: Arc::new(env),
            hook_gates,
            tool_router: Self::tool_router(),
        }
    }

    /// Result of a cache hit; reports it to the daemon without waiting.
    fn hit_line(&self, repo: &RepoContext, gate: &str, marker: &Marker) -> GateLine {
        tracing::info!(gate, "cache hit");
        let env = Arc::clone(&self.env);
        let (repo_root, name, saved) = (
            repo.paths.toplevel.clone(),
            gate.to_owned(),
            marker.duration_ms,
        );
        // Best-effort telemetry, never awaited (a hit never starts a daemon).
        tokio::task::spawn_blocking(move || {
            if let Ok(socket) = dirs::socket_path(&env)
                && socket.exists()
            {
                client::send_hit_event(&socket, &repo_root, &name, saved);
            }
        });
        let mut result = GateResult::new(gate, GateStatus::Pass);
        result.cached = true;
        result.job_id = Some(marker.job_id.clone());
        GateLine {
            result,
            line: format!("PASS (cached, passed {} ago)", age(&marker.passed_at)),
            early: false,
        }
    }

    /// Runs one gate: cache hit, or a daemon job followed to the end. `early`
    /// is cancelled when a sibling gate of the same call failed first.
    #[allow(clippy::too_many_arguments, reason = "one gate of one verify call")]
    async fn verify_gate(
        &self,
        repo: &RepoContext,
        gate: &str,
        params: &VerifyParams,
        ctx: &RequestContext<RoleServer>,
        progress: Option<&Progress>,
        position: (usize, usize),
        early: &CancellationToken,
    ) -> Result<GateLine, McpError> {
        let ct = &ctx.ct;
        let res = repo.select(gate).map_err(|e| invalid(e.to_string()))?;
        let mode = cache_mode(&self.env);
        let extra = params.extra_args.clone().unwrap_or_default();
        let timeout_seconds = params.timeout_seconds;
        let Some(prepared) =
            blocking_or_cancel(ct, move || prepare(res, &mode, &extra, timeout_seconds)).await?
        else {
            return Ok(cancelled_line(gate, None));
        };
        let prepared = match prepared {
            Ok(p) => p,
            Err(e) => return Ok(error_line(gate, &e)),
        };
        let (request, res, off_reason) = match prepared {
            Prepared::Hit(marker) => return Ok(self.hit_line(repo, gate, &marker)),
            Prepared::Run {
                request,
                res,
                off_reason,
            } => (request, res, off_reason),
        };
        if early.is_cancelled() {
            return Ok(early_line(gate, None));
        }
        let timeout = params
            .timeout_seconds
            .map_or(res.gate.timeout, Duration::from_secs);

        let env = Arc::clone(&self.env);
        let Some(socket) = blocking_or_cancel(ct, move || client::ensure_daemon(&env)).await?
        else {
            return Ok(cancelled_line(gate, None));
        };
        let socket = match socket {
            Ok(s) => s,
            Err(e) => {
                return Ok(error_line(
                    gate,
                    &format!("the Sluice daemon is unavailable: {e:#}"),
                ));
            }
        };

        tracing::info!(gate, "running through the daemon");
        let state = Mutex::new(JobState::default());
        let run = daemon::run(&socket, request, ct, early, |msg| {
            if let Ok(mut s) = state.lock() {
                s.update(msg);
            }
        });
        tokio::pin!(run);
        let mut ticker = progress.map(|p| {
            tokio::time::interval_at(tokio::time::Instant::now() + p.interval, p.interval)
        });
        let end = loop {
            tokio::select! {
                end = &mut run => break end,
                _ = async { ticker.as_mut().expect("guarded").tick().await }, if ticker.is_some() => {
                    let message = state
                        .lock()
                        .map(|s| s.describe(gate))
                        .unwrap_or_default();
                    if let Some(p) = progress {
                        p.notify(format!("[{}/{}] {message}", position.0, position.1)).await;
                    }
                }
            }
        };
        match end {
            Ok(RunEnd::Done {
                job_id,
                outcome,
                coalesced,
            }) => Ok(outcome_line(
                &res,
                timeout,
                off_reason.as_deref(),
                (job_id, outcome, coalesced),
            )),
            Ok(RunEnd::Cancelled { job_id }) => Ok(cancelled_line(gate, job_id)),
            Ok(RunEnd::EarlyVerdict { job_id }) => Ok(early_line(gate, job_id)),
            Err(e @ (RunError::NotQueued(_) | RunError::Lost(_))) => {
                Ok(error_line(gate, &e.to_string()))
            }
        }
    }

    /// Every gate at once (ADR-0011 M4). With `fail_fast`, the first gate
    /// that fails, times out or errors cancels the pending ones (their
    /// daemon subscriptions are dropped with reason `early-verdict`).
    /// Returns the lines in `gates` order.
    async fn verify_parallel(
        &self,
        repo: &RepoContext,
        gates: &[String],
        params: &VerifyParams,
        ctx: &RequestContext<RoleServer>,
        progress: Option<&Progress>,
        fail_fast: bool,
    ) -> Result<Vec<GateLine>, McpError> {
        type GateFuture<'a> = std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<GateLine, McpError>> + Send + 'a>,
        >;
        let early = CancellationToken::new();
        let n = gates.len();
        let mut futures: Vec<GateFuture<'_>> = gates
            .iter()
            .enumerate()
            .map(|(i, gate)| {
                Box::pin(self.verify_gate(repo, gate, params, ctx, progress, (i + 1, n), &early))
                    as GateFuture<'_>
            })
            .collect();
        let mut lines: Vec<Option<GateLine>> = (0..n).map(|_| None).collect();
        let mut pending: Vec<usize> = (0..n).collect();
        let mut failed: Option<String> = None;
        while !pending.is_empty() {
            let (k, out) = std::future::poll_fn(|cx| {
                for (k, &i) in pending.iter().enumerate() {
                    if let std::task::Poll::Ready(out) = futures[i].as_mut().poll(cx) {
                        return std::task::Poll::Ready((k, out));
                    }
                }
                std::task::Poll::Pending
            })
            .await;
            let i = pending.remove(k);
            let line = out?;
            let failure = matches!(
                line.result.status,
                GateStatus::Fail | GateStatus::Timeout | GateStatus::Error | GateStatus::Killed
            );
            if fail_fast && failure && failed.is_none() {
                tracing::info!(gate = %line.result.gate, "early verdict: cancelling the pending gates");
                failed = Some(line.result.gate.clone());
                early.cancel();
            }
            lines[i] = Some(line);
        }
        drop(futures);
        let mut lines: Vec<GateLine> = lines.into_iter().flatten().collect();
        if let Some(failed) = failed {
            name_early_verdicts(&mut lines, &failed);
        }
        Ok(lines)
    }

    /// Verify the working tree: run the selected gates (cache hit in milliseconds,
    /// otherwise queued in the shared Sluice daemon) and block until they
    /// finished. The gates run concurrently; by default (`fail_fast`) the call
    /// returns at the first gate that fails and cancels the others. Call this
    /// before committing instead of running test, lint or type-check commands
    /// yourself. Sends a progress notification every 15 s (configurable) when
    /// the request carries a progressToken; without one, the client's idle
    /// timeout (30 minutes in Claude Code) is the effective limit. The result
    /// lists one line per gate and, on failure, the tail of the first failing
    /// log; `is_error` is set when any gate did not pass.
    #[tool(output_schema = schema_for_output::<VerifyReport>())]
    async fn verify(
        &self,
        Parameters(params): Parameters<VerifyParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let dir = workdir(&params.working_directory)?;
        let env = Arc::clone(&self.env);
        let mode = cache_mode(&env);
        let source = Arc::clone(&self.hook_gates);
        let Some(repo) = blocking_or_cancel(&ctx.ct, move || {
            lookup::resolve_repo_hook_gates(&dir, &env, &mode, &*source)
        })
        .await?
        else {
            let report = VerifyReport {
                repository: PathBuf::from(&params.working_directory),
                results: Vec::new(),
                all_passed: false,
                cancelled: true,
                wall_ms: None,
            };
            return result(verify_text(&[], true), &report, true);
        };
        let repo = repo.map_err(|e| invalid(e.to_string()))?;
        let gates = select_gates(&repo.config, params.gates.clone())?;
        let parallel = repo.user.mcp.verify_parallel;
        let fail_fast = params.fail_fast.unwrap_or(true);
        tracing::info!(repo = %repo.paths.toplevel.display(), ?gates, parallel, fail_fast, "verify");

        let progress = ctx.meta.get_progress_token().map(|token| Progress {
            peer: ctx.peer.clone(),
            token,
            interval: repo.user.mcp.progress_interval.max(Duration::from_secs(1)),
            started: Instant::now(),
        });
        let lines = if parallel && gates.len() > 1 {
            self.verify_parallel(&repo, &gates, &params, &ctx, progress.as_ref(), fail_fast)
                .await?
        } else {
            // Sequential (`[mcp] verify_parallel = false`): the pre-0.3.0
            // behaviour, every gate in order, stopping only on cancellation.
            let never = CancellationToken::new();
            let mut lines: Vec<GateLine> = Vec::with_capacity(gates.len());
            for (i, gate) in gates.iter().enumerate() {
                let line = self
                    .verify_gate(
                        &repo,
                        gate,
                        &params,
                        &ctx,
                        progress.as_ref(),
                        (i + 1, gates.len()),
                        &never,
                    )
                    .await?;
                let stop = line.result.status == GateStatus::Cancelled;
                lines.push(line);
                if stop {
                    break;
                }
            }
            lines
        };
        let cancelled = lines
            .iter()
            .any(|g| g.result.status == GateStatus::Cancelled && !g.early);
        let all_passed = !cancelled && lines.iter().all(|g| g.result.status == GateStatus::Pass);
        let mut text = verify_text(&lines, cancelled);
        let note_env = Arc::clone(&self.env);
        let note = blocking(move || intervention_note(&note_env, &logged_interventions(&note_env)))
            .await
            .ok()
            .flatten();
        if let Some(note) = note {
            let _ = write!(text, "\n{note}\n");
        }
        let report = VerifyReport {
            repository: repo.paths.toplevel.clone(),
            results: lines.into_iter().map(|g| g.result).collect(),
            all_passed,
            cancelled,
            wall_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
        };
        result(text, &report, !all_passed)
    }

    /// Show the Sluice daemon: budget, running and queued jobs, totals (same
    /// schema as `sluice status --json`; `daemon` is null when none runs). With
    /// `working_directory`, also the cache state of that tree per gate.
    #[tool]
    async fn status(
        &self,
        Parameters(params): Parameters<StatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir = params
            .working_directory
            .as_deref()
            .map(workdir)
            .transpose()?;
        let env = Arc::clone(&self.env);
        let (report, cache) = blocking(move || {
            let report = match dirs::socket_path(&env)
                .map_err(|e| ClientError::Other(e.into()))
                .and_then(|s| client::connect(&s))
                .and_then(client::Client::status)
            {
                Ok(r) => Ok(Some(r)),
                Err(ClientError::NotRunning(_)) => Ok(None),
                Err(e) => Err(e.to_string()),
            };
            let cache = dir.map(|d| cache_state(&d, &env));
            (report, cache)
        })
        .await?;
        let report = report.map_err(|e| internal(format!("cannot query the daemon: {e}")))?;
        let cache = cache.transpose().map_err(invalid)?;
        let this_repo = cache.as_ref().map(|c| c.0.clone());

        let mut out = match &report {
            Some(r) => text::render_status(r, this_repo.as_deref()),
            None => "sluice: no daemon running\n".to_owned(),
        };
        if let Some(r) = &report {
            let mut events: Vec<sluice_gate_core::cgroup::explain::Event> = r
                .interventions
                .iter()
                .flatten()
                .map(|i| sluice_gate_core::cgroup::explain::Event {
                    at: i.at.clone(),
                    session: i.session.clone(),
                    leaf: i.leaf.clone(),
                    cause: i.cause.clone(),
                    detail: i.detail.clone(),
                })
                .collect();
            let note_env = Arc::clone(&self.env);
            // The daemon's recent rows and the log file it keeps (the same
            // interventions; each is told once by its identity).
            if let Some(note) = blocking(move || {
                events.extend(logged_interventions(&note_env));
                intervention_note(&note_env, &events)
            })
            .await
            .ok()
            .flatten()
            {
                let _ = write!(out, "\n{note}\n");
            }
        }
        let mut value = match &report {
            Some(r) => serde_json::to_value(r).map_err(|e| internal(e.to_string()))?,
            None => serde_json::json!({"schema": STATUS_SCHEMA_VERSION, "daemon": null}),
        };
        if let Some((root, text, json)) = cache {
            let _ = write!(out, "\ncache of {}:\n{text}", root.display());
            if let Some(obj) = value.as_object_mut() {
                obj.insert("cache".to_owned(), json);
            }
        }
        let mut r = CallToolResult::success(vec![ContentBlock::text(out)]);
        r.structured_content = Some(value);
        Ok(r)
    }

    /// List the gates of the repository: those of its .sluice.toml (name,
    /// command, weight, timeout, hooks and cache flag) or, in a repository
    /// without one, the check units of its git hooks, followed by the hooks
    /// and units that are not gates and why.
    #[tool(output_schema = schema_for_output::<GateList>())]
    async fn gate_list(
        &self,
        Parameters(params): Parameters<GateListParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir = workdir(&params.working_directory)?;
        let env = Arc::clone(&self.env);
        let source = Arc::clone(&self.hook_gates);
        let listing = blocking(move || {
            let paths = git::discover(&dir, &env).map_err(|e| e.to_string())?;
            let (config, managers, excluded) = load_gates(&paths, &env, &*source)?;
            let budget = UserConfig::load(&env).map_or_else(
                |_| sluice_gate_core::cpus::default_budget(),
                |u| u.daemon.budget,
            );
            Ok::<_, String>((paths.toplevel, config, budget, managers, excluded))
        })
        .await?
        .map_err(invalid)?;
        let (root, config, budget, managers, excluded) = listing;
        let gates: Vec<GateInfo> = config
            .gates
            .iter()
            .map(|(name, g)| GateInfo {
                name: name.clone(),
                cmd: g.cmd.trim().to_owned(),
                weight: g.weight.ceiling(budget),
                timeout_seconds: g.timeout.as_secs(),
                hooks: g.hooks.clone(),
                cache: g.cache,
                source: if managers.contains_key(name) {
                    "hook"
                } else {
                    "config"
                },
                manager: managers.get(name).cloned(),
            })
            .collect();
        let out = gate_list_text(&gates, &config, &excluded);
        result(
            out,
            &GateList {
                repository: root,
                gates,
                excluded: excluded.into_iter().map(ExcludedInfo::from).collect(),
            },
            false,
        )
    }

    /// Show the last lines of a job log (default: the newest job). Output is
    /// capped at 64 KiB; the full log stays on disk at the returned path.
    #[tool(output_schema = schema_for_output::<LogsReport>())]
    async fn logs(
        &self,
        Parameters(params): Parameters<LogsParams>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(id) = &params.job_id
            && !valid_job_id(id)
        {
            return Err(invalid(format!("invalid job_id '{id}'")));
        }
        let tail = params
            .tail
            .unwrap_or(LOGS_DEFAULT_TAIL)
            .clamp(1, LOGS_MAX_TAIL);
        let env = Arc::clone(&self.env);
        let job = params.job_id.clone();
        let found = blocking(move || find_log(&env, job.as_deref(), tail)).await?;
        let (path, lines) = match found {
            Ok(v) => v,
            Err(why) => return Ok(CallToolResult::error(vec![ContentBlock::text(why)])),
        };
        let start = lines.len().saturating_sub(tail);
        let (lines, omitted) = text::cap_lines(&lines[start..], LOGS_MAX_BYTES);
        let job_id = path
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let mut out = format!("# {}\n", path.display());
        if omitted > 0 {
            let _ = writeln!(out, "… omitted {omitted} bytes");
        }
        for l in &lines {
            let _ = writeln!(out, "{l}");
        }
        result(
            out,
            &LogsReport {
                job_id,
                path,
                lines,
                omitted_bytes: omitted,
            },
            false,
        )
    }

    /// Put a committed branch in the local merge queue (default: the current
    /// branch of the worktree). Call it only after `verify` passed on the
    /// committed branch; refuses a detached HEAD and tracked uncommitted
    /// changes. Returns the job; then stop and tell the human, who approves the
    /// train with `sluice queue approve`. After a conflict or a failure, fix
    /// the branch and call `queue_retry` with the job id (the job keeps the
    /// branch). Errors start with their code (`duplicate-branch`,
    /// `not-supported`, ...).
    #[tool(output_schema = schema_for_output::<JobSchema>())]
    async fn queue_enqueue(
        &self,
        Parameters(params): Parameters<QueueEnqueueParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir = workdir(&params.working_directory)?;
        let env = Arc::clone(&self.env);
        let reply = blocking(move || {
            let request = queue::enqueue_facts(&env, &dir, params.branch.as_deref(), params.base)?;
            let reply = queue::request(&env, &ClientMessage::QueueEnqueue { id: 1, request })?;
            Ok::<_, String>((reply, dir))
        })
        .await?;
        match reply {
            Ok((ServerMessage::QueueJob { job, .. }, dir)) => {
                // The job is queued; the probe only warns (ADR-0025).
                let env = Arc::clone(&self.env);
                let probed = job.clone();
                let warnings =
                    blocking(move || queue::enqueue_warnings(&env, &dir, &probed)).await?;
                result(
                    queue::enqueue_text(&job, &warnings),
                    &queue::enqueue_value(&job, &warnings),
                    false,
                )
            }
            Ok((other, _)) => Ok(tool_error(queue::failure(
                "internal",
                format!("unexpected reply to queue-enqueue: {other:?}"),
            ))),
            Err(why) => Ok(tool_error(why)),
        }
    }

    /// Re-queue a merge-queue job that ended `conflict` or `failed`, with the
    /// current head of its branch (re-read in the repository, like `sluice
    /// queue retry`). Rebase or fix the branch, commit and `verify` first.
    /// Errors start with their code (`conflict-state` for a job in another
    /// state, `not-found`, ...).
    #[tool(output_schema = schema_for_output::<JobSchema>())]
    async fn queue_retry(
        &self,
        Parameters(params): Parameters<QueueJobParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir = workdir(&params.working_directory)?;
        if !valid_job_id(&params.job_id) {
            return Err(invalid(format!("invalid job_id '{}'", params.job_id)));
        }
        let env = Arc::clone(&self.env);
        let reply = blocking(move || queue::send_retry(&env, &dir, &params.job_id)).await?;
        match reply {
            Ok(job) => result(queue::retry_text(&job), &job, false),
            Err(why) => Ok(tool_error(why)),
        }
    }

    /// Cancel a merge-queue job of this repository. A job that is pushing or
    /// waiting for reconcile only records the request (it is cancelled once
    /// nothing landed is proven). Errors start with their code.
    #[tool(output_schema = schema_for_output::<JobSchema>())]
    async fn queue_cancel(
        &self,
        Parameters(params): Parameters<QueueJobParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir = workdir(&params.working_directory)?;
        if !valid_job_id(&params.job_id) {
            return Err(invalid(format!("invalid job_id '{}'", params.job_id)));
        }
        let env = Arc::clone(&self.env);
        let reply = blocking(move || queue::send_cancel(&env, &dir, &params.job_id)).await?;
        match reply {
            Ok(job) => result(queue::cancel_text(&job), &job, false),
            Err(why) => Ok(tool_error(why)),
        }
    }

    /// Show the merge queue of the repository: trains, their members and
    /// state, conflicts with paths, and which trains are ready for a human to
    /// approve. The structured content is the `queue-status` payload
    /// (`sluice queue status --json`, `schema: 1`).
    #[tool(output_schema = schema_for_output::<StatusSchema>())]
    async fn queue_status(
        &self,
        Parameters(params): Parameters<QueueStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir = workdir(&params.working_directory)?;
        let env = Arc::clone(&self.env);
        let report = blocking(move || {
            let paths = queue::discover(&env, &dir)?;
            let common = paths.common_dir;
            let report = queue::fetch_status(&env, Some(common.clone()))?;
            // Trains waiting for a pull request: read it (read-only) and
            // report it, then show what the daemon decided.
            if queue::refresh_pull_requests(&env, &report) {
                return queue::fetch_status(&env, Some(common));
            }
            Ok(report)
        })
        .await?;
        match report {
            Ok(report) => result(queue::render_status(&report), &report, false),
            Err(why) => Ok(tool_error(why)),
        }
    }

    /// Approve a ready train (its members land on the target). Humans normally
    /// approve with `sluice queue approve`; this tool is refused with
    /// `not-allowed` unless the user enabled `[queue] allow_agent_approval =
    /// true` in the user config. Do not call it unless the user told you to.
    /// When allowed, it waits until the train landed, failed or needs a human
    /// to reconcile (up to the base's `push_timeout` plus its
    /// `network_timeout` and a margin), sending progress notifications when
    /// the request carries a progressToken.
    #[tool(output_schema = schema_for_output::<ApproveReport>())]
    async fn queue_approve(
        &self,
        Parameters(params): Parameters<QueueApproveParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let dir = workdir(&params.working_directory)?;
        if !valid_job_id(&params.train_id) {
            return Err(invalid(format!("invalid train_id '{}'", params.train_id)));
        }
        let env = Arc::clone(&self.env);
        let user = blocking(move || UserConfig::load(&env)).await?;
        let user = match user {
            Ok(u) if u.queue.allow_agent_approval => u,
            Ok(_) => return Ok(tool_error(queue::NOT_ALLOWED_TEXT.to_owned())),
            Err(e) => {
                return Ok(tool_error(queue::failure(
                    "not-allowed",
                    format!("cannot read the user config ({e}); approval stays refused"),
                )));
            }
        };
        self.approve(&dir, &params.train_id, &user, &ctx).await
    }
}

/// How long `queue_approve` waits for the landing: the base's
/// `push_timeout` plus one `network_timeout` (pre-push checks), two runner
/// ticks and a 30 s margin (defaults 10 min and 60 s for a daemon that does
/// not report them), like `sluice queue approve`.
fn approve_wait(train: &QueueTrainView, tick: Duration) -> Duration {
    let push = Duration::from_millis(train.push_timeout_ms.unwrap_or(600_000));
    let network = Duration::from_millis(train.network_timeout_ms.unwrap_or(60_000));
    push + network + tick * 2 + Duration::from_secs(30)
}
/// Poll period of `queue_approve`.
const APPROVE_POLL: Duration = Duration::from_secs(1);

/// A tool error result with only a text block.
fn tool_error(text: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}

impl Server {
    /// The body of `queue_approve` once agent approval is known to be allowed.
    async fn approve(
        &self,
        dir: &Path,
        train_id: &str,
        user: &UserConfig,
        ctx: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let ct = &ctx.ct;
        let (env, dir, id) = (
            Arc::clone(&self.env),
            dir.to_path_buf(),
            train_id.to_owned(),
        );
        let prepared = blocking(move || queue::send_approval(&env, &dir, &id)).await?;
        let (common_dir, mut train) = match prepared {
            Ok(v) => v,
            Err(why) => return Ok(tool_error(why)),
        };

        let progress = ctx.meta.get_progress_token().map(|token| Progress {
            peer: ctx.peer.clone(),
            token,
            interval: user.mcp.progress_interval.max(Duration::from_secs(1)),
            started: Instant::now(),
        });
        let started = Instant::now();
        let mut last_progress = Instant::now();
        let mut timed_out = false;
        let wait = approve_wait(&train, user.queue.tick);
        while matches!(
            train.state,
            QueueTrainState::Approved | QueueTrainState::Pushing | QueueTrainState::Ready
        ) {
            if started.elapsed() >= wait {
                timed_out = true;
                break;
            }
            tokio::select! {
                () = ct.cancelled() => {
                    let text = format!(
                        "cancelled: stopped waiting; train {} stays {} and the daemon continues\n",
                        train.train_id,
                        queue::train_state_name(train.state)
                    );
                    return Ok(tool_error(text));
                }
                () = tokio::time::sleep(APPROVE_POLL) => {}
            }
            let (env, dir) = (Arc::clone(&self.env), common_dir.clone());
            let status = blocking(move || queue::fetch_status(&env, Some(dir))).await?;
            let status = match status {
                Ok(s) => s,
                Err(why) => return Ok(tool_error(why)),
            };
            match status
                .trains
                .into_iter()
                .find(|t| t.train_id == train.train_id)
            {
                Some(t) => train = t,
                None => train.state = QueueTrainState::Dissolved,
            }
            if let Some(p) = &progress
                && last_progress.elapsed() >= p.interval
            {
                last_progress = Instant::now();
                p.notify(format!(
                    "train {}: {} for {}",
                    train.train_id,
                    queue::train_state_name(train.state),
                    dur(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
                ))
                .await;
            }
        }
        let landed = train.state == QueueTrainState::Landed;
        let text = queue::approve_text(&train, timed_out);
        let view: TrainSchema = serde_json::from_value(
            serde_json::to_value(&train).map_err(|e| internal(e.to_string()))?,
        )
        .map_err(|e| internal(e.to_string()))?;
        let report = ApproveReport {
            train_id: train.train_id.clone(),
            state: queue::train_state_name(train.state).to_owned(),
            landed,
            train: view,
        };
        result(text, &report, !landed)
    }
}

#[allow(clippy::unused_async_trait_impl)] // generated by `#[tool_handler]`
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("sluice", SLUICE_VERSION))
            .with_instructions(INSTRUCTIONS)
    }
}

/// A repository's gates, the manager of each gate derived from a hook (by
/// gate name) and the hook units left out.
type LoadedGates = (RepoConfig, BTreeMap<String, String>, Vec<HookExclusion>);

/// The gates of a repository: `.sluice.toml`'s, or, when there is none, those
/// of its hooks (with the manager of each and what was left out). A
/// repository with neither is an error.
fn load_gates(
    paths: &git::RepoPaths,
    env: &EnvMap,
    source: &(dyn Fn(&git::RepoPaths, &EnvMap) -> HookGates + Send + Sync),
) -> Result<LoadedGates, String> {
    match RepoConfig::load(&paths.toplevel) {
        Ok(config) => Ok((config, BTreeMap::new(), Vec::new())),
        Err(sluice_gate_core::Error::ConfigNotFound(root)) => {
            let derived = source(paths, env);
            if derived.gates.is_empty() && derived.excluded.is_empty() {
                return Err(sluice_gate_core::Error::ConfigNotFound(root).to_string());
            }
            let managers = derived
                .gates
                .iter()
                .map(|g| (g.name.clone(), g.manager.clone()))
                .collect();
            let excluded = derived.excluded.clone();
            Ok((lookup::hook_config(derived), managers, excluded))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The text of `gate_list`.
fn gate_list_text(gates: &[GateInfo], config: &RepoConfig, excluded: &[HookExclusion]) -> String {
    let width = gates.iter().map(|g| g.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    if gates.is_empty() && excluded.is_empty() {
        out.push_str("no gates defined in .sluice.toml\n");
    }
    if gates.is_empty() && !excluded.is_empty() {
        out.push_str("no .sluice.toml and no hook unit that can be verified\n");
    }
    for (g, (_, gate)) in gates.iter().zip(&config.gates) {
        let origin = g
            .manager
            .as_ref()
            .map_or(String::new(), |m| format!(", from the {m} hook"));
        let _ = writeln!(
            out,
            "{:<width$}  {}  (weight {}, timeout {}, hooks: {}, cache {}{origin})",
            g.name,
            gate.normalized_cmd(),
            gate.weight,
            humantime::format_duration(gate.timeout),
            if g.hooks.is_empty() {
                "-".to_owned()
            } else {
                g.hooks.join(", ")
            },
            if g.cache { "on" } else { "off" }
        );
    }
    if !excluded.is_empty() {
        out.push_str("not gates:\n");
    }
    for e in excluded {
        let what = match (&e.unit, &e.command) {
            (Some(unit), Some(cmd)) => format!("{unit} `{cmd}`"),
            _ => format!("hook {}", e.hook),
        };
        let _ = writeln!(out, "  {what}: {}", e.reason);
    }
    out
}

/// Cache state of the directory's current tree per gate (`status`):
/// repository root, text lines and JSON.
fn cache_state(dir: &Path, env: &EnvMap) -> Result<(PathBuf, String, serde_json::Value), String> {
    let repo = lookup::resolve_repo(dir, env, &CacheMode::Enabled).map_err(|e| e.to_string())?;
    let mut out = String::new();
    let mut gates = Vec::new();
    let (tree, unknown) = match tree::snapshot_entries(&repo.paths, &repo.probe_env, &repo.scratch)
    {
        Ok(SnapshotFull::Known { root, entries }) => (Some((root, entries)), None),
        Ok(SnapshotFull::Unknown { reason }) => (None, Some(reason)),
        Err(e) => (None, Some(e.to_string())),
    };
    match (&tree, &unknown) {
        (Some((t, _)), _) => {
            let _ = writeln!(out, "tree:    {t}");
        }
        (None, Some(why)) => {
            let _ = writeln!(out, "tree:    unknown ({why}); every gate runs uncached");
        }
        (None, None) => {}
    }
    if let Some((tree, entries)) = &tree {
        for (name, gate) in &repo.config.gates {
            let mut g = GateCache {
                gate: name.clone(),
                state: "uncached",
                passed_at: None,
                job_id: None,
                last_run: None,
                message: None,
                scope: None,
            };
            let line = if gate.cache {
                let key_tree = lookup::key_tree(gate, tree, entries);
                match repo
                    .select(name)
                    .and_then(|r| lookup::derive_key(&r, key_tree, &[]))
                    .and_then(|(_, key)| repo.store.lookup(&key, repo.ttl))
                {
                    Ok(marker::Lookup::Hit(m)) => {
                        g.state = "hit";
                        let line =
                            format!("HIT  (passed {} ago, job {})", age(&m.passed_at), m.job_id);
                        g.passed_at = Some(m.passed_at);
                        g.job_id = Some(m.job_id);
                        line
                    }
                    Ok(marker::Lookup::Miss(last)) => {
                        g.state = "miss";
                        match last {
                            Some(m) => {
                                let line = format!(
                                    "MISS (last run {} ago: {}{})",
                                    age(&m.recorded_at),
                                    m.reason,
                                    m.exit_code.map_or(String::new(), |c| format!(", exit {c}"))
                                );
                                g.last_run = Some(serde_json::json!({
                                    "recorded_at": m.recorded_at,
                                    "reason": m.reason,
                                    "exit_code": m.exit_code,
                                    "job_id": m.job_id,
                                    "log_path": m.log_path,
                                }));
                                line
                            }
                            None => "MISS".to_owned(),
                        }
                    }
                    Err(e) => {
                        g.state = "error";
                        g.message = Some(e.to_string());
                        format!("ERROR ({e})")
                    }
                }
            } else {
                "always runs (cache = false)".to_owned()
            };
            let summary = lookup::scope_summary(gate, entries).filter(|_| gate.cache);
            let scope = summary.as_ref().map(lookup::ScopeSummary::status_suffix);
            g.scope = summary.map(|s| {
                serde_json::json!({ "files": s.count.files, "total": s.count.total, "hash": s.hash.as_str() })
            });
            let _ = writeln!(out, "{name:<16} {line}{}", scope.unwrap_or_default());
            gates.push(g);
        }
    }
    let json = serde_json::json!({
        "repository": repo.paths.toplevel,
        "tree": tree.as_ref().map(|(t, _)| t.as_str().to_owned()),
        "tree_unknown": unknown,
        "gates": gates,
    });
    Ok((repo.paths.toplevel, out, json))
}

/// Finds a job log: the daemon first (never started for this), then the
/// log directory. Returns the path and its lines (or the daemon's tail when
/// the file cannot be read).
fn find_log(
    env: &EnvMap,
    job_id: Option<&str>,
    tail: usize,
) -> Result<(PathBuf, Vec<String>), String> {
    let from_daemon = dirs::socket_path(env)
        .ok()
        .filter(|s| s.exists())
        .and_then(|s| client::connect(&s).ok())
        .and_then(|c| c.logs(job_id, tail).ok());
    if let Some((path, lines)) = from_daemon {
        return Ok(match text::read_lines(&path) {
            Ok(all) => (path, all),
            Err(_) => (path, lines),
        });
    }
    let dir = dirs::logs_dir(env);
    let path = match job_id {
        Some(id) => dir.join(format!("{id}.log")),
        None => std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(sluice_gate_core::logfile::is_job_log_name)
            })
            .map(|e| e.path())
            .max()
            .ok_or_else(|| format!("no job logs in {}", dir.display()))?,
    };
    let lines =
        text::read_lines(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    Ok((path, lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_and_error_lines_fail_the_report() {
        let c = cancelled_line("g", Some("j1".into()));
        assert_eq!(c.result.status, GateStatus::Cancelled);
        assert!(verify_text(&[c], true).contains("was cancelled"));
        let e = error_line("g", "the Sluice daemon is unavailable");
        assert_eq!(e.result.status, GateStatus::Error);
        assert!(e.result.message.is_some() && e.line.starts_with("ERROR"));
    }

    /// ADR-0024: the progress message of a running job says when it looks
    /// stalled, and stops saying it when the job is active again.
    #[test]
    fn progress_says_when_the_running_job_looks_stalled() {
        let progress = |stalled_ms| ServerMessage::Progress {
            id: 1,
            job_id: "j1".into(),
            elapsed_ms: 400_000,
            log_bytes: 9,
            position: None,
            stalled_ms,
        };
        let mut state = JobState::default();
        state.update(&ServerMessage::Started {
            id: 1,
            job_id: "j1".into(),
            started_at: "t".into(),
            spool: None,
        });
        state.update(&progress(None));
        assert!(!state.describe("test").contains("STALLED"));
        state.update(&progress(Some(300_000)));
        let line = state.describe("test");
        assert!(
            line.contains(
                "running for 6m40s (job j1); looks STALLED: no CPU, I/O or output for 5m0"
            ),
            "{line}"
        );
        state.update(&progress(None));
        assert!(!state.describe("test").contains("STALLED"));
    }

    #[test]
    fn job_ids_are_checked() {
        assert!(valid_job_id("1761234-42-abc123"));
        assert!(!valid_job_id("../etc/passwd"));
        assert!(!valid_job_id(""));
    }

    #[test]
    fn gate_selection_prefers_pre_commit() {
        let config = RepoConfig::parse(
            "version = 1\n[gates.a]\ncmd = \"true\"\n[gates.b]\ncmd = \"true\"\nhooks = [\"pre-commit\"]\n",
            Path::new("/r/.sluice.toml"),
        )
        .unwrap();
        assert_eq!(select_gates(&config, None).unwrap(), vec!["b"]);
        assert_eq!(
            select_gates(&config, Some(vec!["a".into(), "b".into()])).unwrap(),
            vec!["a", "b"]
        );
        assert!(select_gates(&config, Some(vec!["zz".into()])).is_err());
        let untagged = RepoConfig::parse(
            "version = 1\n[gates.a]\ncmd = \"true\"\n",
            Path::new("/r/x"),
        )
        .unwrap();
        assert_eq!(select_gates(&untagged, Some(vec![])).unwrap(), vec!["a"]);
    }

    #[test]
    fn verify_text_shows_the_first_failing_tail() {
        let mut fail = GateResult::new("e2e", GateStatus::Fail);
        fail.log_path = Some(PathBuf::from("/l/j.log"));
        fail.tail = vec!["boom".into()];
        let lines = vec![
            GateLine {
                result: GateResult::new("test", GateStatus::Pass),
                early: false,
                line: "PASS 1.0s (job j)".into(),
            },
            GateLine {
                result: fail,
                early: false,
                line: "FAIL exit=1 0.1s (job j) log=/l/j.log".into(),
            },
        ];
        let text = verify_text(&lines, false);
        assert_eq!(
            text,
            "test  PASS 1.0s (job j)\ne2e   FAIL exit=1 0.1s (job j) log=/l/j.log\n\
             --- last 1 lines of /l/j.log ---\nboom\n"
        );
    }
}

#[cfg(test)]
mod schema_tests {
    //! The structured content of every tool must validate against the
    //! tool's own output schema, which Claude Code enforces: a field that is
    //! skipped when empty must not be listed as required.

    use super::*;

    /// Definitions whose fields share a name with a field skipped elsewhere
    /// but are always present (`ExcludedInfo::reason` is never empty).
    const ALWAYS_PRESENT: &[&str] = &["ExcludedInfo"];

    fn required_names(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::Array(req)) = map.get("required") {
                    out.extend(req.iter().filter_map(|v| v.as_str().map(str::to_owned)));
                }
                for (k, v) in map {
                    if !ALWAYS_PRESENT.contains(&k.as_str()) {
                        required_names(v, out);
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| required_names(v, out)),
            _ => {}
        }
    }

    #[test]
    fn skipped_fields_are_never_required() {
        let mut required = Vec::new();
        for schema in [
            schema_for_output::<VerifyReport>(),
            schema_for_output::<GateList>(),
            schema_for_output::<LogsReport>(),
            schema_for_output::<JobSchema>(),
            schema_for_output::<StatusSchema>(),
            schema_for_output::<ApproveReport>(),
        ] {
            required_names(
                &serde_json::to_value(schema).expect("schema json"),
                &mut required,
            );
        }
        for skipped in [
            "tail",
            "cancelled",
            "duration_ms",
            "not_cached_reason",
            "message",
            "merge_sha",
            "train_hash",
            "approved_via",
            "approved_at",
            "landed_at",
            "finished_at",
            "note",
            "reason",
            "detail",
            "position",
            "session",
            "push_timeout_ms",
            "network_timeout_ms",
            "prepare",
        ] {
            assert!(
                !required.iter().any(|r| r == skipped),
                "{skipped} is skipped when empty but marked required"
            );
        }
        let pass = GateResult::new("test", GateStatus::Pass);
        let json = serde_json::to_value(&pass).expect("json");
        assert!(
            json.get("tail").is_none(),
            "empty tail must be omitted: {json}"
        );
    }

    #[test]
    fn queue_tools_are_listed_with_descriptions_and_output_schemas() {
        let tools = Server::new(EnvMap::new(), Arc::new(|_, _| HookGates::default()))
            .tool_router
            .list_all();
        for name in [
            "queue_enqueue",
            "queue_status",
            "queue_approve",
            "queue_retry",
            "queue_cancel",
        ] {
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} missing"));
            assert!(
                tool.description.as_deref().is_some_and(|d| !d.is_empty()),
                "{name} has no description"
            );
            assert!(tool.output_schema.is_some(), "{name} has no output schema");
        }
    }

    #[test]
    fn instructions_carry_the_queue_guidance() {
        for needle in [
            "queue_enqueue",
            "and stop",
            "Never approve",
            "conflict",
            "queue_retry",
            "queue_cancel",
            "duplicate-branch",
            "prepare_failed",
            "gate_modified_tree",
            "--trust-member-config",
            "KILLED for lack of memory",
            "waiting for memory",
            "STALLED",
            "probably hung",
            "awaiting_merge",
            "never merge, close or push to the pull request",
            "`pr_closed`",
        ] {
            assert!(INSTRUCTIONS.contains(needle), "missing '{needle}'");
        }
        assert!(!INSTRUCTIONS.contains("only in the CLI"));
    }

    #[test]
    fn approve_wait_follows_the_base_timeouts() {
        let t = QueueTrainView {
            push_timeout_ms: Some(120_000),
            network_timeout_ms: Some(2_000),
            ..QueueTrainView::default()
        };
        assert_eq!(
            approve_wait(&t, Duration::from_secs(5)),
            Duration::from_secs(120 + 2 + 10 + 30)
        );
        assert_eq!(
            approve_wait(&QueueTrainView::default(), Duration::from_secs(5)),
            Duration::from_secs(600 + 60 + 10 + 30)
        );
    }
}
