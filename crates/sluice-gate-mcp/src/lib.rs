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
mod text;

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
use sluice_gate_core::config::RepoConfig;
use sluice_gate_core::git::{self, EnvMap};
use sluice_gate_core::lookup::{self, Lookup, RepoContext, Resolved, RunMaterial};
use sluice_gate_core::marker::{self, Marker};
use sluice_gate_core::mode::{CacheMode, cache_mode};
use sluice_gate_core::tree::{self, Snapshot};
use sluice_gate_core::{SLUICE_VERSION, dirs};
use sluice_gate_proto::client::{self, ClientError};
use sluice_gate_proto::{
    self as proto, GateSpec, OutcomeStatus, RepoInfo, RunRequest, STATUS_SCHEMA_VERSION,
    ServerMessage,
};
use tokio_util::sync::CancellationToken;

use crate::daemon::{RunEnd, RunError};
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
jobs, `logs` shows a job's output, `gate_list` lists the gates of a repository.";

/// Runs the MCP server on stdin/stdout until the client disconnects.
///
/// `env` is the process environment (it locates the daemon socket, the user
/// config and the state directory, and seeds the gate environment).
///
/// # Errors
///
/// When the transport fails to initialise or breaks.
pub async fn serve(env: EnvMap) -> anyhow::Result<()> {
    init_tracing(&env);
    tracing::info!(version = SLUICE_VERSION, "sluice mcp starting");
    let service = Server::new(env).serve(rmcp::transport::stdio()).await?;
    let reason = service.waiting().await?;
    tracing::info!(?reason, "sluice mcp stopped");
    Ok(())
}

fn init_tracing(env: &EnvMap) {
    let filter = env
        .get("SLUICE_LOG")
        .filter(|v| !v.is_empty())
        .and_then(|v| tracing_subscriber::EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
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
    /// Human-readable detail (errors, cancellation).
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Last lines of the log of a failing gate (absent when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tail: Vec<String>,
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
            tail: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
struct VerifyReport {
    /// Repository root.
    repository: PathBuf,
    /// One entry per gate, in run order.
    results: Vec<GateResult>,
    /// True when every gate passed.
    all_passed: bool,
    /// True when the call was cancelled before every gate finished.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    cancelled: bool,
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
}

#[derive(Debug, Serialize, JsonSchema)]
struct GateList {
    /// Repository root.
    repository: PathBuf,
    /// Gates of `.sluice.toml`, by name.
    gates: Vec<GateInfo>,
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
            weight: m.gate.weight,
            timeout_ms: m.timeout_ms,
            env: m.gate.env,
            cache: m.gate.cache,
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
    if let Lookup::Hit(marker) = state {
        return Ok(Prepared::Hit(marker));
    }
    let off_reason = match &state {
        Lookup::Off { reason, .. } => Some(reason.clone()),
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
                ..
            } => {
                if self.running {
                    self.elapsed = Some((*elapsed_ms, Instant::now()));
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
            format!("{gate}: running for {}{job}", dur(ms))
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
}

fn cancelled_line(gate: &str, job_id: Option<String>) -> GateLine {
    let line = job_id.as_deref().map_or_else(
        || "CANCELLED".to_owned(),
        |j| format!("CANCELLED (job {j})"),
    );
    let mut result = GateResult::new(gate, GateStatus::Cancelled);
    result.job_id = job_id;
    result.message = Some("the verification was cancelled".to_owned());
    GateLine { result, line }
}

/// A Sluice-side failure of one gate (daemon unavailable, request refused,
/// connection lost): that gate is an `error`, the remaining ones still run.
fn error_line(gate: &str, why: &str) -> GateLine {
    let mut result = GateResult::new(gate, GateStatus::Error);
    result.message = Some(why.to_owned());
    GateLine {
        result,
        line: format!("ERROR ({why})"),
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
        OutcomeStatus::Error => GateStatus::Error,
    };
    let not_cached = off_reason
        .map(str::to_owned)
        .or_else(|| outcome.not_cached_reason.clone())
        .filter(|_| outcome.status == OutcomeStatus::Pass);
    let line = match outcome.status {
        OutcomeStatus::Cached => "PASS (cached)".to_owned(),
        OutcomeStatus::Pass => format!(
            "PASS {secs:.1}s (job {job_id}){}",
            not_cached
                .as_deref()
                .map_or_else(String::new, |r| format!(" (not cached: {r})"))
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
        OutcomeStatus::Killed if outcome.log_path.is_none() => {
            format!("KILLED before it started (job {job_id}): the daemon stopped")
        }
        OutcomeStatus::Killed => format!("KILLED after {secs:.1}s (job {job_id}) log={log}"),
        OutcomeStatus::Error => format!("ERROR {}", outcome.message),
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
        message: (status == GateStatus::Error).then(|| outcome.message.clone()),
        tail,
    };
    GateLine { result, line }
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
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").finish_non_exhaustive()
    }
}

#[tool_router]
impl Server {
    fn new(env: EnvMap) -> Self {
        Self {
            env: Arc::new(env),
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
        }
    }

    /// Runs one gate: cache hit, or a daemon job followed to the end.
    async fn verify_gate(
        &self,
        repo: &RepoContext,
        gate: &str,
        params: &VerifyParams,
        ctx: &RequestContext<RoleServer>,
        progress: Option<&Progress>,
        position: (usize, usize),
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
        let run = daemon::run(&socket, request, ct, |msg| {
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
            Err(e @ (RunError::NotQueued(_) | RunError::Lost(_))) => {
                Ok(error_line(gate, &e.to_string()))
            }
        }
    }

    /// Verify the working tree: run the selected gates (cache hit in milliseconds,
    /// otherwise queued in the shared Sluice daemon) and block until every gate
    /// finished. Gates run sequentially. Call this before committing instead of
    /// running test, lint or type-check commands yourself. Sends a progress
    /// notification every 15 s (configurable) when the request carries a
    /// progressToken; without one, the client's idle timeout (30 minutes in
    /// Claude Code) is the effective limit. The result lists one line per gate
    /// and, on failure, the tail of the first failing log; `is_error` is set
    /// when any gate did not pass.
    #[tool(output_schema = schema_for_output::<VerifyReport>())]
    async fn verify(
        &self,
        Parameters(params): Parameters<VerifyParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let dir = workdir(&params.working_directory)?;
        let env = Arc::clone(&self.env);
        let mode = cache_mode(&env);
        let Some(repo) =
            blocking_or_cancel(&ctx.ct, move || lookup::resolve_repo(&dir, &env, &mode)).await?
        else {
            let report = VerifyReport {
                repository: PathBuf::from(&params.working_directory),
                results: Vec::new(),
                all_passed: false,
                cancelled: true,
            };
            return result(verify_text(&[], true), &report, true);
        };
        let repo = repo.map_err(|e| invalid(e.to_string()))?;
        let gates = select_gates(&repo.config, params.gates.clone())?;
        tracing::info!(repo = %repo.paths.toplevel.display(), ?gates, "verify");

        let progress = ctx.meta.get_progress_token().map(|token| Progress {
            peer: ctx.peer.clone(),
            token,
            interval: repo.user.mcp.progress_interval.max(Duration::from_secs(1)),
            started: Instant::now(),
        });
        let mut lines: Vec<GateLine> = Vec::with_capacity(gates.len());
        let mut cancelled = false;
        for (i, gate) in gates.iter().enumerate() {
            let line = self
                .verify_gate(
                    &repo,
                    gate,
                    &params,
                    &ctx,
                    progress.as_ref(),
                    (i + 1, gates.len()),
                )
                .await?;
            cancelled = line.result.status == GateStatus::Cancelled;
            lines.push(line);
            if cancelled {
                break;
            }
        }
        let all_passed = !cancelled && lines.iter().all(|g| g.result.status == GateStatus::Pass);
        let text = verify_text(&lines, cancelled);
        let report = VerifyReport {
            repository: repo.paths.toplevel.clone(),
            results: lines.into_iter().map(|g| g.result).collect(),
            all_passed,
            cancelled,
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

    /// List the gates of the repository's .sluice.toml: name, command, weight,
    /// timeout, hooks and cache flag.
    #[tool(output_schema = schema_for_output::<GateList>())]
    async fn gate_list(
        &self,
        Parameters(params): Parameters<GateListParams>,
    ) -> Result<CallToolResult, McpError> {
        let dir = workdir(&params.working_directory)?;
        let env = Arc::clone(&self.env);
        let (root, config) = blocking(move || {
            let paths = git::discover(&dir, &env).map_err(|e| e.to_string())?;
            let config = RepoConfig::load(&paths.toplevel).map_err(|e| e.to_string())?;
            Ok::<_, String>((paths.toplevel, config))
        })
        .await?
        .map_err(invalid)?;
        let gates: Vec<GateInfo> = config
            .gates
            .iter()
            .map(|(name, g)| GateInfo {
                name: name.clone(),
                cmd: g.cmd.trim().to_owned(),
                weight: g.weight,
                timeout_seconds: g.timeout.as_secs(),
                hooks: g.hooks.clone(),
                cache: g.cache,
            })
            .collect();
        let width = gates.iter().map(|g| g.name.len()).max().unwrap_or(0);
        let mut out = String::new();
        if gates.is_empty() {
            out.push_str("no gates defined in .sluice.toml\n");
        }
        for (g, (_, gate)) in gates.iter().zip(&config.gates) {
            let _ = writeln!(
                out,
                "{:<width$}  {}  (weight {}, timeout {}, hooks: {}, cache {})",
                g.name,
                gate.normalized_cmd(),
                g.weight,
                humantime::format_duration(gate.timeout),
                if g.hooks.is_empty() {
                    "-".to_owned()
                } else {
                    g.hooks.join(", ")
                },
                if g.cache { "on" } else { "off" }
            );
        }
        result(
            out,
            &GateList {
                repository: root,
                gates,
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

/// Cache state of the directory's current tree per gate (`status`):
/// repository root, text lines and JSON.
fn cache_state(dir: &Path, env: &EnvMap) -> Result<(PathBuf, String, serde_json::Value), String> {
    let repo = lookup::resolve_repo(dir, env, &CacheMode::Enabled).map_err(|e| e.to_string())?;
    let mut out = String::new();
    let mut gates = Vec::new();
    let (tree, unknown) = match tree::snapshot(&repo.paths, &repo.probe_env, &repo.scratch) {
        Ok(Snapshot::Known(t)) => (Some(t), None),
        Ok(Snapshot::Unknown { reason }) => (None, Some(reason)),
        Err(e) => (None, Some(e.to_string())),
    };
    match (&tree, &unknown) {
        (Some(t), _) => {
            let _ = writeln!(out, "tree:    {t}");
        }
        (None, Some(why)) => {
            let _ = writeln!(out, "tree:    unknown ({why}); every gate runs uncached");
        }
        (None, None) => {}
    }
    if let Some(tree) = &tree {
        for (name, gate) in &repo.config.gates {
            let mut g = GateCache {
                gate: name.clone(),
                state: "uncached",
                passed_at: None,
                job_id: None,
                last_run: None,
                message: None,
            };
            let line = if gate.cache {
                match repo
                    .select(name)
                    .and_then(|r| lookup::derive_key(&r, tree, &[]))
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
            let _ = writeln!(out, "{name:<16} {line}");
            gates.push(g);
        }
    }
    let json = serde_json::json!({
        "repository": repo.paths.toplevel,
        "tree": tree.as_ref().map(|t| t.as_str().to_owned()),
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
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "log"))
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
                line: "PASS 1.0s (job j)".into(),
            },
            GateLine {
                result: fail,
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

    fn required_names(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::Array(req)) = map.get("required") {
                    out.extend(req.iter().filter_map(|v| v.as_str().map(str::to_owned)));
                }
                for v in map.values() {
                    required_names(v, out);
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
}
