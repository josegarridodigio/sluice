//! `sluice run <gate>`: the cache-hit fast path and, on a miss, the gate run.
//!
//! Phase 1 behaviour (no daemon yet):
//!
//! 1. Resolve the repository, `.sluice.toml`, the gate and the user config.
//! 2. `SLUICE_DISABLE=1` runs the gate directly with inherited stdio.
//! 3. Snapshot the working tree, derive the key, look for a marker: a hit
//!    prints one line and exits 0 without running anything.
//! 4. On a miss inside a hook invoked by an agent, apply `agent_hook_policy`
//!    (`fail-fast` by default). Otherwise wait for a slot of the global
//!    `flock` semaphore (one slot per budget unit; a gate takes `weight`
//!    slots, all or nothing) and run the gate.
//! 5. Output goes to `<state>/logs/<job-id>.log`; the caller gets one line
//!    on success and the tail of the log on failure.
//! 6. A pass whose tree is unchanged writes a marker; a pass that modified
//!    the tree, and any failure, writes a miss record.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use sluice_gate_core::agent::{self, Caller};
use sluice_gate_core::config::{Gate, HookPolicy, RepoConfig, UserConfig};
use sluice_gate_core::git::{self, EnvMap, RepoPaths};
use sluice_gate_core::key::{self, CacheKey, KeyMaterial};
use sluice_gate_core::marker::{Lookup, Marker, MarkerStore, MissRecord};
use sluice_gate_core::mode::{CacheMode, cache_mode};
use sluice_gate_core::semaphore::{Semaphore, SlotGuard};
use sluice_gate_core::tree::{self, Snapshot, TreeHash};
use sluice_gate_core::{CACHE_FORMAT_VERSION, SLUICE_VERSION};

use crate::dirs;

/// Exit code when `agent_hook_policy = "fail-fast"` refuses to run.
pub const EXIT_FAIL_FAST: u8 = 3;
/// Exit code when the gate exceeded its timeout (same as `timeout(1)`).
pub const EXIT_TIMEOUT: u8 = 124;
/// Exit code for Sluice's own errors (bad config, no repository, ...).
pub const EXIT_ERROR: u8 = 1;

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
}

/// Everything resolved before deciding between hit, policy and run.
struct Context {
    env: EnvMap,
    paths: RepoPaths,
    user: UserConfig,
    gate_name: String,
    gate: Gate,
    /// Environment of the gate process, computed on first use (it costs a git spawn).
    child_env: std::cell::OnceCell<EnvMap>,
    probe_env: EnvMap,
    scratch: PathBuf,
    store: MarkerStore,
    ttl: Duration,
    hook: Option<String>,
}

/// Outcome of a cache lookup for the current tree.
#[allow(clippy::large_enum_variant)] // one value per invocation; boxing buys nothing
enum CacheState {
    /// Caching is off for this invocation.
    Off(String),
    /// The tree is known and no marker matched.
    Miss {
        tree: TreeHash,
        key: CacheKey,
        cmd: String,
        last_miss: Option<MissRecord>,
    },
}

/// Entry point of `sluice run`.
pub fn run(args: &RunArgs) -> ExitCode {
    let env: EnvMap = std::env::vars().collect();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return fail(&format!("cannot read the current directory: {e}")),
    };
    let ctx = match resolve(&env, &cwd, args) {
        Ok(ctx) => ctx,
        Err(e) => return fail(&e),
    };

    let mode = cache_mode(&env);
    if mode == CacheMode::Bypass {
        eprintln!(
            "sluice: SLUICE_DISABLE is set; running '{}' directly",
            ctx.gate_name
        );
        return run_inline(&ctx, &args.extra);
    }

    let state = match lookup(&ctx, &mode) {
        Decision::Hit(hit) => {
            println!(
                "sluice: {} PASS (cached, passed {} ago)",
                ctx.gate_name,
                age(&hit.passed_at)
            );
            return ExitCode::SUCCESS;
        }
        Decision::Run(state) => state,
    };

    // Policy on a miss, only inside a hook.
    let in_hook =
        ctx.hook.is_some() || env.contains_key("GIT_DIR") || env.contains_key("GIT_INDEX_FILE");
    let caller = agent::detect(
        &env,
        &ctx.user.agent_detection.env_any_set,
        &ctx.user.agent_detection.env_any_equals,
    );
    let defaults = &RepoConfig::load(&ctx.paths.toplevel)
        .map(|c| c.defaults)
        .unwrap_or_default();
    let policy = match (&caller, in_hook) {
        (Caller::Agent { .. }, true) => defaults.agent_hook_policy,
        (Caller::Human, true) => defaults.human_hook_policy,
        (_, false) => HookPolicy::Wait,
    };
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
            CacheState::Miss { .. } => "no cached pass for this tree".to_owned(),
            CacheState::Off(reason) => reason.clone(),
        };
        let rule = match &caller {
            Caller::Agent { rule } => rule.as_str(),
            Caller::Human => "human",
        };
        eprintln!(
            "sluice: '{}' has not passed on this tree ({why}).\n\
             sluice: this hook was invoked by a coding agent ({rule}) and agent_hook_policy is \"fail-fast\".\n\
             sluice: run 'sluice run {}' or the 'verify' MCP tool first, then retry the commit.",
            ctx.gate_name, ctx.gate_name
        );
        return ExitCode::from(EXIT_FAIL_FAST);
    }

    let _slots = if policy == HookPolicy::RunUnqueued {
        Vec::new()
    } else {
        match acquire_budget(&ctx) {
            Ok(slots) => slots,
            Err(e) => return fail(&e),
        }
    };

    execute(&ctx, args, state)
}

fn fail(message: &str) -> ExitCode {
    eprintln!("sluice: error: {message}");
    ExitCode::from(EXIT_ERROR)
}

/// Resolves repository, config and derived environments.
fn resolve(env: &EnvMap, cwd: &Path, args: &RunArgs) -> Result<Context, String> {
    let paths = git::discover(cwd, env).map_err(|e| e.to_string())?;
    let config = RepoConfig::load(&paths.toplevel).map_err(|e| e.to_string())?;
    let gate = config.gate(&args.gate).map_err(|e| e.to_string())?.clone();
    let user = UserConfig::load(env).map_err(|e| e.to_string())?;
    let probe_env = git::probe_env(env, Some(&paths));
    let scratch = tree::scratch_root(env).map_err(|e| e.to_string())?;
    let store = MarkerStore::new(&paths.common_dir);
    let ttl = Duration::from_hours(u64::from(user.cache.ttl_days) * 24);
    Ok(Context {
        env: env.clone(),
        paths,
        user,
        gate_name: args.gate.clone(),
        gate,
        child_env: std::cell::OnceCell::new(),
        probe_env,
        scratch,
        store,
        ttl,
        hook: args.hook.clone(),
    })
}

/// What `lookup` decided for this invocation.
#[allow(clippy::large_enum_variant)]
enum Decision {
    /// A fresh marker matched: nothing to run.
    Hit(Marker),
    /// Run the gate; the state says whether a marker may be written afterwards.
    Run(CacheState),
}

/// Looks the current tree up in the marker store.
fn lookup(ctx: &Context, mode: &CacheMode) -> Decision {
    if let CacheMode::Disabled { reason } = mode {
        eprintln!("sluice: cache disabled ({reason})");
        return Decision::Run(CacheState::Off(reason.clone()));
    }
    if !ctx.gate.cache {
        return Decision::Run(CacheState::Off(format!(
            "gate '{}' has cache = false",
            ctx.gate_name
        )));
    }
    let tree = match tree::snapshot(&ctx.paths, &ctx.probe_env, &ctx.scratch) {
        Ok(Snapshot::Known(tree)) => tree,
        Ok(Snapshot::Unknown { reason }) => {
            eprintln!("sluice: tree unknown ({reason}); running uncached");
            return Decision::Run(CacheState::Off(format!("tree unknown: {reason}")));
        }
        Err(e) => {
            eprintln!("sluice: cannot snapshot the tree ({e}); running uncached");
            return Decision::Run(CacheState::Off(format!("snapshot failed: {e}")));
        }
    };
    let (cmd, key) = match derive_key(ctx, &tree) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sluice: cannot derive the cache key ({e}); running uncached");
            return Decision::Run(CacheState::Off(format!("key derivation failed: {e}")));
        }
    };
    match ctx.store.lookup(&key, ctx.ttl) {
        Ok(Lookup::Hit(marker)) => Decision::Hit(marker),
        Ok(Lookup::Miss(last_miss)) => Decision::Run(CacheState::Miss {
            tree,
            key,
            cmd,
            last_miss,
        }),
        Err(e) => {
            eprintln!("sluice: cannot read the cache ({e}); running uncached");
            Decision::Run(CacheState::Off(format!("cache read failed: {e}")))
        }
    }
}

impl Context {
    /// The caller's environment minus git hook and `CLAUDE*` variables, plus the gate's `env`.
    fn child_env(&self) -> Result<&EnvMap, String> {
        if let Some(env) = self.child_env.get() {
            return Ok(env);
        }
        let mut env = git::child_env(&self.env).map_err(|e| e.to_string())?;
        env.extend(self.gate.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        Ok(self.child_env.get_or_init(|| env))
    }
}

/// Derives the cache key for `tree`; returns the normalised command as well.
fn derive_key(ctx: &Context, tree: &TreeHash) -> Result<(String, CacheKey), String> {
    let (key_inputs, key_files) = if ctx.gate.key_inputs.is_empty() && ctx.gate.key_files.is_empty()
    {
        (Vec::new(), Vec::new())
    } else {
        key::collect_inputs(
            &ctx.gate.key_inputs,
            &ctx.gate.key_files,
            &ctx.paths.toplevel,
            ctx.child_env()?,
        )
        .map_err(|e| e.to_string())?
    };
    let cmd = ctx.gate.normalized_cmd();
    let material = KeyMaterial {
        tree: tree.clone(),
        gate_name: ctx.gate_name.clone(),
        cmd: cmd.clone(),
        env: ctx.gate.env.clone(),
        key_inputs,
        key_files,
    };
    Ok((cmd, key::derive(&material)))
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

/// Builds the gate command: `sh -c '<cmd> "$@"' sh <extra...>`.
fn gate_command(ctx: &Context, extra: &[String]) -> Result<Command, String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{} \"$@\"", ctx.gate.cmd.trim()))
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
        None => ExitCode::from(128 + 15),
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

/// Runs the gate with output captured to a log file, enforcing the timeout.
fn execute(ctx: &Context, args: &RunArgs, state: CacheState) -> ExitCode {
    let exec = match spawn_and_wait(ctx, args) {
        Ok(exec) => exec,
        Err(e) => return fail(&e),
    };
    let secs = exec.duration.as_secs_f64();
    let gate = &ctx.gate_name;

    if exec.timed_out {
        if let CacheState::Miss { key, .. } = &state {
            record_miss(ctx, key, None, "timed out", &exec);
        }
        eprintln!(
            "sluice: {gate} TIMEOUT after {} (job {}) log={}",
            humantime::format_duration(ctx.gate.timeout),
            exec.job_id,
            exec.log_path.display()
        );
        print_tail(&exec.log_path, ctx.user.cache.tail_lines);
        return ExitCode::from(EXIT_TIMEOUT);
    }

    if exec.exit_code != Some(0) {
        if let CacheState::Miss { key, .. } = &state {
            record_miss(ctx, key, exec.exit_code, "gate failed", &exec);
        }
        let code = exec
            .exit_code
            .map_or_else(|| "signal".to_owned(), |c| c.to_string());
        eprintln!(
            "sluice: {gate} FAIL exit={code} {secs:.1}s (job {}) log={}",
            exec.job_id,
            exec.log_path.display()
        );
        print_tail(&exec.log_path, ctx.user.cache.tail_lines);
        return exit_code_of(exec.exit_code);
    }

    let note = match state {
        CacheState::Off(reason) => format!(" (not cached: {reason})"),
        CacheState::Miss { tree, key, cmd, .. } => {
            match write_marker(ctx, &tree, &key, &cmd, &exec) {
                Ok(()) => String::new(),
                Err(reason) => format!(" (not cached: {reason})"),
            }
        }
    };
    println!("sluice: {gate} PASS {secs:.1}s (job {}){note}", exec.job_id);
    ExitCode::SUCCESS
}

fn spawn_and_wait(ctx: &Context, args: &RunArgs) -> Result<Execution, String> {
    let logs = dirs::logs_dir(&ctx.env);
    fs::create_dir_all(&logs).map_err(|e| format!("cannot create {}: {e}", logs.display()))?;
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
        if started.elapsed() > ctx.gate.timeout {
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
        tree::index_tree(&ctx.paths, &ctx.probe_env, &ctx.scratch)
            .ok()
            .and_then(|s| s.tree().cloned())
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

/// Prints the last `lines` lines of the log to stderr.
pub fn print_tail(path: &Path, lines: usize) {
    let Ok(file) = File::open(path) else { return };
    let all: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
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
