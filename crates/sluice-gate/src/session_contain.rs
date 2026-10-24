//! Containment of an agent session from the plugin side (tasks C3 and C4 of
//! `kb/sluice/plan/containment-plan.md`; design
//! `kb/sluice/design/containment.md` sections 2, 3 and 6).
//!
//! - [`contain_session`] runs inside the `SessionStart` hook: it makes sure the
//!   agents scope exists, creates the session node `s-<id>/{ui,tools}`, moves
//!   the agent process and its current descendants into `ui`, and returns the
//!   line for `$CLAUDE_ENV_FILE` ([`tools_line`]).
//! - [`place_self`] is the same self-placement for a shim, before it runs a
//!   heavy tool, from the `SLUICE_SESSION_LEAF` the env line exported.
//!
//! Everything fails open: an error only means the session is not contained,
//! and nothing here ever prints.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sluice_gate_core::cgroup::detect::{self, Missing, OffReason, Outcome};
use sluice_gate_core::cgroup::systemd;
use sluice_gate_core::cgroup::tree::{Tree, TreeSettings, write_pid};
use sluice_gate_core::cgroup::{CGROUP_MOUNT, Layout, PressurePolicy, session_dir_name};
use sluice_gate_core::config::{EnvMap, UserConfig};
use sluice_gate_daemon::containment::stopgap_holder_args;

use crate::shim;

/// Exported by the env-file line: the `tools` leaf of the session, for the
/// shims (C4).
pub const LEAF_VAR: &str = "SLUICE_SESSION_LEAF";
/// Test-only: unit prefix of the tree (`<prefix>.slice`,
/// `<prefix>-agents.scope`), so tests never touch the real `sluice.slice`.
/// It must start with `sluice-test-`. Not part of the user interface.
pub const TEST_PREFIX_VAR: &str = "SLUICE_TEST_CONTAINMENT_PREFIX";

/// The tree layout: standard, or the test prefix.
#[must_use]
pub fn layout(env: &EnvMap, uid: u32) -> Layout {
    match env.get(TEST_PREFIX_VAR) {
        Some(p) if p.starts_with("sluice-test-") && p.chars().all(is_unit_char) => {
            Layout::with_prefix(Path::new(CGROUP_MOUNT), uid, p)
        }
        _ => Layout::standard(uid),
    }
}

fn is_unit_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// What the hook did, for the client log and the env file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contained {
    /// `full`, `partial` or `off`.
    pub label: &'static str,
    /// The line for the env file, when there is one.
    pub line: Option<String>,
}

impl Contained {
    fn off(line: Option<String>) -> Self {
        Self { label: "off", line }
    }
}

/// The env-file line (design section 3): exports the leaf for the shims and
/// moves the shell into `tools`, else renices it; silent in every branch.
/// `renice_too` (CPU cannot be weighted) also renices after a successful move.
#[must_use]
pub fn tools_line(tools: &Path, renice_too: bool) -> String {
    let leaf = shim::quote(&tools.display().to_string());
    let procs = shim::quote(&tools.join("cgroup.procs").display().to_string());
    let after = if renice_too {
        "; renice -n 19 $$ >/dev/null 2>&1"
    } else {
        ""
    };
    format!(
        "export {LEAF_VAR}={leaf}; if [ -w {procs} ]; then echo $$ > {procs}; else renice -n 19 $$ >/dev/null 2>&1; fi{after}"
    )
}

/// The line when nothing can be placed but `nice` still applies.
#[must_use]
pub fn renice_line() -> String {
    "renice -n 19 $$ >/dev/null 2>&1".to_owned()
}

/// `true` when the machine cannot weigh the CPU but `nice` applies (design
/// section 6): no unified cgroups, no systemd, no controller delegated, or
/// `cpu` missing.
fn cpu_by_nice(outcome: &Outcome) -> bool {
    match outcome {
        Outcome::Off { reason } => matches!(
            reason,
            OffReason::CgroupV1Hybrid | OffReason::NoSystemd | OffReason::NoControllers
        ),
        Outcome::Partial { missing } => missing.contains(&Missing::Cpu),
        Outcome::Full => false,
    }
}

/// The agent process: `CLAUDE_PID`, else the first ancestor named `claude`.
#[must_use]
pub fn agent_pid(env: &EnvMap) -> Option<u32> {
    let own = std::process::id();
    let valid = |p: &u32| *p > 1 && *p != own && Path::new(&format!("/proc/{p}")).exists();
    env.get("CLAUDE_PID")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(valid)
        .or_else(|| {
            let mut pid = own;
            for _ in 0..64 {
                let (parent, comm) = stat_of(pid)?;
                if pid != own && comm == "claude" {
                    return Some(pid);
                }
                if parent <= 1 {
                    return None;
                }
                pid = parent;
            }
            None
        })
}

/// `(ppid, comm)` of a process from `/proc/<pid>/stat`.
fn stat_of(pid: u32) -> Option<(u32, String)> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Parses `pid (comm) S ppid ...`; `comm` may contain spaces and parentheses,
/// so the last `)` closes it.
fn parse_stat(text: &str) -> Option<(u32, String)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_owned();
    let ppid = text
        .get(close + 1..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((ppid, comm))
}

/// `pid` and all its descendants, from one pass over `/proc`.
#[must_use]
pub fn with_descendants(pid: u32) -> Vec<u32> {
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(child) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some((ppid, _)) = stat_of(child) {
                children.entry(ppid).or_default().push(child);
            }
        }
    }
    let mut out = vec![pid];
    let mut queue = VecDeque::from([pid]);
    while let Some(p) = queue.pop_front() {
        for &c in children.get(&p).into_iter().flatten() {
            if !out.contains(&c) {
                out.push(c);
                queue.push_back(c);
            }
        }
    }
    out
}

/// `true` when `/proc/<pid>/cgroup` is inside the unit `scope`.
fn in_scope(pid: u32, scope: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()
        .and_then(|t| detect::parse_proc_cgroup(&t))
        .is_some_and(|p| p.split('/').any(|c| c == scope))
}

/// Starts a process that keeps a new agents scope alive until the sessions'
/// own processes do; it is adopted by the scope and lives in the scope's
/// `holder/` leaf. It is `sluice daemon holder --lifetime <secs>`, which ends
/// by itself after `STOPGAP_LIFETIME_SECS` (the scope ends with its last
/// process and the next session recreates it), so it never outlives its
/// purpose whatever happens to the scope; the daemon retires it when it takes
/// the scope over. `None` when it cannot start.
fn spawn_holder() -> Option<std::process::Child> {
    Command::new("/proc/self/exe")
        .arg0("sluice")
        .args(stopgap_holder_args())
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()
}

/// Contains the session: see the module documentation. Never fails; a
/// problem is a `partial` or `off` label.
#[must_use]
pub fn contain_session(
    env: &EnvMap,
    user: &UserConfig,
    session_id: Option<&str>,
    agent: Option<u32>,
) -> Contained {
    let cfg = &user.containment;
    if !cfg.enabled {
        return Contained::off(None);
    }
    let caps = detect::detect(env);
    let nice = cpu_by_nice(&caps.outcome);
    if !caps.usable() {
        tracing::debug!("containment off: {}", caps.summary());
        return Contained::off(nice.then(renice_line));
    }
    let uid = caps.facts.uid;
    let layout = layout(env, uid);
    let session = session_dir_name(session_id, agent.unwrap_or(0));
    if session_id.is_none_or(|s| s.trim().is_empty()) && agent.is_none() {
        return Contained::off(nice.then(renice_line));
    }
    let scope_there = layout.agents_scope_dir().join("cgroup.procs").exists();
    let pressure = if scope_there || !caps.oomd_policy_enabled(cfg) {
        PressurePolicy {
            limit_percent: 30,
            duration: detect::DERIVED_DURATION_CAP,
        }
    } else {
        detect::pressure_policy(env, cfg, uid).unwrap_or(PressurePolicy {
            limit_percent: 30,
            duration: detect::DERIVED_DURATION_CAP,
        })
    };
    let cpus =
        std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(1));
    let settings = TreeSettings::from_config(cfg, &caps, pressure, cpus);
    let tree = Tree::new(layout.clone(), settings, env.clone());

    if !scope_there {
        let mut holder = spawn_holder();
        // Created is not enough: units with the same names may have been
        // stopped moments ago (the scope can end up `failed`), so the scope
        // must be active and the holder inside it, or the holder is killed
        // and the session simply runs uncontained.
        let made = holder.as_ref().is_some_and(|h| {
            tree.ensure_agents_scope(h.id()).is_ok()
                && systemd::is_unit_active(env, &layout.agents_scope, true)
                && in_scope(h.id(), &layout.agents_scope)
        });
        if !made && let Some(h) = holder.as_mut() {
            let _ = h.kill();
            let _ = h.wait();
        }
        // Another hook or the daemon may have created it meanwhile.
        if !layout.agents_scope_dir().join("cgroup.procs").exists() {
            tracing::debug!("containment: agents scope unavailable");
            return Contained::off(nice.then(renice_line));
        }
    }
    let node = match tree.ensure_session(&session) {
        Ok(n) => n,
        Err(e) => {
            tracing::debug!("containment: session {session}: {e}");
            return Contained::off(nice.then(renice_line));
        }
    };
    let mut moved = false;
    if let Some(pid) = agent {
        let own = std::process::id();
        let pids: Vec<u32> = with_descendants(pid)
            .into_iter()
            .filter(|p| *p != own)
            .collect();
        let report = tree.move_pids(&node.ui, &pids);
        moved = report.direct.contains(&pid) || report.attached.contains(&pid);
        for (p, e) in &report.failed {
            tracing::debug!("containment: pid {p} not moved: {e}");
        }
    }
    let full = matches!(caps.outcome, Outcome::Full) && moved;
    Contained {
        label: if full { "full" } else { "partial" },
        line: Some(tools_line(&node.tools.dir, nice)),
    }
}

/// A shim's self-placement (C4): writes this process into the session's
/// `tools` leaf named by [`LEAF_VAR`]. Does nothing without the variable, for
/// a path outside the cgroup mount, or when the write is refused.
pub fn place_self(env: &EnvMap) {
    let Some(leaf) = env.get(LEAF_VAR).filter(|v| !v.is_empty()) else {
        return;
    };
    let leaf = PathBuf::from(leaf);
    if !leaf.starts_with(CGROUP_MOUNT)
        || leaf
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return;
    }
    if let Err(e) = write_pid(&leaf, std::process::id()) {
        tracing::debug!("shim: not placed in {}: {e}", leaf.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_line_is_silent_quoted_and_exports_the_leaf() {
        let line = tools_line(Path::new("/sys/fs/cgroup/a b/it's/tools"), false);
        assert_eq!(
            line,
            "export SLUICE_SESSION_LEAF='/sys/fs/cgroup/a b/it'\\''s/tools'; \
             if [ -w '/sys/fs/cgroup/a b/it'\\''s/tools/cgroup.procs' ]; then \
             echo $$ > '/sys/fs/cgroup/a b/it'\\''s/tools/cgroup.procs'; \
             else renice -n 19 $$ >/dev/null 2>&1; fi"
        );
        assert!(tools_line(Path::new("/x"), true).ends_with("fi; renice -n 19 $$ >/dev/null 2>&1"));
    }

    #[test]
    fn stat_lines_with_awkward_names_parse() {
        assert_eq!(
            parse_stat("12 (a) b (c) S 7 12 12 0"),
            Some((7, "a) b (c".to_owned()))
        );
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn nice_applies_where_cpu_cannot_be_weighted() {
        assert!(cpu_by_nice(&Outcome::Partial {
            missing: vec![Missing::Cpu]
        }));
        assert!(!cpu_by_nice(&Outcome::Partial {
            missing: vec![Missing::Pids]
        }));
        assert!(!cpu_by_nice(&Outcome::Full));
        assert!(cpu_by_nice(&Outcome::Off {
            reason: OffReason::NoSystemd
        }));
        assert!(!cpu_by_nice(&Outcome::Off {
            reason: OffReason::UserManagerUnreachable
        }));
    }

    #[test]
    fn descendants_include_the_process_and_its_children() {
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        let all = with_descendants(std::process::id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(all.contains(&std::process::id()));
        assert!(all.contains(&child.id()));
    }

    #[test]
    fn only_test_prefixes_change_the_layout() {
        let mut env = EnvMap::new();
        env.insert(TEST_PREFIX_VAR.into(), "evil".into());
        assert_eq!(layout(&env, 1000).slice, "sluice.slice");
        env.insert(TEST_PREFIX_VAR.into(), "sluice-test-9".into());
        assert_eq!(
            layout(&env, 1000).agents_scope,
            "sluice-test-9-agents.scope"
        );
    }
}
