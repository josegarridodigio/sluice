//! Stalled-job detection (ADR-0024): report only, never kill.
//!
//! Every few seconds the daemon reads a running job's own counters (its leaf
//! cgroup when contained, its process group otherwise) plus the log bytes the
//! executor counts, and keeps a monotonic `last_progress_at` per job. A job
//! whose counters stayed flat for the whole window is `stalled`; a job that
//! waits for CPU is not quiet; a job whose samples are missing is not quiet
//! either (the window must have been observed). Nothing here ends a job.
//!
//! The decision is pure ([`moved`], [`StallWatch::observe`], [`window_ms`]);
//! the readers take the leaf directory and the proc root so tests use fake
//! trees. Design: `kb/sluice/design/adr/0024-stalled-jobs-are-reported-never-killed.md`.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use sluice_gate_core::config::StallTimeout;

/// Floor of the default window: `max(5 min, 3 x expected)`.
pub const DEFAULT_FLOOR_MS: u64 = 5 * 60 * 1000;
/// Floor of a configured `stall_timeout`: shorter values are raised to it.
pub const CONFIGURED_FLOOR_MS: u64 = 60 * 1000;
/// How often the counters are read.
pub const EVERY_MS: u64 = 5_000;
/// Multiplier of the expected duration in the default window.
const EXPECTED_FACTOR: u64 = 3;
/// A gap between two samples longer than this many cadences is a hole in
/// the observation: the window restarts.
const MAX_GAP_CADENCES: u64 = 4;

/// CPU time counts as progress above this share of one CPU over the
/// interval (a tenth of a percent is a timer tick, 1 % is work), per mille
/// of the interval.
const CPU_NOISE_PERMILLE: u64 = 10;
/// Waiting for CPU counts above this share of the interval, per mille: the
/// job is runnable but starved, not hung.
const WAIT_NOISE_PERMILLE: u64 = 100;
/// Block I/O counts from this many bytes per interval (four pages).
const IO_BYTES_NOISE: u64 = 16 * 1024;
/// ... or this many I/O operations per interval.
const IO_OPS_NOISE: u64 = 16;
/// Minor page faults count from this many per interval (512 KiB of pages).
const FAULTS_NOISE: u64 = 128;
/// Major page faults count from this many per interval.
const MAJOR_FAULTS_NOISE: u64 = 1;
/// Ticks of `/proc/<pid>/stat` in microseconds.
const TICK_US: u64 = 1_000_000 / crate::sampler::USER_HZ;

/// Timing knobs. The default is the production setting; only tests lower
/// the floors and the cadence (`SLUICE_TEST_STALL_FLOOR_MS`,
/// `SLUICE_TEST_STALL_EVERY_MS`, read by the daemon command, or the fields
/// directly for an in-process daemon).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// Floor of the default window, milliseconds.
    pub default_floor_ms: u64,
    /// Floor of a configured `stall_timeout`, milliseconds.
    pub configured_floor_ms: u64,
    /// Cadence of the counter reads, milliseconds.
    pub every_ms: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            default_floor_ms: DEFAULT_FLOOR_MS,
            configured_floor_ms: CONFIGURED_FLOOR_MS,
            every_ms: EVERY_MS,
        }
    }
}

impl Tuning {
    /// The default tuning with the test overrides applied: `floor` lowers
    /// both floors, `every` the cadence (milliseconds; unparsable or zero
    /// values are ignored).
    pub fn with_test_overrides(floor: Option<&str>, every: Option<&str>) -> Self {
        let parse = |v: Option<&str>| {
            v.and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|n| *n > 0)
        };
        let mut t = Self::default();
        if let Some(f) = parse(floor) {
            t.default_floor_ms = f;
            t.configured_floor_ms = f;
        }
        if let Some(e) = parse(every) {
            t.every_ms = e;
        }
        t
    }

    pub(crate) fn max_gap_ms(&self) -> u64 {
        self.every_ms.saturating_mul(MAX_GAP_CADENCES)
    }
}

/// The quiet window after which a job is stalled; `None` when its gate says
/// `stall_timeout = "off"`. The default is `max(5 min, 3 x expected)`: a
/// gate with no history is held to the floor alone, because the floor is
/// the evidence-backed figure (the hangs on record sat idle for 25 minutes
/// or more) and a first run has nothing else to scale with.
pub fn window_ms(setting: StallTimeout, expected_ms: Option<u64>, t: &Tuning) -> Option<u64> {
    match setting {
        StallTimeout::Off => None,
        StallTimeout::After(d) => {
            let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
            Some(ms.max(t.configured_floor_ms))
        }
        StallTimeout::Auto => Some(
            expected_ms
                .map_or(0, |e| e.saturating_mul(EXPECTED_FACTOR))
                .max(t.default_floor_ms),
        ),
    }
}

/// Where a reading came from. Readings of different sources are not
/// comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The job's leaf cgroup.
    Leaf,
    /// The job's process group (no containment).
    Group,
}

/// One reading of a job's cumulative counters. A counter the machine does
/// not give is `None` and is not judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readings {
    /// Where it was read.
    pub source: Source,
    /// CPU time, microseconds.
    pub cpu_us: u64,
    /// Time runnable but waiting for a CPU, microseconds (`cpu.pressure`
    /// `some total`, or the summed `schedstat` run delay).
    pub wait_us: Option<u64>,
    /// Block I/O bytes (read plus written).
    pub io_bytes: Option<u64>,
    /// Block I/O operations.
    pub io_ops: Option<u64>,
    /// Minor and major page faults.
    pub faults: Option<(u64, u64)>,
    /// Processes in the job (threads in the leaf when `pids.current` exists).
    pub procs: u64,
}

/// A reading with the instant it was taken and the log bytes so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Monotonic milliseconds ([`mono_ms`]).
    pub at: u64,
    /// The counters.
    pub readings: Readings,
    /// Bytes written to the job's log.
    pub log_bytes: u64,
}

/// A signal that can show progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// CPU time.
    Cpu,
    /// Waiting for a CPU (a starved job is not hung).
    CpuWait,
    /// Block I/O.
    Io,
    /// Page faults.
    Faults,
    /// Log output.
    Output,
    /// The number of processes.
    Processes,
}

impl Signal {
    /// The spelling used in `status`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::CpuWait => "cpu-wait",
            Self::Io => "io",
            Self::Faults => "faults",
            Self::Output => "output",
            Self::Processes => "processes",
        }
    }
}

fn grew(prev: Option<u64>, cur: Option<u64>) -> u64 {
    prev.zip(cur).map_or(0, |(p, c)| c.saturating_sub(p))
}

/// The signals that moved between two samples of the same source, beyond the
/// noise thresholds: CPU above 1 % of one CPU, waiting for CPU above 10 % of
/// the interval, block I/O of 16 KiB or 16 operations, 128 minor or one
/// major page fault, any log byte, any change of the process count.
/// Samples of different sources compare as nothing moved.
pub fn moved(prev: &Sample, cur: &Sample) -> Vec<Signal> {
    let (p, c) = (&prev.readings, &cur.readings);
    if p.source != c.source {
        return Vec::new();
    }
    let dt_ms = cur.at.saturating_sub(prev.at);
    let mut out = Vec::new();
    // Microseconds against a per-mille share of the interval in milliseconds.
    let share = |delta_us: u64, permille: u64| delta_us > dt_ms.saturating_mul(permille);
    if share(c.cpu_us.saturating_sub(p.cpu_us), CPU_NOISE_PERMILLE) {
        out.push(Signal::Cpu);
    }
    if share(grew(p.wait_us, c.wait_us), WAIT_NOISE_PERMILLE) {
        out.push(Signal::CpuWait);
    }
    if grew(p.io_bytes, c.io_bytes) >= IO_BYTES_NOISE || grew(p.io_ops, c.io_ops) >= IO_OPS_NOISE {
        out.push(Signal::Io);
    }
    let faults = |r: &Readings| r.faults.map(|f| f.0);
    let majors = |r: &Readings| r.faults.map(|f| f.1);
    if grew(faults(p), faults(c)) >= FAULTS_NOISE
        || grew(majors(p), majors(c)) >= MAJOR_FAULTS_NOISE
    {
        out.push(Signal::Faults);
    }
    if cur.log_bytes > prev.log_bytes {
        out.push(Signal::Output);
    }
    if c.procs != p.procs {
        out.push(Signal::Processes);
    }
    out
}

/// The signals a reading can judge (the ones the machine gave).
pub fn observed(s: &Sample) -> Vec<Signal> {
    let r = &s.readings;
    let mut out = vec![Signal::Cpu];
    if r.wait_us.is_some() {
        out.push(Signal::CpuWait);
    }
    if r.io_bytes.is_some() {
        out.push(Signal::Io);
    }
    if r.faults.is_some() {
        out.push(Signal::Faults);
    }
    out.extend([Signal::Output, Signal::Processes]);
    out
}

/// What one observation changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Nothing worth reporting.
    None,
    /// The job has just become stalled after `quiet_ms` without progress.
    Stalled {
        /// How long it had been quiet.
        quiet_ms: u64,
    },
    /// A stalled job showed progress after `quiet_ms` of silence.
    Resumed {
        /// How long the stall had lasted.
        quiet_ms: u64,
    },
}

/// What `status` shows of a stalled job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalledView {
    /// How long the job has shown no progress.
    pub for_ms: u64,
    /// The signals that were flat.
    pub flat: Vec<&'static str>,
}

/// The progress record of one running job. Starts with the first
/// observation: after a daemon restart there is no prior knowledge, the
/// window runs from the first sample.
#[derive(Debug, Clone)]
pub struct StallWatch {
    /// Monotonic instant of the last progress (or of the first sample).
    last_progress_at: u64,
    prev: Option<Sample>,
    stalled: bool,
    longest_quiet_ms: u64,
}

impl StallWatch {
    /// A watch that starts observing at `now` (monotonic milliseconds).
    pub fn new(now: u64) -> Self {
        Self {
            last_progress_at: now,
            prev: None,
            stalled: false,
            longest_quiet_ms: 0,
        }
    }

    /// Whether `sample` shows progress over the previous one. A missing
    /// sample (the job's counters could not be read) and a hole in the
    /// observation are not quiet; the first sample judges nothing.
    fn progressed(&self, now: u64, sample: Option<&Sample>, max_gap_ms: u64) -> bool {
        let Some(sample) = sample else { return true };
        match &self.prev {
            None => false,
            Some(prev) if now.saturating_sub(prev.at) > max_gap_ms => true,
            Some(prev) => !moved(prev, sample).is_empty(),
        }
    }

    /// Feeds the sample taken at `now` (`None`: unreadable) with the quiet
    /// window of the job (`None`: never stalled) and the longest gap between
    /// samples that still counts as observed.
    pub fn observe(
        &mut self,
        now: u64,
        sample: Option<Sample>,
        window_ms: Option<u64>,
        max_gap_ms: u64,
    ) -> Change {
        let progressed = self.progressed(now, sample.as_ref(), max_gap_ms);
        let quiet = now.saturating_sub(self.last_progress_at);
        self.longest_quiet_ms = self.longest_quiet_ms.max(quiet);
        self.prev = sample;
        if progressed {
            self.last_progress_at = now;
        }
        let was = self.stalled;
        self.stalled = !progressed && (was || window_ms.is_some_and(|w| quiet >= w));
        match (was, self.stalled) {
            (false, true) => Change::Stalled { quiet_ms: quiet },
            (true, false) => Change::Resumed { quiet_ms: quiet },
            _ => Change::None,
        }
    }

    /// Whether the job is stalled now.
    pub fn is_stalled(&self) -> bool {
        self.stalled
    }

    /// How long the job has been quiet, while it is stalled.
    pub fn stalled_for(&self, now: u64) -> Option<u64> {
        self.stalled
            .then(|| now.saturating_sub(self.last_progress_at))
    }

    /// The longest quiet window of the job so far, the open one included.
    pub fn longest_quiet_ms(&self, now: u64) -> u64 {
        self.longest_quiet_ms
            .max(now.saturating_sub(self.last_progress_at))
    }

    /// What `status` shows while the job is stalled.
    pub fn view(&self, now: u64) -> Option<StalledView> {
        let for_ms = self.stalled_for(now)?;
        let flat = self
            .prev
            .as_ref()
            .map_or_else(Vec::new, observed)
            .into_iter()
            .map(Signal::label)
            .collect();
        Some(StalledView { for_ms, flat })
    }

    /// Instant the cadence of the next read is measured from, for tests.
    #[cfg(test)]
    fn last_progress_at(&self) -> u64 {
        self.last_progress_at
    }
}

/// Monotonic milliseconds since the first call in this process. The window
/// never goes backwards with the wall clock.
pub fn mono_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

/// The number after `key` on a `key value` line.
fn flat_value(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(' '))?
        .trim()
        .parse()
        .ok()
}

/// `total=` of the `some` line of a pressure file, microseconds.
fn pressure_total(text: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with("some "))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("total=")?.parse().ok())
}

/// `(bytes, operations)` summed over the devices of an `io.stat`.
fn io_totals(text: &str) -> (u64, u64) {
    let (mut bytes, mut ops) = (0u64, 0u64);
    for field in text.lines().flat_map(|l| l.split_whitespace().skip(1)) {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let Ok(n) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "rbytes" | "wbytes" => bytes = bytes.saturating_add(n),
            "rios" | "wios" => ops = ops.saturating_add(n),
            _ => {}
        }
    }
    (bytes, ops)
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The job's counters from its leaf cgroup (`cpu.stat`, `cpu.pressure`,
/// `io.stat`, `memory.stat`, `pids.current`); `None` when the leaf has no
/// readable `cpu.stat` (it is gone, or the machine gives none).
pub fn read_leaf(leaf: &Path) -> Option<Readings> {
    let cpu_us = flat_value(&read(&leaf.join("cpu.stat"))?, "usage_usec")?;
    let io = read(&leaf.join("io.stat")).map(|t| io_totals(&t));
    let faults = read(&leaf.join("memory.stat"))
        .and_then(|t| Some((flat_value(&t, "pgfault")?, flat_value(&t, "pgmajfault")?)));
    let procs = read(&leaf.join("pids.current"))
        .and_then(|t| t.trim().parse().ok())
        .or_else(|| read(&leaf.join("cgroup.procs")).map(|t| t.lines().count() as u64))?;
    Some(Readings {
        source: Source::Leaf,
        cpu_us,
        wait_us: read(&leaf.join("cpu.pressure")).and_then(|t| pressure_total(&t)),
        io_bytes: io.map(|i| i.0),
        io_ops: io.map(|i| i.1),
        faults,
        procs,
    })
}

/// `(minor, major)` page faults of a `/proc/<pid>/stat` (fields 10 and 12).
fn stat_faults(stat: &str) -> Option<(u64, u64)> {
    let f = crate::sampler::stat_fields(stat)?;
    Some((f.get(7)?.parse().ok()?, f.get(9)?.parse().ok()?))
}

/// `(bytes, operations)` of a `/proc/<pid>/io`: block bytes read and
/// written, `syscr` and `syscw`.
fn proc_io(text: &str) -> (u64, u64) {
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix(':'))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    (
        get("read_bytes").saturating_add(get("write_bytes")),
        get("syscr").saturating_add(get("syscw")),
    )
}

/// The job's counters from its process group: the summed `utime + stime +
/// cutime + cstime` of its processes, the run delay of their tasks, their
/// page faults and `/proc/<pid>/io` where readable, and the process count.
/// `None` when no process is left.
pub fn read_group(proc_root: &Path, pids: &[u32]) -> Option<Readings> {
    if pids.is_empty() {
        return None;
    }
    let ticks = pids
        .iter()
        .filter_map(|&p| crate::sampler::leader_ticks(proc_root, p))
        .fold(0u64, u64::saturating_add);
    let delay_ns = pids
        .iter()
        .flat_map(|&p| crate::sampler::read_tasks(proc_root, p))
        .filter_map(|t| t.2.map(|s| s.1))
        .fold(0u64, u64::saturating_add);
    let stats: Vec<(u64, u64)> = pids
        .iter()
        .filter_map(|p| read(&proc_root.join(p.to_string()).join("stat")))
        .filter_map(|s| stat_faults(&s))
        .collect();
    let ios: Vec<(u64, u64)> = pids
        .iter()
        .filter_map(|p| read(&proc_root.join(p.to_string()).join("io")))
        .map(|t| proc_io(&t))
        .collect();
    Some(Readings {
        source: Source::Group,
        cpu_us: ticks.saturating_mul(TICK_US),
        wait_us: (delay_ns > 0).then_some(delay_ns / 1000),
        io_bytes: (!ios.is_empty()).then(|| ios.iter().map(|i| i.0).sum()),
        io_ops: (!ios.is_empty()).then(|| ios.iter().map(|i| i.1).sum()),
        faults: (!stats.is_empty()).then(|| {
            (
                stats.iter().map(|s| s.0).sum(),
                stats.iter().map(|s| s.1).sum(),
            )
        }),
        procs: pids.len() as u64,
    })
}

#[cfg(test)]
mod tests;
