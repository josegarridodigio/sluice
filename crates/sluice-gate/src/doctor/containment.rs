//! The `containment` group of `sluice doctor` (design section 6, S4 messages)
//! and the `session` check.
//!
//! Detection is the core `cgroup::detect` module; this file only turns its
//! facts into checks. Every builder is pure over fabricated facts so the
//! messages are unit-tested; [`checks`] gathers the real ones. Nothing is
//! created, moved or written: a check never changes the machine.

use std::path::Path;

use sluice_gate_core::cgroup::detect::{
    self, AncestorPolicy, Capabilities, DoctorLine, Level, OOMD_PER_UNIT_DURATION, Outcome,
};
use sluice_gate_core::cgroup::explain;
use sluice_gate_core::cgroup::tree::{self, Layout};
use sluice_gate_core::cgroup::{CGROUP_MOUNT, PressurePolicy};
use sluice_gate_core::config::{
    ContainmentConfig, EnvMap, GatesOomScoreAdj, MemoryPolicy, UserConfig,
};

use super::{Check, Status, check};

/// Group name of every check built here (JSON `group`).
const GROUP: &str = "containment";

fn grouped(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
    let mut c = check(name, status, detail);
    c.group = Some(GROUP);
    c
}

fn status_of(level: Level) -> Status {
    match level {
        Level::Ok | Level::Info => Status::Ok,
        Level::Warn => Status::Warn,
    }
}

/// All checks of the group, plus `session` when run inside an agent shell.
pub(super) fn checks(env: &EnvMap) -> Vec<Check> {
    let user = UserConfig::load(env).unwrap_or_default();
    let cfg = &user.containment;
    let caps = detect::detect(env);
    let layout = crate::session_contain::layout(env, caps.facts.uid);
    let mut out = summary_and_lines(&caps, cfg);
    out.push(cgroupfs_writable(&caps));
    if caps.usable() {
        let manager = detect::manager_cgroup_dir(Path::new(CGROUP_MOUNT), caps.facts.uid);
        out.push(controller_files(&layout, &manager));
        out.push(slice(&layout));
        if cfg.enabled {
            out.push(gates_oom(&GatesOomFacts::gather(
                &layout,
                cfg,
                caps.oomd_active,
            )));
        }
    }
    let policy = caps.oomd_policy_enabled(cfg).then(|| {
        let ancestor = detect::system_ancestor_policy(env, caps.facts.uid);
        let policy = detect::pressure_policy(env, cfg, caps.facts.uid).ok();
        (ancestor, policy)
    });
    out.push(memory_policy(&caps, cfg, policy.as_ref()));
    if let Some((_, Some(agents))) = &policy {
        out.push(gates_policy(&GatesPolicyFacts::gather(&layout, *agents)));
    }
    out.push(memory_guard(&GuardFacts {
        on: user.admission.memory_guard,
        reserve: user.admission.memory_reserve.clone(),
        oomd_gap: oomd_gap(&caps, cfg),
    }));
    if let Some(c) = gates_ceiling(cfg) {
        out.push(c);
    }
    if let Some(c) = cpu_fallback(&caps) {
        out.push(c);
    }
    if let Some(c) = session(&SessionFacts::gather(env, &caps)) {
        out.push(c);
    }
    out
}

/// The `containment` summary check and the S4 checks 1 to 8 (the `oomd`
/// line of the core is replaced by [`memory_policy`]).
fn summary_and_lines(caps: &Capabilities, cfg: &ContainmentConfig) -> Vec<Check> {
    let mut out = Vec::new();
    let summary = caps.summary();
    let text = summary
        .strip_prefix("containment: ")
        .unwrap_or(&summary)
        .to_owned();
    let status = match &caps.outcome {
        Outcome::Full => Status::Ok,
        _ if !cfg.enabled => Status::Ok,
        _ => Status::Warn,
    };
    let text = if cfg.enabled {
        text
    } else {
        format!("{text}; disabled by [containment] enabled = false")
    };
    out.push(grouped("containment", status, text));
    for DoctorLine {
        code,
        level,
        message,
    } in caps.doctor_lines()
    {
        if code == "oomd" {
            continue;
        }
        out.push(grouped(code, status_of(level), message));
    }
    out
}

/// Whether cgroupfs can be written (the S4 `cg-writable` check, also when it
/// is fine).
fn cgroupfs_writable(caps: &Capabilities) -> Check {
    use sluice_gate_core::cgroup::FsKind;
    match (caps.facts.fs, caps.facts.read_only) {
        (FsKind::Cgroup2, false) => {
            grouped("cg-writable", Status::Ok, "cgroup filesystem writable")
        }
        (FsKind::Cgroup2, true) => grouped(
            "cg-writable",
            Status::Ok,
            "cgroup filesystem is read-only (container or sandbox): containment off; the \
             runtime's own limits apply",
        ),
        _ => grouped(
            "cg-writable",
            Status::Ok,
            "no cgroup v2 filesystem to write: containment off",
        ),
    }
}

/// Delegated controllers by the presence of `cpu.weight`, `memory.current`
/// and `pids.max` in the agents scope when it exists, else in the user
/// manager's cgroup (the files are the authority, not the version).
fn controller_files(layout: &Layout, manager: &Path) -> Check {
    let scope = layout.agents_scope_dir();
    let (dir, label) = if scope.is_dir() {
        (scope, layout.agents_scope.clone())
    } else {
        (
            manager.to_path_buf(),
            "the user manager's cgroup".to_owned(),
        )
    };
    let found = detect::leaf_controllers(&dir);
    let mut missing = Vec::new();
    if !found.cpu {
        missing.push("cpu.weight");
    }
    if !found.memory {
        missing.push("memory.current");
    }
    if !found.pids {
        missing.push("pids.max");
    }
    if missing.is_empty() {
        grouped(
            "controller-files",
            Status::Ok,
            format!(
                "cpu.weight, memory.current and pids.max present in {label}: cpu, memory and pids are delegated"
            ),
        )
    } else {
        grouped(
            "controller-files",
            Status::Warn,
            format!(
                "{} missing in {label}: the matching limit is skipped (cpu falls back to nice)",
                missing.join(", ")
            ),
        )
    }
}

/// S4 check 9: whether the slice and scopes exist yet.
fn slice(layout: &Layout) -> Check {
    let agents = layout.agents_scope_dir().join("cgroup.procs").exists();
    let gates = layout.daemon_scope_dir().join("cgroup.procs").exists();
    match (agents, gates) {
        (true, true) => grouped(
            "slice",
            Status::Ok,
            format!("{} present (agents scope, gates scope)", layout.slice),
        ),
        (false, false) => grouped(
            "slice",
            Status::Ok,
            format!(
                "{} not created yet (created by the daemon on first use)",
                layout.slice
            ),
        ),
        (true, false) => grouped(
            "slice",
            Status::Ok,
            format!(
                "{} present (agents scope; the gates scope appears when the daemon runs)",
                layout.slice
            ),
        ),
        (false, true) => grouped(
            "slice",
            Status::Ok,
            format!(
                "{} present (gates scope; the agents scope appears with the first session)",
                layout.slice
            ),
        ),
    }
}

/// What the `gates-oom` check needs (ADR-0019 piece 2).
#[derive(Debug, Clone, Default)]
pub(super) struct GatesOomFacts {
    /// `[containment] gates_oom_score_adj` resolved (`auto` is 800).
    pub adj: u16,
    /// Whether the config says `auto`.
    pub auto: bool,
    /// `oom_score_adj` of this process: the daemon inherits whatever its
    /// starter had, and a job never gets less than that.
    pub own_adj: Option<i64>,
    /// Cgroups from `jobs/` up to the root with `memory.oom.group = 1`.
    pub group_ancestors: Vec<String>,
    /// `systemd-oomd` is running.
    pub oomd_active: bool,
}

impl GatesOomFacts {
    fn gather(layout: &Layout, cfg: &ContainmentConfig, oomd_active: bool) -> Self {
        Self {
            adj: cfg.gates_oom_score_adj.resolve(),
            auto: cfg.gates_oom_score_adj == GatesOomScoreAdj::Auto,
            own_adj: std::fs::read_to_string("/proc/self/oom_score_adj")
                .ok()
                .and_then(|t| t.trim().parse().ok()),
            group_ancestors: ancestors_with_oom_group(&layout.jobs_dir(), Path::new(CGROUP_MOUNT)),
            oomd_active,
        }
    }
}

/// Directories from `start` up to (and including) `mount` whose
/// `memory.oom.group` reads `1`, as paths relative to `mount`. The kernel
/// kills the highest ancestor with the flag, so one above a job leaf would
/// widen a gate's OOM kill to everything under it.
pub(super) fn ancestors_with_oom_group(start: &Path, mount: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dir = Some(start);
    while let Some(d) = dir {
        let flagged =
            std::fs::read_to_string(d.join("memory.oom.group")).is_ok_and(|t| t.trim() == "1");
        if flagged {
            let rel = d.strip_prefix(mount).unwrap_or(d);
            let name = rel.display().to_string();
            found.push(if name.is_empty() {
                "/".to_owned()
            } else {
                name
            });
        }
        if d == mount {
            break;
        }
        dir = d.parent();
    }
    found
}

/// The `gates-oom` check: the kernel's preference for gate processes and
/// whether an ancestor would widen a kill.
pub(super) fn gates_oom(f: &GatesOomFacts) -> Check {
    let adj = if f.auto {
        format!("{} (auto)", f.adj)
    } else {
        f.adj.to_string()
    };
    let inherited = match f.own_adj {
        Some(own) if own > i64::from(f.adj) => format!(
            "; this shell runs at {own}, and a job never gets less than what the daemon inherited"
        ),
        _ => String::new(),
    };
    let oomd = if f.oomd_active {
        "systemd-oomd is active (it ranks by memory pressure and size, not by oom_score_adj)"
    } else {
        "systemd-oomd is not active (only the kernel's OOM killer acts)"
    };
    if f.group_ancestors.is_empty() {
        return grouped(
            "gates-oom",
            Status::Ok,
            format!(
                "gate jobs run at oom_score_adj >= {adj}{inherited} and each job leaf has \
                 memory.oom.group = 1 (a killed job dies whole); no ancestor of jobs/ has \
                 memory.oom.group = 1; {oomd}"
            ),
        );
    }
    grouped(
        "gates-oom",
        Status::Warn,
        format!(
            "memory.oom.group = 1 on {}: a kernel OOM kill inside a gate job would be widened \
             to that whole cgroup (the kernel kills the highest ancestor with the flag); set it \
             back to 0 (OOMPolicy=continue). Gate jobs run at oom_score_adj >= {adj}{inherited}; \
             {oomd}",
            f.group_ancestors.join(", ")
        ),
    )
}

/// Whether the memory policy of the agents scope is on, and why not.
/// `policy` is `(system policy, derived policy)` when it is on.
fn memory_policy(
    caps: &Capabilities,
    cfg: &ContainmentConfig,
    policy: Option<&(AncestorPolicy, Option<PressurePolicy>)>,
) -> Check {
    let off = |why: String| {
        grouped(
            "memory-policy",
            Status::Ok,
            format!("memory policy off: {why}; memory is only accounted, never limited"),
        )
    };
    if !cfg.enabled {
        return off("containment is disabled by [containment] enabled = false".to_owned());
    }
    if !caps.usable() {
        return off("containment is off".to_owned());
    }
    if cfg.memory_policy == MemoryPolicy::None {
        return off("[containment] memory_policy = \"none\"".to_owned());
    }
    if !caps.controllers.memory {
        return off("the memory controller is not delegated".to_owned());
    }
    if !caps.oomd_active {
        return off("systemd-oomd is not active".to_owned());
    }
    match caps.systemd_version {
        Some(v) if v >= OOMD_PER_UNIT_DURATION => {}
        Some(v) => {
            return off(format!(
                "systemd-oomd is active but systemd {v} (< {OOMD_PER_UNIT_DURATION}) has no per-unit pressure duration"
            ));
        }
        None => return off("the systemd version is unknown".to_owned()),
    }
    let version = caps.systemd_version.unwrap_or(0);
    let Some((ancestor, Some(p))) = policy else {
        return grouped(
            "memory-policy",
            Status::Ok,
            format!("memory policy on (systemd-oomd active, systemd {version})"),
        );
    };
    let system = match (ancestor.limit_percent, ancestor.duration) {
        (Some(l), Some(d)) => format!("{l:.0}% / {}s", d.as_secs()),
        _ => "built-in defaults".to_owned(),
    };
    let how =
        if cfg.pressure_limit_override().is_some() || cfg.pressure_duration_override().is_some() {
            "explicit values from the config".to_owned()
        } else {
            format!("derived from the system policy on user@UID.service, {system}")
        };
    grouped(
        "memory-policy",
        Status::Ok,
        format!(
            "memory policy on: systemd-oomd acts on the agents scope at {}% memory pressure for {}s ({how}; systemd {version}, systemd-oomd active)",
            p.limit_percent,
            p.duration.as_secs()
        ),
    )
}

/// What the `gates-memory-policy` check needs (ADR-0019 piece 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GatesPolicyFacts {
    /// The agents' policy (the derived or configured one).
    pub agents: PressurePolicy,
    /// The gates' policy Sluice creates ([`detect::derive_gates_policy`]).
    pub gates: PressurePolicy,
    /// `daemon/` carries `user.oomd_omit`: `None` when the leaf does not
    /// exist (the daemon is not running).
    pub daemon_omitted: Option<bool>,
}

impl GatesPolicyFacts {
    fn gather(layout: &Layout, agents: PressurePolicy) -> Self {
        let leaf = layout.daemon_leaf_dir();
        Self {
            agents,
            gates: detect::derive_gates_policy(agents),
            daemon_omitted: leaf.is_dir().then(|| tree::has_oomd_omit(&leaf)),
        }
    }
}

/// The policy of the gates' scope: where oomd looks first, and whether the
/// daemon itself is protected.
pub(super) fn gates_policy(f: &GatesPolicyFacts) -> Check {
    let omit = match f.daemon_omitted {
        Some(true) => "daemon/ has user.oomd_omit (oomd never picks the daemon)",
        Some(false) => {
            return grouped(
                "gates-memory-policy",
                Status::Warn,
                format!(
                    "{}; but daemon/ has no user.oomd_omit (a filesystem without user extended \
                     attributes, or a daemon started before this version): oomd could pick the \
                     daemon and orphan its running jobs",
                    gates_line(f)
                ),
            );
        }
        None => "daemon/ does not exist yet (the daemon marks it user.oomd_omit when it starts)",
    };
    grouped(
        "gates-memory-policy",
        Status::Ok,
        format!("{}; {omit}", gates_line(f)),
    )
}

fn gates_line(f: &GatesPolicyFacts) -> String {
    format!(
        "systemd-oomd acts on the gates' scope at {}% for {}s, before the agents' scope ({}s)",
        f.gates.limit_percent,
        f.gates.duration.as_secs(),
        f.agents.duration.as_secs()
    )
}

/// Why `systemd-oomd` cannot protect the gates' scope on this machine, or
/// `None` when its policy applies (the conditions of
/// [`Capabilities::oomd_policy_enabled`], in words).
fn oomd_gap(caps: &Capabilities, cfg: &ContainmentConfig) -> Option<String> {
    if !cfg.enabled {
        return Some("containment is disabled by [containment] enabled = false".to_owned());
    }
    if !caps.usable() {
        return Some("containment is off".to_owned());
    }
    if cfg.memory_policy == MemoryPolicy::None {
        return Some("[containment] memory_policy = \"none\"".to_owned());
    }
    if !caps.controllers.memory {
        return Some("the memory controller is not delegated".to_owned());
    }
    if !caps.oomd_active {
        return Some("systemd-oomd is not active".to_owned());
    }
    match caps.systemd_version {
        Some(v) if v >= OOMD_PER_UNIT_DURATION => None,
        Some(v) => Some(format!(
            "systemd {v} (< {OOMD_PER_UNIT_DURATION}) has no per-unit pressure duration"
        )),
        None => Some("the systemd version is unknown".to_owned()),
    }
}

/// What the `memory-guard` check needs (ADR-0019 piece 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GuardFacts {
    /// `[admission] memory_guard`.
    pub on: bool,
    /// `[admission] memory_reserve` as written (`auto` or a size).
    pub reserve: String,
    /// Why systemd-oomd does not protect the gates' scope; `None` when it does.
    pub oomd_gap: Option<String>,
}

/// The `memory-guard` check: whether Sluice's own guard is on and, when it is
/// off where systemd-oomd cannot act, a suggestion to turn it on.
pub(super) fn memory_guard(f: &GuardFacts) -> Check {
    let reserve = &f.reserve;
    let detail = match (f.on, &f.oomd_gap) {
        (true, None) => format!(
            "memory guard on: after 3 s with MemAvailable below the reserve ({reserve}), \
             the gates holding more than it and memory stalls, Sluice cancels its own \
             running job with the largest working set (systemd-oomd also acts on the \
             gates' scope)"
        ),
        (true, Some(why)) => format!(
            "memory guard on: after 3 s with MemAvailable below the reserve ({reserve}), \
             the gates holding more than it and memory stalls, Sluice cancels its own \
             running job with the largest working set; it is the only protection here \
             ({why})"
        ),
        (false, None) => "memory guard off (the default): systemd-oomd acts on the gates' \
             scope; [admission] memory_guard = true adds Sluice's own last line"
            .to_owned(),
        (false, Some(why)) => format!(
            "memory guard off and systemd-oomd cannot protect the gates here ({why}): \
             when the machine runs out of memory the kernel picks the victim, not Sluice; \
             consider [admission] memory_guard = true in the user config"
        ),
    };
    grouped("memory-guard", Status::Ok, detail)
}

/// The `gates-ceiling` check (ADR-0019 piece 5): shown only when
/// `[containment] gates_memory_max` or `gates_memory_swap_max` is set.
pub(super) fn gates_ceiling(cfg: &ContainmentConfig) -> Option<Check> {
    let max = cfg.gates_memory_max_bytes();
    let swap = cfg.gates_memory_swap_max_bytes();
    if max.is_none() && swap.is_none() {
        return None;
    }
    let part = |name: &str, bytes: Option<u64>| {
        format!(
            "{name} {}",
            bytes.map_or_else(
                || "unset".to_owned(),
                sluice_gate_daemon::memkill::format_bytes
            )
        )
    };
    Some(grouped(
        "gates-ceiling",
        Status::Ok,
        format!(
            "gate jobs together are capped on jobs/ ({}; {}): past it the kernel kills one \
             job whole, reported as killed for lack of memory; the daemon and the agents are \
             never inside that limit",
            part("memory.max", max),
            part("memory.swap.max", swap)
        ),
    ))
}

/// S4 check 11.
fn cpu_fallback(caps: &Capabilities) -> Option<Check> {
    (caps.usable() && !caps.controllers.cpu).then(|| {
        grouped(
            "fallback",
            Status::Ok,
            "CPU fallback: shell niced to +19 (applies only among processes of the same cgroup)",
        )
    })
}

// ---------------------------------------------------------------- session

/// Where a process is, in terms of the session tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Where {
    Ui,
    Tools,
    Outside,
}

impl Where {
    fn word(self) -> &'static str {
        match self {
            Self::Ui => "in ui",
            Self::Tools => "in tools",
            Self::Outside => "outside the session",
        }
    }
}

/// `ui`, `tools` or outside, from a cgroup path.
pub(super) fn classify(path: Option<&str>) -> Where {
    let Some(path) = path else {
        return Where::Outside;
    };
    let in_session = explain::session_in_path(path).is_some();
    match path.rsplit('/').next() {
        Some("ui") if in_session => Where::Ui,
        Some("tools") if in_session => Where::Tools,
        _ => Where::Outside,
    }
}

/// The last two components of a cgroup path (`s-abc/tools`).
fn short(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    parts[parts.len().saturating_sub(2)..].join("/")
}

/// What `session` needs to know.
#[derive(Debug, Clone, Default)]
pub(super) struct SessionFacts {
    /// This looks like an agent shell (`CLAUDECODE`, the leaf variable, an
    /// agent ancestor, or a session cgroup).
    pub in_agent_shell: bool,
    /// This shell's cgroup path.
    pub shell: Option<String>,
    /// The agent's pid and cgroup path, when found.
    pub agent: Option<(u32, Option<String>)>,
    /// `SLUICE_SESSION_LEAF`, when exported.
    pub leaf_var: Option<String>,
    /// The leaf's `cgroup.procs` can be opened for writing.
    pub leaf_writable: bool,
    /// Containment is off: the short reason of the summary.
    pub off_reason: Option<String>,
}

impl SessionFacts {
    fn gather(env: &EnvMap, caps: &Capabilities) -> Self {
        let shell = explain::own_cgroup();
        let agent = crate::session_contain::agent_pid(env).map(|pid| {
            let cg = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
                .ok()
                .and_then(|t| detect::parse_proc_cgroup(&t));
            (pid, cg)
        });
        let leaf_var = env
            .get(explain::LEAF_VAR)
            .filter(|v| !v.is_empty())
            .cloned();
        let leaf_writable = leaf_var.as_deref().is_some_and(|leaf| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(Path::new(leaf).join("cgroup.procs"))
                .is_ok()
        });
        let in_agent_shell = env.contains_key("CLAUDECODE")
            || leaf_var.is_some()
            || agent.is_some()
            || shell
                .as_deref()
                .is_some_and(|s| explain::session_in_path(s).is_some());
        let off_reason = match &caps.outcome {
            Outcome::Off { reason } => Some(reason.short()),
            _ => None,
        };
        Self {
            in_agent_shell,
            shell,
            agent,
            leaf_var,
            leaf_writable,
            off_reason,
        }
    }
}

/// The `session` check: where this shell and the agent are and why. `None`
/// outside an agent shell.
pub(super) fn session(f: &SessionFacts) -> Option<Check> {
    if !f.in_agent_shell {
        return None;
    }
    let shell = classify(f.shell.as_deref());
    let agent = f
        .agent
        .as_ref()
        .map(|(pid, cg)| (*pid, classify(cg.as_deref())));
    let shell_at = f
        .shell
        .as_deref()
        .map_or_else(String::new, |s| format!(" ({})", short(s)));
    let agent_text = match agent {
        Some((pid, w)) => format!("agent (pid {pid}) is {}", w.word()),
        None => "agent process not found".to_owned(),
    };
    let (status, why) = match shell {
        Where::Tools => (Status::Ok, String::new()),
        Where::Ui if f.leaf_var.is_some() && !f.leaf_writable => (
            Status::Ok,
            "; the shell cannot write the tools leaf here (a sandbox or a read-only cgroupfs), so it stays in ui at nice 19".to_owned(),
        ),
        Where::Ui => (
            Status::Warn,
            "; the session line was not run by this shell (SLUICE_SESSION_LEAF is unset), so it stays in ui".to_owned(),
        ),
        Where::Outside => {
            let why = match (&f.off_reason, &f.leaf_var) {
                (Some(r), _) => format!("containment is off ({r})"),
                (None, None) => "the plugin's session hook has not contained this session (SLUICE_SESSION_LEAF is unset): is the sluice plugin enabled? restart the session".to_owned(),
                (None, Some(_)) if !f.leaf_writable => "the session leaf cannot be written from here (a sandbox or a read-only cgroupfs)".to_owned(),
                (None, Some(_)) => "the session line has not run in this shell yet".to_owned(),
            };
            (Status::Warn, format!("; {why}"))
        }
    };
    Some(grouped(
        "session",
        status,
        format!(
            "this shell is {}{shell_at}; {agent_text}{why}",
            shell.word()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_gate_core::cgroup::detect::{Facts, FsKind, evaluate, parse_controllers};

    const MANAGER: &str = "/user.slice/user-1000.slice/user@1000.service";

    fn facts() -> Facts {
        Facts {
            uid: 1000,
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

    fn detail<'a>(checks: &'a [Check], name: &str) -> &'a str {
        &checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no {name} in {checks:?}"))
            .detail
    }

    #[test]
    fn a_full_host_says_so_with_the_s4_messages() {
        let caps = evaluate(facts());
        let out = summary_and_lines(&caps, &ContainmentConfig::default());
        assert_eq!(detail(&out, "containment"), "full");
        assert_eq!(detail(&out, "cg-v2"), "cgroup v2 (unified hierarchy)");
        assert_eq!(detail(&out, "systemd"), "systemd 259");
        assert_eq!(
            detail(&out, "user-manager"),
            "systemd user manager reachable"
        );
        assert_eq!(
            detail(&out, "delegate-cpu"),
            "cpu delegated to the user manager: CPU weights available"
        );
        assert_eq!(
            detail(&out, "own-cgroup"),
            "this shell is inside the user manager's tree: it can place itself"
        );
        assert!(out.iter().all(|c| c.group == Some("containment")));
        assert!(out.iter().all(|c| c.name != "oomd"));
    }

    #[test]
    fn a_partial_and_an_off_host_name_the_cause() {
        let mut f = facts();
        f.manager_controllers = Some(parse_controllers("memory pids\n"));
        let out = summary_and_lines(&evaluate(f), &ContainmentConfig::default());
        assert_eq!(
            detail(&out, "containment"),
            "partial (memory, pids; cpu by nice)"
        );
        assert!(detail(&out, "delegate-cpu").contains("should (an override"));
        let mut f = facts();
        f.manager_socket = false;
        let out = summary_and_lines(&evaluate(f), &ContainmentConfig::default());
        assert_eq!(
            detail(&out, "containment"),
            "off (user manager not reachable)"
        );
        let c = out.iter().find(|c| c.name == "user-manager").unwrap();
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("Typical for containers, cron and su"));
        // Disabled by the user: off is fine, and it says why.
        let cfg = ContainmentConfig {
            enabled: false,
            ..ContainmentConfig::default()
        };
        let out = summary_and_lines(&evaluate(facts()), &cfg);
        assert!(detail(&out, "containment").contains("disabled by [containment]"));
    }

    #[test]
    fn the_memory_policy_shows_the_derived_thresholds() {
        let cfg = ContainmentConfig::default();
        let caps = evaluate(facts());
        let ancestor = AncestorPolicy {
            limit_percent: Some(50.0),
            duration: Some(std::time::Duration::from_secs(20)),
        };
        let p = detect::derive_pressure_policy(&ancestor);
        let c = memory_policy(&caps, &cfg, Some(&(ancestor, Some(p))));
        assert_eq!(c.status, Status::Ok);
        assert_eq!(
            c.detail,
            "memory policy on: systemd-oomd acts on the agents scope at 30% memory pressure for \
             15s (derived from the system policy on user@UID.service, 50% / 20s; systemd 259, \
             systemd-oomd active)"
        );
        // Off, with each reason.
        let mut f = facts();
        f.oomd_active = false;
        let c = memory_policy(&evaluate(f), &cfg, None);
        assert!(
            c.detail
                .starts_with("memory policy off: systemd-oomd is not active")
        );
        let mut f = facts();
        f.systemd_version = Some(255);
        let c = memory_policy(&evaluate(f), &cfg, None);
        assert!(c.detail.contains("systemd 255 (< 257)"), "{}", c.detail);
        let none = ContainmentConfig {
            memory_policy: MemoryPolicy::None,
            ..ContainmentConfig::default()
        };
        let c = memory_policy(&caps, &none, None);
        assert!(c.detail.contains("memory_policy = \"none\""));
        let mut f = facts();
        f.manager_socket = false;
        let c = memory_policy(&evaluate(f), &cfg, None);
        assert!(c.detail.contains("containment is off"));
    }

    #[test]
    fn an_ancestor_with_oom_group_is_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path();
        let jobs = mount.join("user.slice/user@1000.service/sluice.slice/daemon.scope/jobs");
        std::fs::create_dir_all(&jobs).unwrap();
        // Nothing flagged: no finding, and the check is fine.
        assert_eq!(ancestors_with_oom_group(&jobs, mount), Vec::<String>::new());
        std::fs::write(jobs.join("memory.oom.group"), "0\n").unwrap();
        let ok = gates_oom(&GatesOomFacts {
            adj: 800,
            auto: true,
            own_adj: Some(200),
            group_ancestors: ancestors_with_oom_group(&jobs, mount),
            oomd_active: true,
        });
        assert_eq!(ok.status, Status::Ok);
        assert!(
            ok.detail.contains("oom_score_adj >= 800 (auto)"),
            "{}",
            ok.detail
        );
        assert!(
            ok.detail.contains("systemd-oomd is active"),
            "{}",
            ok.detail
        );
        // Flagged above the leaves, at two levels (jobs/ itself and the user manager).
        std::fs::write(jobs.join("memory.oom.group"), "1\n").unwrap();
        let manager = mount.join("user.slice/user@1000.service");
        std::fs::write(manager.join("memory.oom.group"), "1\n").unwrap();
        let found = ancestors_with_oom_group(&jobs, mount);
        assert_eq!(
            found,
            [
                "user.slice/user@1000.service/sluice.slice/daemon.scope/jobs",
                "user.slice/user@1000.service"
            ]
        );
        let warn = gates_oom(&GatesOomFacts {
            adj: 600,
            auto: false,
            own_adj: Some(900),
            group_ancestors: found,
            oomd_active: false,
        });
        assert_eq!(warn.status, Status::Warn);
        assert!(warn.detail.contains("widened"), "{}", warn.detail);
        assert!(warn.detail.contains("user@1000.service"), "{}", warn.detail);
        assert!(
            warn.detail.contains("oom_score_adj >= 600;"),
            "{}",
            warn.detail
        );
        assert!(
            warn.detail.contains("this shell runs at 900"),
            "{}",
            warn.detail
        );
        assert!(
            warn.detail.contains("systemd-oomd is not active"),
            "{}",
            warn.detail
        );
    }

    #[test]
    fn controller_files_are_judged_by_their_presence() {
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path();
        let layout = Layout::with_prefix(mount, 1000, "sluice-test-doc");
        let manager = detect::manager_cgroup_dir(mount, 1000);
        std::fs::create_dir_all(&manager).unwrap();
        let c = controller_files(&layout, &manager);
        assert_eq!(c.status, Status::Warn);
        assert!(
            c.detail
                .contains("cpu.weight, memory.current, pids.max missing in the user manager"),
            "{}",
            c.detail
        );
        for f in ["cpu.weight", "memory.current", "pids.max"] {
            std::fs::write(manager.join(f), "1\n").unwrap();
        }
        let c = controller_files(&layout, &manager);
        assert_eq!(c.status, Status::Ok, "{}", c.detail);
        assert!(c.detail.contains("pids.max present in the user manager"));
        // The scope wins when it exists.
        let scope = layout.agents_scope_dir();
        std::fs::create_dir_all(&scope).unwrap();
        std::fs::write(scope.join("cpu.weight"), "30\n").unwrap();
        let c = controller_files(&layout, &manager);
        assert!(
            c.detail
                .contains("memory.current, pids.max missing in sluice-test-doc-agents.scope"),
            "{}",
            c.detail
        );
    }

    #[test]
    fn the_slice_check_follows_the_directories() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::with_prefix(dir.path(), 1000, "sluice-test-doc");
        assert!(slice(&layout).detail.contains("not created yet"));
        let scope = layout.agents_scope_dir();
        std::fs::create_dir_all(&scope).unwrap();
        std::fs::write(scope.join("cgroup.procs"), "").unwrap();
        assert!(
            slice(&layout)
                .detail
                .contains("gates scope appears when the daemon runs")
        );
        let d = layout.daemon_scope_dir();
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("cgroup.procs"), "").unwrap();
        assert!(
            slice(&layout)
                .detail
                .contains("(agents scope, gates scope)")
        );
    }

    fn session_facts(shell: &str, agent: &str) -> SessionFacts {
        SessionFacts {
            in_agent_shell: true,
            shell: Some(shell.to_owned()),
            agent: Some((4242, Some(agent.to_owned()))),
            leaf_var: Some("/sys/fs/cgroup/x/s-abc/tools".into()),
            leaf_writable: true,
            off_reason: None,
        }
    }

    #[test]
    fn the_session_check_says_where_and_why() {
        let base = "/user.slice/user-1000.slice/user@1000.service/sluice.slice/sluice-agents.scope";
        let tools = format!("{base}/s-abc/tools");
        let ui = format!("{base}/s-abc/ui");
        // Contained: shell in tools, agent in ui.
        let c = session(&session_facts(&tools, &ui)).unwrap();
        assert_eq!(c.status, Status::Ok);
        assert_eq!(
            c.detail,
            "this shell is in tools (s-abc/tools); agent (pid 4242) is in ui"
        );
        // A sandbox: the shell stays in ui because it cannot write the leaf.
        let mut f = session_facts(&ui, &ui);
        f.leaf_writable = false;
        let c = session(&f).unwrap();
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("stays in ui at nice 19"), "{}", c.detail);
        // Outside because the hook never ran.
        let mut f = session_facts("/user.slice/app.scope", "/user.slice/app.scope");
        f.leaf_var = None;
        let c = session(&f).unwrap();
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("outside the session"), "{}", c.detail);
        assert!(
            c.detail.contains("SLUICE_SESSION_LEAF is unset"),
            "{}",
            c.detail
        );
        // Outside because containment is off on this machine.
        f.off_reason = Some("user manager not reachable".into());
        let c = session(&f).unwrap();
        assert!(
            c.detail
                .contains("containment is off (user manager not reachable)")
        );
        // Not an agent shell: no check at all.
        f.in_agent_shell = false;
        assert!(session(&f).is_none());
    }

    #[test]
    fn classification_needs_a_session_component() {
        assert_eq!(classify(Some("/a/s-x/ui")), Where::Ui);
        assert_eq!(classify(Some("/a/s-x/tools")), Where::Tools);
        assert_eq!(classify(Some("/a/b/ui")), Where::Outside);
        assert_eq!(classify(None), Where::Outside);
    }

    fn policies() -> (PressurePolicy, PressurePolicy) {
        let agents = PressurePolicy {
            limit_percent: 30,
            duration: std::time::Duration::from_secs(5),
        };
        (agents, detect::derive_gates_policy(agents))
    }

    #[test]
    fn the_gates_policy_line_names_limit_durations_and_the_omit_mark() {
        let (agents, gates) = policies();
        let facts = |daemon_omitted| GatesPolicyFacts {
            agents,
            gates,
            daemon_omitted,
        };
        let ok = gates_policy(&facts(Some(true)));
        assert_eq!(ok.status, Status::Ok);
        assert_eq!(
            ok.detail,
            "systemd-oomd acts on the gates' scope at 30% for 3s, before the agents' scope (5s); \
             daemon/ has user.oomd_omit (oomd never picks the daemon)"
        );
        let missing = gates_policy(&facts(Some(false)));
        assert_eq!(missing.status, Status::Warn);
        assert!(
            missing.detail.contains("no user.oomd_omit"),
            "{}",
            missing.detail
        );
        let not_running = gates_policy(&facts(None));
        assert_eq!(not_running.status, Status::Ok);
        assert!(not_running.detail.contains("does not exist yet"));
        assert_eq!(ok.group, Some("containment"));
    }

    #[test]
    fn the_gates_policy_facts_read_the_daemon_leaf() {
        let (agents, gates) = policies();
        let root = tempfile::tempdir().unwrap();
        let layout = Layout {
            mount: root.path().to_path_buf(),
            user_manager: root.path().to_path_buf(),
            slice: "x.slice".into(),
            daemon_scope: "x-daemon.scope".into(),
            agents_scope: "x-agents.scope".into(),
        };
        let f = GatesPolicyFacts::gather(&layout, agents);
        assert_eq!((f.agents, f.gates, f.daemon_omitted), (agents, gates, None));
        std::fs::create_dir_all(layout.daemon_leaf_dir()).unwrap();
        let f = GatesPolicyFacts::gather(&layout, agents);
        assert_eq!(f.daemon_omitted, Some(false));
    }

    #[test]
    fn the_guard_line_says_whether_it_is_on_and_suggests_it_where_oomd_cannot_act() {
        let facts = |on, gap: Option<&str>| GuardFacts {
            on,
            reserve: "auto".into(),
            oomd_gap: gap.map(str::to_owned),
        };
        let off_gap = memory_guard(&facts(false, Some("systemd-oomd is not active")));
        assert_eq!(off_gap.group, Some("containment"));
        assert!(off_gap.detail.contains("systemd-oomd is not active"));
        assert!(
            off_gap
                .detail
                .contains("consider [admission] memory_guard = true")
        );
        let off_ok = memory_guard(&facts(false, None));
        assert!(off_ok.detail.starts_with("memory guard off (the default)"));
        assert!(!off_ok.detail.contains("consider"));
        let on_alone = memory_guard(&facts(
            true,
            Some("systemd 255 (< 257) has no per-unit duration"),
        ));
        assert!(on_alone.detail.contains("the only protection here"));
        assert!(on_alone.detail.contains("reserve (auto)"));
        let on_both = memory_guard(&facts(true, None));
        assert!(on_both.detail.contains("systemd-oomd also acts"));
        assert!(!on_both.detail.contains("only protection"));
    }

    #[test]
    fn the_gates_ceiling_is_shown_only_when_set() {
        assert!(gates_ceiling(&ContainmentConfig::default()).is_none());
        let cfg = ContainmentConfig {
            gates_memory_max: "20G".into(),
            ..ContainmentConfig::default()
        };
        let c = gates_ceiling(&cfg).unwrap();
        assert_eq!(c.group, Some("containment"));
        assert!(c.detail.contains("memory.max 20.0 GiB"), "{}", c.detail);
        assert!(c.detail.contains("memory.swap.max unset"));
    }

    #[test]
    fn the_oomd_gap_names_the_first_missing_condition() {
        let cfg = ContainmentConfig::default();
        let caps = evaluate(facts());
        assert_eq!(oomd_gap(&caps, &cfg), None);
        let mut f = facts();
        f.oomd_active = false;
        assert_eq!(
            oomd_gap(&evaluate(f), &cfg).as_deref(),
            Some("systemd-oomd is not active")
        );
        let mut f = facts();
        f.systemd_version = Some(255);
        assert!(
            oomd_gap(&evaluate(f), &cfg)
                .unwrap()
                .contains("systemd 255")
        );
        let none = ContainmentConfig {
            memory_policy: MemoryPolicy::None,
            ..ContainmentConfig::default()
        };
        assert!(oomd_gap(&caps, &none).unwrap().contains("memory_policy"));
        let off = ContainmentConfig {
            enabled: false,
            ..ContainmentConfig::default()
        };
        assert!(oomd_gap(&caps, &off).unwrap().contains("disabled"));
    }
}
