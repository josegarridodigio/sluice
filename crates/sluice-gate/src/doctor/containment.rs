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
use sluice_gate_core::cgroup::tree::Layout;
use sluice_gate_core::cgroup::{CGROUP_MOUNT, PressurePolicy};
use sluice_gate_core::config::{ContainmentConfig, EnvMap, MemoryPolicy, UserConfig};

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
    }
    let policy = caps.oomd_policy_enabled(cfg).then(|| {
        let ancestor = detect::system_ancestor_policy(env, caps.facts.uid);
        let policy = detect::pressure_policy(env, cfg, caps.facts.uid).ok();
        (ancestor, policy)
    });
    out.push(memory_policy(&caps, cfg, policy.as_ref()));
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
             5s (derived from the system policy on user@UID.service, 50% / 20s; systemd 259, \
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
}
