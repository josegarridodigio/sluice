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
use sluice_gate_core::agent::{self, Caller};
use sluice_gate_core::config::HookPolicy;
use sluice_gate_core::git::{self, EnvMap};
use sluice_gate_core::key::CacheKey;
use sluice_gate_core::lookup::{self, Lookup as CacheState, Resolved, RunMaterial};
use sluice_gate_core::marker::{Lookup, Marker, MissRecord};
use sluice_gate_core::mode::{CacheMode, cache_mode};
use sluice_gate_core::semaphore::{Semaphore, SlotGuard};
use sluice_gate_core::tree::{self, Snapshot, TreeHash};
use sluice_gate_core::{CACHE_FORMAT_VERSION, SLUICE_VERSION};
use sluice_gate_proto::{
    self as proto, GateSpec, Outcome, OutcomeStatus, RepoInfo, RunRequest, ServerMessage,
};

use crate::dirs;
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

/// Entry point of `sluice run`.
pub fn run(args: &RunArgs) -> ExitCode {
    let env: EnvMap = std::env::vars().collect();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return fail(&format!("cannot read the current directory: {e}")),
    };
    let mode = cache_mode(&env);
    let ctx = match resolve(&env, &cwd, args, &mode) {
        Ok(ctx) => ctx,
        Err(e) => return fail(&e),
    };

    if args.explain {
        return explain(&ctx, &env, &mode);
    }
    if mode == CacheMode::Bypass {
        eprintln!(
            "sluice: SLUICE_DISABLE is set; running '{}' directly",
            ctx.gate_name
        );
        return run_inline(&ctx, &args.extra);
    }

    let state = match lookup::lookup(&ctx, &mode) {
        CacheState::Hit(hit) => {
            println!(
                "sluice: {} PASS (cached, passed {} ago)",
                ctx.gate_name,
                age(&hit.passed_at)
            );
            if let Ok(socket) = sluice_gate_core::dirs::socket_path(&env)
                && socket.exists()
            {
                client::send_hit_event(
                    &socket,
                    &ctx.paths.toplevel,
                    &ctx.gate_name,
                    hit.duration_ms,
                );
            }
            return ExitCode::SUCCESS;
        }
        CacheState::Off {
            reason,
            notice: Some(notice),
        } => {
            eprintln!("sluice: {notice}");
            CacheState::Off {
                reason,
                notice: None,
            }
        }
        state => state,
    };

    // Policy on a miss, only inside a hook and only when the cache was consulted:
    // with caching off or an unknown tree the gate can never be cached, so
    // refusing to run would block the caller forever.
    let (caller, in_hook, policy) = decide_policy(&ctx, &env, &state);
    if policy == HookPolicy::FailFast {
        let why = match &state {
            CacheState::Miss {
                last_miss: Some(m), ..
            } => format!(
                "last run failed {} ago{}",
                age(&m.recorded_at),
                m.exit_code
                    .map_or(String::new(), |c| format!(" with exit code {c}"))
            ),
            CacheState::Miss { .. } | CacheState::Hit(_) => {
                "no cached pass for this tree".to_owned()
            }
            CacheState::Off { reason, .. } => reason.clone(),
        };
        let rule = match &caller {
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
        let _ = in_hook;
        return ExitCode::from(EXIT_FAIL_FAST);
    }

    if policy == HookPolicy::RunUnqueued {
        return execute(&ctx, args, state);
    }

    let request = match run_request(&ctx, args, &caller, &state) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    match via_daemon(&ctx, args, request, &state) {
        Ok(code) => return code,
        Err(why) => eprintln!("sluice: daemon unavailable ({why}); running in-process"),
    }
    let _slots = match acquire_budget(&ctx) {
        Ok(slots) => slots,
        Err(e) => return fail(&e),
    };
    execute(&ctx, args, state)
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
        match caller {
            Caller::Agent { .. } => proto::Caller::Agent,
            Caller::Human => proto::Caller::Human,
        },
    ))
}

/// Converts the shared run material into the protocol request.
fn to_request(m: RunMaterial, caller: proto::Caller) -> RunRequest {
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
        caller,
        session: m.session,
        client_pid: std::process::id(),
        hook: m.hook,
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
/// the in-process run); once queued, every result is final.
fn via_daemon(
    ctx: &Context,
    args: &RunArgs,
    request: RunRequest,
    state: &CacheState,
) -> Result<ExitCode, String> {
    let socket = client::ensure_daemon(&ctx.env).map_err(|e| format!("{e:#}"))?;
    let conn = client::connect(&socket).map_err(|e| e.to_string())?;
    let gate = &ctx.gate_name;
    let mut queued = false;
    let mut follower: Option<LogFollower> = None;
    let result = conn.run(request, |msg| match msg {
        ServerMessage::Queued {
            job_id,
            position,
            coalesced,
            ..
        } if !queued => {
            queued = true;
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
        Ok(done) => Ok(report_outcome(ctx, state, &done.job_id, &done.outcome)),
        Err(ClientError::Closed | ClientError::Daemon(_)) if !queued => {
            Err("the daemon refused or dropped the request".to_owned())
        }
        Err(e) if !queued => Err(e.to_string()),
        Err(e) => Ok(fail(&format!("lost the connection to the daemon: {e}"))),
    }
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
    if outcome.status == OutcomeStatus::Killed && outcome.log_path.is_none() {
        // Dropped from the queue (daemon stop, cancellation): it never ran.
        eprintln!(
            "sluice: {} KILLED before it started (job {job_id}): the daemon stopped",
            ctx.gate_name
        );
        return ExitCode::from(EXIT_KILLED);
    }
    let finish = match outcome.status {
        OutcomeStatus::Cached => {
            let passed = match state {
                CacheState::Miss { key, .. } => match ctx.store.lookup(key, ctx.ttl) {
                    Ok(Lookup::Hit(m)) => format!(", passed {} ago", age(&m.passed_at)),
                    _ => String::new(),
                },
                CacheState::Off { .. } | CacheState::Hit(_) => String::new(),
            };
            println!("sluice: {} PASS (cached{passed})", ctx.gate_name);
            return ExitCode::SUCCESS;
        }
        OutcomeStatus::Error => return fail(&outcome.message),
        OutcomeStatus::Pass => Finish::Pass {
            note: match (state, &outcome.not_cached_reason) {
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
            eprintln!("sluice: {gate} KILLED after {secs:.1}s (job {job}) log={log}");
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
        (Caller::Agent { .. }, true, CacheState::Miss { .. } | CacheState::Hit(_)) => {
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
fn explain(ctx: &Context, env: &EnvMap, mode: &CacheMode) -> ExitCode {
    println!("repository:   {}", ctx.paths.toplevel.display());
    println!("common dir:   {}", ctx.paths.common_dir.display());
    println!(
        "git:          {}",
        git::version().unwrap_or_else(|e| e.to_string())
    );
    println!("gate:         {} ({})", ctx.gate_name, ctx.gate.cmd.trim());
    println!("cache mode:   {mode:?}");
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
    match tree::snapshot(&ctx.paths, &ctx.probe_env, &ctx.scratch) {
        Ok(Snapshot::Known(tree)) => {
            println!("tree:         {tree}");
            match lookup::derive_key(ctx, &tree) {
                Ok((_, key)) => {
                    println!("key:          {key}");
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
        Ok(Snapshot::Unknown { reason }) => println!("tree:         unknown ({reason})"),
        Err(e) => println!("tree:         error ({e})"),
    }
    println!("markers:      {}", ctx.store.dir().display());
    ExitCode::SUCCESS
}

/// Resolves repository, config and derived environments.
fn resolve(env: &EnvMap, cwd: &Path, args: &RunArgs, mode: &CacheMode) -> Result<Context, String> {
    let res = lookup::resolve(cwd, env, &args.gate, mode).map_err(|e| e.to_string())?;
    Ok(Context {
        tail_lines: res.tail_lines(),
        res,
        hook: args.hook.clone(),
    })
}

/// Takes `weight` slots of the global semaphore, all or nothing.
fn acquire_budget(ctx: &Context) -> Result<Vec<SlotGuard>, String> {
    let slots = ctx.user.daemon.budget.max(1);
    let needed = ctx.gate.weight.clamp(1, slots) as usize;
    let semaphore =
        Semaphore::new(&ctx.scratch.join("semaphore"), slots).map_err(|e| e.to_string())?;
    let mut backoff = Duration::from_millis(25);
    let mut announced = false;
    loop {
        let mut guards = Vec::with_capacity(needed);
        while guards.len() < needed {
            match semaphore.try_acquire().map_err(|e| e.to_string())? {
                Some(guard) => guards.push(guard),
                None => break,
            }
        }
        if guards.len() == needed {
            return Ok(guards);
        }
        drop(guards);
        if !announced {
            eprintln!(
                "sluice: waiting for {needed} of {slots} budget units for '{}'",
                ctx.gate_name
            );
            announced = true;
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(500));
    }
}

/// Builds the gate command: `sh -c '<cmd>'`, or `sh -c '<cmd> "$@"' sh <extra...>`
/// when extra arguments are given (appending `"$@"` to a compound command such
/// as a `for` loop would be a syntax error, so it is only added when needed).
fn gate_command(ctx: &Context, extra: &[String]) -> Result<Command, String> {
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
        .envs(ctx.child_env()?);
    Ok(cmd)
}

/// `SLUICE_DISABLE=1`: run the gate with inherited stdio and exit with its code.
fn run_inline(ctx: &Context, extra: &[String]) -> ExitCode {
    let mut cmd = match gate_command(ctx, extra) {
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
            CacheState::Off { reason, .. } => format!(" (not cached: {reason})"),
            CacheState::Hit(_) => String::new(),
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
            tail: &[],
        },
    )
}

fn spawn_and_wait(ctx: &Context, args: &RunArgs) -> Result<Execution, String> {
    let logs = dirs::logs_dir(&ctx.env);
    sluice_gate_core::dirs::create_private_dir(&logs)
        .map_err(|e| format!("cannot create {}: {e}", logs.display()))?;
    let job_id = dirs::job_id();
    let log_path = logs.join(format!("{job_id}.log"));
    let log = File::create(&log_path)
        .map_err(|e| format!("cannot create {}: {e}", log_path.display()))?;
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

    let mut child = gate_command(ctx, &args.extra)?
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
    Ok(Execution {
        exit_code: status.code(),
        timed_out,
        duration,
        log_path,
        job_id,
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
fn pump(reader: &mut dyn Read, log: &Mutex<File>, verbose: bool) {
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
    fn exit_code_maps_signals_to_143() {
        assert_eq!(exit_code_of(Some(0)), ExitCode::SUCCESS);
        assert_eq!(exit_code_of(None), ExitCode::from(143));
    }
}
