//! Runs one job process: a new process group, CPU and I/O priority, both
//! output pipes captured, a timeout and a cancel sequence.
//!
//! Design: `kb/sluice/design/executor.md`, section "Executor". A gate
//! ([`Launch::Shell`], [`Sink::Log`]) writes a log with the Phase 1 framing:
//! a `# sluice job …` / `# command: …` header followed by a blank line, the
//! interleaved output, and a `# exit: …` footer. A `sluice exec` job
//! ([`Launch::Argv`], [`Sink::Spool`]) writes the framed spool of [`spool`]
//! instead, keeping stdout and stderr apart.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use sluice_gate_core::logfile::{self, JobLog};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::{Error, Result};

/// Grace period between SIGTERM and SIGKILL on timeout and cancel.
pub const KILL_GRACE: Duration = Duration::from_secs(5);

/// Size of the chunks read from the child's pipes.
const CHUNK: usize = 8 * 1024;

/// Polling interval while waiting for a process group to disappear.
const POLL: Duration = Duration::from_millis(20);

/// Framed output spool of `sluice exec` jobs.
///
/// Format: the magic [`MAGIC`], then records `[stream: u8 (1|2)][len: u32
/// big-endian][bytes]`. Stream 1 is stdout, 2 is stderr. Records are
/// appended whole under one mutex, so a reader that sees a partial record
/// only has to wait for the rest.
pub mod spool {
    use std::io::Write;

    /// First bytes of every spool file.
    pub const MAGIC: &[u8] = b"SLUICEX1\n";
    /// Stream id of stdout.
    pub const STDOUT: u8 = 1;
    /// Stream id of stderr.
    pub const STDERR: u8 = 2;
    /// Size of a record header.
    pub const HEADER: usize = 5;

    /// Appends one record.
    ///
    /// # Errors
    ///
    /// I/O errors of `out`.
    pub fn write_record(out: &mut impl Write, stream: u8, data: &[u8]) -> std::io::Result<()> {
        let len = u32::try_from(data.len()).map_err(std::io::Error::other)?;
        let mut header = [0u8; HEADER];
        header[0] = stream;
        header[1..].copy_from_slice(&len.to_be_bytes());
        out.write_all(&header)?;
        out.write_all(data)
    }

    /// Splits `buf` (without the magic) into complete records; returns them
    /// and the number of bytes consumed (a trailing partial record is left).
    pub fn parse(buf: &[u8]) -> (Vec<(u8, &[u8])>, usize) {
        let mut out = Vec::new();
        let mut at = 0;
        while buf.len() - at >= HEADER {
            let mut len = [0u8; 4];
            len.copy_from_slice(&buf[at + 1..at + HEADER]);
            let len = u32::from_be_bytes(len) as usize;
            if buf.len() - at - HEADER < len {
                break;
            }
            out.push((buf[at], &buf[at + HEADER..at + HEADER + len]));
            at += HEADER + len;
        }
        (out, at)
    }

    /// Both streams of a spool file merged in arrival order (for `logs` and
    /// the `done` tail); `None` when `bytes` is not a spool.
    pub fn merged(bytes: &[u8]) -> Option<Vec<u8>> {
        let body = bytes.strip_prefix(MAGIC)?;
        let (records, _) = parse(body);
        Some(
            records
                .into_iter()
                .flat_map(|(_, d)| d.iter().copied())
                .collect(),
        )
    }
}

/// What to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// A gate: `sh -c '<cmd>'`, or `sh -c '<cmd> "$@"' sh <extra...>`.
    Shell {
        /// Shell command.
        cmd: String,
        /// Extra arguments appended as `"$@"`.
        extra: Vec<String>,
    },
    /// `sluice exec`: an argv run directly, no shell.
    Argv {
        /// Program (absolute path resolved by the client).
        program: String,
        /// Arguments after the program.
        args: Vec<String>,
    },
}

/// Where the output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// One interleaved log with the Phase 1 header and footer.
    Log,
    /// The framed [`spool`], streams kept apart, no header or footer.
    Spool,
}

/// Everything needed to run one job.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    /// Job identifier (used in the log header).
    pub job_id: String,
    /// Gate name or exec label (used in the log header and messages).
    pub gate: String,
    /// What to start.
    pub launch: Launch,
    /// Where the output goes.
    pub sink: Sink,
    /// Working directory of the gate.
    pub cwd: PathBuf,
    /// Complete environment of the gate (nothing else is inherited).
    pub env: BTreeMap<String, String>,
    /// Maximum run time, measured from the spawn.
    pub timeout: Duration,
    /// CPU niceness applied to the gate's leader process.
    pub nice: u8,
    /// I/O priority, applied through the `ionice` binary when present.
    pub ionice: Option<IoPriority>,
    /// Log file; created (with its parent directories) and truncated. When
    /// a gate's log ([`Sink::Log`]) cannot be created the gate runs anyway
    /// with its output kept in memory for the tail ([`Finished::log_error`]);
    /// a spool ([`Sink::Spool`]) is required.
    pub log_path: PathBuf,
}

/// I/O scheduling priority for `ionice`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoPriority {
    /// Scheduling class.
    pub class: IoClass,
    /// Level within the class (0 highest, 7 lowest; ignored by `Idle`).
    pub level: u8,
}

/// I/O scheduling class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoClass {
    /// Best-effort (`ionice -c 2`).
    BestEffort,
    /// Idle (`ionice -c 3`).
    Idle,
}

impl IoClass {
    /// Numeric class understood by `ionice -c`.
    fn number(self) -> u8 {
        match self {
            Self::BestEffort => 2,
            Self::Idle => 3,
        }
    }
}

/// How a gate run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecStatus {
    /// The gate exited on its own with this code.
    Exited(i32),
    /// The leader was terminated by this signal, not sent by our timeout or
    /// cancel sequence.
    Signaled(i32),
    /// The timeout expired and the group was killed.
    Timeout,
    /// The run was cancelled and the group was killed.
    Killed,
}

/// Result of a finished run.
#[derive(Debug, Clone)]
pub struct Finished {
    /// How the run ended.
    pub status: ExecStatus,
    /// Time from spawn to exit.
    pub duration: Duration,
    /// Total bytes written to the log (header and footer included).
    pub log_bytes: u64,
    /// The log file (absent when [`Finished::log_error`] is set).
    pub log_path: PathBuf,
    /// Why the log file could not be created; the output was then kept in
    /// memory (the last [`logfile::MEMORY_LOG_CAP`] bytes) for the tail.
    pub log_error: Option<String>,
    /// The in-memory output when the log file could not be created.
    memory: Option<Vec<u8>>,
}

impl Finished {
    /// Last `n` lines of the output: from the log file, or from memory when
    /// the file could not be created.
    #[must_use]
    pub fn tail(&self, n: usize) -> Vec<String> {
        match &self.memory {
            Some(bytes) => logfile::last_lines(bytes, n),
            None => tail_lines(&self.log_path, n),
        }
    }
}

/// Identity of the spawned process, for persistence and orphan reconciliation.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    /// Pid of the gate's leader (`sh`).
    pub pid: u32,
    /// Process group id (equal to `pid`).
    pub pgid: i32,
    /// Start time in clock ticks (field 22 of `/proc/<pid>/stat`).
    pub proc_start_ticks: Option<u64>,
    /// Spawn time, unix milliseconds.
    pub started_at: u64,
}

/// Cloneable handle on a running gate, usable while [`Running::wait`] is pending.
#[derive(Debug, Clone)]
pub struct RunningHandle {
    cancel: Arc<Notify>,
    bytes: Arc<AtomicU64>,
}

impl RunningHandle {
    /// Requests the kill sequence; `wait` then resolves with [`ExecStatus::Killed`].
    pub fn cancel(&self) {
        self.cancel.notify_one();
    }

    /// Bytes written to the log so far.
    pub fn log_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// A gate that is running.
#[derive(Debug)]
pub struct Running {
    child: Child,
    pumps: Vec<JoinHandle<()>>,
    handle: RunningHandle,
    info: ProcessInfo,
    log: Arc<Mutex<JobLog>>,
    log_error: Option<String>,
    sink: Sink,
    log_path: PathBuf,
    started: Instant,
    timeout: Duration,
    nice: u8,
    ionice: Option<IoPriority>,
}

impl Running {
    /// Identity of the spawned process.
    pub fn info(&self) -> &ProcessInfo {
        &self.info
    }

    /// Bytes written to the log so far (for progress events).
    pub fn log_bytes(&self) -> u64 {
        self.handle.log_bytes()
    }

    /// Requests the kill sequence (SIGTERM to the group, 5 s grace, SIGKILL);
    /// `wait` then resolves with [`ExecStatus::Killed`].
    pub fn cancel(&self) {
        self.handle.cancel();
    }

    /// A cloneable handle to cancel or observe the run while `wait` is pending.
    pub fn handle(&self) -> RunningHandle {
        self.handle.clone()
    }

    /// Waits for the process to exit (or the timeout/cancel sequence to
    /// complete) and for both output pumps to finish, writes the footer and
    /// returns the result.
    pub async fn wait(mut self) -> Finished {
        let pgid = self.info.pgid;
        // Priorities are applied here, not in `spawn`: the caller registers
        // the group with the gate reaper between the two, and the helper
        // processes take long enough on a loaded machine that a daemon
        // killed meanwhile would leave an unregistered gate behind.
        tokio::spawn(apply_priority(self.info.pid, self.nice, self.ionice));
        let deadline = tokio::time::Instant::from_std(self.started + self.timeout);
        let cancel = Arc::clone(&self.handle.cancel);

        let (exit, forced) = tokio::select! {
            status = self.child.wait() => (status.ok(), None),
            () = tokio::time::sleep_until(deadline) => (None, Some(ExecStatus::Timeout)),
            () = cancel.notified() => (None, Some(ExecStatus::Killed)),
        };
        let exit = if forced.is_some() {
            let ((), status) = tokio::join!(terminate_group(pgid, KILL_GRACE), self.child.wait());
            status.ok()
        } else {
            exit
        };
        let duration = self.started.elapsed();

        // The leader is gone; output may still be buffered or held by
        // background processes of the group. Keep honouring the deadline and
        // cancel so a lingering grandchild cannot hang the run forever.
        let mut pumps = std::pin::pin!(join_all(std::mem::take(&mut self.pumps)));
        if forced.is_some() {
            pumps.await;
        } else {
            tokio::select! {
                () = &mut pumps => {}
                () = tokio::time::sleep_until(deadline) => {
                    kill_group(pgid, Signal::SIGKILL);
                    pumps.await;
                }
                () = cancel.notified() => {
                    kill_group(pgid, Signal::SIGKILL);
                    pumps.await;
                }
            }
        }

        let status = forced.unwrap_or_else(|| exit_status(exit));
        if self.sink == Sink::Log {
            let code = exit
                .and_then(|s| s.code())
                .map_or_else(|| "signal".to_owned(), |c| c.to_string());
            let suffix = match status {
                ExecStatus::Timeout => " (timed out)",
                ExecStatus::Killed => " (killed)",
                ExecStatus::Exited(_) | ExecStatus::Signaled(_) => "",
            };
            let footer = format!(
                "\n# exit: {code} after {:.1}s{suffix}\n",
                duration.as_secs_f64()
            );
            write_log(&self.log, &self.handle.bytes, footer.as_bytes());
        }
        let memory = self.log.lock().ok().and_then(|mut log| {
            let _ = log.flush();
            log.memory()
        });

        Finished {
            status,
            duration,
            log_bytes: self.handle.log_bytes(),
            log_path: self.log_path,
            log_error: self.log_error,
            memory,
        }
    }
}

/// Maps the leader's exit status to an [`ExecStatus`].
fn exit_status(exit: Option<ExitStatus>) -> ExecStatus {
    use std::os::unix::process::ExitStatusExt;
    match exit {
        Some(s) => s.code().map_or_else(
            || ExecStatus::Signaled(s.signal().unwrap_or(0)),
            ExecStatus::Exited,
        ),
        None => ExecStatus::Signaled(0),
    }
}

/// Awaits every task, ignoring panics.
async fn join_all(tasks: Vec<JoinHandle<()>>) {
    for task in tasks {
        let _ = task.await;
    }
}

/// Spawns the job in its own process group, with exactly `spec.env` as
/// environment and stdin null: `sh -c '<cmd>'` (or `sh -c '<cmd> "$@"' sh
/// <extra...>` when extra arguments are given) for [`Launch::Shell`], the
/// argv itself for [`Launch::Argv`]. Both outputs are captured into
/// `spec.log_path`: behind the Phase 1 header for [`Sink::Log`], as framed
/// records for [`Sink::Spool`].
///
/// Priorities are applied when [`Running::wait`] starts, through the
/// `renice` and `ionice` binaries when they are in `PATH` (failures are
/// ignored); `spawn` returns as soon as the process exists, so the caller
/// can register its group (gate reaper, store) before anything slow runs.
///
/// A gate's log that cannot be created (read-only `logs/` or state
/// directory) does not stop the gate: its output is kept in memory for the
/// tail and [`Finished::log_error`] says why.
///
/// # Errors
///
/// [`Error::Io`] when a spool cannot be created or the process cannot start
/// (for example, a missing working directory).
#[expect(
    clippy::unused_async,
    reason = "kept async for the callers; it awaited the priority helpers before they moved to `Running::wait`"
)]
pub async fn spawn(spec: ExecSpec) -> Result<Running> {
    let io = |context: String| move |source| Error::Io { context, source };

    let (log, log_error) = match spec.sink {
        Sink::Log => JobLog::create(&spec.log_path),
        Sink::Spool => {
            if let Some(parent) = spec.log_path.parent() {
                sluice_gate_core::dirs::create_private_dir(parent)
                    .map_err(io(format!("cannot create {}", parent.display())))?;
            }
            let file = File::create(&spec.log_path)
                .map_err(io(format!("cannot create {}", spec.log_path.display())))?;
            (JobLog::File(file), None)
        }
    };
    if let Some(e) = &log_error {
        tracing::warn!(job = %spec.job_id, "{e}; the output is kept in memory for the tail");
    }
    let log = Arc::new(Mutex::new(log));
    let bytes = Arc::new(AtomicU64::new(0));
    let mut command = command(&spec, &log, &bytes);
    let mut child = command
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(false)
        .spawn()
        .map_err(io(format!(
            "cannot start gate '{}' in {}",
            spec.gate,
            spec.cwd.display()
        )))?;
    let started = Instant::now();
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let Some(pid) = child.id() else {
        return Err(Error::Io {
            context: format!("gate '{}' exited before it could be tracked", spec.gate),
            source: std::io::Error::other("no pid"),
        });
    };
    let group = i32::try_from(pid).unwrap_or(i32::MAX);
    let proc_start_ticks = proc_start_ticks(pid);

    let framed = |stream: u8| (spec.sink == Sink::Spool).then_some(stream);
    let mut pumps = Vec::with_capacity(2);
    if let Some(out) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(
            out,
            Arc::clone(&log),
            Arc::clone(&bytes),
            framed(spool::STDOUT),
        )));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(
            err,
            Arc::clone(&log),
            Arc::clone(&bytes),
            framed(spool::STDERR),
        )));
    }

    Ok(Running {
        child,
        pumps,
        handle: RunningHandle {
            cancel: Arc::new(Notify::new()),
            bytes,
        },
        info: ProcessInfo {
            pid,
            pgid: group,
            proc_start_ticks,
            started_at,
        },
        log,
        log_error,
        sink: spec.sink,
        log_path: spec.log_path,
        started,
        timeout: spec.timeout,
        nice: spec.nice,
        ionice: spec.ionice,
    })
}

/// The command to spawn; writes the log header (gates) or the spool magic.
fn command(spec: &ExecSpec, log: &Mutex<JobLog>, bytes: &AtomicU64) -> Command {
    let command = match &spec.launch {
        Launch::Shell { cmd, extra } => {
            let header = format!(
                "# sluice job {}: gate '{}' in {}\n# command: {}\n\n",
                spec.job_id,
                spec.gate,
                spec.cwd.display(),
                cmd
            );
            write_log(log, bytes, header.as_bytes());
            // `"$@"` is appended only when extra arguments exist: a compound
            // command (`for … done`) followed by `"$@"` would be a shell
            // syntax error.
            let script = if extra.is_empty() {
                cmd.trim().to_owned()
            } else {
                format!("{} \"$@\"", cmd.trim())
            };
            let mut c = Command::new("sh");
            c.arg("-c").arg(script).arg("sh").args(extra);
            c
        }
        Launch::Argv { program, args } => {
            let mut c = Command::new(program);
            c.args(args);
            c
        }
    };
    if spec.sink == Sink::Spool {
        write_log(log, bytes, spool::MAGIC);
    }
    command
}

/// Lowers the CPU and I/O priority of `pid` with the `renice` and `ionice`
/// binaries. Everything here is best effort.
async fn apply_priority(pid: u32, nice: u8, ionice: Option<IoPriority>) {
    if nice > 0 {
        if let Some(renice) = find_in_path("renice") {
            run_quietly(Command::new(renice).args([
                "-n",
                &nice.to_string(),
                "-p",
                &pid.to_string(),
            ]))
            .await;
        } else {
            tracing::debug!("renice not found; gate {pid} keeps the daemon's niceness");
        }
    }
    if let Some(prio) = ionice {
        if let Some(ionice) = find_in_path("ionice") {
            run_quietly(Command::new(ionice).args([
                "-c",
                &prio.class.number().to_string(),
                "-n",
                &prio.level.to_string(),
                "-p",
                &pid.to_string(),
            ]))
            .await;
        } else {
            tracing::debug!("ionice not found; gate {pid} keeps the default I/O class");
        }
    }
}

/// Runs a helper command with null stdio, logging failures at debug level.
async fn run_quietly(cmd: &mut Command) {
    let result = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    match result {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::debug!("priority helper {cmd:?} ended with {status}"),
        Err(e) => tracing::debug!("priority helper {cmd:?} failed: {e}"),
    }
}

/// Finds an executable named `name` in the daemon's `PATH`.
fn find_in_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Appends `data` to the log and adds its length to the byte counter.
fn write_log(log: &Mutex<JobLog>, bytes: &AtomicU64, data: &[u8]) {
    if let Ok(mut file) = log.lock()
        && file.write_all(data).is_ok()
    {
        bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
    }
}

/// Appends one spool record (header and data in one locked write).
fn write_record(log: &Mutex<JobLog>, bytes: &AtomicU64, stream: u8, data: &[u8]) {
    let mut record = Vec::with_capacity(spool::HEADER + data.len());
    if spool::write_record(&mut record, stream, data).is_ok() {
        write_log(log, bytes, &record);
    }
}

/// Copies a child stream into the log in 8 KiB chunks until end of file;
/// with `framed`, each chunk becomes a spool record of that stream.
async fn pump<R: AsyncRead + Unpin>(
    mut reader: R,
    log: Arc<Mutex<JobLog>>,
    bytes: Arc<AtomicU64>,
    framed: Option<u8>,
) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => match framed {
                Some(stream) => write_record(&log, &bytes, stream, &buf[..n]),
                None => write_log(&log, &bytes, &buf[..n]),
            },
        }
    }
}

/// Sends `signal` to the whole process group `pgid`. Errors (typically
/// `ESRCH`, the group is gone) are ignored.
pub fn kill_group(pgid: i32, signal: Signal) {
    if pgid > 1 {
        let _ = killpg(Pid::from_raw(pgid), signal);
    }
}

/// True while at least one process (zombies included) belongs to `pgid`.
fn group_alive(pgid: i32) -> bool {
    pgid > 1 && killpg(Pid::from_raw(pgid), None).is_ok()
}

/// Sends SIGTERM to the group, then SIGKILL after `grace` unless the group
/// is gone sooner. Used on timeout, cancel and orphan reconciliation.
pub async fn terminate_group(pgid: i32, grace: Duration) {
    if !group_alive(pgid) {
        return;
    }
    kill_group(pgid, Signal::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !group_alive(pgid) {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
    kill_group(pgid, Signal::SIGKILL);
}

/// True when the process group `pgid`, whose leader was recorded with start
/// ticks `leader_ticks`, still holds processes of that run, so it may be
/// killed: the leader itself is alive and [`process_matches`], or the leader
/// is gone but members of the group remain (the kernel never hands out a pid
/// number while it still names a live process group, so they can only be
/// the run's descendants). A leader pid that now belongs to another process
/// (pid reuse) is never matched. Used by the reaper and by the start-up
/// reconciliation.
pub fn group_is_ours(pgid: i32, leader_ticks: Option<u64>) -> bool {
    let Ok(leader) = u32::try_from(pgid) else {
        return false;
    };
    if pgid <= 1 || leader_ticks.is_none() {
        return false;
    }
    match proc_start_ticks(leader) {
        Some(ticks) => Some(ticks) == leader_ticks,
        None => group_alive(pgid),
    }
}

/// Sends SIGTERM to every group, then SIGKILL after `grace` to those still
/// alive; blocking (the reaper process has no runtime).
pub fn terminate_groups_blocking(pgids: &[i32], grace: Duration) {
    for &pgid in pgids {
        kill_group(pgid, Signal::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && pgids.iter().any(|&p| group_alive(p)) {
        std::thread::sleep(POLL);
    }
    for &pgid in pgids {
        if group_alive(pgid) {
            kill_group(pgid, Signal::SIGKILL);
        }
    }
}

/// True when `/proc/<pid>` exists and its start ticks equal `expected`
/// (guard against pid reuse); false otherwise, including when `expected`
/// is `None`.
pub fn process_matches(pid: u32, expected: Option<u64>) -> bool {
    expected.is_some() && proc_start_ticks(pid) == expected
}

/// Start time of `pid` in clock ticks since boot: field 22 of
/// `/proc/<pid>/stat`. `None` when the process does not exist.
pub fn proc_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "<pid> (<comm>) <state> ..."; comm may contain spaces and parentheses,
    // so fields are counted after the last ')': index 0 is field 3.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Last `n` lines of a file (empty when it cannot be read). A [`spool`]
/// file is decoded first (both streams merged, invalid UTF-8 replaced).
pub fn tail_lines(path: &Path, n: usize) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut reader = BufReader::new(file);
    let is_spool = reader
        .fill_buf()
        .is_ok_and(|head| head.starts_with(spool::MAGIC));
    let mut all: Vec<String> = if is_spool {
        let mut bytes = Vec::new();
        if std::io::Read::read_to_end(&mut reader, &mut bytes).is_err() {
            return Vec::new();
        }
        let merged = spool::merged(&bytes).unwrap_or_default();
        String::from_utf8_lossy(&merged)
            .lines()
            .map(str::to_owned)
            .collect()
    } else {
        reader.lines().map_while(std::result::Result::ok).collect()
    };
    let start = all.len().saturating_sub(n);
    all.split_off(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dir: &Path, cmd: &str) -> ExecSpec {
        let mut env = BTreeMap::new();
        env.insert(
            "PATH".to_owned(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
        );
        ExecSpec {
            job_id: "j1".to_owned(),
            gate: "test".to_owned(),
            launch: Launch::Shell {
                cmd: cmd.to_owned(),
                extra: Vec::new(),
            },
            sink: Sink::Log,
            cwd: dir.to_path_buf(),
            env,
            timeout: Duration::from_secs(20),
            nice: 5,
            ionice: Some(IoPriority {
                class: IoClass::Idle,
                level: 7,
            }),
            log_path: dir.join("logs").join("j1.log"),
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    /// Members of `pgid`, found by scanning `/proc` (field 5 is the pgrp).
    fn group_members(pgid: i32) -> Vec<i32> {
        let mut out = Vec::new();
        for entry in fs::read_dir("/proc")
            .unwrap()
            .filter_map(std::result::Result::ok)
        {
            let Ok(member) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(i) = stat.rfind(')') else { continue };
            let fields: Vec<&str> = stat[i + 1..].split_whitespace().collect();
            if fields.first() != Some(&"Z") && fields.get(2) == Some(&pgid.to_string().as_str()) {
                out.push(member);
            }
        }
        out
    }

    #[tokio::test]
    async fn exit_code_output_and_footer() {
        let dir = tempfile::tempdir().unwrap();
        let running = spawn(spec(dir.path(), "echo out; echo err >&2; exit 3"))
            .await
            .unwrap();
        let done = running.wait().await;
        assert_eq!(done.status, ExecStatus::Exited(3));
        assert!(done.log_bytes > 0);
        let log = read(&done.log_path);
        assert!(log.starts_with(&format!(
            "# sluice job j1: gate 'test' in {}\n# command: echo out; echo err >&2; exit 3\n\n",
            dir.path().display()
        )));
        assert!(log.contains("out\n"));
        assert!(log.contains("err\n"));
        assert!(log.contains("\n# exit: 3 after "), "{log}");
        assert_eq!(done.log_bytes, log.len() as u64);
    }

    #[tokio::test]
    async fn extra_args_reach_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = spec(dir.path(), "echo");
        s.launch = Launch::Shell {
            cmd: "echo".to_owned(),
            extra: vec!["a".to_owned(), "b".to_owned()],
        };
        let done = spawn(s).await.unwrap().wait().await;
        assert_eq!(done.status, ExecStatus::Exited(0));
        assert!(read(&done.log_path).contains("\na b\n"));
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = spec(dir.path(), "sh -c 'sleep 30; true' & sleep 30");
        s.timeout = Duration::from_millis(300);
        let running = spawn(s).await.unwrap();
        let info = running.info().clone();
        assert_eq!(i64::from(info.pid), i64::from(info.pgid));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(group_members(info.pgid).len() >= 3, "grandchildren running");
        let t = Instant::now();
        let done = running.wait().await;
        assert_eq!(done.status, ExecStatus::Timeout);
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(!Path::new(&format!("/proc/{}", info.pid)).exists());
        let ok = std::process::Command::new("kill")
            .args(["-0", &info.pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!ok.success());
        // SIGKILLed members may linger as zombies until init reaps them.
        for _ in 0..50 {
            if group_members(info.pgid).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(group_members(info.pgid), Vec::<i32>::new());
        assert!(read(&done.log_path).contains("(timed out)"));
    }

    #[tokio::test]
    async fn cancel_kills_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let running = spawn(spec(dir.path(), "sleep 30")).await.unwrap();
        let pgid = running.info().pgid;
        tokio::time::sleep(Duration::from_millis(100)).await;
        running.cancel();
        let t = Instant::now();
        let done = running.wait().await;
        assert_eq!(done.status, ExecStatus::Killed);
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(group_members(pgid), Vec::<i32>::new());
    }

    #[tokio::test]
    async fn cancel_through_handle_while_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let running = spawn(spec(dir.path(), "sleep 30")).await.unwrap();
        let handle = running.handle();
        let waiter = tokio::spawn(running.wait());
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.cancel();
        let done = waiter.await.unwrap();
        assert_eq!(done.status, ExecStatus::Killed);
    }

    #[tokio::test]
    async fn process_matches_guards_pid_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let running = spawn(spec(dir.path(), "sleep 30")).await.unwrap();
        let info = running.info().clone();
        let ticks = info.proc_start_ticks;
        assert!(ticks.is_some());
        assert!(process_matches(info.pid, ticks));
        assert!(!process_matches(info.pid, ticks.map(|t| t + 1)));
        assert!(!process_matches(info.pid, None));
        running.cancel();
        running.wait().await;
        assert!(!process_matches(info.pid, ticks));
    }

    #[test]
    fn tail_lines_returns_the_last_n() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let body = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&path, body).unwrap();
        assert_eq!(tail_lines(&path, 3), ["line 8", "line 9", "line 10"]);
        assert_eq!(tail_lines(&path, 30).len(), 10);
        assert_eq!(tail_lines(&dir.path().join("missing"), 3).len(), 0);
    }

    #[tokio::test]
    async fn environment_is_exactly_the_map() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = spec(dir.path(), "env | sort");
        s.env.insert("SLUICE_MARKER".to_owned(), "yes".to_owned());
        let done = spawn(s).await.unwrap().wait().await;
        assert_eq!(done.status, ExecStatus::Exited(0));
        let log = read(&done.log_path);
        assert!(log.contains("\nSLUICE_MARKER=yes\n"));
        assert!(!log.contains("\nHOME="), "{log}");
    }

    #[tokio::test]
    async fn argv_with_spool_keeps_streams_apart_and_reports_signals() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = spec(dir.path(), "");
        s.launch = Launch::Argv {
            program: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf out1; printf err1 >&2; printf out2".to_owned(),
            ],
        };
        s.sink = Sink::Spool;
        s.log_path = dir.path().join("logs").join("j1.exec");
        let done = spawn(s.clone()).await.unwrap().wait().await;
        assert_eq!(done.status, ExecStatus::Exited(0));
        let bytes = fs::read(&done.log_path).unwrap();
        assert!(bytes.starts_with(spool::MAGIC));
        let (records, used) = spool::parse(&bytes[spool::MAGIC.len()..]);
        assert_eq!(used, bytes.len() - spool::MAGIC.len());
        let stream = |id: u8| -> Vec<u8> {
            records
                .iter()
                .filter(|(s, _)| *s == id)
                .flat_map(|(_, d)| d.iter().copied())
                .collect()
        };
        assert_eq!(stream(spool::STDOUT), b"out1out2");
        assert_eq!(stream(spool::STDERR), b"err1");
        assert_eq!(done.log_bytes, bytes.len() as u64);
        let tail = tail_lines(&done.log_path, 5).join("\n");
        assert!(tail.contains("out1") && tail.contains("err1"), "{tail}");

        s.launch = Launch::Argv {
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "kill -KILL $$".to_owned()],
        };
        let done = spawn(s).await.unwrap().wait().await;
        assert_eq!(done.status, ExecStatus::Signaled(9));
    }

    #[test]
    fn spool_parse_leaves_partial_records() {
        let mut buf = Vec::new();
        spool::write_record(&mut buf, spool::STDOUT, b"abc").unwrap();
        spool::write_record(&mut buf, spool::STDERR, b"").unwrap();
        spool::write_record(&mut buf, spool::STDOUT, b"xyz").unwrap();
        let (records, used) = spool::parse(&buf[..buf.len() - 1]);
        assert_eq!(
            records,
            vec![(spool::STDOUT, &b"abc"[..]), (spool::STDERR, &b""[..])]
        );
        assert_eq!(used, 13);
        let mut file = spool::MAGIC.to_vec();
        file.extend_from_slice(&buf);
        assert_eq!(spool::merged(&file).unwrap(), b"abcxyz");
        assert!(spool::merged(b"plain log").is_none());
    }

    #[tokio::test]
    async fn missing_cwd_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = spec(dir.path(), "true");
        s.cwd = dir.path().join("does-not-exist");
        assert!(matches!(spawn(s).await, Err(Error::Io { .. })));
    }
}
