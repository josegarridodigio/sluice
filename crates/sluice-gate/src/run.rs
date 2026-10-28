//! `sluice run <gate>`: the cache-hit fast path and, on a miss, the gate run.
//!
//! 1. Resolve the repository, `.sluice.toml`, the gate and the user config.
//! 2. `SLUICE_DISABLE=1` runs the gate directly with inherited stdio.
//! 3. Snapshot the working tree, derive the key, look for a marker: a hit
//!    prints one line, sends a best-effort `hit` event to a running daemon
//!    and exits 0 without running anything (no async runtime involved).
//! 4. On a miss inside a hook invoked by an agent, apply `agent_hook_policy`
//!    (`fail-fast` by default). `run-unqueued` runs the gate in-process
//!    right away.
//! 5. Policy `wait`: send a `run` request to the daemon (started on demand)
//!    and print its outcome. When the daemon cannot be reached or started,
//!    fall back to the Phase 1 in-process run under the global `flock`
//!    semaphore (one slot per budget unit; a gate takes `weight` slots).
//! 6. Output goes to `<state>/logs/<job-id>.log`; the caller gets one line
//!    on success and the tail of the log on failure. A pass whose tree is
//!    unchanged writes a marker; a pass that modified the tree, and any
//!    failure, writes a miss record (the daemon does the same on its side).
//!
//! Queue-only gates (`mode = "queue-only"`) skip the marker lookup and the
//! fail-fast hook policy and always run under the budget; simultaneous
//! identical requests coalesce through a key in its own namespace, and
//! nothing is ever written to the cache. Inside a Sluice job (`SLUICE_JOB`
//! set) a miss runs in-process right away, unqueued (decision `nested`): the
//! parent job already holds the budget.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use sha2::{Digest, Sha256};
use sluice_gate_core::agent::{self, Caller};
use sluice_gate_core::config::HookPolicy;
use sluice_gate_core::git::{self, EnvMap};
use sluice_gate_core::hookgate::HookGates;
use sluice_gate_core::key::CacheKey;
use sluice_gate_core::logfile::{self, JobLog};
use sluice_gate_core::lookup::{self, Lookup as CacheState, Resolved, RunMaterial};
use sluice_gate_core::marker::{Lookup, Marker, MissRecord};
use sluice_gate_core::mode::{CacheMode, cache_mode};
use sluice_gate_core::tree::{self, SnapshotFull, TreeHash};
use sluice_gate_core::{CACHE_FORMAT_VERSION, JOB_ENV_VAR, SLUICE_VERSION};
use sluice_gate_proto::{
    self as proto, FEATURE_QUEUE_ONLY, GateSpec, Outcome, OutcomeStatus, RepoInfo, RunRequest,
    ServerMessage,
};

use crate::clientlog;
use crate::dirs;
use crate::hookentry::gates;
use sluice_gate_proto::client::{self, ClientError};

/// Exit code when `agent_hook_policy = "fail-fast"` refuses to run.
pub const EXIT_FAIL_FAST: u8 = 3;
/// Exit code when the gate exceeded its timeout (same as `timeout(1)`).
pub const EXIT_TIMEOUT: u8 = 124;
/// Exit code for Sluice's own errors (bad config, no repository, ...).
pub const EXIT_ERROR: u8 = 1;
/// Exit code when the gate was killed (128 + SIGTERM).
pub const EXIT_KILLED: u8 = 143;
/// Poll period of the log follower used by `-v` on the daemon path.
const FOLLOW_EVERY: Duration = Duration::from_millis(500);

/// Grace period between SIGTERM and SIGKILL on timeout.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Arguments of `sluice run`.
#[derive(Debug)]
pub struct RunArgs {
    /// Gate name.
    pub gate: String,
    /// Extra arguments appended to the gate command.
    pub extra: Vec<String>,
    /// Hook that invoked us (`pre-commit`, `pre-push`, ...), if any.
    pub hook: Option<String>,
    /// Stream the gate output to stderr while it runs.
    pub verbose: bool,
    /// Print what Sluice would do (detection, policy, tree, key, lookup) and exit.
    pub explain: bool,
}

/// Everything resolved before deciding between hit, policy and run: the
/// shared [`Resolved`] plus what only `sluice run` needs.
struct Context {
    res: Resolved,
    /// Lines of log shown on failure (repo override or user config).
    tail_lines: usize,
    hook: Option<String>,
}

impl std::ops::Deref for Context {
    type Target = Resolved;

    fn deref(&self) -> &Resolved {
        &self.res
    }
}

impl Context {
    /// The gate process environment ([`Resolved::child_env`]).
    fn child_env(&self) -> Result<&EnvMap, String> {
        self.res.child_env().map_err(|e| e.to_string())
    }
}

/// What `client.log` records about one invocation, filled in as the run
/// progresses.
#[derive(Debug, Default)]
struct Trace {
    /// `agent` or `human`.
    caller: Option<&'static str>,
    /// Working-tree hash, when it was computed.
    tree: Option<String>,
    /// Cache key, when it was derived.
    key: Option<String>,
    /// Short code of why the cache was off, when it was (`skip-worktree`).
    reason: Option<&'static str>,
    /// Decision keyword (see [`crate::clientlog`]); `None` until decided.
    decision: Option<&'static str>,
}

/// Entry point of `sluice run`: runs the gate and appends one line to
/// `client.log` (never failing the command because of it).
pub fn run(args: &RunArgs) -> ExitCode {
    let started = Instant::now();
    let mut trace = Trace::default();
    let code = run_traced(args, &mut trace);
    if !args.explain {
        record_invocation(args, &trace, exit_byte(code), started);
    }
    code
}

/// Appends the `client.log` line of one invocation; a failure to write it is
/// only logged.
fn record_invocation(args: &RunArgs, trace: &Trace, exit: u8, started: Instant) {
    let env: EnvMap = sluice_gate_core::config::process_env();
    let cwd = std::env::current_dir().unwrap_or_default();
    let decision = trace.decision.unwrap_or(if exit == EXIT_ERROR {
        "error"
    } else {
        "miss-run"
    });
    let entry = clientlog::Entry {
        pid: std::process::id(),
        caller: trace.caller.unwrap_or("human"),
        hook: args.hook.as_deref(),
        gate: &args.gate,
        tree: trace.tree.as_deref(),
        key: trace.key.as_deref(),
        reason: trace.reason,
        decision,
        exit,
        duration_ms: started.elapsed().as_millis(),
        cwd: &cwd,
    };
    if let Err(e) = clientlog::record(&env, &entry) {
        tracing::debug!("cannot write client.log: {e}");
    }
}

/// Runs one unit of a repository hook (a synthetic gate, see
/// `hookentry::runner`) exactly like `sluice run` runs a gate (cache lookup,
/// daemon queue, in-process fallback, logs, `client.log`), except that a
/// miss never applies `agent_hook_policy`: the hook runs the unit, it does
/// not ask for a `verify` first. Returns the exit byte `sluice run` would.
pub(crate) fn run_unit(res: Resolved, hook: &str) -> u8 {
    let started = Instant::now();
    let env = res.env.clone();
    let args = RunArgs {
        gate: res.gate_name.clone(),
        extra: Vec::new(),
        hook: Some(hook.to_owned()),
        verbose: false,
        explain: false,
    };
    let ctx = Context {
        tail_lines: res.tail_lines(),
        hook: args.hook.clone(),
        res,
    };
    let mut trace = Trace {
        caller: Some(caller_label(&ctx, &env)),
        ..Trace::default()
    };
    let mode = cache_mode(&env);
    let code = run_resolved(&ctx, &args, &mode, &mut trace, false);
    let exit = exit_byte(code);
    record_invocation(&args, &trace, exit, started);
    exit
}

/// `agent` or `human`, for `client.log`.
fn caller_label(ctx: &Context, env: &EnvMap) -> &'static str {
    match agent::detect(
        env,
        &ctx.user.agent_detection.env_any_set,
        &ctx.user.agent_detection.env_any_equals,
    ) {
        Caller::Agent { .. } => "agent",
        Caller::Human => "human",
    }
}

/// `ExitCode` has no accessor: recover the byte through its debug form.
fn exit_byte(code: ExitCode) -> u8 {
    format!("{code:?}")
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

fn run_traced(args: &RunArgs, trace: &mut Trace) -> ExitCode {
    let env: EnvMap = sluice_gate_core::config::process_env();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return fail(&format!("cannot read the current directory: {e}")),
    };
    let mode = cache_mode(&env);
    let ctx = match resolve(&env, &cwd, args, &mode) {
        Ok(ctx) => ctx,
        Err(e) => return fail(&e),
    };
    crate::logging::init_cli(&env, &ctx.user.debug.level);
    trace.caller = Some(caller_label(&ctx, &env));

    if args.explain {
        return explain(&ctx, &env, &mode, &args.extra);
    }
    run_resolved(&ctx, args, &mode, trace, true)
}

/// Everything after the gate is resolved: bypass, cache lookup, hit, policy
/// and run. `enforce_policy` applies `agent_hook_policy` on a miss inside a
/// hook (`sluice run`); a hook unit always waits for the daemon instead.
fn run_resolved(
    ctx: &Context,
    args: &RunArgs,
    mode: &CacheMode,
    trace: &mut Trace,
    enforce_policy: bool,
) -> ExitCode {
    let env = &ctx.env;
    if *mode == CacheMode::Bypass {
        trace.decision = Some("disabled");
        eprintln!(
            "sluice: SLUICE_DISABLE is set; running '{}' directly",
            ctx.gate_name
        );
        return run_inline(ctx, &args.extra);
    }

    // Queue-only: no marker lookup; the coalescing key in its own namespace.
    let queue_only = ctx
        .gate
        .is_queue_only()
        .then(|| queue_only_key(ctx, &args.extra));
    let lookup_state = if queue_only.is_some() {
        CacheState::Off {
            reason: "queue-only gate".to_owned(),
            notice: None,
            code: "queue-only",
        }
    } else {
        lookup::lookup(ctx, mode, &args.extra)
    };
    let state = match lookup_state {
        CacheState::Hit { marker: hit, tree } => return report_hit(ctx, &hit, &tree, trace),
        state @ CacheState::Off {
            notice: Some(_), ..
        } => announce_cache_off(state, trace),
        state => state,
    };

    // Policy on a miss, only inside a hook and only when the cache was consulted:
    // with caching off or an unknown tree the gate can never be cached, so
    // refusing to run would block the caller forever.
    if let CacheState::Miss { tree, key, .. } = &state {
        trace.tree = Some(tree.as_str().to_owned());
        trace.key = Some(key.as_str().to_owned());
    }
    if let Some(parent) = env.get(JOB_ENV_VAR).filter(|v| !v.is_empty()) {
        // Inside a Sluice job: queueing would wait for budget the parent holds.
        trace.decision = Some("nested");
        eprintln!(
            "sluice: inside Sluice job {parent}; running '{}' directly",
            ctx.gate_name
        );
        return execute(ctx, args, state);
    }
    let (caller, policy) = if enforce_policy {
        let (caller, _in_hook, policy) = decide_policy(ctx, env, &state);
        (caller, policy)
    } else {
        let caller = agent::detect(
            env,
            &ctx.user.agent_detection.env_any_set,
            &ctx.user.agent_detection.env_any_equals,
        );
        (caller, HookPolicy::Wait)
    };
    if policy == HookPolicy::FailFast {
        trace.decision = Some("miss-fail-fast");
        print_fail_fast(ctx, &state, &caller);
        return ExitCode::from(EXIT_FAIL_FAST);
    }
    if policy == HookPolicy::RunUnqueued {
        trace.decision = Some("in-process");
        return execute(ctx, args, state);
    }
    run_queued(ctx, args, &caller, state, queue_only.flatten(), trace)
}

/// A fresh marker matched: one line, a best-effort `hit` event, exit 0.
fn report_hit(ctx: &Context, hit: &Marker, tree: &TreeHash, trace: &mut Trace) -> ExitCode {
    trace.decision = Some("hit");
    // The current tree: on a scoped hit `hit.tree` is an earlier one.
    trace.tree = Some(tree.as_str().to_owned());
    trace.key = Some(hit.key.as_str().to_owned());
    println!(
        "sluice: {} PASS (cached, passed {} ago)",
        ctx.gate_name,
        age(&hit.passed_at)
    );
    if let Ok(socket) = sluice_gate_core::dirs::socket_path(&ctx.env)
        && socket.exists()
    {
        client::send_hit_event(
            &socket,
            &ctx.paths.toplevel,
            &ctx.gate_name,
            hit.duration_ms,
        );
    }
    ExitCode::SUCCESS
}

/// Prints the one-line reason the cache is off on stderr (with or without
/// `-v`, hooks included: a cache that is off must never go unnoticed), records
/// its code for `client.log` and returns the state with the notice consumed.
fn announce_cache_off(state: CacheState, trace: &mut Trace) -> CacheState {
    match state {
        CacheState::Off {
            reason,
            notice: Some(notice),
            code,
        } => {
            eprintln!("sluice: {notice}");
            trace.reason = Some(code);
            CacheState::Off {
                reason,
                notice: None,
                code,
            }
        }
        other => other,
    }
}

/// Policy `wait`: the daemon, else the in-process run under the semaphore.
fn run_queued(
    ctx: &Context,
    args: &RunArgs,
    caller: &Caller,
    state: CacheState,
    queue_only: Option<(TreeHash, CacheKey)>,
    trace: &mut Trace,
) -> ExitCode {
    let env = &ctx.env;
    if let Some((_, key)) = &queue_only {
        trace.key = Some(key.as_str().to_owned());
    }
    let request = match run_request(ctx, args, caller, &state) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    // A socket path that can never work (an over-long `SLUICE_SOCKET`, an
    // unsafe fallback directory) is a configuration error to fix, not a
    // reason to lose the queue silently with an in-process run.
    if let Err(e @ sluice_gate_core::Error::SocketPath { .. }) =
        sluice_gate_core::dirs::socket_path(env)
    {
        return fail(&e.to_string());
    }
    match via_daemon(ctx, args, request, queue_only, &state, trace) {
        Ok(code) => return code,
        Err(why) => eprintln!("sluice: daemon unavailable ({why}); running in-process"),
    }
    trace.decision = Some("in-process");
    let _slots = match crate::budget::acquire(
        &ctx.scratch,
        ctx.user.daemon.budget,
        ctx.gate.weight.ceiling(ctx.user.daemon.budget),
        &ctx.gate_name,
        false,
    ) {
        Ok(slots) => slots,
        Err(e) => return fail(&e),
    };
    execute(ctx, args, state)
}

/// Tree and coalescing key of a queue-only gate: `sha256("queue-only\0" K)`
/// where K is the gate's normal key on the current tree, so it can never
/// match a marker. `None` when the tree or K is unknown (no coalescing).
fn queue_only_key(ctx: &Context, extra: &[String]) -> Option<(TreeHash, CacheKey)> {
    let Ok(SnapshotFull::Known {
        root: tree,
        entries,
    }) = tree::snapshot_entries(&ctx.paths, &ctx.probe_env, &ctx.scratch)
    else {
        return None;
    };
    let scoped = lookup::key_tree(&ctx.gate, &tree, &entries);
    let (_, key) = lookup::derive_key(ctx, scoped, extra).ok()?;
    let digest = Sha256::new()
        .chain_update(b"queue-only\0")
        .chain_update(key.as_str().as_bytes())
        .finalize();
    let key = CacheKey::try_from(hex::encode(digest)).ok()?;
    Some((tree, key))
}

/// Explains on stderr why `agent_hook_policy = "fail-fast"` refused to run.
fn print_fail_fast(ctx: &Context, state: &CacheState, caller: &Caller) {
    let why = match state {
        CacheState::Miss {
            last_miss: Some(m), ..
        } => format!(
            "last run failed {} ago{}",
            age(&m.recorded_at),
            m.exit_code
                .map_or(String::new(), |c| format!(" with exit code {c}"))
        ),
        CacheState::Miss { .. } | CacheState::Hit { .. } => {
            "no cached pass for this tree".to_owned()
        }
        CacheState::Off { reason, .. } => reason.clone(),
    };
    let rule = match caller {
        Caller::Agent { rule } => rule.as_str(),
        Caller::Human => "human",
    };
    let hook = ctx.hook.as_deref().unwrap_or("a git hook");
    eprintln!(
        "sluice: '{}' has not passed on this tree ({why}).\n\
         sluice: {hook} was invoked by a coding agent ({rule}) and agent_hook_policy is \"fail-fast\".\n\
         sluice: call the 'verify' MCP tool (or run 'sluice run {}') first, then retry the git command.",
        ctx.gate_name, ctx.gate_name
    );
}

/// Builds the daemon `run` request with everything precomputed.
fn run_request(
    ctx: &Context,
    args: &RunArgs,
    caller: &Caller,
    state: &CacheState,
) -> Result<RunRequest, String> {
    let material = lookup::run_material(ctx, state, &args.extra, ctx.hook.as_deref())
        .map_err(|e| e.to_string())?;
    Ok(to_request(
        material,
        ctx.user.daemon.budget,
        match caller {
            Caller::Agent { .. } => proto::Caller::Agent,
            Caller::Human => proto::Caller::Human,
        },
    ))
}

/// Converts the shared run material into the protocol request.
fn to_request(m: RunMaterial, budget: u32, caller: proto::Caller) -> RunRequest {
    RunRequest {
        repo: RepoInfo {
            toplevel: m.paths.toplevel,
            common_dir: m.paths.common_dir,
            git_dir: m.paths.git_dir,
        },
        gate: GateSpec {
            name: m.gate_name,
            cmd: m.gate.cmd,
            weight: m.gate.weight.ceiling(budget),
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
        caller,
        session: m.session,
        client_pid: std::process::id(),
        hook: m.hook,
        no_marker: false,
        requested_at: Some(sluice_gate_core::dirs::now_ms()),
    }
}

/// Copies new bytes of a log file to stderr every 500 ms (`-v` on the daemon path).
struct LogFollower {
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl LogFollower {
    fn start(path: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut offset = 0u64;
            loop {
                let last = flag.load(Ordering::Acquire);
                if let Ok(mut file) = File::open(&path) {
                    use std::io::Seek as _;
                    if file.seek(std::io::SeekFrom::Start(offset)).is_ok() {
                        let mut buf = Vec::new();
                        if file.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                            offset += buf.len() as u64;
                            let _ = std::io::stderr().write_all(&buf);
                        }
                    }
                }
                if last {
                    return;
                }
                thread::park_timeout(FOLLOW_EVERY);
            }
        });
        Self { stop, thread }
    }

    fn finish(self) {
        self.stop.store(true, Ordering::Release);
        self.thread.thread().unpark();
        let _ = self.thread.join();
    }
}

/// Runs the gate through the daemon. `Err` carries why the daemon could
/// not be used before the request was queued (the caller falls back to
/// the in-process run); once queued, every result is final. `queue_only`
/// carries the tree and coalescing key of a queue-only gate; they are sent
/// (with `no_marker`) only to a daemon announcing `queue-only`, otherwise
/// the request goes without a key (runs, never coalesces, never cached).
fn via_daemon(
    ctx: &Context,
    args: &RunArgs,
    mut request: RunRequest,
    queue_only: Option<(TreeHash, CacheKey)>,
    state: &CacheState,
    trace: &mut Trace,
) -> Result<ExitCode, String> {
    let socket = client::ensure_daemon(&ctx.env).map_err(|e| format!("{e:#}"))?;
    let conn = client::connect(&socket).map_err(|e| e.to_string())?;
    if let Some((tree, key)) = queue_only
        && conn.supports(FEATURE_QUEUE_ONLY)
    {
        request.tree = Some(tree);
        request.key = Some(key);
        request.no_marker = true;
    }
    let gate = &ctx.gate_name;
    let mut queued = false;
    let mut started = false;
    let mut follower: Option<LogFollower> = None;
    let mut stalled = false;
    let result = conn.run(request, |msg| match msg {
        ServerMessage::Progress {
            job_id, stalled_ms, ..
        } => stall_notice(gate, job_id, *stalled_ms, &mut stalled),
        ServerMessage::Queued {
            job_id,
            position,
            coalesced,
            ..
        } if !queued => {
            queued = true;
            trace.decision = Some(if *coalesced { "coalesced" } else { "miss-run" });
            let busy = client::connect(&socket)
                .and_then(client::Client::status)
                .map(|s| format!(", {} of {} units busy", s.budget.used, s.budget.total))
                .unwrap_or_default();
            let sharing = if *coalesced {
                format!(", sharing job {job_id}")
            } else {
                String::new()
            };
            eprintln!("sluice: {gate} queued (position {position}{busy}){sharing}");
        }
        ServerMessage::Started { job_id, .. } => {
            started = true;
            eprintln!("sluice: {gate} running (job {job_id})");
            if args.verbose && follower.is_none() {
                let path = dirs::logs_dir(&ctx.env).join(format!("{job_id}.log"));
                follower = Some(LogFollower::start(path));
            }
        }
        _ => {}
    });
    if let Some(f) = follower {
        f.finish();
    }
    match result {
        Ok(done) => {
            if done.outcome.status == OutcomeStatus::Cached {
                // The daemon found a fresh marker: nothing ran.
                trace.decision = Some("hit");
            }
            Ok(report_outcome(ctx, state, &done.job_id, &done.outcome))
        }
        Err(ClientError::Closed | ClientError::Daemon(_)) if !queued => {
            Err("the daemon refused or dropped the request".to_owned())
        }
        Err(e) if !queued => Err(e.to_string()),
        // The daemon died (SIGKILL, OOM, crash): its reaper kills the gate's
        // process group, so the run is reported as killed, never as a pass.
        Err(e) if e.is_disconnect() && started => {
            eprintln!("sluice: error: the daemon died while running {gate}; it was killed");
            Ok(ExitCode::from(EXIT_KILLED))
        }
        Err(e) if e.is_disconnect() => Ok(fail(&format!(
            "the daemon died while {gate} was queued; it did not run"
        ))),
        Err(e) => Ok(fail(&format!("lost the connection to the daemon: {e}"))),
    }
}

/// The one-line notice when the running job becomes stalled (no CPU, I/O or
/// output for its whole quiet window), and when it is active again
/// (ADR-0024). Sluice only reports: it never ends the job.
fn stall_notice(gate: &str, job_id: &str, stalled_ms: Option<u64>, noted: &mut bool) {
    match (stalled_ms, *noted) {
        (Some(ms), false) => eprintln!(
            "sluice: {gate} looks stalled: no CPU, I/O or output for {} (job {job_id}); it is \
             probably hung, look at what it is waiting for",
            crate::status::dur(ms)
        ),
        (None, true) => eprintln!("sluice: {gate} is active again (job {job_id})"),
        _ => return,
    }
    *noted = stalled_ms.is_some();
}

/// Prints a daemon outcome exactly as the in-process path would.
fn report_outcome(
    ctx: &Context,
    state: &CacheState,
    done_job: &str,
    outcome: &Outcome,
) -> ExitCode {
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
        .unwrap_or_else(|| done_job.to_owned());
    if outcome.status == OutcomeStatus::Killed
        && outcome.log_path.is_none()
        && !outcome.killed_for_memory()
    {
        // Dropped from the queue (daemon stop, cancellation): it never ran.
        eprintln!(
            "sluice: {} KILLED before it started (job {job_id}): the daemon stopped",
            ctx.gate_name
        );
        return ExitCode::from(EXIT_KILLED);
    }
    if let Some(e) = &outcome.log_error {
        eprintln!("sluice: warning: {e}");
    }
    let finish = match outcome.status {
        OutcomeStatus::Cached => {
            let passed = match state {
                CacheState::Miss { key, .. } => match ctx.store.lookup(key, ctx.ttl) {
                    Ok(Lookup::Hit(m)) => format!(", passed {} ago", age(&m.passed_at)),
                    _ => String::new(),
                },
                CacheState::Off { .. } | CacheState::Hit { .. } => String::new(),
            };
            println!("sluice: {} PASS (cached{passed})", ctx.gate_name);
            // Why nothing ran, when the daemon says so (a pass that appeared
            // while queued, a gate verified by the merge train's push).
            if !outcome.message.is_empty() {
                eprintln!("sluice: {}", outcome.message);
            }
            return ExitCode::SUCCESS;
        }
        OutcomeStatus::Error => return fail(&outcome.message),
        // Never a pass: a status from a newer daemon is an error.
        OutcomeStatus::Unknown => {
            return fail("the daemon reported an outcome this client does not know; update sluice");
        }
        OutcomeStatus::Pass => Finish::Pass {
            note: match (state, &outcome.not_cached_reason) {
                _ if ctx.gate.is_queue_only() => String::new(),
                (CacheState::Off { reason, .. }, _) | (_, Some(reason)) => {
                    format!(" (not cached: {reason})")
                }
                _ => String::new(),
            },
        },
        OutcomeStatus::Fail => Finish::Fail,
        OutcomeStatus::Timeout => Finish::Timeout,
        OutcomeStatus::Killed => Finish::Killed,
    };
    report(
        ctx,
        &Report {
            finish,
            exit_code: outcome.exit_code,
            duration: Duration::from_millis(outcome.duration_ms),
            job_id: &job_id,
            log_path: outcome.log_path.as_deref().unwrap_or(Path::new("-")),
            tail: &outcome.tail,
            memory_kill: outcome
                .killed_for_memory()
                .then_some(outcome.message.as_str()),
        },
    )
}

/// How a gate run ended, for [`report`].
enum Finish {
    /// Exit 0; `note` is empty or ` (not cached: …)`.
    Pass { note: String },
    /// Non-zero exit or death by signal.
    Fail,
    /// Timeout exceeded.
    Timeout,
    /// Killed by Sluice (forced stop).
    Killed,
}

/// Everything the summary lines need, from either execution path.
struct Report<'a> {
    finish: Finish,
    exit_code: Option<i32>,
    duration: Duration,
    job_id: &'a str,
    log_path: &'a Path,
    /// Tail sent by the daemon, used when the log file cannot be read.
    tail: &'a [String],
    /// The daemon's message when the job was killed for lack of memory.
    memory_kill: Option<&'a str>,
}

/// Prints the Phase 1 summary (and tail) of a run and returns its exit code.
fn report(ctx: &Context, r: &Report<'_>) -> ExitCode {
    let gate = &ctx.gate_name;
    let secs = r.duration.as_secs_f64();
    let (job, log) = (r.job_id, r.log_path.display());
    match &r.finish {
        Finish::Pass { note } => {
            println!("sluice: {gate} PASS {secs:.1}s (job {job}){note}");
            ExitCode::SUCCESS
        }
        Finish::Timeout => {
            eprintln!(
                "sluice: {gate} TIMEOUT after {} (job {job}) log={log}",
                humantime::format_duration(ctx.gate.timeout)
            );
            print_tail_or(r.log_path, ctx.tail_lines, r.tail);
            ExitCode::from(EXIT_TIMEOUT)
        }
        Finish::Killed => {
            eprintln!(
                "{}",
                killed_headline(gate, r.memory_kill.is_some(), secs, job, &log.to_string())
            );
            if let Some(message) = r.memory_kill {
                eprintln!("sluice: {message}");
            }
            print_tail_or(r.log_path, ctx.tail_lines, r.tail);
            ExitCode::from(EXIT_KILLED)
        }
        Finish::Fail => {
            let code = r
                .exit_code
                .map_or_else(|| "signal".to_owned(), |c| c.to_string());
            eprintln!("sluice: {gate} FAIL exit={code} {secs:.1}s (job {job}) log={log}");
            print_tail_or(r.log_path, ctx.tail_lines, r.tail);
            exit_code_of(r.exit_code)
        }
    }
}

/// The headline of a killed run. A memory kill has one wording in every
/// client (MCP `verify`, `sluice exec`); the daemon's message follows it.
fn killed_headline(gate: &str, memory: bool, secs: f64, job: &str, log: &str) -> String {
    let cause = if memory { " for lack of memory" } else { "" };
    format!("sluice: {gate} KILLED{cause} after {secs:.1}s (job {job}) log={log}")
}

fn fail(message: &str) -> ExitCode {
    eprintln!("sluice: error: {message}");
    ExitCode::from(EXIT_ERROR)
}

/// Who called, whether we are inside a hook, and which policy applies on this miss.
fn decide_policy(ctx: &Context, env: &EnvMap, state: &CacheState) -> (Caller, bool, HookPolicy) {
    let in_hook =
        ctx.hook.is_some() || env.contains_key("GIT_DIR") || env.contains_key("GIT_INDEX_FILE");
    let caller = agent::detect(
        env,
        &ctx.user.agent_detection.env_any_set,
        &ctx.user.agent_detection.env_any_equals,
    );
    let policy = match (&caller, in_hook, state) {
        (_, false, _) => HookPolicy::Wait,
        // No `verify` can ever satisfy fail-fast for a queue-only gate (it is
        // never cached): the hook waits instead of looping.
        (Caller::Agent { .. }, true, _) if ctx.gate.is_queue_only() => {
            match ctx.config.defaults.agent_hook_policy {
                HookPolicy::FailFast => HookPolicy::Wait,
                other => other,
            }
        }
        (Caller::Agent { .. }, true, CacheState::Miss { .. } | CacheState::Hit { .. }) => {
            ctx.config.defaults.agent_hook_policy
        }
        (Caller::Agent { .. }, true, CacheState::Off { .. }) => {
            match ctx.config.defaults.agent_hook_policy {
                HookPolicy::FailFast => HookPolicy::Wait,
                other => other,
            }
        }
        (Caller::Human, true, _) => ctx.config.defaults.human_hook_policy,
    };
    (caller, in_hook, policy)
}

/// `sluice run --explain`: print every decision without running anything.
fn explain(ctx: &Context, env: &EnvMap, mode: &CacheMode, extra: &[String]) -> ExitCode {
    println!("repository:   {}", ctx.paths.toplevel.display());
    println!("common dir:   {}", ctx.paths.common_dir.display());
    println!(
        "git:          {}",
        git::version().unwrap_or_else(|e| e.to_string())
    );
    println!("gate:         {} ({})", ctx.gate_name, ctx.gate.cmd.trim());
    // The same conditions `lookup::lookup` and `run_traced` apply before they
    // consult the store: when one holds, a lookup line would be misleading.
    let lookup_off = match mode {
        CacheMode::Enabled if ctx.gate.is_queue_only() => {
            println!("cache mode:   enabled");
            Some("queue-only gate".to_owned())
        }
        CacheMode::Enabled if !ctx.gate.cache => {
            println!("cache mode:   enabled");
            Some(format!("gate '{}' has cache = false", ctx.gate_name))
        }
        CacheMode::Enabled => {
            println!("cache mode:   enabled");
            None
        }
        CacheMode::Disabled { reason } => {
            println!("cache mode:   disabled ({reason})");
            Some(reason.clone())
        }
        CacheMode::Bypass => {
            println!(
                "cache mode:   bypass (SLUICE_DISABLE is set: the gate runs directly, no cache, no queue)"
            );
            Some("SLUICE_DISABLE is set".to_owned())
        }
    };
    let caller = agent::detect(
        env,
        &ctx.user.agent_detection.env_any_set,
        &ctx.user.agent_detection.env_any_equals,
    );
    println!("caller:       {caller:?}");
    let in_hook =
        ctx.hook.is_some() || env.contains_key("GIT_DIR") || env.contains_key("GIT_INDEX_FILE");
    println!(
        "hook:         {} (in_hook = {in_hook})",
        ctx.hook.as_deref().unwrap_or("none")
    );
    println!(
        "policies:     agent = {:?}, human = {:?}",
        ctx.config.defaults.agent_hook_policy, ctx.config.defaults.human_hook_policy
    );
    println!(
        "budget:       {} units, gate weight {}",
        ctx.user.daemon.budget, ctx.gate.weight
    );
    match tree::snapshot_entries(&ctx.paths, &ctx.probe_env, &ctx.scratch) {
        Ok(SnapshotFull::Known {
            root: tree,
            entries,
        }) => {
            println!("tree:         {tree}");
            if let Some(s) = lookup::scope_summary(&ctx.gate, &entries) {
                println!(
                    "scope:        {} of {} files, {}",
                    s.count.files, s.count.total, s.hash
                );
            }
            match lookup::derive_key(ctx, lookup::key_tree(&ctx.gate, &tree, &entries), extra) {
                Ok((_, key)) => {
                    println!("key:          {key}");
                    if let Some(reason) = &lookup_off {
                        println!("lookup:       OFF ({reason})");
                        println!("markers:      {}", ctx.store.dir().display());
                        return ExitCode::SUCCESS;
                    }
                    match ctx.store.lookup(&key, ctx.ttl) {
                        Ok(Lookup::Hit(m)) => println!(
                            "lookup:       HIT (passed {} ago, job {})",
                            age(&m.passed_at),
                            m.job_id
                        ),
                        Ok(Lookup::Miss(Some(m))) => println!(
                            "lookup:       MISS (last run {} ago: {})",
                            age(&m.recorded_at),
                            m.reason
                        ),
                        Ok(Lookup::Miss(None)) => println!("lookup:       MISS"),
                        Err(e) => println!("lookup:       error ({e})"),
                    }
                }
                Err(e) => println!("key:          error ({e})"),
            }
        }
        Ok(SnapshotFull::Unknown { reason }) => println!("tree:         unknown ({reason})"),
        Err(e) => println!("tree:         error ({e})"),
    }
    println!("markers:      {}", ctx.store.dir().display());
    ExitCode::SUCCESS
}

/// Resolves repository, config and derived environments.
fn resolve(env: &EnvMap, cwd: &Path, args: &RunArgs, mode: &CacheMode) -> Result<Context, String> {
    // Without `.sluice.toml` the gates are the check units of the hooks.
    let res = match lookup::resolve(cwd, env, &args.gate, mode) {
        Err(sluice_gate_core::Error::ConfigNotFound(_)) => {
            let derive = |paths: &git::RepoPaths, env: &EnvMap| {
                gates::hook_of_name(&args.gate).map_or_else(HookGates::default, |hook| {
                    gates::derive(paths, env, Some(hook))
                })
            };
            lookup::resolve_hook_gates(cwd, env, &args.gate, mode, &derive)
        }
        other => other,
    }
    .map_err(|e| e.to_string())?;
    Ok(Context {
        tail_lines: res.tail_lines(),
        res,
        hook: args.hook.clone(),
    })
}

/// Builds the gate command: `sh -c '<cmd>'`, or `sh -c '<cmd> "$@"' sh <extra...>`
/// when extra arguments are given (appending `"$@"` to a compound command such
/// as a `for` loop would be a syntax error, so it is only added when needed).
/// The process carries `SLUICE_JOB=<job_id>`.
fn gate_command(ctx: &Context, extra: &[String], job_id: &str) -> Result<Command, String> {
    let mut cmd = Command::new("sh");
    let script = if extra.is_empty() {
        ctx.gate.cmd.trim().to_owned()
    } else {
        format!("{} \"$@\"", ctx.gate.cmd.trim())
    };
    cmd.arg("-c")
        .arg(script)
        .arg("sh")
        .args(extra)
        .current_dir(&ctx.paths.toplevel)
        .env_clear()
        .envs(ctx.child_env()?)
        .env(JOB_ENV_VAR, job_id);
    Ok(cmd)
}

/// `SLUICE_DISABLE=1`: run the gate with inherited stdio and exit with its code.
fn run_inline(ctx: &Context, extra: &[String]) -> ExitCode {
    let job_id = format!("inproc-{}", std::process::id());
    let mut cmd = match gate_command(ctx, extra, &job_id) {
        Ok(cmd) => cmd,
        Err(e) => return fail(&e),
    };
    match cmd.status() {
        Ok(status) => exit_code_of(status.code()),
        Err(e) => fail(&format!("cannot start the gate: {e}")),
    }
}

fn exit_code_of(code: Option<i32>) -> ExitCode {
    match code {
        Some(c) => ExitCode::from(u8::try_from(c & 0xff).unwrap_or(1)),
        None => ExitCode::from(EXIT_KILLED),
    }
}

/// Result of one gate execution.
struct Execution {
    exit_code: Option<i32>,
    timed_out: bool,
    duration: Duration,
    log_path: PathBuf,
    job_id: String,
    /// The output when the log file could not be created (kept in memory).
    memory_tail: Vec<String>,
}

/// Runs the gate in-process with output captured to a log file, enforcing the timeout.
fn execute(ctx: &Context, args: &RunArgs, state: CacheState) -> ExitCode {
    let exec = match spawn_and_wait(ctx, args) {
        Ok(exec) => exec,
        Err(e) => return fail(&e),
    };
    let finish = if exec.timed_out {
        if let CacheState::Miss { key, .. } = &state {
            record_miss(ctx, key, None, "timed out", &exec);
        }
        Finish::Timeout
    } else if exec.exit_code != Some(0) {
        if let CacheState::Miss { key, .. } = &state {
            record_miss(ctx, key, exec.exit_code, "gate failed", &exec);
        }
        Finish::Fail
    } else {
        let note = match state {
            _ if ctx.gate.is_queue_only() => String::new(),
            CacheState::Off { reason, .. } => format!(" (not cached: {reason})"),
            CacheState::Hit { .. } => String::new(),
            CacheState::Miss { tree, key, cmd, .. } => {
                match write_marker(ctx, &tree, &key, &cmd, &exec) {
                    Ok(()) => String::new(),
                    Err(reason) => format!(" (not cached: {reason})"),
                }
            }
        };
        Finish::Pass { note }
    };
    report(
        ctx,
        &Report {
            finish,
            exit_code: exec.exit_code,
            duration: exec.duration,
            job_id: &exec.job_id,
            log_path: &exec.log_path,
            tail: &exec.memory_tail,
            memory_kill: None,
        },
    )
}

fn spawn_and_wait(ctx: &Context, args: &RunArgs) -> Result<Execution, String> {
    let job_id = dirs::job_id();
    let log_path = dirs::logs_dir(&ctx.env).join(format!("{job_id}.log"));
    // An unwritable log never stops the gate: the output is kept in memory
    // for the tail and the error is reported once.
    let (log, log_error) = JobLog::create(&log_path);
    if let Some(e) = &log_error {
        eprintln!("sluice: warning: {e}; the output was kept in memory for the tail only");
    }
    let log = Arc::new(Mutex::new(log));
    {
        let mut l = log.lock().map_err(|_| "log lock poisoned".to_owned())?;
        let _ = writeln!(
            l,
            "# sluice job {job_id}: gate '{}' in {}\n# command: {}\n",
            ctx.gate_name,
            ctx.paths.toplevel.display(),
            ctx.gate.cmd
        );
    }

    let mut child = gate_command(ctx, &args.extra, &job_id)?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start the gate: {e}"))?;
    let started = Instant::now();
    let pid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(0));

    let readers = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ];
    let pumps: Vec<_> = readers
        .into_iter()
        .flatten()
        .map(|mut reader| {
            let log = Arc::clone(&log);
            let verbose = args.verbose;
            thread::spawn(move || pump(&mut *reader, &log, verbose))
        })
        .collect();

    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("cannot wait for the gate: {e}"))?
        {
            break status;
        }
        if !ctx.gate.timeout.is_zero() && started.elapsed() > ctx.gate.timeout {
            timed_out = true;
            kill_tree(pid, Signal::SIGTERM);
            let grace = Instant::now();
            let status = loop {
                if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
                    break s;
                }
                if grace.elapsed() > KILL_GRACE {
                    kill_tree(pid, Signal::SIGKILL);
                    break child.wait().map_err(|e| e.to_string())?;
                }
                thread::sleep(Duration::from_millis(20));
            };
            break status;
        }
        thread::sleep(Duration::from_millis(20));
    };
    for pump in pumps {
        let _ = pump.join();
    }
    let duration = started.elapsed();
    if let Ok(mut l) = log.lock() {
        let _ = writeln!(
            l,
            "\n# exit: {} after {:.1}s{}",
            status
                .code()
                .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
            duration.as_secs_f64(),
            if timed_out { " (timed out)" } else { "" }
        );
    }
    let memory_tail = log
        .lock()
        .ok()
        .and_then(|l| l.memory())
        .map(|bytes| logfile::last_lines(&bytes, usize::MAX))
        .unwrap_or_default();
    Ok(Execution {
        exit_code: status.code(),
        timed_out,
        duration,
        log_path,
        job_id,
        memory_tail,
    })
}

/// Sends `signal` to `root` and every descendant found in `/proc`.
///
/// The gate shares our process group so that Ctrl-C from a terminal reaches
/// it; on timeout the whole tree must die, or an orphaned grandchild would
/// keep the output pipes open. Linux only (reads `/proc/<pid>/stat`).
fn kill_tree(root: Pid, signal: Signal) {
    let mut children: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.filter_map(Result::ok) {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // "<pid> (<comm>) <state> <ppid> ..."; comm may contain spaces and parentheses.
            let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
                continue;
            };
            if let Some(ppid) = rest
                .split_whitespace()
                .nth(1)
                .and_then(|p| p.parse::<i32>().ok())
            {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut order = vec![root.as_raw()];
    let mut i = 0;
    while i < order.len() {
        if let Some(kids) = children.get(&order[i]) {
            order.extend(kids);
        }
        i += 1;
    }
    for pid in order.into_iter().rev() {
        let _ = kill(Pid::from_raw(pid), signal);
    }
}

/// Copies a child stream into the log (and stderr when verbose).
fn pump(reader: &mut dyn Read, log: &Mutex<JobLog>, verbose: bool) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut l) = log.lock() {
                    let _ = l.write_all(&buf[..n]);
                }
                if verbose {
                    let _ = std::io::stderr().write_all(&buf[..n]);
                }
            }
        }
    }
}

/// Writes a marker when the tree is unchanged after the run.
fn write_marker(
    ctx: &Context,
    tree: &TreeHash,
    key: &CacheKey,
    cmd: &str,
    exec: &Execution,
) -> Result<(), String> {
    let after =
        tree::snapshot(&ctx.paths, &ctx.probe_env, &ctx.scratch).map_err(|e| e.to_string())?;
    if after.tree() != Some(tree) {
        record_miss(ctx, key, Some(0), "the gate modified the tree", exec);
        return Err("the gate modified the tree; verify the new tree again".to_owned());
    }
    let committed_tree = if ctx.hook.as_deref() == Some("pre-commit") {
        lookup::committed_tree(ctx)
    } else {
        None
    };
    let marker = Marker {
        format: CACHE_FORMAT_VERSION,
        key: key.clone(),
        tree: tree.clone(),
        gate: ctx.gate_name.clone(),
        cmd: cmd.to_owned(),
        passed_at: now_rfc3339(),
        duration_ms: u64::try_from(exec.duration.as_millis()).unwrap_or(u64::MAX),
        exit_code: 0,
        job_id: exec.job_id.clone(),
        sluice_version: SLUICE_VERSION.to_owned(),
        committed_tree,
    };
    ctx.store.write(&marker).map_err(|e| e.to_string())?;
    let _ = ctx.store.prune(ctx.ttl);
    Ok(())
}

fn record_miss(
    ctx: &Context,
    key: &CacheKey,
    exit_code: Option<i32>,
    reason: &str,
    exec: &Execution,
) {
    let miss = MissRecord {
        format: CACHE_FORMAT_VERSION,
        key: key.clone(),
        gate: ctx.gate_name.clone(),
        exit_code,
        reason: reason.to_owned(),
        job_id: exec.job_id.clone(),
        log_path: Some(exec.log_path.clone()),
        recorded_at: now_rfc3339(),
    };
    if let Err(e) = ctx.store.write_miss(&miss) {
        eprintln!("sluice: cannot record the miss ({e})");
    }
}

/// Prints the last `lines` lines of the log to stderr; `fallback` (the
/// daemon's tail) is used when the log file cannot be read.
fn print_tail_or(path: &Path, lines: usize, fallback: &[String]) {
    let all: Vec<String> = match File::open(path) {
        Ok(file) => BufReader::new(file).lines().map_while(Result::ok).collect(),
        Err(_) if !fallback.is_empty() => fallback.to_vec(),
        Err(_) => return,
    };
    // Skip Sluice's own framing lines so the tail shows what the gate printed.
    let mut all: Vec<String> = all
        .into_iter()
        .filter(|l| {
            !(l.starts_with("# sluice job ")
                || l.starts_with("# command: ")
                || l.starts_with("# exit: "))
        })
        .collect();
    while all.last().is_some_and(|l| l.trim().is_empty()) {
        all.pop();
    }
    while all.first().is_some_and(|l| l.trim().is_empty()) {
        all.remove(0);
    }
    let start = all.len().saturating_sub(lines);
    eprintln!(
        "--- last {} lines of {} ---",
        all.len() - start,
        path.display()
    );
    for line in &all[start..] {
        eprintln!("{line}");
    }
}

/// Current time as RFC 3339 with seconds precision.
pub fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string()
}

/// Human-readable age of an RFC 3339 timestamp (`3s`, `5m`, `2h`, `3d`).
pub fn age(rfc3339: &str) -> String {
    let Ok(then) = humantime::parse_rfc3339(rfc3339) else {
        return "?".to_owned();
    };
    let secs = std::time::SystemTime::now()
        .duration_since(then)
        .unwrap_or_default()
        .as_secs();
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_formats_units() {
        let now = std::time::SystemTime::now();
        let fmt = |d: Duration| humantime::format_rfc3339_seconds(now - d).to_string();
        assert_eq!(age(&fmt(Duration::from_secs(5))), "5s");
        assert_eq!(age(&fmt(Duration::from_mins(5))), "5m");
        assert_eq!(age(&fmt(Duration::from_hours(2))), "2h");
        assert_eq!(age(&fmt(Duration::from_hours(72))), "3d");
        assert_eq!(age("garbage"), "?");
    }

    #[test]
    fn a_memory_kill_has_the_one_wording_of_every_client() {
        assert_eq!(
            killed_headline("test", true, 41.24, "j1", "/l/j1.log"),
            "sluice: test KILLED for lack of memory after 41.2s (job j1) log=/l/j1.log"
        );
        assert_eq!(
            killed_headline("test", false, 2.0, "j2", "/l/j2.log"),
            "sluice: test KILLED after 2.0s (job j2) log=/l/j2.log"
        );
    }

    #[test]
    fn exit_code_maps_signals_to_143() {
        assert_eq!(exit_code_of(Some(0)), ExitCode::SUCCESS);
        assert_eq!(exit_code_of(None), ExitCode::from(143));
    }

    #[test]
    fn exit_byte_reads_the_code_back() {
        assert_eq!(exit_byte(ExitCode::SUCCESS), 0);
        assert_eq!(exit_byte(ExitCode::from(3)), 3);
        assert_eq!(exit_byte(ExitCode::from(143)), 143);
    }
}
