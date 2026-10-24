//! The cgroup tree of Sluice (design section 1) and the operations on it.
//!
//! ```text
//! user@UID.service/
//! `-- sluice.slice                    CPUWeight=50
//!     |-- sluice-daemon.scope         Delegate=yes (daemon + gate jobs)
//!     |   |-- daemon/
//!     |   `-- jobs/<job-id>/
//!     `-- sluice-agents.scope         Delegate=yes, own oomd policy
//!         |-- holder/
//!         `-- s-<session-id>/         pids.max; +cpu +memory +pids
//!             |-- ui/                 cpu.weight 400
//!             `-- tools/              cpu.weight 25
//! ```
//!
//! Units are created through the user manager ([`super::systemd`]); inside
//! the delegated scopes Sluice owns the subtree and writes cgroupfs files
//! directly (single-writer rule of systemd's `CGROUP_DELEGATION`). Creation
//! order follows spike S3: a process lives in a leaf before controllers are
//! enabled for children, then the session node, its `subtree_control`, then
//! the leaves. Nothing here panics; every failure is a [`CgroupError`].

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{ContainmentConfig, EnvMap};

use super::detect::{
    Capabilities, Controllers, PressurePolicy, leaf_controllers, manager_cgroup_dir,
};
use super::systemd::{self, OomProperties, ScopeSpec};
use super::{CGROUP_MOUNT, CgResult, CgroupError, MAX_SESSION_ID};

/// Extended attribute that asks `systemd-oomd` to avoid killing a cgroup.
pub const OOMD_AVOID_XATTR: &str = "user.oomd_avoid";

/// Names of the unit and cgroup directories of one Sluice tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Cgroup v2 mount.
    pub mount: PathBuf,
    /// `user@UID.service` directory under the mount.
    pub user_manager: PathBuf,
    /// Slice unit name (`sluice.slice`).
    pub slice: String,
    /// Daemon scope unit name.
    pub daemon_scope: String,
    /// Agents scope unit name.
    pub agents_scope: String,
}

/// Directory of a slice unit relative to its parent manager: systemd nests
/// `a-b-c.slice` as `a.slice/a-b.slice/a-b-c.slice`.
#[must_use]
pub fn slice_relative_path(slice: &str) -> PathBuf {
    let base = slice.strip_suffix(".slice").unwrap_or(slice);
    let mut out = PathBuf::new();
    let mut prefix = String::new();
    for part in base.split('-').filter(|p| !p.is_empty()) {
        if !prefix.is_empty() {
            prefix.push('-');
        }
        prefix.push_str(part);
        out.push(format!("{prefix}.slice"));
    }
    out
}

impl Layout {
    /// The production layout (`sluice.slice`, `sluice-daemon.scope`,
    /// `sluice-agents.scope`) for `uid`.
    #[must_use]
    pub fn standard(uid: u32) -> Self {
        Self::with_prefix(Path::new(CGROUP_MOUNT), uid, "sluice")
    }

    /// A layout whose units are `<prefix>.slice`, `<prefix>-daemon.scope` and
    /// `<prefix>-agents.scope` (tests use `sluice-test-<pid>`).
    #[must_use]
    pub fn with_prefix(mount: &Path, uid: u32, prefix: &str) -> Self {
        Self {
            mount: mount.to_path_buf(),
            user_manager: manager_cgroup_dir(mount, uid),
            slice: format!("{prefix}.slice"),
            daemon_scope: format!("{prefix}-daemon.scope"),
            agents_scope: format!("{prefix}-agents.scope"),
        }
    }

    /// Cgroup directory of the slice.
    #[must_use]
    pub fn slice_dir(&self) -> PathBuf {
        self.user_manager.join(slice_relative_path(&self.slice))
    }

    /// Cgroup directory of the daemon scope.
    #[must_use]
    pub fn daemon_scope_dir(&self) -> PathBuf {
        self.slice_dir().join(&self.daemon_scope)
    }

    /// Cgroup directory of the agents scope.
    #[must_use]
    pub fn agents_scope_dir(&self) -> PathBuf {
        self.slice_dir().join(&self.agents_scope)
    }

    /// `daemon/` leaf of the daemon scope.
    #[must_use]
    pub fn daemon_leaf_dir(&self) -> PathBuf {
        self.daemon_scope_dir().join("daemon")
    }

    /// `jobs/` node of the daemon scope.
    #[must_use]
    pub fn jobs_dir(&self) -> PathBuf {
        self.daemon_scope_dir().join("jobs")
    }

    /// `holder/` leaf of the agents scope.
    #[must_use]
    pub fn holder_dir(&self) -> PathBuf {
        self.agents_scope_dir().join("holder")
    }

    /// Cgroup directory of a session (`name` is `s-<id>`).
    #[must_use]
    pub fn session_dir(&self, name: &str) -> PathBuf {
        self.agents_scope_dir().join(name)
    }
}

/// A cgroup a process can be moved into, with what `AttachProcessesToUnit`
/// needs as the fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    /// Cgroup directory.
    pub dir: PathBuf,
    /// Owning delegated scope unit.
    pub unit: String,
    /// Path inside the unit (`/s-abc/ui`).
    pub rel: String,
}

impl Leaf {
    /// The `cgroup.procs` file of the leaf.
    #[must_use]
    pub fn procs_file(&self) -> PathBuf {
        self.dir.join("cgroup.procs")
    }
}

/// The two leaves of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Session directory name (`s-<id>`).
    pub name: String,
    /// The session node.
    pub dir: PathBuf,
    /// Leaf of the agent and its MCP servers.
    pub ui: Leaf,
    /// Leaf of the commands the agent runs.
    pub tools: Leaf,
    /// Controllers whose files exist in the leaves (the authority: a weight
    /// is only written where `cpu.weight` exists).
    pub controllers: Controllers,
}

/// How [`Tree::move_pid`] placed a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacedBy {
    /// Direct write to `cgroup.procs`.
    Direct,
    /// `AttachProcessesToUnit` (after the direct write was refused).
    Attached,
}

/// Result of [`Tree::move_pids`].
#[derive(Debug)]
pub struct MoveReport {
    /// Moved by writing `cgroup.procs`.
    pub direct: Vec<u32>,
    /// Moved by the manager after a refused write.
    pub attached: Vec<u32>,
    /// Not moved, with the reason.
    pub failed: Vec<(u32, CgroupError)>,
}

/// What [`Tree::prune`] removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    /// Empty session cgroups removed.
    pub sessions: usize,
    /// Empty job cgroups removed.
    pub jobs: usize,
    /// Session or job cgroups left because they still hold processes.
    pub busy: usize,
}

/// `memory.events` counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MemoryEvents {
    /// Times `memory.high` was exceeded.
    pub high: u64,
    /// Times `memory.max` was hit.
    pub max: u64,
    /// OOM conditions.
    pub oom: u64,
    /// Processes killed by the kernel OOM killer.
    pub oom_kill: u64,
    /// Groups killed as a unit (`memory.oom.group`).
    pub oom_group_kill: u64,
}

/// Readings of one cgroup; `None` where the controller or file is absent.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// `usage_usec` of `cpu.stat`.
    pub cpu_usec: Option<u64>,
    /// `memory.current`.
    pub memory_current: Option<u64>,
    /// `memory.peak`.
    pub memory_peak: Option<u64>,
    /// `pids.current`.
    pub pids_current: Option<u64>,
    /// `memory.events`.
    pub memory_events: MemoryEvents,
}

/// Values written into the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSettings {
    /// `CPUWeight` of the slice; `None` when `cpu` is not delegated.
    pub slice_weight: Option<u32>,
    /// `CPUWeight` of the daemon scope.
    pub daemon_weight: Option<u32>,
    /// `CPUWeight` of the agents scope.
    pub agents_weight: Option<u32>,
    /// `cpu.weight` of `ui` leaves.
    pub ui_weight: u32,
    /// `cpu.weight` of `tools` leaves.
    pub tools_weight: u32,
    /// `pids.max` of a session; `None` when `pids` is not delegated.
    pub pids_max: Option<u32>,
    /// `CPUQuotaPerSecUSec` of the agents scope (the optional ceiling).
    pub cpu_quota_usec_per_sec: Option<u64>,
    /// Opt-in `memory.high` per session, bytes.
    pub memory_high: Option<u64>,
    /// Opt-in `memory.max` per session, bytes.
    pub memory_max: Option<u64>,
    /// Own memory pressure policy of the agents scope.
    pub oom: Option<OomProperties>,
}

impl TreeSettings {
    /// Derives the settings from the user config and what detection found.
    /// `pressure` is the (derived or overridden) policy, used only when the
    /// capabilities allow it (`systemd-oomd` active, systemd >= 257, policy
    /// `oomd`). `cpus` converts a `cpu_ceiling` percentage to a quota.
    #[must_use]
    pub fn from_config(
        cfg: &ContainmentConfig,
        caps: &Capabilities,
        pressure: PressurePolicy,
        cpus: u32,
    ) -> Self {
        let cpu = caps.controllers.cpu;
        let weight = |w: u32| cpu.then_some(w);
        Self {
            slice_weight: weight(cfg.slice_cpu_weight),
            daemon_weight: weight(cfg.gates_cpu_weight),
            agents_weight: weight(cfg.agents_cpu_weight),
            ui_weight: cfg.ui_cpu_weight,
            tools_weight: cfg.tools_cpu_weight,
            pids_max: caps.controllers.pids.then_some(cfg.pids_max),
            cpu_quota_usec_per_sec: cfg
                .cpu_ceiling_percent()
                .filter(|_| cpu)
                .map(|p| u64::from(p) * u64::from(cpus.max(1)) * 10_000),
            memory_high: cfg.memory_high_bytes().filter(|_| caps.controllers.memory),
            memory_max: cfg.memory_max_bytes().filter(|_| caps.controllers.memory),
            oom: caps.oomd_policy_enabled(cfg).then_some(OomProperties {
                limit_percent: pressure.limit_percent,
                duration: Some(pressure.duration),
            }),
        }
    }
}

/// A Sluice tree: where it is, what to write, and the environment for the
/// tools it shells out to.
#[derive(Debug, Clone)]
pub struct Tree {
    /// Unit and directory names.
    pub layout: Layout,
    /// Values to apply.
    pub settings: TreeSettings,
    env: EnvMap,
}

// ---------------------------------------------------------------------------
// cgroupfs helpers
// ---------------------------------------------------------------------------

/// Writes `value` to the cgroup control file `dir/name`.
///
/// # Errors
///
/// [`CgroupError::Io`] (permission denied, read-only, no such file, busy).
pub fn write_control(dir: &Path, name: &str, value: &str) -> CgResult<()> {
    let path = dir.join(name);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|mut f| f.write_all(value.as_bytes()))
        .map_err(|e| CgroupError::io("writing cgroup file", path, e))
}

/// Like [`write_control`] but a missing file (controller not enabled) is
/// not an error: returns whether the value was written.
fn write_if_present(dir: &Path, name: &str, value: &str) -> CgResult<bool> {
    if !dir.join(name).exists() {
        return Ok(false);
    }
    write_control(dir, name, value).map(|()| true)
}

/// Writes `pid` to `<leaf_dir>/cgroup.procs`: the self-move of `cg-exec` and
/// the direct path of [`Tree::move_pid`].
///
/// # Errors
///
/// [`CgroupError::Io`].
pub fn write_pid(leaf_dir: &Path, pid: u32) -> CgResult<()> {
    write_control(leaf_dir, "cgroup.procs", &format!("{pid}\n"))
}

/// Enables `cpu`, `memory` and `pids` for the children of `dir`, for those
/// present in its `cgroup.controllers`. Returns the enabled ones.
fn enable_controllers(dir: &Path) -> CgResult<Vec<&'static str>> {
    let available = systemd::read_to_string(&dir.join("cgroup.controllers"))?;
    let wanted: Vec<&'static str> = ["cpu", "memory", "pids"]
        .into_iter()
        .filter(|c| available.split_whitespace().any(|a| a == *c))
        .collect();
    if !wanted.is_empty() {
        let line = wanted
            .iter()
            .map(|c| format!("+{c}"))
            .collect::<Vec<_>>()
            .join(" ");
        write_control(dir, "cgroup.subtree_control", &line)?;
    }
    Ok(wanted)
}

/// Creates one cgroup directory; an existing one is fine. The parent must
/// exist: Sluice never builds the units' own directories by hand (systemd
/// owns them), so a missing scope is an error, not something to `mkdir -p`.
fn make_dir(path: &Path) -> CgResult<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(CgroupError::io("creating cgroup", path, e)),
    }
}

fn valid_node_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|rest| {
        !rest.is_empty()
            && rest.len() <= MAX_SESSION_ID
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

/// Parses `key value` lines (`cpu.stat`, `memory.events`).
#[must_use]
pub fn parse_keyed_u64(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let mut w = l.split_whitespace();
        (w.next()? == key).then(|| w.next()?.parse().ok())?
    })
}

/// `memory.events` text to counters.
#[must_use]
pub fn parse_memory_events(text: &str) -> MemoryEvents {
    let get = |k| parse_keyed_u64(text, k).unwrap_or(0);
    MemoryEvents {
        high: get("high"),
        max: get("max"),
        oom: get("oom"),
        oom_kill: get("oom_kill"),
        oom_group_kill: get("oom_group_kill"),
    }
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Reads the usage counters of a cgroup directory. Missing files give `None`.
#[must_use]
pub fn read_usage(dir: &Path) -> Usage {
    Usage {
        cpu_usec: fs::read_to_string(dir.join("cpu.stat"))
            .ok()
            .and_then(|t| parse_keyed_u64(&t, "usage_usec")),
        memory_current: read_u64(&dir.join("memory.current")),
        memory_peak: read_u64(&dir.join("memory.peak")),
        pids_current: read_u64(&dir.join("pids.current")),
        memory_events: fs::read_to_string(dir.join("memory.events"))
            .map(|t| parse_memory_events(&t))
            .unwrap_or_default(),
    }
}

/// Sets `user.oomd_avoid=1` on `dir`. `true` when it was set; unsupported
/// filesystems and permission failures return `false` silently.
#[must_use]
pub fn set_oomd_avoid(dir: &Path) -> bool {
    rustix::fs::setxattr(dir, OOMD_AVOID_XATTR, b"1", rustix::fs::XattrFlags::empty()).is_ok()
}

/// Reads `user.oomd_avoid` back (`true` when set to `1`).
#[must_use]
pub fn has_oomd_avoid(dir: &Path) -> bool {
    let mut buf = [0u8; 4];
    rustix::fs::getxattr(dir, OOMD_AVOID_XATTR, &mut buf).is_ok_and(|n| n >= 1 && buf[0] == b'1')
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn old_enough(path: &Path, min_age: Duration) -> bool {
    if min_age.is_zero() {
        return true;
    }
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age >= min_age)
}

fn remove_cgroup(path: &Path) -> bool {
    fs::remove_dir(path).is_ok()
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

impl Tree {
    /// A tree handle. `env` is passed to `busctl` and `systemctl`
    /// (`XDG_RUNTIME_DIR` must be in it for the user manager to be found).
    #[must_use]
    pub fn new(layout: Layout, settings: TreeSettings, env: EnvMap) -> Self {
        Self {
            layout,
            settings,
            env,
        }
    }

    /// The environment the tools run with.
    #[must_use]
    pub fn env(&self) -> &EnvMap {
        &self.env
    }

    /// Creates (or updates) the slice with its `CPUWeight`.
    ///
    /// # Errors
    ///
    /// Tool errors from `systemctl`.
    pub fn ensure_slice(&self) -> CgResult<()> {
        let assignments: Vec<String> = self
            .settings
            .slice_weight
            .map(|w| format!("CPUWeight={w}"))
            .into_iter()
            .collect();
        systemd::set_slice_properties(&self.env, &self.layout.slice, &assignments)
    }

    /// Creates the scope (adopting `pid`) unless its cgroup exists, then
    /// builds the nodes inside it: the leaf `first_leaf` receives `pid`,
    /// controllers are enabled for children, and each of `nodes` is created
    /// with controllers enabled for its children.
    fn ensure_scope(
        &self,
        spec: &ScopeSpec,
        scope_dir: &Path,
        first_leaf: &str,
        pid: u32,
        nodes: &[&str],
    ) -> CgResult<()> {
        self.ensure_slice()?;
        if !scope_dir.join("cgroup.procs").exists() {
            systemd::start_transient_scope(&self.env, spec)?;
            if !wait_for(&scope_dir.join("cgroup.procs"), Duration::from_secs(3)) {
                return Err(CgroupError::io(
                    "waiting for the scope cgroup",
                    scope_dir,
                    std::io::Error::new(std::io::ErrorKind::NotFound, "cgroup did not appear"),
                ));
            }
        }
        let leaf = scope_dir.join(first_leaf);
        make_dir(&leaf)?;
        // A process that is not in the scope yet (a second start finding the
        // scope alive) may live where a direct write is refused, for example
        // a login session scope: `move_pid` then asks the manager.
        self.move_pid(
            &Leaf {
                dir: leaf,
                unit: spec.name.clone(),
                rel: format!("/{first_leaf}"),
            },
            pid,
        )?;
        enable_controllers(scope_dir)?;
        for node in nodes {
            let dir = scope_dir.join(node);
            make_dir(&dir)?;
            enable_controllers(&dir)?;
        }
        Ok(())
    }

    /// Creates the daemon scope (`daemon/` holding `pid`, `jobs/` for the
    /// per-job leaves). Idempotent: with the scope already there only the
    /// move and the missing nodes are redone.
    ///
    /// # Errors
    ///
    /// Tool or filesystem errors; the caller keeps running uncontained.
    pub fn ensure_daemon_scope(&self, pid: u32) -> CgResult<()> {
        let spec = ScopeSpec {
            name: self.layout.daemon_scope.clone(),
            slice: self.layout.slice.clone(),
            pids: vec![pid],
            description: "Sluice daemon and gate jobs".to_owned(),
            cpu_weight: self.settings.daemon_weight,
            cpu_quota_usec_per_sec: None,
            oom: None,
        };
        self.ensure_scope(
            &spec,
            &self.layout.daemon_scope_dir(),
            "daemon",
            pid,
            &["jobs"],
        )
    }

    /// Creates the agents scope adopting the `holder` process (a tiny process
    /// that keeps the scope alive), with the memory pressure policy and the
    /// optional CPU ceiling of the settings. Idempotent.
    ///
    /// # Errors
    ///
    /// Tool or filesystem errors; the caller keeps running uncontained.
    pub fn ensure_agents_scope(&self, holder_pid: u32) -> CgResult<()> {
        let spec = ScopeSpec {
            name: self.layout.agents_scope.clone(),
            slice: self.layout.slice.clone(),
            pids: vec![holder_pid],
            description: "Sluice agent sessions".to_owned(),
            cpu_weight: self.settings.agents_weight,
            cpu_quota_usec_per_sec: self.settings.cpu_quota_usec_per_sec,
            oom: self.settings.oom,
        };
        self.ensure_scope(
            &spec,
            &self.layout.agents_scope_dir(),
            "holder",
            holder_pid,
            &[],
        )
    }

    /// Creates (or completes) `s-<id>/{ui,tools}` with weights, `pids.max`
    /// and the opt-in memory limits, and marks `ui` with `user.oomd_avoid`.
    /// An existing session is fine (resume reuses the name).
    ///
    /// # Errors
    ///
    /// [`CgroupError::Invalid`] for a name that is not `s-<[A-Za-z0-9_-]{1,64}>`,
    /// filesystem errors otherwise (the agents scope must exist).
    pub fn ensure_session(&self, name: &str) -> CgResult<Session> {
        if !valid_node_name(name, "s-") {
            return Err(CgroupError::Invalid(format!("session name {name:?}")));
        }
        let dir = self.layout.session_dir(name);
        make_dir(&dir)?;
        enable_controllers(&dir)?;
        let mk = |leaf: &str, weight: u32| -> CgResult<Leaf> {
            let ldir = dir.join(leaf);
            make_dir(&ldir)?;
            write_if_present(&ldir, "cpu.weight", &weight.to_string())?;
            Ok(Leaf {
                dir: ldir,
                unit: self.layout.agents_scope.clone(),
                rel: format!("/{name}/{leaf}"),
            })
        };
        let ui = mk("ui", self.settings.ui_weight)?;
        let tools = mk("tools", self.settings.tools_weight)?;
        if let Some(p) = self.settings.pids_max {
            write_if_present(&dir, "pids.max", &p.to_string())?;
        }
        if let Some(b) = self.settings.memory_high {
            write_if_present(&dir, "memory.high", &b.to_string())?;
        }
        if let Some(b) = self.settings.memory_max {
            write_if_present(&dir, "memory.max", &b.to_string())?;
        }
        let _ = set_oomd_avoid(&ui.dir);
        let controllers = leaf_controllers(&ui.dir);
        Ok(Session {
            name: name.to_owned(),
            dir,
            ui,
            tools,
            controllers,
        })
    }

    /// Creates `jobs/<id>` for a gate or exec job and returns its leaf.
    ///
    /// # Errors
    ///
    /// [`CgroupError::Invalid`] for an id with characters outside
    /// `[A-Za-z0-9_-]`, filesystem errors otherwise.
    pub fn job_leaf(&self, job_id: &str) -> CgResult<Leaf> {
        if !valid_node_name(&format!("j-{job_id}"), "j-") {
            return Err(CgroupError::Invalid(format!("job id {job_id:?}")));
        }
        let dir = self.layout.jobs_dir().join(job_id);
        make_dir(&dir)?;
        Ok(Leaf {
            dir,
            unit: self.layout.daemon_scope.clone(),
            rel: format!("/jobs/{job_id}"),
        })
    }

    /// Moves `pid` into `leaf`: a direct write to `cgroup.procs`, and on
    /// `EACCES` one `AttachProcessesToUnit` through the user manager.
    ///
    /// # Errors
    ///
    /// The direct write's error when it is not a permission problem, or the
    /// attach error when both paths fail.
    pub fn move_pid(&self, leaf: &Leaf, pid: u32) -> CgResult<PlacedBy> {
        match write_pid(&leaf.dir, pid) {
            Ok(()) => Ok(PlacedBy::Direct),
            Err(e) if e.is_permission_denied() => {
                systemd::attach_processes(&self.env, &leaf.unit, &leaf.rel, &[pid])?;
                Ok(PlacedBy::Attached)
            }
            Err(e) => Err(e),
        }
    }

    /// Moves several processes: direct writes first, then one
    /// `AttachProcessesToUnit` for those refused with a permission error.
    /// Processes that exited meanwhile end up in `failed`.
    #[must_use]
    pub fn move_pids(&self, leaf: &Leaf, pids: &[u32]) -> MoveReport {
        let mut report = MoveReport {
            direct: Vec::new(),
            attached: Vec::new(),
            failed: Vec::new(),
        };
        let mut refused = Vec::new();
        for &pid in pids {
            match write_pid(&leaf.dir, pid) {
                Ok(()) => report.direct.push(pid),
                Err(e) if e.is_permission_denied() => refused.push(pid),
                Err(e) => report.failed.push((pid, e)),
            }
        }
        if !refused.is_empty() {
            match systemd::attach_processes(&self.env, &leaf.unit, &leaf.rel, &refused) {
                Ok(()) => report.attached = refused,
                Err(e) => {
                    let msg = e.to_string();
                    for pid in refused {
                        report.failed.push((
                            pid,
                            CgroupError::Invalid(format!("attach of pid {pid}: {msg}")),
                        ));
                    }
                }
            }
        }
        report
    }

    /// Removes empty session and job cgroups older than `min_age` (a fresh
    /// session may be about to receive its first process). `rmdir` on a
    /// cgroup fails while it holds processes, so nothing alive is touched.
    #[must_use]
    pub fn prune(&self, min_age: Duration) -> PruneReport {
        let mut report = PruneReport::default();
        let prune_children = |parent: &Path, prefix: &str, nested: bool| -> (usize, usize) {
            let (mut removed, mut busy) = (0, 0);
            let Ok(entries) = fs::read_dir(parent) else {
                return (0, 0);
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                if !path.is_dir() || !name.to_string_lossy().starts_with(prefix) {
                    continue;
                }
                if !old_enough(&path, min_age) {
                    continue;
                }
                if nested && let Ok(leaves) = fs::read_dir(&path) {
                    for leaf in leaves.flatten() {
                        if leaf.path().is_dir() {
                            let _ = remove_cgroup(&leaf.path());
                        }
                    }
                }
                if remove_cgroup(&path) {
                    removed += 1;
                } else {
                    busy += 1;
                }
            }
            (removed, busy)
        };
        let (s, sb) = prune_children(&self.layout.agents_scope_dir(), "s-", true);
        let (j, jb) = prune_children(&self.layout.jobs_dir(), "", false);
        report.sessions = s;
        report.jobs = j;
        report.busy = sb + jb;
        report
    }

    /// Usage of a cgroup directory (a session node sums its leaves).
    #[must_use]
    pub fn usage(&self, dir: &Path) -> Usage {
        read_usage(dir)
    }

    /// Stops both scopes and the slice. For tests and `daemon stop --purge`;
    /// the processes inside are killed by systemd.
    ///
    /// # Errors
    ///
    /// Tool errors; units that are not loaded are fine.
    pub fn stop_all(&self) -> CgResult<()> {
        systemd::stop_units(
            &self.env,
            &[
                &self.layout.agents_scope,
                &self.layout.daemon_scope,
                &self.layout.slice,
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_paths_nest_by_dash() {
        assert_eq!(
            slice_relative_path("sluice.slice"),
            PathBuf::from("sluice.slice")
        );
        assert_eq!(
            slice_relative_path("sluice-test-123.slice"),
            PathBuf::from("sluice.slice/sluice-test.slice/sluice-test-123.slice")
        );
    }

    #[test]
    fn standard_layout_paths() {
        let l = Layout::standard(1000);
        let um = "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service";
        assert_eq!(l.slice_dir(), PathBuf::from(format!("{um}/sluice.slice")));
        assert_eq!(
            l.daemon_scope_dir(),
            PathBuf::from(format!("{um}/sluice.slice/sluice-daemon.scope"))
        );
        assert_eq!(
            l.agents_scope_dir(),
            PathBuf::from(format!("{um}/sluice.slice/sluice-agents.scope"))
        );
        assert_eq!(l.daemon_leaf_dir(), l.daemon_scope_dir().join("daemon"));
        assert_eq!(l.jobs_dir(), l.daemon_scope_dir().join("jobs"));
        assert_eq!(l.holder_dir(), l.agents_scope_dir().join("holder"));
        assert_eq!(l.session_dir("s-abc"), l.agents_scope_dir().join("s-abc"));
    }

    #[test]
    fn prefixed_layout_for_tests() {
        let l = Layout::with_prefix(Path::new("/m"), 7, "sluice-test-9");
        assert_eq!(l.slice, "sluice-test-9.slice");
        assert_eq!(l.daemon_scope, "sluice-test-9-daemon.scope");
        assert_eq!(l.agents_scope, "sluice-test-9-agents.scope");
        assert_eq!(
            l.agents_scope_dir(),
            PathBuf::from(
                "/m/user.slice/user-7.slice/user@7.service/sluice.slice/sluice-test.slice/sluice-test-9.slice/sluice-test-9-agents.scope"
            )
        );
    }

    #[test]
    fn node_names_are_validated() {
        assert!(valid_node_name("s-abc_1-2", "s-"));
        assert!(!valid_node_name("s-", "s-"));
        assert!(!valid_node_name("s-a/b", "s-"));
        assert!(!valid_node_name("s-..", "s-"));
        assert!(!valid_node_name("abc", "s-"));
        assert!(!valid_node_name(&format!("s-{}", "x".repeat(65)), "s-"));
    }

    #[test]
    fn counters_parse() {
        assert_eq!(
            parse_keyed_u64(
                "usage_usec 123\nuser_usec 100\nsystem_usec 23\n",
                "usage_usec"
            ),
            Some(123)
        );
        assert_eq!(parse_keyed_u64("a 1\n", "b"), None);
        let ev = parse_memory_events("low 0\nhigh 4\nmax 2\noom 1\noom_kill 1\noom_group_kill 0\n");
        assert_eq!(
            ev,
            MemoryEvents {
                high: 4,
                max: 2,
                oom: 1,
                oom_kill: 1,
                oom_group_kill: 0
            }
        );
        assert_eq!(parse_memory_events(""), MemoryEvents::default());
    }

    #[test]
    fn usage_of_a_plain_directory_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_usage(dir.path()), Usage::default());
        fs::write(dir.path().join("cpu.stat"), "usage_usec 77\n").unwrap();
        fs::write(dir.path().join("memory.current"), "4096\n").unwrap();
        fs::write(dir.path().join("pids.current"), "3\n").unwrap();
        let u = read_usage(dir.path());
        assert_eq!(
            (u.cpu_usec, u.memory_current, u.pids_current, u.memory_peak),
            (Some(77), Some(4096), Some(3), None)
        );
    }

    #[test]
    fn write_helpers_fail_typed_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let err = write_control(dir.path(), "missing", "1").unwrap_err();
        assert!(matches!(err, CgroupError::Io { .. }));
        assert!(!err.is_permission_denied());
        assert!(!write_if_present(dir.path(), "missing", "1").unwrap());
        fs::write(dir.path().join("cpu.weight"), "").unwrap();
        assert!(write_if_present(dir.path(), "cpu.weight", "25").unwrap());
        assert_eq!(
            fs::read_to_string(dir.path().join("cpu.weight")).unwrap(),
            "25"
        );
        assert!(enable_controllers(dir.path()).is_err());
    }

    #[test]
    fn prune_removes_only_empty_directories() {
        let root = tempfile::tempdir().unwrap();
        let layout = Layout {
            mount: root.path().to_path_buf(),
            user_manager: root.path().to_path_buf(),
            slice: "x.slice".into(),
            daemon_scope: "x-daemon.scope".into(),
            agents_scope: "x-agents.scope".into(),
        };
        let tree = Tree::new(
            layout.clone(),
            TreeSettings {
                slice_weight: None,
                daemon_weight: None,
                agents_weight: None,
                ui_weight: 400,
                tools_weight: 25,
                pids_max: None,
                cpu_quota_usec_per_sec: None,
                memory_high: None,
                memory_max: None,
                oom: None,
            },
            EnvMap::new(),
        );
        for d in ["s-a/ui", "s-a/tools", "s-b/ui", "holder"] {
            fs::create_dir_all(layout.agents_scope_dir().join(d)).unwrap();
        }
        fs::create_dir_all(layout.jobs_dir().join("j1")).unwrap();
        // s-b is "busy": a plain file in a directory makes remove_dir fail,
        // like a populated cgroup does.
        fs::write(layout.agents_scope_dir().join("s-b/ui/cgroup.procs"), "1\n").unwrap();
        let r = tree.prune(Duration::ZERO);
        assert_eq!(
            r,
            PruneReport {
                sessions: 1,
                jobs: 1,
                busy: 1
            }
        );
        assert!(!layout.session_dir("s-a").exists());
        assert!(layout.session_dir("s-b").exists());
        assert!(layout.holder_dir().exists(), "holder is never pruned");
        // a young session survives a grace period
        fs::create_dir_all(layout.session_dir("s-new/ui")).unwrap();
        let r = tree.prune(Duration::from_secs(3600));
        assert_eq!(r.sessions, 0);
        assert!(layout.session_dir("s-new").exists());
    }

    #[test]
    fn bad_names_never_touch_the_filesystem() {
        let tree = Tree::new(
            Layout::with_prefix(Path::new("/nonexistent"), 1, "x"),
            TreeSettings {
                slice_weight: None,
                daemon_weight: None,
                agents_weight: None,
                ui_weight: 1,
                tools_weight: 1,
                pids_max: None,
                cpu_quota_usec_per_sec: None,
                memory_high: None,
                memory_max: None,
                oom: None,
            },
            EnvMap::new(),
        );
        assert!(matches!(
            tree.ensure_session("../x"),
            Err(CgroupError::Invalid(_))
        ));
        assert!(matches!(tree.job_leaf("a/b"), Err(CgroupError::Invalid(_))));
        assert!(tree.ensure_session("s-ok").is_err());
    }

    #[test]
    fn settings_follow_capabilities() {
        use crate::cgroup::detect::{Facts, FsKind, evaluate, parse_controllers};
        let facts = |ctl: &str, oomd: bool, ver: u32| Facts {
            uid: 1,
            fs: FsKind::Cgroup2,
            read_only: false,
            systemctl_present: true,
            systemd_version: Some(ver),
            manager_socket: true,
            manager_state: Some("running".into()),
            manager_controllers: Some(parse_controllers(ctl)),
            own_cgroup: None,
            system_bus: true,
            oomd_active: oomd,
        };
        let cfg = ContainmentConfig {
            cpu_ceiling: "50%".into(),
            ..ContainmentConfig::default()
        };
        let p = PressurePolicy {
            limit_percent: 30,
            duration: Duration::from_secs(5),
        };
        let full =
            TreeSettings::from_config(&cfg, &evaluate(facts("cpu memory pids", true, 259)), p, 8);
        assert_eq!(full.slice_weight, Some(50));
        assert_eq!(full.pids_max, Some(4096));
        assert_eq!(full.cpu_quota_usec_per_sec, Some(4_000_000));
        assert_eq!(
            full.oom,
            Some(OomProperties {
                limit_percent: 30,
                duration: Some(Duration::from_secs(5))
            })
        );
        let nocpu =
            TreeSettings::from_config(&cfg, &evaluate(facts("memory pids", true, 255)), p, 8);
        assert_eq!(nocpu.slice_weight, None);
        assert_eq!(nocpu.cpu_quota_usec_per_sec, None);
        assert_eq!(nocpu.oom, None, "systemd 255 has no per-unit duration");
        let nomem = TreeSettings::from_config(&cfg, &evaluate(facts("cpu pids", true, 259)), p, 8);
        assert_eq!(nomem.oom, None);
        let noopt =
            TreeSettings::from_config(&cfg, &evaluate(facts("cpu memory pids", false, 259)), p, 8);
        assert_eq!(noopt.oom, None, "no systemd-oomd, no policy");
    }
}
