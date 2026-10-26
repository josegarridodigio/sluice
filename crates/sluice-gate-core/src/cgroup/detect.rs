#![allow(clippy::case_sensitive_file_extension_comparisons)]
//! What the machine allows (design section 6, spike S4).
//!
//! [`gather`] reads the facts (cheap, read-only, nothing is created or
//! moved) and [`evaluate`] turns them into [`Capabilities`] with an
//! [`Outcome`]; the second step is pure, so the matrix of the S4 note is
//! unit-tested with fabricated [`Facts`]. The doctor messages are the S4 ones.

use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::sys::statfs::{CGROUP2_SUPER_MAGIC, TMPFS_MAGIC};

use crate::config::{ContainmentConfig, EnvMap, MemoryPolicy};

use super::systemd;
use super::{CGROUP_MOUNT, CgResult, CgroupError};

/// First systemd release with a per-unit `ManagedOOMMemoryPressureDurationUSec`.
pub const OOMD_PER_UNIT_DURATION: u32 = 257;
/// First systemd release whose user manager delegates `cpu`.
pub const CPU_DELEGATION_VERSION: u32 = 252;

/// What `/sys/fs/cgroup` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    /// cgroup v2, the unified hierarchy.
    Cgroup2,
    /// A `tmpfs` of v1 controllers (pure v1, or hybrid).
    V1OrHybrid,
    /// Not a cgroup filesystem, or not mounted.
    NoCgroup,
}

/// Raw observations, as gathered by [`gather`].
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// Numeric user id.
    pub uid: u32,
    /// Kind of `/sys/fs/cgroup`, by `statfs` magic.
    pub fs: FsKind,
    /// The cgroup mount is read-only (container, sandbox).
    pub read_only: bool,
    /// `systemctl` could be spawned.
    pub systemctl_present: bool,
    /// `systemctl --version` major version.
    pub systemd_version: Option<u32>,
    /// `$XDG_RUNTIME_DIR/systemd/private` or `/bus` is a socket.
    pub manager_socket: bool,
    /// `systemctl --user is-system-running` output.
    pub manager_state: Option<String>,
    /// Words of `cgroup.controllers` of `user@UID.service`; `None` unreadable.
    pub manager_controllers: Option<Vec<String>>,
    /// Own cgroup path from `/proc/self/cgroup` (without the `0::`).
    pub own_cgroup: Option<String>,
    /// `/run/dbus/system_bus_socket` exists.
    pub system_bus: bool,
    /// `systemctl is-active systemd-oomd`.
    pub oomd_active: bool,
}

/// Controllers the user manager delegates (the ones Sluice uses).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Controllers {
    /// `cpu`: weights.
    pub cpu: bool,
    /// `memory`: accounting, `memory.high`/`max`, oomd.
    pub memory: bool,
    /// `pids`: `pids.max`.
    pub pids: bool,
}

/// Where the calling process lives, relative to the user manager's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Under `user@UID.service`: the process can move itself.
    SelfMoveOk,
    /// In a login `session-N.scope`: cannot move itself, attach by the manager.
    LoginScope,
    /// Somewhere else (container, other manager).
    Outside,
    /// Unknown (unreadable `/proc/self/cgroup`).
    Unknown,
}

/// Why containment is off, with the `doctor` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OffReason {
    /// cgroup v1 or hybrid.
    CgroupV1Hybrid,
    /// No cgroup filesystem.
    NoCgroupFs,
    /// The cgroup mount is read-only.
    ReadOnly,
    /// No `systemctl`.
    NoSystemd,
    /// No user manager socket (or no answer).
    UserManagerUnreachable,
    /// The user manager is in this state (`starting`, `stopping`, ...).
    UserManagerState(String),
    /// `cgroup.controllers` of the user manager is unreadable.
    ControllersUnreadable,
    /// None of `cpu`, `memory`, `pids` is delegated.
    NoControllers,
}

impl OffReason {
    /// Stable identifier (the `doctor` check code).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::CgroupV1Hybrid | Self::NoCgroupFs => "cg-v2",
            Self::ReadOnly => "cg-writable",
            Self::NoSystemd => "systemd",
            Self::UserManagerUnreachable | Self::UserManagerState(_) => "user-manager",
            Self::ControllersUnreadable | Self::NoControllers => "delegate-cpu",
        }
    }

    /// The `doctor` message (cause, consequence, fix when one exists).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::CgroupV1Hybrid => "cgroup v1/hybrid: containment off (needs the unified hierarchy). Boot with systemd.unified_cgroup_hierarchy=1 or use a distribution that defaults to cgroup v2.".to_owned(),
            Self::NoCgroupFs => "no cgroup filesystem visible: containment off".to_owned(),
            Self::ReadOnly => "cgroup filesystem is read-only (container or sandbox): containment off; the runtime's own limits apply".to_owned(),
            Self::NoSystemd => "no systemd: containment off (nice fallback active)".to_owned(),
            Self::UserManagerUnreachable => "systemd user manager not reachable (no $XDG_RUNTIME_DIR/systemd/private): containment off. Typical for containers, cron and su. Run inside a login session (loginctl enable-linger USER for headless machines).".to_owned(),
            Self::UserManagerState(s) => format!("systemd user manager is {s}: retry later"),
            Self::ControllersUnreadable => "cannot read cgroup.controllers of user@UID.service: containment off".to_owned(),
            Self::NoControllers => "no cgroup controller (cpu, memory, pids) is delegated to the user manager: containment off".to_owned(),
        }
    }

    /// Short reason for the summary line `containment: off (<reason>)`.
    #[must_use]
    pub fn short(&self) -> String {
        match self {
            Self::CgroupV1Hybrid => "cgroup v1/hybrid".to_owned(),
            Self::NoCgroupFs => "no cgroup filesystem".to_owned(),
            Self::ReadOnly => "read-only cgroup filesystem: container".to_owned(),
            Self::NoSystemd => "no systemd".to_owned(),
            Self::UserManagerUnreachable => "user manager not reachable".to_owned(),
            Self::UserManagerState(s) => format!("user manager is {s}"),
            Self::ControllersUnreadable => "user manager cgroup unreadable".to_owned(),
            Self::NoControllers => "no controllers delegated".to_owned(),
        }
    }
}

/// What is missing when containment is only partial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// `cpu` not delegated: weights unavailable, `nice` fallback.
    Cpu,
    /// `memory` not delegated.
    Memory,
    /// `pids` not delegated.
    Pids,
    /// Login-scope sessions cannot be attached: no system bus.
    LoginScopeAttach,
}

/// The result of detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Everything works.
    Full,
    /// Works with the listed gaps.
    Partial {
        /// What is missing.
        missing: Vec<Missing>,
    },
    /// Containment is not used.
    Off {
        /// Why.
        reason: OffReason,
    },
}

/// `doctor` severity, as in the existing doctor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// All good.
    Ok,
    /// Needs attention.
    Warn,
    /// Informational.
    Info,
}

/// One line of the `Contain` section of `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorLine {
    /// Stable check code (S4 matrix).
    pub code: &'static str,
    /// Severity.
    pub level: Level,
    /// Message.
    pub message: String,
}

/// Facts plus verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The verdict.
    pub outcome: Outcome,
    /// The delegated controllers (all false when off).
    pub controllers: Controllers,
    /// Where the calling process lives.
    pub placement: Placement,
    /// `systemd-oomd` is active.
    pub oomd_active: bool,
    /// systemd major version when known.
    pub systemd_version: Option<u32>,
    /// The observations behind the verdict.
    pub facts: Facts,
}

fn line(code: &'static str, level: Level, message: impl Into<String>) -> DoctorLine {
    DoctorLine {
        code,
        level,
        message: message.into(),
    }
}

impl Capabilities {
    /// `true` unless the outcome is `Off`.
    #[must_use]
    pub fn usable(&self) -> bool {
        !matches!(self.outcome, Outcome::Off { .. })
    }

    /// The own memory pressure policy of the agents scope can be set:
    /// `systemd-oomd` is active and systemd has the per-unit duration.
    #[must_use]
    pub fn oomd_policy_supported(&self) -> bool {
        self.usable()
            && self.controllers.memory
            && self.oomd_active
            && self
                .systemd_version
                .is_some_and(|v| v >= OOMD_PER_UNIT_DURATION)
    }

    /// Same, also honouring `[containment] memory_policy`.
    #[must_use]
    pub fn oomd_policy_enabled(&self, cfg: &ContainmentConfig) -> bool {
        cfg.memory_policy == MemoryPolicy::Oomd && self.oomd_policy_supported()
    }

    /// The summary split as `sluice status` shows it: `("full", None)`,
    /// `("partial", Some("memory, pids; cpu by nice"))` or
    /// `("off", Some("<reason>"))`.
    #[must_use]
    pub fn level_detail(&self) -> (&'static str, Option<String>) {
        let text = self.summary();
        let detail = |level: &str| {
            text.strip_prefix(&format!("containment: {level} ("))
                .and_then(|r| r.strip_suffix(')'))
                .map(str::to_owned)
        };
        match &self.outcome {
            Outcome::Full => ("full", None),
            Outcome::Partial { .. } => ("partial", detail("partial")),
            Outcome::Off { .. } => ("off", detail("off")),
        }
    }

    /// `containment: full`, `containment: partial (memory, pids; cpu by nice)`
    /// or `containment: off (<reason>)`.
    #[must_use]
    pub fn summary(&self) -> String {
        match &self.outcome {
            Outcome::Full => "containment: full".to_owned(),
            Outcome::Off { reason } => format!("containment: off ({})", reason.short()),
            Outcome::Partial { missing } => {
                let mut have = Vec::new();
                if self.controllers.cpu {
                    have.push("cpu");
                }
                if self.controllers.memory {
                    have.push("memory");
                }
                if self.controllers.pids {
                    have.push("pids");
                }
                let mut parts = vec![have.join(", ")];
                if missing.contains(&Missing::Cpu) {
                    parts.push("cpu by nice".to_owned());
                }
                let lacking: Vec<&str> = missing
                    .iter()
                    .filter_map(|m| match m {
                        Missing::Memory => Some("memory"),
                        Missing::Pids => Some("pids"),
                        _ => None,
                    })
                    .collect();
                if !lacking.is_empty() {
                    parts.push(format!("no {}", lacking.join(", ")));
                }
                if missing.contains(&Missing::LoginScopeAttach) {
                    parts.push("login-scope sessions not attachable: no system bus".to_owned());
                }
                format!("containment: partial ({})", parts.join("; "))
            }
        }
    }

    /// The checks of the `Contain` section that need no live session
    /// (S4 checks 1 to 8; checks 9 to 12 belong to the caller, which knows the
    /// sessions). Never fails.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn doctor_lines(&self) -> Vec<DoctorLine> {
        let f = &self.facts;
        let mut out = Vec::new();
        // 1 cg-v2 and 2 cg-writable
        match f.fs {
            FsKind::Cgroup2 => out.push(line("cg-v2", Level::Ok, "cgroup v2 (unified hierarchy)")),
            FsKind::V1OrHybrid => out.push(line(
                "cg-v2",
                Level::Warn,
                OffReason::CgroupV1Hybrid.message(),
            )),
            FsKind::NoCgroup => {
                out.push(line("cg-v2", Level::Info, OffReason::NoCgroupFs.message()));
            }
        }
        if f.fs == FsKind::Cgroup2 && f.read_only {
            out.push(line(
                "cg-writable",
                Level::Info,
                OffReason::ReadOnly.message(),
            ));
        }
        // 3 systemd
        match f.systemd_version {
            Some(v) => out.push(line("systemd", Level::Ok, format!("systemd {v}"))),
            None if !f.systemctl_present => {
                out.push(line("systemd", Level::Info, OffReason::NoSystemd.message()));
            }
            None => out.push(line("systemd", Level::Info, "systemd version not readable")),
        }
        // 4 user-manager
        match (&f.manager_socket, f.manager_state.as_deref()) {
            (false, _) | (true, None) => out.push(line(
                "user-manager",
                Level::Warn,
                OffReason::UserManagerUnreachable.message(),
            )),
            (true, Some("running" | "degraded")) => {
                out.push(line(
                    "user-manager",
                    Level::Ok,
                    "systemd user manager reachable",
                ));
            }
            (true, Some(s)) => out.push(line(
                "user-manager",
                Level::Warn,
                OffReason::UserManagerState(s.to_owned()).message(),
            )),
        }
        // 5 delegate-cpu and 6 delegate-memory-pids
        if f.manager_controllers.is_some() {
            let c = self.controllers;
            if c.cpu {
                out.push(line(
                    "delegate-cpu",
                    Level::Ok,
                    "cpu delegated to the user manager: CPU weights available",
                ));
            } else if let Some(v) = f.systemd_version.filter(|v| *v < CPU_DELEGATION_VERSION) {
                out.push(line("delegate-cpu", Level::Warn, format!("cpu is not delegated to user@.service on systemd {v} (< {CPU_DELEGATION_VERSION}): CPU containment off, nice fallback active. Fix: admin drop-in, see docs (Delegate=cpu memory pids).")));
            } else {
                let v = f
                    .systemd_version
                    .map_or_else(|| "?".to_owned(), |v| v.to_string());
                out.push(line("delegate-cpu", Level::Warn, format!("cpu is not delegated although systemd {v} should (an override or a distribution patch removed it): check systemctl show user@UID.service -p Delegate")));
            }
            if c.memory && c.pids {
                out.push(line(
                    "delegate-memory-pids",
                    Level::Ok,
                    "memory and pids delegated: memory.high and pids.max available",
                ));
            } else {
                let mut which = Vec::new();
                if !c.memory {
                    which.push("memory");
                }
                if !c.pids {
                    which.push("pids");
                }
                out.push(line(
                    "delegate-memory-pids",
                    Level::Warn,
                    format!(
                        "memory/pids not delegated ({}): that limit is skipped",
                        which.join(", ")
                    ),
                ));
            }
        }
        // 7 own-cgroup and 8 system-bus
        match (&self.placement, f.own_cgroup.as_deref()) {
            (Placement::SelfMoveOk, _) => out.push(line(
                "own-cgroup",
                Level::Ok,
                "this shell is inside the user manager's tree: it can place itself",
            )),
            (Placement::LoginScope, Some(p)) => {
                let scope = p.rsplit('/').next().unwrap_or(p);
                out.push(line("own-cgroup", Level::Warn, format!("this shell is in login scope {scope} (SSH/TTY/GDM): it cannot move itself (cgroup rule). The agent is attached through the user manager at session start instead.")));
                if f.system_bus {
                    out.push(line(
                        "system-bus",
                        Level::Ok,
                        "system bus reachable (needed to attach login-scope processes)",
                    ));
                } else {
                    out.push(line("system-bus", Level::Warn, "system bus not reachable: login-scope shells cannot be attached; those sessions run uncontained"));
                }
            }
            (Placement::Outside, Some(p)) => out.push(line(
                "own-cgroup",
                Level::Info,
                format!("this shell is in {p}, outside the user manager: containment off for it"),
            )),
            _ => {}
        }
        // oomd (memory policy of the agents scope)
        if f.oomd_active {
            let v = f.systemd_version.unwrap_or(0);
            if v >= OOMD_PER_UNIT_DURATION {
                out.push(line(
                    "oomd",
                    Level::Ok,
                    "systemd-oomd active: the agents scope gets its own memory pressure policy",
                ));
            } else {
                out.push(line("oomd", Level::Info, format!("systemd-oomd active but systemd {v} (< {OOMD_PER_UNIT_DURATION}) has no per-unit pressure duration: memory is accounted only")));
            }
        } else {
            out.push(line(
                "oomd",
                Level::Info,
                "systemd-oomd not active: memory is accounted only",
            ));
        }
        out
    }
}

/// Classifies an own cgroup path against the user manager's.
#[must_use]
pub fn classify_placement(own: Option<&str>, uid: u32) -> Placement {
    let Some(own) = own else {
        return Placement::Unknown;
    };
    let manager = format!("/user.slice/user-{uid}.slice/user@{uid}.service");
    if own == manager || own.starts_with(&format!("{manager}/")) {
        return Placement::SelfMoveOk;
    }
    let user_slice = format!("/user.slice/user-{uid}.slice/");
    if let Some(rest) = own.strip_prefix(&user_slice)
        && rest.starts_with("session-")
        && rest
            .split('/')
            .next()
            .is_some_and(|s| s.ends_with(".scope"))
    {
        return Placement::LoginScope;
    }
    Placement::Outside
}

/// The cgroup path of `/proc/self/cgroup` (the `0::` line).
#[must_use]
pub fn parse_proc_cgroup(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_owned())
}

/// The words of a `cgroup.controllers` file.
#[must_use]
pub fn parse_controllers(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// Pure verdict over [`Facts`] (the order is the S4 algorithm).
#[must_use]
pub fn evaluate(facts: Facts) -> Capabilities {
    let placement = classify_placement(facts.own_cgroup.as_deref(), facts.uid);
    let controllers = facts
        .manager_controllers
        .as_ref()
        .map(|c| Controllers {
            cpu: c.iter().any(|w| w == "cpu"),
            memory: c.iter().any(|w| w == "memory"),
            pids: c.iter().any(|w| w == "pids"),
        })
        .unwrap_or_default();
    let off = |reason: OffReason, facts: Facts| Capabilities {
        outcome: Outcome::Off { reason },
        controllers: Controllers::default(),
        placement: placement.clone(),
        oomd_active: facts.oomd_active,
        systemd_version: facts.systemd_version,
        facts,
    };
    match facts.fs {
        FsKind::V1OrHybrid => return off(OffReason::CgroupV1Hybrid, facts),
        FsKind::NoCgroup => return off(OffReason::NoCgroupFs, facts),
        FsKind::Cgroup2 => {}
    }
    if facts.read_only {
        return off(OffReason::ReadOnly, facts);
    }
    if !facts.systemctl_present {
        return off(OffReason::NoSystemd, facts);
    }
    if !facts.manager_socket {
        return off(OffReason::UserManagerUnreachable, facts);
    }
    match facts.manager_state.as_deref() {
        Some("running" | "degraded") => {}
        Some(other) => {
            let reason = OffReason::UserManagerState(other.to_owned());
            return off(reason, facts);
        }
        None => return off(OffReason::UserManagerUnreachable, facts),
    }
    if facts.manager_controllers.is_none() {
        return off(OffReason::ControllersUnreadable, facts);
    }
    if !(controllers.cpu || controllers.memory || controllers.pids) {
        return off(OffReason::NoControllers, facts);
    }
    let mut missing = Vec::new();
    if !controllers.cpu {
        missing.push(Missing::Cpu);
    }
    if !controllers.memory {
        missing.push(Missing::Memory);
    }
    if !controllers.pids {
        missing.push(Missing::Pids);
    }
    if placement == Placement::LoginScope && !facts.system_bus {
        missing.push(Missing::LoginScopeAttach);
    }
    let outcome = if missing.is_empty() {
        Outcome::Full
    } else {
        Outcome::Partial { missing }
    };
    Capabilities {
        outcome,
        controllers,
        placement,
        oomd_active: facts.oomd_active,
        systemd_version: facts.systemd_version,
        facts,
    }
}

fn is_socket(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
}

fn fs_kind(root: &Path) -> FsKind {
    match nix::sys::statfs::statfs(root) {
        Ok(s) if s.filesystem_type() == CGROUP2_SUPER_MAGIC => FsKind::Cgroup2,
        Ok(s) if s.filesystem_type() == TMPFS_MAGIC => FsKind::V1OrHybrid,
        _ => FsKind::NoCgroup,
    }
}

fn mount_read_only(root: &Path) -> bool {
    nix::sys::statvfs::statvfs(root)
        .is_ok_and(|s| s.flags().contains(nix::sys::statvfs::FsFlags::ST_RDONLY))
}

/// The user manager's cgroup directory under `root`.
#[must_use]
pub fn manager_cgroup_dir(root: &Path, uid: u32) -> PathBuf {
    root.join(format!("user.slice/user-{uid}.slice/user@{uid}.service"))
}

/// Runtime directory: `$XDG_RUNTIME_DIR` when set and non-empty, else
/// `/run/user/<uid>`.
#[must_use]
pub fn runtime_dir(env: &EnvMap, uid: u32) -> PathBuf {
    env.get("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(format!("/run/user/{uid}")), PathBuf::from)
}

/// Gathers the facts for the calling process. Read-only: nothing is created
/// or moved; processes are spawned only once the cheap checks pass.
#[must_use]
pub fn gather(env: &EnvMap) -> Facts {
    gather_at(env, Path::new(CGROUP_MOUNT))
}

/// [`gather`] with an explicit cgroup mount (tests).
#[must_use]
pub fn gather_at(env: &EnvMap, root: &Path) -> Facts {
    let uid = rustix::process::getuid().as_raw();
    let fs = fs_kind(root);
    let read_only = fs == FsKind::Cgroup2 && mount_read_only(root);
    let own_cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|t| parse_proc_cgroup(&t));
    let system_bus = is_socket(Path::new("/run/dbus/system_bus_socket"));
    let mut facts = Facts {
        uid,
        fs,
        read_only,
        systemctl_present: false,
        systemd_version: None,
        manager_socket: false,
        manager_state: None,
        manager_controllers: None,
        own_cgroup,
        system_bus,
        oomd_active: false,
    };
    if fs != FsKind::Cgroup2 || read_only {
        return facts;
    }
    match systemd::systemd_version(env) {
        Ok(v) => {
            facts.systemctl_present = true;
            facts.systemd_version = Some(v);
        }
        Err(CgroupError::ToolMissing { .. }) => return facts,
        Err(_) => facts.systemctl_present = true,
    }
    let rt = runtime_dir(env, uid);
    facts.manager_socket = is_socket(&rt.join("systemd/private")) || is_socket(&rt.join("bus"));
    if !facts.manager_socket {
        return facts;
    }
    facts.manager_state = systemd::user_manager_state(env).ok();
    facts.manager_controllers =
        systemd::read_to_string(&manager_cgroup_dir(root, uid).join("cgroup.controllers"))
            .ok()
            .map(|t| parse_controllers(&t));
    facts.oomd_active = systemd::is_unit_active(env, "systemd-oomd", false);
    facts
}

/// Detects what the machine allows, for the calling process.
#[must_use]
pub fn detect(env: &EnvMap) -> Capabilities {
    evaluate(gather(env))
}

/// Controllers whose files exist in a cgroup directory (the authority when
/// creating: presence of `cpu.weight`, `memory.current`, `pids.max`).
#[must_use]
pub fn leaf_controllers(dir: &Path) -> Controllers {
    Controllers {
        cpu: dir.join("cpu.weight").exists(),
        memory: dir.join("memory.current").exists(),
        pids: dir.join("pids.max").exists(),
    }
}

// ---------------------------------------------------------------------------
// Memory pressure policy derived from the system's own oomd configuration
// ---------------------------------------------------------------------------

/// Built-in `systemd-oomd` defaults (oomd.conf man page).
pub const OOMD_DEFAULT_LIMIT_PERCENT: f64 = 60.0;
/// Built-in `DefaultMemoryPressureDurationSec`.
pub const OOMD_DEFAULT_DURATION: Duration = Duration::from_secs(20);
/// Ceiling of the derived limit.
pub const DERIVED_LIMIT_CAP: f64 = 30.0;
/// Fraction of the ancestor's limit the derived limit may use.
pub const DERIVED_LIMIT_FRACTION: f64 = 0.6;
/// Ceiling of the derived duration.
pub const DERIVED_DURATION_CAP: Duration = Duration::from_secs(5);

/// The policy `systemd-oomd` applies to the user manager (what watches us).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AncestorPolicy {
    /// Limit in percent, when known.
    pub limit_percent: Option<f64>,
    /// Duration, when known.
    pub duration: Option<Duration>,
}

/// Limit and duration set on the agents scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressurePolicy {
    /// `ManagedOOMMemoryPressureLimit`, whole percent.
    pub limit_percent: u8,
    /// `ManagedOOMMemoryPressureDurationSec`.
    pub duration: Duration,
}

fn parse_duration_text(text: &str) -> Option<Duration> {
    let t = text.trim();
    if t.is_empty() || t == "infinity" || t.starts_with('[') {
        return None;
    }
    if t.bytes().all(|b| b.is_ascii_digit()) {
        return t
            .parse::<u64>()
            .ok()
            .map(Duration::from_micros)
            .filter(|d| !d.is_zero());
    }
    humantime::parse_duration(t).ok().filter(|d| !d.is_zero())
}

fn parse_percent_text(text: &str) -> Option<f64> {
    text.trim()
        .strip_suffix('%')
        .and_then(|n| n.trim().parse::<f64>().ok())
        .filter(|p| *p > 0.0)
}

/// Policy from `systemctl show <user manager> -p ManagedOOMMemoryPressureLimit
/// -p ManagedOOMMemoryPressureDurationUSec` output: the limit is a raw
/// fixed-point value (0 means unset), the duration a timespan or `[not set]`.
#[must_use]
pub fn ancestor_from_show(text: &str) -> AncestorPolicy {
    let map = systemd::parse_show(text);
    AncestorPolicy {
        limit_percent: map
            .get("ManagedOOMMemoryPressureLimit")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|raw| *raw > 0)
            .map(systemd::fixed_to_percent),
        duration: map
            .get("ManagedOOMMemoryPressureDurationUSec")
            .and_then(|v| parse_duration_text(v)),
    }
}

/// What `oomctl dump` says.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OomctlInfo {
    /// `Default Memory Pressure Limit`.
    pub default_limit_percent: Option<f64>,
    /// `Default Memory Pressure Duration`.
    pub default_duration: Option<Duration>,
    /// Policy of the deepest monitored cgroup that is the user manager or an
    /// ancestor of it.
    pub monitored: AncestorPolicy,
}

/// Parses `oomctl dump`. `manager_path` is the user manager's cgroup path
/// (`/user.slice/user-1000.slice/user@1000.service`); monitored ancestors of
/// it count too.
#[must_use]
pub fn parse_oomctl_dump(text: &str, manager_path: &str) -> OomctlInfo {
    let mut info = OomctlInfo::default();
    let mut current: Option<(String, AncestorPolicy)> = None;
    let mut best: Option<(usize, AncestorPolicy)> = None;
    let mut in_pressure_section = false;
    let flush = |cur: &mut Option<(String, AncestorPolicy)>,
                 best: &mut Option<(usize, AncestorPolicy)>| {
        if let Some((path, pol)) = cur.take() {
            let is_ancestor = path == manager_path
                || manager_path
                    .strip_prefix(path.trim_end_matches('/'))
                    .is_some_and(|rest| rest.starts_with('/'));
            if is_ancestor && best.as_ref().is_none_or(|(len, _)| path.len() > *len) {
                *best = Some((path.len(), pol));
            }
        }
    };
    for raw in text.lines() {
        let t = raw.trim();
        if let Some(v) = t.strip_prefix("Default Memory Pressure Limit:") {
            info.default_limit_percent = parse_percent_text(v);
        } else if let Some(v) = t.strip_prefix("Default Memory Pressure Duration:") {
            info.default_duration = parse_duration_text(v);
        } else if t.ends_with("Monitored CGroups:") {
            flush(&mut current, &mut best);
            in_pressure_section = t.starts_with("Memory Pressure");
        } else if let Some(p) = t.strip_prefix("Path:") {
            flush(&mut current, &mut best);
            if in_pressure_section {
                current = Some((p.trim().to_owned(), AncestorPolicy::default()));
            }
        } else if let Some((_, pol)) = current.as_mut() {
            if let Some(v) = t.strip_prefix("Memory Pressure Limit:") {
                pol.limit_percent = parse_percent_text(v);
            } else if let Some(v) = t.strip_prefix("Memory Pressure Duration:") {
                pol.duration = parse_duration_text(v);
            }
        }
    }
    flush(&mut current, &mut best);
    if let Some((_, pol)) = best {
        info.monitored = pol;
    }
    info
}

/// `DefaultMemoryPressureLimit` and `DefaultMemoryPressureDurationSec` of an
/// `oomd.conf` text.
#[must_use]
pub fn parse_oomd_conf(text: &str) -> (Option<f64>, Option<Duration>) {
    let mut limit = None;
    let mut duration = None;
    for l in text.lines() {
        let l = l.trim();
        if l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if let Some(v) = l.strip_prefix("DefaultMemoryPressureLimit=") {
            limit = parse_percent_text(v);
        } else if let Some(v) = l.strip_prefix("DefaultMemoryPressureDurationSec=") {
            duration = humantime::parse_duration(v.trim())
                .ok()
                .or_else(|| v.trim().parse::<u64>().ok().map(Duration::from_secs))
                .filter(|d| !d.is_zero());
        }
    }
    (limit, duration)
}

/// Merges the sources, most specific first: `systemctl show` of the user
/// manager, the monitored ancestor in `oomctl dump`, oomd's defaults in
/// `oomctl dump`, `oomd.conf`, then the built-in 60 % / 20 s.
#[must_use]
pub fn resolve_ancestor_policy(
    show: &AncestorPolicy,
    oomctl: &OomctlInfo,
    conf: (Option<f64>, Option<Duration>),
) -> AncestorPolicy {
    AncestorPolicy {
        limit_percent: show
            .limit_percent
            .or(oomctl.monitored.limit_percent)
            .or(oomctl.default_limit_percent)
            .or(conf.0)
            .or(Some(OOMD_DEFAULT_LIMIT_PERCENT)),
        duration: show
            .duration
            .or(oomctl.monitored.duration)
            .or(oomctl.default_duration)
            .or(conf.1)
            .or(Some(OOMD_DEFAULT_DURATION)),
    }
}

/// The policy for the agents scope: `limit = min(30 %, 0.6 x ancestor limit)`,
/// `duration = min(5 s, ancestor duration / 4)`, never below 1 % and 1 s.
/// Unknown ancestor values count as oomd's built-in 60 % and 20 s.
#[must_use]
pub fn derive_pressure_policy(ancestor: &AncestorPolicy) -> PressurePolicy {
    let limit = ancestor.limit_percent.unwrap_or(OOMD_DEFAULT_LIMIT_PERCENT);
    let derived = (limit * DERIVED_LIMIT_FRACTION)
        .min(DERIVED_LIMIT_CAP)
        .round()
        .clamp(1.0, 99.0);
    let duration = ancestor
        .duration
        .unwrap_or(OOMD_DEFAULT_DURATION)
        .checked_div(4)
        .unwrap_or(DERIVED_DURATION_CAP)
        .min(DERIVED_DURATION_CAP)
        .max(Duration::from_secs(1));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    PressurePolicy {
        limit_percent: derived as u8,
        duration,
    }
}

/// The policy for the gates' scope (`sluice-daemon.scope`), derived from the
/// agents' one: the **same limit** (an oomd trigger on the gates' scope must
/// fire whenever the agents' one would; a laxer limit would let the agents'
/// policy win the race again, which is the incident) and
/// `duration = max(1 s, ceil(agents / 2))` in whole seconds, so with both scopes
/// over their limit oomd acts on the shorter one, the gates' (measured, W6d).
/// Never equal to or longer than the agents' duration unless that is 1 s (the
/// documented minimum of the property).
#[must_use]
pub fn derive_gates_policy(agents: PressurePolicy) -> PressurePolicy {
    let half = agents.duration.as_secs().div_ceil(2);
    let secs = half.max(1);
    // A sub-second agents duration (a user override) stays above the gates'.
    let duration = Duration::from_secs(secs).min(agents.duration.max(Duration::from_secs(1)));
    PressurePolicy {
        limit_percent: agents.limit_percent,
        duration,
    }
}

/// The policy `systemd-oomd` applies to the user manager on this machine
/// (read-only: `systemctl show`, `oomctl dump`, `oomd.conf`, then the
/// built-ins), the input of the derived thresholds.
#[must_use]
pub fn system_ancestor_policy(env: &EnvMap, uid: u32) -> AncestorPolicy {
    let unit = format!("user@{uid}.service");
    let show = systemd::show(
        env,
        &unit,
        &[
            "ManagedOOMMemoryPressureLimit",
            "ManagedOOMMemoryPressureDurationUSec",
        ],
    )
    .map(|m| {
        ancestor_from_show(&format!(
            "ManagedOOMMemoryPressureLimit={}\nManagedOOMMemoryPressureDurationUSec={}\n",
            m.get("ManagedOOMMemoryPressureLimit")
                .map_or("0", String::as_str),
            m.get("ManagedOOMMemoryPressureDurationUSec")
                .map_or("[not set]", String::as_str)
        ))
    })
    .unwrap_or_default();
    let manager_path = format!("/user.slice/user-{uid}.slice/user@{uid}.service");
    let oomctl = parse_oomctl_dump(&systemd::oomctl_dump(env), &manager_path);
    let conf = ["/etc/systemd/oomd.conf", "/usr/lib/systemd/oomd.conf"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map_or((None, None), |t| parse_oomd_conf(&t));
    resolve_ancestor_policy(&show, &oomctl, conf)
}

/// Reads the system's policy ([`system_ancestor_policy`]) and applies the
/// config overrides (`"auto"` derives).
///
/// # Errors
///
/// Never fails today (every source is optional); the `Result` keeps the
/// signature stable if a mandatory source is added.
pub fn pressure_policy(
    env: &EnvMap,
    cfg: &ContainmentConfig,
    uid: u32,
) -> CgResult<PressurePolicy> {
    let derived =
        if cfg.pressure_limit_override().is_some() && cfg.pressure_duration_override().is_some() {
            None
        } else {
            Some(derive_pressure_policy(&system_ancestor_policy(env, uid)))
        };
    let base = derived.unwrap_or(PressurePolicy {
        limit_percent: 30,
        duration: DERIVED_DURATION_CAP,
    });
    Ok(PressurePolicy {
        limit_percent: cfg.pressure_limit_override().unwrap_or(base.limit_percent),
        duration: cfg.pressure_duration_override().unwrap_or(base.duration),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: u32 = 1000;
    const MANAGER: &str = "/user.slice/user-1000.slice/user@1000.service";

    fn facts() -> Facts {
        Facts {
            uid: UID,
            fs: FsKind::Cgroup2,
            read_only: false,
            systemctl_present: true,
            systemd_version: Some(259),
            manager_socket: true,
            manager_state: Some("running".into()),
            manager_controllers: Some(parse_controllers("cpu memory pids\n")),
            own_cgroup: Some(format!("{MANAGER}/app.slice/x.scope")),
            system_bus: true,
            oomd_active: true,
        }
    }

    fn off_reason(f: Facts) -> OffReason {
        match evaluate(f).outcome {
            Outcome::Off { reason } => reason,
            other => panic!("expected off, got {other:?}"),
        }
    }

    #[test]
    fn proc_cgroup_fixtures() {
        let own = parse_proc_cgroup(&format!("0::{MANAGER}/session.slice/dbus.service\n"));
        assert_eq!(
            own.as_deref(),
            Some(format!("{MANAGER}/session.slice/dbus.service").as_str())
        );
        assert_eq!(parse_proc_cgroup("0::/\n").as_deref(), Some("/"));
        // v1 style lines have no `0::` entry
        assert_eq!(parse_proc_cgroup("12:memory:/x\n1:name=systemd:/x\n"), None);
    }

    #[test]
    fn placement_classification() {
        let p = |s: &str| classify_placement(Some(s), UID);
        assert_eq!(
            p(&format!("{MANAGER}/app.slice/a.scope")),
            Placement::SelfMoveOk
        );
        assert_eq!(p(MANAGER), Placement::SelfMoveOk);
        assert_eq!(
            p("/user.slice/user-1000.slice/session-2.scope"),
            Placement::LoginScope
        );
        assert_eq!(
            p("/user.slice/user-1000.slice/session-2.scope/child"),
            Placement::LoginScope
        );
        assert_eq!(p("/"), Placement::Outside);
        assert_eq!(p("/system.slice/docker.service"), Placement::Outside);
        // another user's manager is not ours
        assert_eq!(
            p("/user.slice/user-1001.slice/user@1001.service/x"),
            Placement::Outside
        );
        assert_eq!(classify_placement(None, UID), Placement::Unknown);
    }

    #[test]
    fn full_on_a_modern_desktop() {
        let c = evaluate(facts());
        assert_eq!(c.outcome, Outcome::Full);
        assert_eq!(c.summary(), "containment: full");
        assert!(c.oomd_policy_supported());
        assert_eq!(c.placement, Placement::SelfMoveOk);
    }

    #[test]
    fn partial_without_cpu_names_the_fallback() {
        let mut f = facts();
        f.manager_controllers = Some(parse_controllers("memory pids"));
        f.systemd_version = Some(249);
        let c = evaluate(f);
        assert_eq!(
            c.outcome,
            Outcome::Partial {
                missing: vec![Missing::Cpu]
            }
        );
        assert_eq!(
            c.summary(),
            "containment: partial (memory, pids; cpu by nice)"
        );
        assert!(
            !c.oomd_policy_supported(),
            "systemd 249 has no per-unit duration"
        );
        let lines = c.doctor_lines();
        let cpu = lines.iter().find(|l| l.code == "delegate-cpu").unwrap();
        assert_eq!(cpu.level, Level::Warn);
        assert!(
            cpu.message.contains("systemd 249 (< 252)"),
            "{}",
            cpu.message
        );
    }

    #[test]
    fn cpu_missing_on_new_systemd_has_the_override_message() {
        let mut f = facts();
        f.manager_controllers = Some(parse_controllers("memory pids"));
        let cpu = evaluate(f)
            .doctor_lines()
            .into_iter()
            .find(|l| l.code == "delegate-cpu")
            .unwrap();
        assert!(
            cpu.message.contains("although systemd 259 should"),
            "{}",
            cpu.message
        );
    }

    #[test]
    fn partial_without_memory_or_pids() {
        let mut f = facts();
        f.manager_controllers = Some(parse_controllers("cpu"));
        let c = evaluate(f);
        assert_eq!(c.summary(), "containment: partial (cpu; no memory, pids)");
        let l = c.doctor_lines();
        let mp = l.iter().find(|l| l.code == "delegate-memory-pids").unwrap();
        assert!(mp.message.contains("memory, pids"));
    }

    #[test]
    fn login_scope_without_system_bus_is_partial() {
        let mut f = facts();
        f.own_cgroup = Some("/user.slice/user-1000.slice/session-2.scope".into());
        f.system_bus = false;
        let c = evaluate(f);
        assert_eq!(
            c.outcome,
            Outcome::Partial {
                missing: vec![Missing::LoginScopeAttach]
            }
        );
        assert!(c.summary().contains("no system bus"));
        let l = c.doctor_lines();
        assert!(
            l.iter()
                .any(|l| l.code == "own-cgroup" && l.message.contains("session-2.scope"))
        );
        assert!(
            l.iter()
                .any(|l| l.code == "system-bus" && l.level == Level::Warn)
        );
    }

    #[test]
    fn off_reasons_follow_the_s4_order() {
        let mut f = facts();
        f.fs = FsKind::V1OrHybrid;
        assert_eq!(off_reason(f), OffReason::CgroupV1Hybrid);
        let mut f = facts();
        f.fs = FsKind::NoCgroup;
        assert_eq!(off_reason(f), OffReason::NoCgroupFs);
        let mut f = facts();
        f.read_only = true;
        assert_eq!(off_reason(f), OffReason::ReadOnly);
        let mut f = facts();
        f.systemctl_present = false;
        assert_eq!(off_reason(f), OffReason::NoSystemd);
        let mut f = facts();
        f.manager_socket = false;
        assert_eq!(off_reason(f), OffReason::UserManagerUnreachable);
        let mut f = facts();
        f.manager_state = Some("stopping".into());
        assert_eq!(
            off_reason(f),
            OffReason::UserManagerState("stopping".into())
        );
        let mut f = facts();
        f.manager_state = Some("degraded".into());
        assert_eq!(evaluate(f).outcome, Outcome::Full);
        let mut f = facts();
        f.manager_controllers = None;
        assert_eq!(off_reason(f), OffReason::ControllersUnreadable);
        let mut f = facts();
        f.manager_controllers = Some(parse_controllers("cpuset io"));
        assert_eq!(off_reason(f), OffReason::NoControllers);
    }

    #[test]
    fn off_messages_are_the_s4_texts() {
        assert_eq!(
            OffReason::ReadOnly.message(),
            "cgroup filesystem is read-only (container or sandbox): containment off; the runtime's own limits apply"
        );
        assert!(OffReason::UserManagerUnreachable.message().starts_with("systemd user manager not reachable (no $XDG_RUNTIME_DIR/systemd/private): containment off."));
        assert_eq!(
            OffReason::NoSystemd.message(),
            "no systemd: containment off (nice fallback active)"
        );
        assert_eq!(
            OffReason::UserManagerState("starting".into()).message(),
            "systemd user manager is starting: retry later"
        );
        let mut f = facts();
        f.read_only = true;
        assert_eq!(
            evaluate(f).summary(),
            "containment: off (read-only cgroup filesystem: container)"
        );
        let mut f = facts();
        f.manager_socket = false;
        assert_eq!(
            evaluate(f).summary(),
            "containment: off (user manager not reachable)"
        );
    }

    #[test]
    fn controllers_parsing() {
        assert_eq!(
            parse_controllers("cpuset cpu io memory hugetlb pids rdma misc\n").len(),
            8
        );
        assert_eq!(parse_controllers("").len(), 0);
    }

    #[test]
    fn oomd_policy_needs_oomd_version_and_memory() {
        let mut f = facts();
        f.oomd_active = false;
        assert!(!evaluate(f).oomd_policy_supported());
        let mut f = facts();
        f.systemd_version = Some(256);
        assert!(!evaluate(f).oomd_policy_supported());
        let mut f = facts();
        f.systemd_version = Some(257);
        let c = evaluate(f);
        assert!(c.oomd_policy_supported());
        let mut cfg = ContainmentConfig::default();
        assert!(c.oomd_policy_enabled(&cfg));
        cfg.memory_policy = MemoryPolicy::None;
        assert!(!c.oomd_policy_enabled(&cfg));
    }

    // ---- derived pressure policy -------------------------------------------

    fn show_text(limit: &str, duration: &str) -> String {
        format!(
            "ManagedOOMMemoryPressureLimit={limit}\nManagedOOMMemoryPressureDurationUSec={duration}\n"
        )
    }

    fn derived(show: &str, oomctl: &str, conf: &str) -> PressurePolicy {
        let policy = resolve_ancestor_policy(
            &ancestor_from_show(show),
            &parse_oomctl_dump(oomctl, MANAGER),
            parse_oomd_conf(conf),
        );
        derive_pressure_policy(&policy)
    }

    fn pp(limit: u8, secs: u64) -> PressurePolicy {
        PressurePolicy {
            limit_percent: limit,
            duration: Duration::from_secs(secs),
        }
    }

    #[test]
    fn derived_policy_from_show_output() {
        // 50 % / 20 s (Ubuntu): 30 % / 5 s
        assert_eq!(derived(&show_text("2147483648", "20s"), "", ""), pp(30, 5));
        // 80 % / 20 s (Fedora): the cap applies
        assert_eq!(derived(&show_text("3435973836", "20s"), "", ""), pp(30, 5));
        // 40 % / 8 s: 24 % / 2 s
        assert_eq!(derived(&show_text("1717986918", "8s"), "", ""), pp(24, 2));
        // not set anywhere: oomd's 60 % / 20 s give 30 % / 5 s
        assert_eq!(derived(&show_text("0", "[not set]"), "", ""), pp(30, 5));
        // tiny ancestor values never go below 1 % and 1 s
        assert_eq!(derived(&show_text("42949673", "2s"), "", ""), pp(1, 1));
        // a duration in raw microseconds is understood
        assert_eq!(
            derived(&show_text("1717986918", "8000000"), "", ""),
            pp(24, 2)
        );
    }

    const OOMCTL: &str = "Dry Run: no\n\
Swap Used Limit: 90.00%\n\
Default Memory Pressure Limit: 60.00%\n\
Default Memory Pressure Duration: 20s\n\
System Context:\n\tMemory: Used: 5.7G, Total: 30.5G\n\
Swap Monitored CGroups:\n\
Memory Pressure Monitored CGroups:\n\
\tPath: /user.slice/user-1000.slice/user@1000.service\n\
\t\tMemory Pressure Limit: 40.00%\n\
\t\tMemory Pressure Duration: 8s\n\
\t\tPressure: Avg10: 0.00, Avg60: 0.00, Avg300: 0.06, Total: 1min 36s\n\
\tPath: /user.slice/user-1001.slice/user@1001.service\n\
\t\tMemory Pressure Limit: 90.00%\n\
\t\tMemory Pressure Duration: 60s\n";

    #[test]
    fn gates_policy_has_the_same_limit_and_a_shorter_duration() {
        // 5 s agents (Ubuntu): 3 s, rounded up to whole seconds
        assert_eq!(derive_gates_policy(pp(30, 5)), pp(30, 3));
        assert_eq!(derive_gates_policy(pp(24, 2)), pp(24, 1));
        assert_eq!(derive_gates_policy(pp(30, 20)), pp(30, 10));
        assert_eq!(derive_gates_policy(pp(30, 4)), pp(30, 2));
        // never below 1 s, equal to the agents' only when that is 1 s
        assert_eq!(derive_gates_policy(pp(1, 1)), pp(1, 1));
        assert_eq!(
            derive_gates_policy(PressurePolicy {
                limit_percent: 30,
                duration: Duration::ZERO
            })
            .duration,
            Duration::from_secs(1)
        );
        for secs in 2..=120 {
            let g = derive_gates_policy(pp(30, secs));
            assert!(g.duration >= Duration::from_secs(1), "{secs}");
            assert!(g.duration < Duration::from_secs(secs), "{secs}");
        }
    }

    #[test]
    fn gates_policy_follows_the_agents_override() {
        // a user override of the agents' duration keeps the ordering
        let agents = pp(45, 9);
        let gates = derive_gates_policy(agents);
        assert_eq!(gates, pp(45, 5));
        assert!(gates.duration < agents.duration);
    }

    #[test]
    fn oomctl_dump_is_the_second_source() {
        // `show` has nothing (as on the development machine), oomctl names the manager.
        assert_eq!(derived(&show_text("0", "[not set]"), OOMCTL, ""), pp(24, 2));
        let info = parse_oomctl_dump(OOMCTL, MANAGER);
        assert_eq!(info.default_limit_percent, Some(60.0));
        assert_eq!(info.default_duration, Some(Duration::from_secs(20)));
        assert_eq!(info.monitored.limit_percent, Some(40.0));
        assert_eq!(info.monitored.duration, Some(Duration::from_secs(8)));
    }

    #[test]
    fn oomctl_monitored_ancestor_counts() {
        let dump = "Memory Pressure Monitored CGroups:\n\
\tPath: /user.slice\n\
\t\tMemory Pressure Limit: 80.00%\n\
\t\tMemory Pressure Duration: 12s\n";
        let info = parse_oomctl_dump(dump, MANAGER);
        assert_eq!(info.monitored.limit_percent, Some(80.0));
        assert_eq!(derive_pressure_policy(&info.monitored), pp(30, 3));
        // a sibling that only shares a prefix is not an ancestor
        let sib = "Memory Pressure Monitored CGroups:\n\tPath: /user.slice/user-100\n\t\tMemory Pressure Limit: 10.00%\n";
        assert_eq!(
            parse_oomctl_dump(sib, MANAGER).monitored.limit_percent,
            None
        );
    }

    #[test]
    fn oomd_conf_and_defaults() {
        let conf = "[OOM]\n#DefaultMemoryPressureLimit=60%\nDefaultMemoryPressureLimit=50%\nDefaultMemoryPressureDurationSec=8s\n";
        assert_eq!(
            parse_oomd_conf(conf),
            (Some(50.0), Some(Duration::from_secs(8)))
        );
        assert_eq!(parse_oomd_conf("# nothing\n"), (None, None));
        // the conf is the third source
        assert_eq!(derived(&show_text("0", "[not set]"), "", conf), pp(30, 2));
        // everything missing: built-in
        assert_eq!(derived("", "", ""), pp(30, 5));
    }

    #[test]
    fn overrides_win_over_the_derivation() {
        let env = EnvMap::new();
        let cfg = ContainmentConfig {
            memory_pressure_limit: "25%".into(),
            memory_pressure_duration: "3s".into(),
            ..ContainmentConfig::default()
        };
        let p = pressure_policy(&env, &cfg, UID).unwrap();
        assert_eq!(p, pp(25, 3));
    }

    #[test]
    fn real_detection_never_panics_and_is_consistent() {
        // Runs on whatever machine this is (CI included): only invariants.
        let caps = detect(&crate::config::process_env());
        match &caps.outcome {
            Outcome::Off { reason } => assert!(caps.summary().contains(&reason.short())),
            _ => assert!(caps.controllers.cpu || caps.controllers.memory || caps.controllers.pids),
        }
        assert!(caps.doctor_lines().len() >= 3);
    }

    #[test]
    fn empty_runtime_dir_means_no_user_manager() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = crate::config::process_env();
        env.insert("XDG_RUNTIME_DIR".into(), dir.path().display().to_string());
        let caps = evaluate(gather(&env));
        // v1 hosts and containers are off for other reasons; on a v2 host with
        // systemd the empty runtime dir must be the reason.
        if caps.facts.fs == FsKind::Cgroup2 && !caps.facts.read_only && caps.facts.systemctl_present
        {
            assert_eq!(
                caps.outcome,
                Outcome::Off {
                    reason: OffReason::UserManagerUnreachable
                }
            );
        }
    }
}
