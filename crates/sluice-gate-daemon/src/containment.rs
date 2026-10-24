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
use sluice_gate_core::cgroup::tree::{Layout, PruneReport, Tree, TreeSettings, write_pid};
use sluice_gate_core::cgroup::{CGROUP_MOUNT, CgroupError};
use sluice_gate_core::config::{ContainmentConfig, EnvMap};

use crate::interventions::{InterventionRow, MemoryWatcher, parse_oomd_journal, watch_targets};
use crate::reaper::ReaperCommand;

/// Empty session and job cgroups younger than this are left alone (a fresh
/// session may be about to receive its first process).
pub const PRUNE_GRACE: Duration = Duration::from_secs(60);

/// How far back the first journal read looks.
const FIRST_JOURNAL_LOOKBACK: Duration = Duration::from_secs(3600);
/// Overlap of consecutive journal reads (rows are stored once anyway).
const JOURNAL_OVERLAP: Duration = Duration::from_secs(30);

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
    agents_ready: bool,
    oomd_active: bool,
    holder: Mutex<Option<Child>>,
    watcher: Mutex<MemoryWatcher>,
    journal: Mutex<JournalState>,
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
        if !cfg.enabled {
            tracing::debug!("containment disabled by [containment] enabled = false");
            return None;
        }
        let caps = detect::detect(&setup.env);
        if !caps.usable() {
            tracing::info!("{}; the daemon runs as without containment", caps.summary());
            return None;
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
            return None;
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
        Some(Self {
            tree,
            exe: setup.exe.clone(),
            agents_ready,
            oomd_active: caps.oomd_active,
            holder: Mutex::new(holder),
            watcher: Mutex::new(MemoryWatcher::new()),
            journal: Mutex::new(JournalState::default()),
        })
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
    /// places a job in it (`sluice cg-exec <leaf> --`), to be put before the
    /// `nice`/`ionice` chain. Empty (the job runs unplaced) when the leaf
    /// cannot be created.
    #[must_use]
    pub fn job_prefix(&self, job_id: &str) -> Vec<OsString> {
        match self.tree.job_leaf(job_id) {
            Ok(leaf) => vec![
                self.exe.clone().into_os_string(),
                "cg-exec".into(),
                leaf.dir.into_os_string(),
                "--".into(),
            ],
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
        let mut rows = lock(&self.watcher).poll(&targets, now_ms);
        if self.agents_ready && self.oomd_active {
            rows.extend(self.journal_rows());
        }
        rows
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

/// The stopgap holder the session hook starts when it has to create the
/// agents scope before any daemon runs: `sleep 86400`.
fn is_stopgap_holder(cmdline: &[u8]) -> bool {
    cmdline == b"sleep\x0086400\x00"
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
    use super::is_stopgap_holder;

    #[test]
    fn only_the_hooks_sleep_is_a_stopgap_holder() {
        assert!(is_stopgap_holder(b"sleep\x0086400\x00"));
        assert!(!is_stopgap_holder(b"sleep\x00600\x00"));
        assert!(!is_stopgap_holder(b"sluice\x00daemon\x00holder\x00"));
        assert!(!is_stopgap_holder(b""));
    }
}
