//! The daemon's side of agent resource containment (ADR-0017, design
//! `kb/sluice/design/containment.md`, section 2 rows "daemon").
//!
//! [`Containment::start`] runs once, after the daemon detached: it detects
//! what the machine allows, adopts the daemon into `sluice-daemon.scope`
//! (`daemon/` leaf), creates `sluice-agents.scope` around a holder process
//! (`holder/` leaf) and applies the weights and the derived `systemd-oomd`
//! policy. Every failure is logged once and the daemon carries on exactly as
//! 0.4.1 (no scopes, no job leaves). Gate and exec jobs then start through
//! `sluice cg-exec <leaf> -- ...` ([`Containment::job_prefix`]); the daemon
//! prunes empty leaves and records kills by the kernel OOM killer and
//! `systemd-oomd` ([`Containment::collect_interventions`]).
//!
//! Units and cgroups are created and written only through the core
//! `cgroup` module; external tools only through its shell-out layer.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sluice_gate_core::cgroup::detect::{self, PressurePolicy};
use sluice_gate_core::cgroup::systemd;
use sluice_gate_core::cgroup::tree::{
    Layout, PruneReport, Tree, TreeSettings, read_usage, write_pid,
};
use sluice_gate_core::cgroup::{CGROUP_MOUNT, CgroupError};
use sluice_gate_core::config::{ContainmentConfig, EnvMap};
use sluice_gate_proto as proto;

use crate::interventions::{
    InterventionRow, MemoryWatcher, find_job_kill, parse_oomd_journal, watch_targets,
};
use crate::reaper::ReaperCommand;

/// Empty session and job cgroups younger than this are left alone (a fresh
/// session may be about to receive its first process).
pub const PRUNE_GRACE: Duration = Duration::from_secs(60);

/// How far back the first journal read looks.
const FIRST_JOURNAL_LOOKBACK: Duration = Duration::from_secs(3600);
/// Overlap of consecutive journal reads (rows are stored once anyway).
const JOURNAL_OVERLAP: Duration = Duration::from_secs(30);

/// Pause before the journal is asked about a kill that just happened.
const JOURNAL_SETTLE: Duration = Duration::from_millis(400);

/// What the leaf of a finished job said (see [`Containment::read_job_end`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobMemoryEnd {
    /// `oom_kill` of the leaf.
    pub oom_kill: u64,
    /// `oom_group_kill` of the leaf.
    pub oom_group_kill: u64,
    /// `memory.peak` of the leaf.
    pub peak_bytes: Option<u64>,
    /// Interventions to record for kills no earlier poll had seen.
    pub rows: Vec<InterventionRow>,
}

/// What the CLI hands the daemon to enable containment.
#[derive(Debug, Clone)]
pub struct ContainmentSetup {
    /// Environment of the tools shelled out to (needs `XDG_RUNTIME_DIR`).
    pub env: EnvMap,
    /// Test-only unit name prefix (`sluice-test-<pid>`): the units become
    /// `<prefix>.slice`, `<prefix>-daemon.scope` and `<prefix>-agents.scope`.
    /// `None` uses `sluice.slice`, `sluice-daemon.scope`, `sluice-agents.scope`.
    pub unit_prefix: Option<String>,
    /// The `sluice` executable for `cg-exec` (`/proc/self/exe` in the CLI).
    pub exe: PathBuf,
    /// How to start `sluice daemon holder`.
    pub holder: ReaperCommand,
}

/// A running containment: the tree, the holder and the watchers.
#[derive(Debug)]
pub struct Containment {
    tree: Tree,
    exe: PathBuf,
    /// `--oom-score-adj` handed to `cg-exec` (`[containment] gates_oom_score_adj`).
    oom_score_adj: u16,
    agents_ready: bool,
    oomd_active: bool,
    holder: Mutex<Option<Child>>,
    watcher: Mutex<MemoryWatcher>,
    journal: Mutex<JournalState>,
    /// `full` or `partial`, with what is missing, for `status`.
    level: &'static str,
    detail: Option<String>,
    cpu: Mutex<CpuWindow>,
}

/// The agents scope's CPU over the last two samples.
#[derive(Debug, Default)]
struct CpuWindow {
    last: Option<(Instant, u64)>,
    /// `(percent of all CPUs, window length in ms)`.
    percent: Option<(f64, u64)>,
}

#[derive(Debug, Default)]
struct JournalState {
    /// Unix seconds of the last read; 0 before the first.
    last_read: u64,
    /// `journalctl` is missing: stop trying.
    disabled: bool,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Spawns the holder with a pipe on its stdin. The write end stays in the
/// daemon (close-on-exec, so no job inherits it) and the kernel closes it
/// when the daemon exits for any reason: the holder reads until end of file
/// and then exits, without polling and without `unsafe`.
fn spawn_holder(cmd: &ReaperCommand) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt as _;
    std::process::Command::new(&cmd.program)
        .arg0(&cmd.arg0)
        .args(&cmd.args)
        .env_clear()
        .current_dir("/")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

impl Containment {
    /// Detects, places the daemon and its holder, and builds the tree.
    /// `None` (after one log line saying why) when containment is disabled,
    /// not available, or the daemon scope cannot be built: the daemon then
    /// runs as in 0.4.1.
    #[must_use]
    pub fn start(setup: &ContainmentSetup, cfg: &ContainmentConfig) -> Option<Self> {
        Self::start_reporting(setup, cfg).ok()
    }

    /// Same as [`Containment::start`], but says why containment is off
    /// (the text shown as `containment: off (<reason>)`).
    ///
    /// # Errors
    ///
    /// The reason containment is not on.
    pub fn start_reporting(
        setup: &ContainmentSetup,
        cfg: &ContainmentConfig,
    ) -> Result<Self, String> {
        if !cfg.enabled {
            tracing::debug!("containment disabled by [containment] enabled = false");
            return Err("disabled by [containment] enabled = false".to_owned());
        }
        let caps = detect::detect(&setup.env);
        if !caps.usable() {
            tracing::info!("{}; the daemon runs as without containment", caps.summary());
            return Err(caps.level_detail().1.unwrap_or_default());
        }
        let uid = caps.facts.uid;
        let pressure = if caps.oomd_policy_enabled(cfg) {
            detect::pressure_policy(&setup.env, cfg, uid).unwrap_or(PressurePolicy {
                limit_percent: 30,
                duration: detect::DERIVED_DURATION_CAP,
            })
        } else {
            PressurePolicy {
                limit_percent: 30,
                duration: detect::DERIVED_DURATION_CAP,
            }
        };
        let cpus =
            std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(1));
        let settings = TreeSettings::from_config(cfg, &caps, pressure, cpus);
        let layout = match &setup.unit_prefix {
            Some(prefix) => Layout::with_prefix(Path::new(CGROUP_MOUNT), uid, prefix),
            None => Layout::standard(uid),
        };
        let tree = Tree::new(layout, settings, setup.env.clone());
        let pid = std::process::id();

        // One retry: a previous daemon's scope may be stopping right now.
        let placed = tree.ensure_daemon_scope(pid).or_else(|first| {
            tracing::debug!("daemon scope, first attempt: {first}");
            std::thread::sleep(Duration::from_millis(300));
            tree.ensure_daemon_scope(pid)
        });
        if let Err(e) = placed {
            tracing::warn!(
                "containment: cannot place the daemon ({e}); running as without containment"
            );
            return Err(format!("cannot place the daemon: {e}"));
        }
        let (holder, agents_ready) = match spawn_holder(&setup.holder) {
            Ok(child) => {
                let attempt = |t: &Tree| t.ensure_agents_scope(child.id());
                let result = attempt(&tree).or_else(|first| {
                    tracing::debug!("agents scope, first attempt: {first}");
                    std::thread::sleep(Duration::from_millis(300));
                    attempt(&tree)
                });
                match result {
                    Ok(()) => {
                        retire_stopgap_holders(&tree.layout.holder_dir(), child.id());
                        (Some(child), true)
                    }
                    Err(e) => {
                        tracing::warn!(
                            "containment: cannot create the agents scope ({e}); only the daemon and its jobs are contained"
                        );
                        (Some(child), false)
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "containment: cannot start the holder ({e}); only the daemon and its jobs are contained"
                );
                (None, false)
            }
        };
        tracing::info!(
            "{}; daemon in {}, agents scope {}",
            caps.summary(),
            tree.layout.daemon_scope,
            if agents_ready {
                tree.layout.agents_scope.as_str()
            } else {
                "unavailable"
            }
        );
        let (mut level, mut detail) = caps.level_detail();
        if !agents_ready && level == "full" {
            level = "partial";
            detail = Some("agents scope unavailable".to_owned());
        }
        let watcher =
            MemoryWatcher::with_backstop(tree.layout.jobs_dir(), tree.layout.daemon_scope_dir());
        Ok(Self {
            tree,
            exe: setup.exe.clone(),
            oom_score_adj: cfg.gates_oom_score_adj.resolve(),
            agents_ready,
            oomd_active: caps.oomd_active,
            holder: Mutex::new(holder),
            watcher: Mutex::new(watcher),
            journal: Mutex::new(JournalState::default()),
            level,
            detail,
            cpu: Mutex::new(CpuWindow::default()),
        })
    }

    /// Takes one CPU sample of the agents scope (the daemon calls it every
    /// tick): the CPU the agents used since the previous sample, which no
    /// Sluice job spent (jobs live in the daemon scope), as a percentage of
    /// `cpus` CPUs.
    pub fn sample(&self, cpus: u32) {
        let Some(usec) = read_usage(&self.tree.layout.agents_scope_dir()).cpu_usec else {
            return;
        };
        let now = Instant::now();
        let mut w = lock(&self.cpu);
        if let Some((then, before)) = w.last {
            let wall = now.duration_since(then);
            if let Some(percent) = window_percent(before, usec, wall, cpus) {
                let ms = u64::try_from(wall.as_millis()).unwrap_or(u64::MAX);
                w.percent = Some((percent, ms));
            }
        }
        w.last = Some((now, usec));
    }

    /// The containment state and the live sessions for the `status` reply.
    #[must_use]
    pub fn status(&self) -> (proto::ContainmentInfo, Vec<proto::SessionUsage>) {
        let (percent, window) = match lock(&self.cpu).percent {
            Some((p, ms)) => (Some((p * 10.0).round() / 10.0), Some(ms)),
            None => (None, None),
        };
        let info = proto::ContainmentInfo {
            level: self.level.to_owned(),
            detail: self.detail.clone(),
            outside_jobs_cpu_percent: percent,
            window_ms: window,
            gates_oom_policy: self.tree.settings.daemon_oom.map(|o| proto::OomPolicyInfo {
                limit_percent: u32::from(o.limit_percent),
                duration_secs: o.duration.map_or(0, |d| d.as_secs()),
            }),
        };
        (info, self.sessions())
    }

    /// Usage of the live session cgroups (`s-*` under the agents scope that
    /// still hold a process), by name.
    #[must_use]
    pub fn sessions(&self) -> Vec<proto::SessionUsage> {
        session_usages(&self.tree.layout.agents_scope_dir())
    }

    /// The tree (unit names and directories).
    #[must_use]
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Moves the reaper (started after the daemon was placed, so it already
    /// inherited `daemon/`) into `daemon/` explicitly. Best effort.
    pub fn adopt_reaper(&self, pid: u32) {
        if let Err(e) = write_pid(&self.tree.layout.daemon_leaf_dir(), pid) {
            tracing::debug!("containment: reaper {pid} not moved to daemon/: {e}");
        }
    }

    /// Creates the leaf `jobs/<job-id>` and returns the argv prefix that
    /// places a job in it (`sluice cg-exec <leaf> --oom-score-adj N --`), to
    /// be put before the `nice`/`ionice` chain. Empty (the job runs unplaced) when the leaf
    /// cannot be created.
    #[must_use]
    pub fn job_prefix(&self, job_id: &str) -> Vec<OsString> {
        match self.tree.job_leaf(job_id) {
            Ok(leaf) => cg_exec_prefix(&self.exe, leaf.dir.as_os_str(), self.oom_score_adj),
            Err(e) => {
                tracing::debug!(job = %job_id, "containment: no job leaf: {e}");
                Vec::new()
            }
        }
    }

    /// Removes the leaf of a finished job. A leaf that still holds
    /// processes (a stray background process of the job) stays for the
    /// periodic pruning.
    pub fn release_job(&self, job_id: &str) {
        let dir = self.tree.layout.jobs_dir().join(job_id);
        match std::fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::debug!(job = %job_id, "containment: job leaf kept: {e}"),
        }
    }

    /// Removes empty `s-*` session cgroups and job leaves older than
    /// [`PRUNE_GRACE`].
    #[must_use]
    pub fn prune(&self) -> PruneReport {
        self.tree.prune(PRUNE_GRACE)
    }

    /// New interventions since the last call: growth of the kill counters
    /// in `memory.events` of the leaves under the agents scope and `jobs/`,
    /// and `systemd-oomd` journal lines naming a Sluice cgroup. Blocking
    /// (reads files, runs `journalctl`).
    #[must_use]
    pub fn collect_interventions(&self, now_ms: u64) -> Vec<InterventionRow> {
        let layout = &self.tree.layout;
        let targets = watch_targets(
            &layout.agents_scope_dir(),
            &layout.daemon_scope_dir(),
            &layout.jobs_dir(),
        );
        let mut rows = {
            let mut watcher = lock(&self.watcher);
            let mut rows = watcher.poll(&targets, now_ms);
            rows.extend(watcher.backstop(now_ms));
            rows
        };
        if self.agents_ready && self.oomd_active {
            rows.extend(self.journal_rows());
        }
        rows
    }

    /// Reads what the kernel counted in the leaf of a job that just ended
    /// (it is removed right after): its OOM counters and `memory.peak`, and
    /// the rows for kills the periodic poll had not reported. Blocking.
    #[must_use]
    pub fn read_job_end(&self, job_id: &str, now_ms: u64) -> JobMemoryEnd {
        let dir = self.tree.layout.jobs_dir().join(job_id);
        let usage = read_usage(&dir);
        let events = usage.memory_events;
        let rows = lock(&self.watcher).settle_job(
            &self.tree.layout.daemon_scope_dir(),
            &dir,
            now_ms,
            (events.oom_kill, events.oom_group_kill),
        );
        JobMemoryEnd {
            oom_kill: events.oom_kill,
            oom_group_kill: events.oom_group_kill,
            peak_bytes: usage.memory_peak,
            rows,
        }
    }

    /// The one bounded journal query of a job that ended by `SIGKILL`
    /// without any kernel counter: the `systemd-oomd` line naming its leaf,
    /// if there is one (`since_unix` is when the job started). Waits a moment
    /// first, because oomd logs the kill around the time it sends it.
    /// Blocking; `None` without `systemd-oomd`, without `journalctl` or on
    /// any failure.
    #[must_use]
    pub fn oomd_killed_job(&self, job_id: &str, since_unix: u64) -> Option<InterventionRow> {
        if !self.oomd_active || lock(&self.journal).disabled {
            return None;
        }
        std::thread::sleep(JOURNAL_SETTLE);
        match systemd::journal_oomd(self.tree.env(), since_unix, false) {
            Ok(text) => find_job_kill(&text, job_id),
            Err(e) => {
                tracing::debug!(job = %job_id, "containment: journal read failed: {e}");
                None
            }
        }
    }

    fn journal_rows(&self) -> Vec<InterventionRow> {
        let mut state = lock(&self.journal);
        if state.disabled {
            return Vec::new();
        }
        let now = unix_now();
        let since = if state.last_read == 0 {
            now.saturating_sub(FIRST_JOURNAL_LOOKBACK.as_secs())
        } else {
            state.last_read.saturating_sub(JOURNAL_OVERLAP.as_secs())
        };
        let mut rows = Vec::new();
        for user in [true, false] {
            match systemd::journal_oomd(self.tree.env(), since, user) {
                Ok(text) => rows.extend(parse_oomd_journal(&text)),
                Err(CgroupError::ToolMissing { .. }) => {
                    tracing::debug!("containment: journalctl is not installed; no oomd history");
                    state.disabled = true;
                    return rows;
                }
                Err(e) => tracing::debug!("containment: journal read failed: {e}"),
            }
        }
        state.last_read = now;
        rows
    }

    /// Lets the holder go (its stdin closes) and waits a moment for it.
    pub fn shutdown(&self) {
        let Some(mut child) = lock(&self.holder).take() else {
            return;
        };
        drop(child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// `<exe> cg-exec <leaf> --oom-score-adj <adj> --`: the argv prefix that
/// places a job in `leaf` and raises its `oom_score_adj`. A `leaf` of
/// [`NO_LEAF`] only raises the adj.
#[must_use]
pub fn cg_exec_prefix(exe: &Path, leaf: &std::ffi::OsStr, adj: u16) -> Vec<OsString> {
    vec![
        exe.as_os_str().to_owned(),
        "cg-exec".into(),
        leaf.to_owned(),
        "--oom-score-adj".into(),
        adj.to_string().into(),
        "--".into(),
    ]
}

/// What `cg-exec` takes as the leaf when there is none.
pub const NO_LEAF: &str = "-";

/// CPU used between two readings of `usage_usec`, as a percentage of `cpus`
/// CPUs over `wall`. `None` for a zero window or a counter that went back.
#[must_use]
pub fn window_percent(before_usec: u64, after_usec: u64, wall: Duration, cpus: u32) -> Option<f64> {
    let wall_us = u64::try_from(wall.as_micros()).unwrap_or(u64::MAX);
    if wall_us == 0 || after_usec < before_usec {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let percent =
        (after_usec - before_usec) as f64 * 100.0 / (wall_us as f64 * f64::from(cpus.max(1)));
    Some(percent)
}

/// Usage of every `s-*` session cgroup below `scope` that holds a process
/// (cgroups with `pids.current = 0` are waiting to be pruned), sorted by name.
#[must_use]
pub fn session_usages(scope: &Path) -> Vec<proto::SessionUsage> {
    let Ok(entries) = std::fs::read_dir(scope) else {
        return Vec::new();
    };
    let mut out: Vec<proto::SessionUsage> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_prefix("s-")?.to_owned();
            let usage = read_usage(&e.path());
            if usage.pids_current == Some(0) {
                return None;
            }
            #[allow(clippy::cast_precision_loss)]
            let cpu_seconds = usage.cpu_usec.unwrap_or(0) as f64 / 1_000_000.0;
            Some(proto::SessionUsage {
                id,
                cpu_seconds,
                memory_bytes: usage.memory_current,
                memory_peak_bytes: usage.memory_peak,
                procs: usage.pids_current,
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// How long the stopgap holder lives (seconds). The agents scope needs a
/// live process to be created; after that the sessions' own processes (the
/// agent in `ui`) keep it alive, so the holder only bridges the first
/// moments. A bounded life means a holder can never outlive its purpose (it
/// leaked from a test once, as a `sleep 86400`).
pub const STOPGAP_LIFETIME_SECS: u64 = 600;

/// Arguments (after the executable) of the stopgap holder the session hook
/// starts when it has to create the agents scope before any daemon runs:
/// `sluice daemon holder --lifetime <secs>`.
#[must_use]
pub fn stopgap_holder_args() -> [String; 4] {
    [
        "daemon".to_owned(),
        "holder".to_owned(),
        "--lifetime".to_owned(),
        STOPGAP_LIFETIME_SECS.to_string(),
    ]
}

/// The stopgap holder of the session hook: `sluice daemon holder --lifetime
/// <secs>` (arg0 `sluice`), or the `sleep 86400` an earlier build used.
fn is_stopgap_holder(cmdline: &[u8]) -> bool {
    if cmdline == b"sleep\x0086400\x00" {
        return true;
    }
    let Some(body) = cmdline.strip_suffix(b"\x00") else {
        return false;
    };
    let parts: Vec<&[u8]> = body.split(|b| *b == 0).collect();
    matches!(
        parts.as_slice(),
        [_, b"daemon", b"holder", b"--lifetime", secs]
            if !secs.is_empty() && secs.iter().all(u8::is_ascii_digit)
    )
}

/// With the daemon up, its own holder owns the agents scope: ends the hook's
/// stopgap `sleep 86400` processes found in `holder/` (only there, only that
/// exact command line; never the daemon's own holder).
fn retire_stopgap_holders(holder_dir: &Path, own: u32) {
    use nix::sys::signal::{Signal, kill};
    let Ok(text) = std::fs::read_to_string(holder_dir.join("cgroup.procs")) else {
        return;
    };
    for pid in text.lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
        if pid == own {
            continue;
        }
        let stopgap =
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| is_stopgap_holder(&c));
        if stopgap && let Ok(raw) = i32::try_from(pid) {
            tracing::debug!(pid, "containment: retiring the hook's stopgap holder");
            let _ = kill(nix::unistd::Pid::from_raw(raw), Signal::SIGTERM);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{is_stopgap_holder, session_usages, window_percent};
    use std::time::Duration;

    #[test]
    fn outside_jobs_cpu_is_a_share_of_all_cpus() {
        // 2 CPU-seconds in 10 s on 4 CPUs: 5 %.
        let p = window_percent(1_000_000, 3_000_000, Duration::from_secs(10), 4).unwrap();
        assert!((p - 5.0).abs() < 1e-9, "{p}");
        assert_eq!(window_percent(5, 4, Duration::from_secs(1), 4), None);
        assert_eq!(window_percent(1, 2, Duration::ZERO, 4), None);
        // Zero CPUs never divide by zero.
        assert!(window_percent(0, 1_000_000, Duration::from_secs(1), 0).is_some());
    }

    #[test]
    fn live_sessions_are_read_from_their_cgroup_files() {
        let dir = tempfile::tempdir().unwrap();
        let write = |session: &str, procs: &str| {
            let d = dir.path().join(session);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("cpu.stat"), "usage_usec 12500000\nuser_usec 1\n").unwrap();
            std::fs::write(d.join("memory.current"), "432013312\n").unwrap();
            std::fs::write(d.join("memory.peak"), "500000000\n").unwrap();
            std::fs::write(d.join("pids.current"), format!("{procs}\n")).unwrap();
        };
        write("s-bbb", "18");
        write("s-aaa", "3");
        write("s-empty", "0");
        std::fs::create_dir_all(dir.path().join("holder")).unwrap();
        let sessions = session_usages(dir.path());
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["aaa", "bbb"],
            "the empty session and holder/ are left out"
        );
        assert!((sessions[0].cpu_seconds - 12.5).abs() < 1e-9);
        assert_eq!(sessions[1].memory_bytes, Some(432_013_312));
        assert_eq!(sessions[1].memory_peak_bytes, Some(500_000_000));
        assert_eq!(sessions[1].procs, Some(18));
        assert_eq!(session_usages(&dir.path().join("missing")).len(), 0);
    }

    #[test]
    fn only_the_hooks_holder_is_a_stopgap_holder() {
        assert!(is_stopgap_holder(b"sleep\x0086400\x00"));
        assert!(is_stopgap_holder(
            b"sluice\x00daemon\x00holder\x00--lifetime\x00600\x00"
        ));
        assert!(!is_stopgap_holder(
            b"sluice\x00daemon\x00holder\x00--lifetime\x00x\x00"
        ));
        let mut line = b"sluice\x00".to_vec();
        for a in &super::stopgap_holder_args() {
            line.extend_from_slice(a.as_bytes());
            line.push(0);
        }
        assert!(is_stopgap_holder(&line));
        assert!(!is_stopgap_holder(b"sleep\x00600\x00"));
        assert!(!is_stopgap_holder(b"sluice\x00daemon\x00holder\x00"));
        assert!(!is_stopgap_holder(b""));
    }
}
