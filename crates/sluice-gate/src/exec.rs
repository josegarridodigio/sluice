//! `sluice exec [options] -- <cmd> [args...]`: run any command under the
//! budget, without cache.
//!
//! Design: `kb/sluice/design/command-control.md` section 2 (ADR-0010).
//!
//! 1. Inside a Sluice job (`SLUICE_JOB` set) the command replaces this
//!    process directly: the parent job's weight already covers it.
//! 2. `argv[0]` is resolved in `PATH`, skipping Sluice's shims (127 / 126
//!    when it cannot run).
//! 3. The daemon (started on demand) gets an `exec` request; the command
//!    runs in its own process group with stdin `/dev/null` and its output
//!    framed into a spool file that this client replays byte for byte to
//!    its own stdout and stderr. SIGINT, SIGTERM and SIGHUP are forwarded.
//!    The exit code is the command's (128 + N when killed by signal N).
//! 4. Without a usable daemon (down, too old to announce `exec`, refusing
//!    before `queued`) the command runs in-process under the `flock`
//!    budget semaphore, with inherited stdio.

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{SigSet, Signal, killpg};
use nix::sys::signalfd::SignalFd;
use nix::unistd::{Pid, getpgrp, tcgetpgrp, tcsetpgrp};
use sluice_gate_core::agent::{self, Caller};
use sluice_gate_core::config::UserConfig;
use sluice_gate_core::git::{self, EnvMap};
use sluice_gate_core::mode::{CacheMode, cache_mode};
use sluice_gate_core::{JOB_ENV_VAR, tree, which};
use sluice_gate_daemon::executor::spool;
use sluice_gate_proto::client::{self, ClientError, ExecConn, ExecControl};
use sluice_gate_proto::{
    self as proto, ExecRequest, ExecSignal, FEATURE_EXEC, Outcome, OutcomeStatus, ServerMessage,
};

/// Exit code for a Sluice error after the request was accepted.
pub const EXIT_SLUICE: u8 = 125;
/// Exit code of `--timeout` (same as `timeout(1)`).
const EXIT_TIMEOUT: u8 = 124;
/// Exit code when the daemon killed the job (128 + SIGTERM).
const EXIT_KILLED: u8 = 143;
/// Exit code when our stdout is a closed pipe (128 + SIGPIPE).
const EXIT_PIPE: u8 = 141;
/// Maximum label length.
const LABEL_MAX: usize = 48;
/// The queued line is printed once the request has waited this long.
const QUEUED_NOTICE_AFTER: Duration = Duration::from_secs(1);
/// Spool poll period while output flows, and its idle ceiling.
const POLL_MIN: Duration = Duration::from_millis(20);
const POLL_MAX: Duration = Duration::from_millis(100);
/// Grace between SIGTERM and SIGKILL on an in-process timeout.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Arguments of `sluice exec`.
#[derive(Debug)]
pub struct ExecArgs {
    /// Budget units (`--weight`); default `ceil(budget / 2)`.
    pub weight: Option<u32>,
    /// Label shown in `status` and messages (`--label`).
    pub label: Option<String>,
    /// Coalescing key (`--coalesce-key`).
    pub coalesce_key: Option<String>,
    /// Timeout (`--timeout`).
    pub timeout: Option<Duration>,
    /// Omit Sluice's informational lines (errors are still printed).
    pub quiet: bool,
    /// Command and arguments.
    pub argv: Vec<String>,
}

/// Everything resolved before choosing the daemon or the in-process path.
struct Job {
    program: PathBuf,
    argv: Vec<String>,
    label: String,
    weight: u32,
    budget: u32,
    cwd: PathBuf,
    env: EnvMap,
    user: UserConfig,
    timeout: Option<Duration>,
    coalesce: Option<String>,
    quiet: bool,
}

impl Job {
    fn info(&self, line: &str) {
        if !self.quiet {
            eprintln!("sluice: {line}");
        }
    }
}

/// Characters kept in a label besides ASCII letters and digits.
fn label_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || " ._:/-".contains(c)
}

/// Keeps letters, digits, space and `._:/-`, at most 48 characters.
fn sanitize_label(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|c| label_char(*c))
        .take(LABEL_MAX)
        .collect();
    let kept = kept.trim();
    if kept.is_empty() {
        "exec".to_owned()
    } else {
        kept.to_owned()
    }
}

/// The first two words of argv, the program as its base name (`cargo build`).
fn default_label(argv: &[String]) -> String {
    let first = argv.first().map_or("", |a| {
        Path::new(a)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(a)
    });
    let words: Vec<&str> = std::iter::once(first)
        .chain(argv.get(1).map(String::as_str))
        .collect();
    sanitize_label(&words.join(" "))
}

/// Repository identity for the per-repository cap: the main worktree (the
/// parent of a common dir named `.git`) or the common dir itself.
fn repo_identity(cwd: &Path, env: &EnvMap) -> Option<PathBuf> {
    let paths = git::discover(cwd, env).ok()?;
    let common = paths.common_dir;
    if common.file_name().is_some_and(|n| n == ".git")
        && let Some(parent) = common.parent()
    {
        return Some(parent.to_path_buf());
    }
    Some(common)
}

/// The signals forwarded to the command.
fn forwarded_signals() -> SigSet {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGHUP);
    mask
}

fn exec_signal(signo: i32) -> Option<ExecSignal> {
    match Signal::try_from(signo).ok()? {
        Signal::SIGINT => Some(ExecSignal::Int),
        Signal::SIGTERM => Some(ExecSignal::Term),
        Signal::SIGHUP => Some(ExecSignal::Hup),
        _ => None,
    }
}

fn code(n: i32) -> ExitCode {
    ExitCode::from(u8::try_from(n & 0xff).unwrap_or(EXIT_SLUICE))
}

fn signal_code(signo: i32) -> ExitCode {
    code(128 + signo)
}

fn sluice_error(message: &str) -> ExitCode {
    eprintln!("sluice: exec: {message}");
    ExitCode::from(EXIT_SLUICE)
}

/// Entry point of `sluice exec`.
pub fn exec(args: &ExecArgs) -> ExitCode {
    let env: EnvMap = sluice_gate_core::config::process_env();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return sluice_error(&format!("cannot read the current directory: {e}")),
    };
    let Some(name) = args.argv.first() else {
        return sluice_error("no command given");
    };
    let program = match which::resolve(name, &env, &cwd, true) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sluice: exec: cannot run '{name}': {e}");
            return ExitCode::from(e.exit_code());
        }
    };
    if let Some(parent) = env.get(JOB_ENV_VAR).filter(|v| !v.is_empty()) {
        // Nested inside a Sluice job: no second job, no queue (the parent
        // holds the budget; queueing here could wait for it forever).
        tracing::debug!(parent = %parent, "nested exec; running directly");
        let err = Command::new(&program)
            .arg0(name)
            .args(&args.argv[1..])
            .exec();
        eprintln!("sluice: exec: cannot run '{name}': {err}");
        return ExitCode::from(126);
    }
    let user = match UserConfig::load(&env) {
        Ok(u) => u,
        Err(e) => {
            if !args.quiet {
                eprintln!("sluice: warning: {e}; using the default settings");
            }
            UserConfig::default()
        }
    };
    let budget = user.daemon.budget.max(1);
    let job = Job {
        program,
        argv: args.argv.clone(),
        label: args
            .label
            .as_deref()
            .map_or_else(|| default_label(&args.argv), sanitize_label),
        weight: args
            .weight
            .unwrap_or_else(|| user.agent.exec_weight_or(budget))
            .max(1),
        budget,
        cwd,
        env,
        user,
        timeout: args.timeout.filter(|t| !t.is_zero()),
        coalesce: args.coalesce_key.clone(),
        quiet: args.quiet,
    };
    if cache_mode(&job.env) == CacheMode::Bypass {
        // SLUICE_DISABLE=1: a direct run (with a terminal, if there is one).
        return in_process(&job);
    }
    if !sluice_gate_core::config::process_env_is_utf8() {
        // The protocol carries the environment as UTF-8 strings: a daemon
        // run would lose the other entries. The in-process run inherits the
        // raw environment unchanged.
        job.info(&format!(
            "the environment is not valid UTF-8; running '{}' in-process",
            job.label
        ));
        return in_process(&job);
    }
    match via_daemon(&job) {
        Ok(code) => code,
        Err(why) => {
            job.info(&format!(
                "daemon unavailable ({why}); running '{}' in-process",
                job.label
            ));
            in_process(&job)
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon path
// ---------------------------------------------------------------------------

/// Where the request stands, shared with the signal and notice threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Queued,
    Running,
    Done,
}

/// State shared by the main thread and its helpers.
struct Shared {
    phase: Mutex<Phase>,
    /// First signal received (0 = none).
    signal: AtomicI32,
    /// The phase when the first signal arrived was `Queued`.
    signalled_while_queued: AtomicBool,
    /// The queued notice was printed (so `started after` follows).
    noticed: AtomicBool,
}

impl Shared {
    fn phase(&self) -> Phase {
        *self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_phase(&self, p: Phase) {
        *self.phase.lock().unwrap_or_else(PoisonError::into_inner) = p;
    }
}

/// Reads blocked signals from a signalfd and acts on them: hang up while
/// queued, forward while running, hang up on a second signal.
fn signal_thread(fd: SignalFd, control: Arc<ExecControl>, shared: Arc<Shared>) {
    thread::spawn(move || {
        loop {
            let info = match fd.read_signal() {
                Ok(Some(info)) => info,
                Ok(None) => continue,
                Err(_) => return,
            };
            let signo = i32::try_from(info.ssi_signo).unwrap_or(0);
            let first = shared
                .signal
                .compare_exchange(0, signo, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
            let phase = {
                let guard = shared.phase.lock().unwrap_or_else(PoisonError::into_inner);
                *guard
            };
            if !first {
                control.hang_up();
                continue;
            }
            match (phase, exec_signal(signo)) {
                (Phase::Running, Some(sig)) => {
                    if control.signal(sig).is_err() {
                        control.hang_up();
                    }
                }
                (Phase::Queued, _) => {
                    shared.signalled_while_queued.store(true, Ordering::SeqCst);
                    control.hang_up();
                }
                _ => control.hang_up(),
            }
        }
    });
}

/// Prints the queued line once the request has waited a second.
fn notice_thread(job_label: String, socket: PathBuf, position: usize, shared: Arc<Shared>) {
    thread::spawn(move || {
        thread::sleep(QUEUED_NOTICE_AFTER);
        if shared.phase() != Phase::Queued {
            return;
        }
        let busy = client::connect(&socket)
            .and_then(client::Client::status)
            .map(|s| format!(", {} of {} units busy", s.budget.used, s.budget.total))
            .unwrap_or_default();
        if shared.phase() == Phase::Queued {
            shared.noticed.store(true, Ordering::SeqCst);
            eprintln!("sluice: exec '{job_label}' queued (position {position}{busy})");
        }
    });
}

/// Replays a spool file to fd 1 and fd 2 until told to finish, then to EOF.
struct Replay {
    finish: Arc<AtomicBool>,
    broken_pipe: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl Replay {
    fn start(path: PathBuf, control: Arc<ExecControl>) -> Self {
        let finish = Arc::new(AtomicBool::new(false));
        let broken_pipe = Arc::new(AtomicBool::new(false));
        let (f, b) = (Arc::clone(&finish), Arc::clone(&broken_pipe));
        let thread = thread::spawn(move || {
            if let Err(e) = replay(&path, &f) {
                if e.kind() == ErrorKind::BrokenPipe {
                    b.store(true, Ordering::SeqCst);
                    // A closed pipe ends the job, as it would in a pipeline.
                    control.hang_up();
                } else {
                    eprintln!("sluice: exec: cannot replay {}: {e}", path.display());
                }
            }
        });
        Self {
            finish,
            broken_pipe,
            thread,
        }
    }

    /// Drains the spool to EOF and returns whether stdout was a closed pipe.
    fn finish(self) -> bool {
        self.finish.store(true, Ordering::SeqCst);
        self.thread.thread().unpark();
        let _ = self.thread.join();
        self.broken_pipe.load(Ordering::SeqCst)
    }
}

/// The replay loop: whole records only, stream 1 to stdout, 2 to stderr.
fn replay(path: &Path, finish: &AtomicBool) -> std::io::Result<()> {
    let mut file = File::open(path)?;
    let mut pending: Vec<u8> = Vec::new();
    let mut magic_seen = false;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut idle = POLL_MIN;
    let (mut out, mut err) = (std::io::stdout().lock(), std::io::stderr().lock());
    loop {
        // Read before checking `finish`: after `done` one more pass reads
        // everything up to EOF.
        let last = finish.load(Ordering::SeqCst);
        let mut progressed = false;
        loop {
            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            progressed = true;
            pending.extend_from_slice(&chunk[..n]);
            if !magic_seen {
                if pending.len() < spool::MAGIC.len() {
                    continue;
                }
                if !pending.starts_with(spool::MAGIC) {
                    return Err(std::io::Error::other("not a sluice spool file"));
                }
                pending.drain(..spool::MAGIC.len());
                magic_seen = true;
            }
            let (records, used) = spool::parse(&pending);
            for (stream, data) in records {
                if stream == spool::STDERR {
                    err.write_all(data)?;
                    err.flush()?;
                } else {
                    out.write_all(data)?;
                }
            }
            out.flush()?;
            pending.drain(..used);
        }
        if last {
            return Ok(());
        }
        idle = if progressed {
            POLL_MIN
        } else {
            (idle * 2).min(POLL_MAX)
        };
        thread::park_timeout(idle);
    }
}

/// Runs the command through the daemon. `Err` carries why the daemon
/// could not be used before the request was queued (the caller falls back
/// in-process); after `queued` every result is final.
fn via_daemon(job: &Job) -> Result<ExitCode, String> {
    let socket = client::ensure_daemon(&job.env).map_err(|e| format!("{e:#}"))?;
    let conn = client::connect(&socket).map_err(|e| e.to_string())?;
    if !conn.supports(FEATURE_EXEC) {
        return Err(
            "the running daemon does not support exec; `sluice daemon restart` upgrades it"
                .to_owned(),
        );
    }
    let request = exec_request(job);

    // Block the forwarded signals before any thread starts (threads inherit
    // the mask); the signal thread reads them from a signalfd.
    let mask = forwarded_signals();
    mask.thread_block()
        .map_err(|e| format!("cannot block signals: {e}"))?;
    let unblock = |why: String| {
        let _ = mask.thread_unblock();
        why
    };
    let fd = SignalFd::new(&mask).map_err(|e| unblock(format!("cannot read signals: {e}")))?;
    let mut conn = conn.exec(request).map_err(|e| unblock(e.to_string()))?;
    let control = Arc::new(conn.control().map_err(|e| unblock(e.to_string()))?);
    // The first reply decides: `queued`, or an error that means "fall back".
    let first = match conn.recv() {
        Ok(m @ ServerMessage::Queued { .. }) => m,
        Ok(other) => return Err(unblock(format!("unexpected reply to exec: {other:?}"))),
        Err(ClientError::Closed) => {
            return Err(unblock("the daemon closed the connection".to_owned()));
        }
        Err(e) => return Err(unblock(e.to_string())),
    };
    let shared = Arc::new(Shared {
        phase: Mutex::new(Phase::Queued),
        signal: AtomicI32::new(0),
        signalled_while_queued: AtomicBool::new(false),
        noticed: AtomicBool::new(false),
    });
    signal_thread(fd, Arc::clone(&control), Arc::clone(&shared));
    Ok(follow(job, &socket, conn, &control, &shared, first))
}

/// The `exec` request for `job`.
fn exec_request(job: &Job) -> ExecRequest {
    ExecRequest {
        argv: std::iter::once(job.program.display().to_string())
            .chain(job.argv[1..].iter().cloned())
            .collect(),
        cwd: job.cwd.clone(),
        env: job.env.clone(),
        label: job.label.clone(),
        weight: job.weight,
        timeout_ms: job
            .timeout
            .map_or(0, |t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
        coalesce: job.coalesce.clone(),
        repo: repo_identity(&job.cwd, &job.env),
        caller: match agent::detect(
            &job.env,
            &job.user.agent_detection.env_any_set,
            &job.user.agent_detection.env_any_equals,
        ) {
            Caller::Agent { .. } => proto::Caller::Agent,
            Caller::Human => proto::Caller::Human,
        },
        session: job
            .env
            .get("CLAUDE_CODE_SESSION_ID")
            .filter(|s| !s.is_empty())
            .cloned(),
        client_pid: std::process::id(),
    }
}

/// Follows an accepted request: notices, spool replay, signals, exit code.
fn follow(
    job: &Job,
    socket: &Path,
    mut conn: ExecConn,
    control: &Arc<ExecControl>,
    shared: &Arc<Shared>,
    first: ServerMessage,
) -> ExitCode {
    let queued_at = Instant::now();
    let mut job_id = String::new();
    let mut replay: Option<Replay> = None;
    let mut started = false;
    let mut msg = Ok(first);
    let result = loop {
        match msg {
            Ok(ServerMessage::Queued {
                job_id: id,
                position,
                coalesced,
                ..
            }) => {
                job_id = id;
                if coalesced {
                    job.info(&format!("exec '{}' sharing job {job_id}", job.label));
                }
                if !job.quiet {
                    notice_thread(
                        job.label.clone(),
                        socket.to_path_buf(),
                        position,
                        Arc::clone(shared),
                    );
                }
            }
            Ok(ServerMessage::Started { spool, .. }) => {
                started = true;
                shared.set_phase(Phase::Running);
                if shared.noticed.load(Ordering::SeqCst) {
                    job.info(&format!(
                        "exec '{}' started after {:.1}s (job {job_id})",
                        job.label,
                        queued_at.elapsed().as_secs_f64()
                    ));
                }
                if replay.is_none()
                    && let Some(path) = spool
                {
                    replay = Some(Replay::start(path, Arc::clone(control)));
                }
            }
            Ok(ServerMessage::Done { outcome, .. }) => {
                shared.set_phase(Phase::Done);
                break Ok(outcome);
            }
            Ok(_) => {}
            Err(e) => {
                shared.set_phase(Phase::Done);
                break Err(e);
            }
        }
        msg = conn.recv();
    };
    let broken_pipe = replay.is_some_and(Replay::finish);
    let signo = shared.signal.load(Ordering::SeqCst);
    if broken_pipe {
        return ExitCode::from(EXIT_PIPE);
    }
    match result {
        Ok(outcome) => report(job, &job_id, &outcome, signo),
        Err(_) if signo != 0 => {
            if shared.signalled_while_queued.load(Ordering::SeqCst) {
                job.info(&format!("exec '{}' interrupted while queued", job.label));
            }
            signal_code(signo)
        }
        Err(e) if e.is_disconnect() && started => sluice_error(&format!(
            "the daemon died while running exec '{}'; it was killed",
            job.label
        )),
        Err(e) => sluice_error(&format!("lost the connection to the daemon: {e}")),
    }
}

/// The headline of an `exec` killed for lack of memory, worded like the one
/// of `sluice run` and MCP `verify`; the daemon's message follows it.
fn memory_kill_headline(label: &str, secs: f64, job_id: &str) -> String {
    format!("sluice: exec '{label}' KILLED for lack of memory after {secs:.1}s (job {job_id})")
}

/// Maps the daemon's outcome to the exit code (and the few lines worth printing).
fn report(job: &Job, job_id: &str, outcome: &Outcome, signo: i32) -> ExitCode {
    match outcome.status {
        OutcomeStatus::Pass | OutcomeStatus::Cached => ExitCode::SUCCESS,
        OutcomeStatus::Fail => match (outcome.signal, outcome.exit_code) {
            (Some(s), _) => signal_code(s),
            (None, Some(n)) => code(n),
            (None, None) => sluice_error(&outcome.message),
        },
        OutcomeStatus::Timeout => {
            let d = job.timeout.unwrap_or_default();
            eprintln!(
                "sluice: exec '{}' TIMEOUT after {}",
                job.label,
                humantime::format_duration(d)
            );
            ExitCode::from(EXIT_TIMEOUT)
        }
        // Killed by the kernel or `systemd-oomd` for lack of memory: say so
        // (an agent must not retry it blindly), not "the daemon stopped".
        OutcomeStatus::Killed if outcome.killed_for_memory() => {
            eprintln!(
                "{}",
                memory_kill_headline(
                    &job.label,
                    Duration::from_millis(outcome.duration_ms).as_secs_f64(),
                    job_id
                )
            );
            eprintln!("sluice: {}", outcome.message);
            ExitCode::from(EXIT_KILLED)
        }
        OutcomeStatus::Killed if signo != 0 => signal_code(signo),
        OutcomeStatus::Killed => {
            eprintln!(
                "sluice: exec '{}' KILLED: the daemon stopped (job {job_id})",
                job.label
            );
            ExitCode::from(EXIT_KILLED)
        }
        OutcomeStatus::Error => sluice_error(&outcome.message),
        // Never a pass: a status from a newer daemon is an error.
        OutcomeStatus::Unknown => {
            sluice_error("the daemon reported an outcome this client does not know; update sluice")
        }
    }
}

// ---------------------------------------------------------------------------
// In-process fallback
// ---------------------------------------------------------------------------

/// Runs the command here: budget slots of the `flock` semaphore, inherited
/// stdio and environment, `SLUICE_JOB=inproc-<pid>`. The command leads its
/// own process group, so a timeout and the forwarded signals reach every
/// process it started (`killpg`), as on the daemon path. When this process
/// owns the terminal, the terminal is handed to the command's group for the
/// run and taken back afterwards (what a shell does for a foreground job).
fn in_process(job: &Job) -> ExitCode {
    let _slots = match tree::scratch_root(&job.env)
        .map_err(|e| e.to_string())
        .and_then(|s| crate::budget::acquire(&s, job.budget, job.weight, &job.label, job.quiet))
    {
        Ok(slots) => Some(slots),
        Err(e) => {
            job.info(&format!(
                "warning: cannot take budget slots ({e}); running unthrottled"
            ));
            None
        }
    };
    let name = &job.argv[0];
    let mut child = match Command::new(&job.program)
        .arg0(name)
        .args(&job.argv[1..])
        .current_dir(&job.cwd)
        .env(JOB_ENV_VAR, format!("inproc-{}", std::process::id()))
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sluice: exec: cannot run '{name}': {e}");
            return ExitCode::from(126);
        }
    };
    let pgid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(0));
    let terminal = Terminal::hand_to(pgid);
    // Blocked only now: the child keeps a clean signal mask.
    let mask = forwarded_signals();
    if mask.thread_block().is_ok()
        && let Ok(fd) = SignalFd::new(&mask)
    {
        thread::spawn(move || {
            while let Ok(read) = fd.read_signal() {
                if let Some(info) = read
                    && let Ok(sig) = Signal::try_from(i32::try_from(info.ssi_signo).unwrap_or(0))
                {
                    let _ = killpg(pgid, sig);
                }
            }
        });
    }
    let deadline = job.timeout.map(|t| Instant::now() + t);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => {
                drop(terminal);
                return sluice_error(&format!("cannot wait for the command: {e}"));
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break None;
        }
        thread::sleep(POLL_MIN);
    };
    let Some(status) = status else {
        let _ = killpg(pgid, Signal::SIGTERM);
        let _ = killpg(pgid, Signal::SIGCONT);
        let grace = Instant::now() + KILL_GRACE;
        while Instant::now() < grace && group_alive(pgid, &mut child) {
            thread::sleep(POLL_MIN);
        }
        let _ = killpg(pgid, Signal::SIGKILL);
        let _ = child.wait();
        drop(terminal);
        eprintln!(
            "sluice: exec '{}' TIMEOUT after {}",
            job.label,
            humantime::format_duration(job.timeout.unwrap_or_default())
        );
        return ExitCode::from(EXIT_TIMEOUT);
    };
    drop(terminal);
    match (status.code(), status.signal()) {
        (Some(n), _) => code(n),
        (None, Some(s)) => signal_code(s),
        (None, None) => ExitCode::from(EXIT_SLUICE),
    }
}

/// True while a process of the group `pgid` exists; the leader is reaped as
/// soon as it exits (a zombie leader would keep the group alive).
fn group_alive(pgid: Pid, leader: &mut std::process::Child) -> bool {
    let _ = leader.try_wait();
    killpg(pgid, None).is_ok()
}

/// The controlling terminal lent to the command's process group; given
/// back to this process's group on drop.
struct Terminal {
    own: Pid,
}

impl Terminal {
    /// Makes `pgid` the foreground group of the terminal on stdin when this
    /// process's group holds it now; `None` otherwise (no terminal, or a
    /// background run), and then nothing changes.
    fn hand_to(pgid: Pid) -> Option<Self> {
        let stdin = std::io::stdin();
        let own = getpgrp();
        if tcgetpgrp(&stdin).ok()? != own {
            return None;
        }
        let ok = with_sigttou_blocked(|| tcsetpgrp(&stdin, pgid).is_ok());
        // A read of the terminal before the hand-over stopped the command
        // (SIGTTIN): let it go on.
        let _ = killpg(pgid, Signal::SIGCONT);
        ok.then_some(Self { own })
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let stdin = std::io::stdin();
        with_sigttou_blocked(|| {
            let _ = tcsetpgrp(&stdin, self.own);
        });
    }
}

/// Runs `f` with SIGTTOU blocked in this thread (`tcsetpgrp` from a
/// background group would otherwise stop the process).
fn with_sigttou_blocked<T>(f: impl FnOnce() -> T) -> T {
    let mut ttou = SigSet::empty();
    ttou.add(Signal::SIGTTOU);
    let blocked = ttou.thread_block().is_ok();
    let out = f();
    if blocked {
        let _ = ttou.thread_unblock();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_short_and_safe() {
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            default_label(&argv(&["/usr/bin/cargo", "build", "--release"])),
            "cargo build"
        );
        assert_eq!(default_label(&argv(&["make"])), "make");
        assert_eq!(sanitize_label("a;b`c$(d) e:f/g-h.i_j"), "abcd e:f/g-h.i_j");
        assert_eq!(sanitize_label(&"x".repeat(100)).len(), LABEL_MAX);
        assert_eq!(sanitize_label("$$$"), "exec");
    }

    #[test]
    fn a_memory_kill_has_the_one_wording_of_every_client() {
        assert_eq!(
            memory_kill_headline("cargo build", 41.24, "j1"),
            "sluice: exec 'cargo build' KILLED for lack of memory after 41.2s (job j1)"
        );
    }

    #[test]
    fn exit_codes() {
        assert_eq!(signal_code(2), ExitCode::from(130));
        assert_eq!(signal_code(15), ExitCode::from(143));
        assert_eq!(code(256 + 42), ExitCode::from(42));
        assert_eq!(exec_signal(2), Some(ExecSignal::Int));
        assert_eq!(exec_signal(9), None);
    }
}
