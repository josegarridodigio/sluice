#![allow(clippy::too_many_lines)]
//! Integration tests of the daemon's containment (task C2) against the real
//! systemd user manager: the daemon lives in its own delegated scope, gate
//! and exec jobs run in one cgroup leaf each, the agents scope and its
//! holder exist, a restart reuses the units, and a host that cannot contain
//! leaves the 0.4.1 behaviour.
//!
//! Safety rules: no CPU load, no memory pressure, no memory limits; only
//! processes started here are touched; every unit is named
//! `sluice-test-<pid>-<case>*` (the daemon is told through the test-only
//! `SLUICE_TEST_CONTAINMENT_PREFIX`) and stopped by a drop guard; the daemon runs
//! with a private HOME and runtime directory and a short `/tmp/sa-*` socket,
//! so it never meets the real daemon, lock or units. Tests that need the user
//! manager skip (printing why) when it is unreachable.

mod common;

use std::fs;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;
use sluice_gate_core::cgroup::detect::{self, Outcome};
use sluice_gate_core::cgroup::systemd;
use sluice_gate_core::cgroup::tree::{Tree, TreeSettings};
use sluice_gate_core::cgroup::{CGROUP_MOUNT, Layout};
use sluice_gate_core::config::{ContainmentConfig, EnvMap, process_env};
use tempfile::TempDir;

const PROBE_GATE: &str = r#"
[gates.probe]
cmd = "cat /proc/self/cgroup > \"$SLUICE_TEST_COUNTER.cg\"; sleep 1"
"#;

fn wait_for(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    cond()
}

fn procs(dir: &Path) -> Vec<u32> {
    fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// The job leaves (subdirectories of `jobs/`; the control files are skipped).
fn job_leaves(layout: &Layout) -> Vec<std::path::PathBuf> {
    fs::read_dir(layout.jobs_dir())
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

fn cgroup_of(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap_or_default()
        .trim()
        .trim_start_matches("0::")
        .to_owned()
}

fn cmdline_of(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

/// Stacks alive in this process (see `Drop for Stack`).
static LIVE: std::sync::Mutex<usize> = std::sync::Mutex::new(0);

/// A test daemon with a private HOME, runtime directory and short socket,
/// its units named after `prefix`, stopped (with its units) on drop.
struct Stack {
    sb: Sandbox,
    prefix: String,
    /// Where the systemd user manager lives (the real runtime directory);
    /// `None` leaves the daemon with its private, empty one (no manager).
    manager_runtime: Option<String>,
    socket_dir: TempDir,
    kids: Vec<Child>,
    env: EnvMap,
}

impl Stack {
    /// `None` (after printing why) when this host cannot be contained.
    fn new(case: &str, config: &str, with_manager: bool) -> Option<Self> {
        let env = process_env();
        let manager_runtime = if with_manager {
            let caps = detect::detect(&env);
            if caps.outcome != Outcome::Full {
                println!("skipped: needs a full containment host: {}", caps.summary());
                return None;
            }
            env.get("XDG_RUNTIME_DIR").cloned()
        } else {
            None
        };
        if with_manager && manager_runtime.is_none() {
            return None;
        }
        let socket_dir = tempfile::Builder::new()
            .prefix("sa-")
            .tempdir_in("/tmp")
            .expect("short tempdir");
        *LIVE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        Some(Self {
            sb: Sandbox::new(config),
            prefix: format!("sluice-test-{}-{case}", std::process::id()),
            manager_runtime,
            socket_dir,
            kids: Vec::new(),
            env,
        })
    }

    fn envs(&self) -> Vec<(String, String)> {
        let mut e = self.sb.env();
        e.push((
            "SLUICE_SOCKET".into(),
            self.socket_dir.path().join("d.sock").display().to_string(),
        ));
        e.push(("SLUICE_TEST_CONTAINMENT_PREFIX".into(), self.prefix.clone()));
        if let Some(rt) = &self.manager_runtime {
            e.push(("SLUICE_TEST_MANAGER_RUNTIME_DIR".into(), rt.clone()));
        }
        e
    }

    fn cmd(&self, args: &[&str]) -> StdCommand {
        let mut c = StdCommand::new(Sandbox::binary());
        c.args(args)
            .current_dir(&self.sb.repo)
            .env_clear()
            .envs(self.envs())
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd(args).output().expect("run sluice")
    }

    fn status(&self) -> Option<serde_json::Value> {
        let out = self.run(&["status", "--json"]);
        out.status
            .success()
            .then(|| serde_json::from_slice(&out.stdout).ok())
            .flatten()
    }

    /// Starts the daemon (detached) and waits until it answers; returns its pid.
    fn start_daemon(&self) -> u32 {
        let out = self.run(&["daemon"]);
        assert!(out.status.success(), "daemon start: {out:?}");
        let mut pid = 0;
        assert!(
            wait_for(Duration::from_secs(15), || {
                self.status().is_some_and(|s| {
                    pid = u32::try_from(s["daemon"]["pid"].as_u64().unwrap_or(0)).unwrap_or(0);
                    pid > 0
                })
            }),
            "the daemon did not answer; log:\n{}",
            self.daemon_log()
        );
        pid
    }

    fn stop_daemon(&self) {
        let _ = self.run(&["daemon", "stop", "--force"]);
    }

    fn daemon_log(&self) -> String {
        fs::read_to_string(self.sb.home.join("state/sluice/daemon.log")).unwrap_or_default()
    }

    fn layout(&self) -> Layout {
        let uid = detect::gather(&self.env).uid;
        Layout::with_prefix(Path::new(CGROUP_MOUNT), uid, &self.prefix)
    }

    fn tree(&self) -> Tree {
        let caps = detect::detect(&self.env);
        let cfg = ContainmentConfig::default();
        let pressure = detect::pressure_policy(&self.env, &cfg, caps.facts.uid).unwrap();
        Tree::new(
            self.layout(),
            TreeSettings::from_config(&cfg, &caps, pressure, 4),
            self.env.clone(),
        )
    }

    fn sleeper(&mut self) -> u32 {
        let child = StdCommand::new("sleep")
            .arg("600")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep starts");
        let pid = child.id();
        self.kids.push(child);
        pid
    }

    fn units(&self) -> Vec<String> {
        systemd::list_units(&self.env, &format!("{}*", self.prefix)).unwrap_or_default()
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.stop_daemon();
        for k in &mut self.kids {
            let _ = k.kill();
            let _ = k.wait();
        }
        if self.manager_runtime.is_some() {
            let layout = self.layout();
            let _ = systemd::stop_units(
                &self.env,
                &[
                    layout.agents_scope.as_str(),
                    layout.daemon_scope.as_str(),
                    layout.slice.as_str(),
                ],
            );
            // Nothing the test started may outlive its units.
            let left = common::cgtree::kill_leftovers(&layout);
            if !left.is_empty() {
                eprintln!("cleanup: processes outlived {}: {left:?}", layout.slice);
            }
        }
        // The implicit parent slice named after this process is shared by
        // the cases running in parallel: the last one out stops it (under
        // the lock, so a case being created waits for the stop to finish).
        let mut live = LIVE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *live -= 1;
        if *live == 0 && self.manager_runtime.is_some() {
            let parent = format!("sluice-test-{}.slice", std::process::id());
            let _ = systemd::stop_units(&self.env, &[parent.as_str()]);
        }
    }
}

#[test]
fn the_daemon_and_its_jobs_live_in_their_own_cgroups() {
    let Some(stack) = Stack::new("jobs", PROBE_GATE, true) else {
        return;
    };
    let layout = stack.layout();
    let daemon_pid = stack.start_daemon();

    // The daemon: in `<prefix>-daemon.scope/daemon`.
    let own = cgroup_of(daemon_pid);
    assert!(
        own.ends_with(&format!("{}/daemon", layout.daemon_scope)),
        "daemon cgroup: {own}\nlog:\n{}",
        stack.daemon_log()
    );
    assert!(procs(&layout.daemon_leaf_dir()).contains(&daemon_pid));
    // The reaper is in `daemon/` too.
    let reapers: Vec<u32> = procs(&layout.daemon_leaf_dir())
        .into_iter()
        .filter(|p| cmdline_of(*p).contains("reaper"))
        .collect();
    assert_eq!(reapers.len(), 1, "the reaper is in daemon/");

    // The agents scope and its holder.
    let holders: Vec<u32> = procs(&layout.holder_dir())
        .into_iter()
        .filter(|p| cmdline_of(*p).contains("daemon holder"))
        .collect();
    assert_eq!(holders.len(), 1, "one holder in holder/");
    let shown = systemd::show(
        &stack.env,
        &layout.agents_scope,
        &["ActiveState", "CPUWeight", "Delegate"],
    )
    .expect("show agents scope");
    assert_eq!(shown["ActiveState"], "active");
    assert_eq!(shown["CPUWeight"], "30");
    assert_eq!(shown["Delegate"], "yes");
    let daemon_unit = systemd::show(
        &stack.env,
        &layout.daemon_scope,
        &["CPUWeight", "ActiveState"],
    )
    .unwrap();
    assert_eq!(daemon_unit["CPUWeight"], "100");
    assert_eq!(daemon_unit["ActiveState"], "active");

    // A gate job: its process is in `jobs/<id>` while it runs.
    let mut run = stack.cmd(&["run", "probe"]).spawn().expect("sluice run");
    assert!(
        wait_for(Duration::from_secs(10), || {
            job_leaves(&layout).iter().any(|p| !procs(p).is_empty())
        }),
        "no job leaf with a process appeared; log:\n{}",
        stack.daemon_log()
    );
    let leaf = job_leaves(&layout)
        .into_iter()
        .find(|p| !procs(p).is_empty())
        .expect("busy job leaf");
    let members = procs(&leaf);
    assert!(
        members
            .iter()
            .any(|p| cmdline_of(*p).contains("sleep") || cmdline_of(*p).contains("sh -c")),
        "the job's processes are in its leaf: {members:?}"
    );
    assert!(run.wait().expect("run ends").success());
    let seen = fs::read_to_string(format!("{}.cg", stack.sb.counter.display())).unwrap();
    assert!(
        seen.contains(&format!("{}/jobs/", layout.daemon_scope)),
        "the gate saw its own cgroup: {seen}"
    );
    // The leaf is gone once the job ended.
    assert!(
        wait_for(Duration::from_secs(5), || {
            job_leaves(&layout).is_empty()
        }),
        "the job leaf was not pruned"
    );

    // An exec job goes through the same placement.
    let out = stack.run(&["exec", "--", "sh", "-c", "cat /proc/self/cgroup; sleep 0.3"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&format!("{}/jobs/", layout.daemon_scope)),
        "exec job cgroup: {text}"
    );
    assert!(
        wait_for(Duration::from_secs(5), || {
            job_leaves(&layout).is_empty()
        }),
        "the exec leaf was not pruned"
    );

    // Everything is stopped when the test ends.
    drop(stack);
}

#[test]
fn a_restart_reuses_the_agents_scope() {
    let Some(mut stack) = Stack::new("reuse", "", true) else {
        return;
    };
    let layout = stack.layout();
    let first = stack.start_daemon();
    let props = ["ControlGroup", "InvocationID"];
    let scope_before =
        systemd::show(&stack.env, &layout.agents_scope, &props).expect("agents scope exists");

    // A process of the test in a session keeps the scope alive when the
    // daemon (and its holder) goes away.
    let tree = stack.tree();
    let session = tree.ensure_session("s-reuse").expect("session");
    let kid = stack.sleeper();
    tree.move_pid(&session.tools, kid)
        .expect("move the sleeper");

    stack.stop_daemon();
    assert!(
        wait_for(Duration::from_secs(10), || {
            !Path::new(&format!("/proc/{first}")).exists() && procs(&layout.holder_dir()).is_empty()
        }),
        "the first daemon and its holder end together"
    );
    assert_eq!(procs(&session.tools.dir), [kid], "the session survives");

    let second = stack.start_daemon();
    assert_ne!(first, second);
    let scope_after =
        systemd::show(&stack.env, &layout.agents_scope, &props).expect("agents scope exists");
    assert_eq!(
        scope_before["ControlGroup"], scope_after["ControlGroup"],
        "the same scope"
    );
    assert_eq!(
        scope_before["InvocationID"], scope_after["InvocationID"],
        "reused, not recreated"
    );
    let holders = procs(&layout.holder_dir());
    assert_eq!(holders.len(), 1, "a fresh holder in holder/: {holders:?}");
    assert!(cmdline_of(holders[0]).contains("daemon holder"));
    assert_eq!(procs(&session.tools.dir), [kid], "the session is untouched");
    assert!(
        cgroup_of(second).ends_with(&format!("{}/daemon", layout.daemon_scope)),
        "the new daemon is placed again"
    );
    let scopes = stack
        .units()
        .into_iter()
        .filter(|u| u.contains(".scope"))
        .count();
    assert_eq!(scopes, 2, "exactly the two scopes");
}

#[test]
fn without_a_user_manager_the_daemon_runs_as_before() {
    // The daemon keeps its private, empty runtime directory: no manager
    // socket there, so detection says off and nothing is created.
    let Some(stack) = Stack::new("off", PROBE_GATE, false) else {
        return;
    };
    let daemon_pid = stack.start_daemon();
    assert!(
        !cgroup_of(daemon_pid).contains("sluice-test"),
        "not moved: {}",
        cgroup_of(daemon_pid)
    );
    let log = stack.daemon_log();
    assert!(log.contains("containment: off"), "log:\n{log}");

    let out = stack.run(&["run", "probe"]);
    assert!(out.status.success(), "{out:?}");
    let seen = fs::read_to_string(format!("{}.cg", stack.sb.counter.display())).unwrap();
    assert!(
        !seen.contains("/jobs/") && !seen.contains("sluice-test"),
        "no leaf without containment: {seen}"
    );
    assert!(stack.units().is_empty(), "{:?}", stack.units());
    // `status` says so, with the reason.
    let status = stack.status().expect("status");
    assert_eq!(status["containment"]["level"], "off", "{status}");
    assert!(status.get("sessions").is_none(), "{status}");
    let text = String::from_utf8_lossy(&stack.run(&["status"]).stdout).into_owned();
    assert!(text.contains("containment: off ("), "{text}");
}

#[test]
fn the_daemon_adopts_a_scope_the_hook_created_and_retires_its_stopgap_holder() {
    let Some(mut stack) = Stack::new("hook", "", true) else {
        return;
    };
    let layout = stack.layout();
    // What the session hook does when no daemon runs: a bounded holder,
    // `sluice daemon holder --lifetime <secs>`.
    let mut hook_holder = StdCommand::new(Sandbox::binary());
    hook_holder
        .arg0("sluice")
        .args(sluice_gate_daemon::containment::stopgap_holder_args())
        .env_clear()
        .stdin(Stdio::null());
    let stopgap = hook_holder.spawn().expect("the stopgap holder starts");
    let stopgap_pid = stopgap.id();
    stack.kids.push(stopgap);
    stack
        .tree()
        .ensure_agents_scope(stopgap_pid)
        .expect("hook-style scope");
    assert_eq!(procs(&layout.holder_dir()), [stopgap_pid]);

    let daemon = stack.start_daemon();
    assert!(
        cgroup_of(daemon).ends_with(&format!("{}/daemon", layout.daemon_scope)),
        "the daemon is placed next to the existing agents scope"
    );
    // The daemon's holder takes over and the stopgap goes away.
    assert!(
        wait_for(Duration::from_secs(10), || {
            let now = procs(&layout.holder_dir());
            now.len() == 1 && cmdline_of(now[0]).contains("daemon holder")
        }),
        "holder/ now: {:?}\nlog:\n{}",
        procs(&layout.holder_dir()),
        stack.daemon_log()
    );
    let shown = systemd::show(&stack.env, &layout.agents_scope, &["ActiveState"]).unwrap();
    assert_eq!(shown["ActiveState"], "active");
}

#[test]
fn containment_can_be_switched_off_in_the_config() {
    let Some(stack) = Stack::new("cfgoff", PROBE_GATE, true) else {
        return;
    };
    stack
        .sb
        .write_user_config("[containment]\nenabled = false\n");
    let daemon_pid = stack.start_daemon();
    assert!(
        !cgroup_of(daemon_pid).contains("sluice-test"),
        "not moved: {}",
        cgroup_of(daemon_pid)
    );
    let out = stack.run(&["run", "probe"]);
    assert!(out.status.success(), "{out:?}");
    let seen = fs::read_to_string(format!("{}.cg", stack.sb.counter.display())).unwrap();
    assert!(!seen.contains("/jobs/"), "no leaf when disabled: {seen}");
    assert!(stack.units().is_empty(), "{:?}", stack.units());
    let text = String::from_utf8_lossy(&stack.run(&["status"]).stdout).into_owned();
    assert!(
        text.contains("containment: off (disabled by [containment] enabled = false)"),
        "{text}"
    );
}

/// `status` shows the state and the live sessions from cgroupfs, and `doctor`
/// run from the session's `tools` leaf says where the shell and the agent
/// are. Only `sleep`s and the doctor's own shell are moved; no load.
#[test]
fn status_and_doctor_show_a_real_session() {
    let Some(mut stack) = Stack::new("obs", "", true) else {
        return;
    };
    let layout = stack.layout();
    stack.start_daemon();
    let tree = stack.tree();
    let session = tree.ensure_session("s-obsv").expect("session");
    let agent = stack.sleeper();
    let report = tree.move_pids(&session.ui, &[agent]);
    assert!(report.failed.is_empty(), "{:?}", report.failed);

    let status = stack.status().expect("status");
    assert_eq!(status["containment"]["level"], "full", "{status}");
    let sessions = status["sessions"].as_array().expect("sessions");
    let mine = sessions
        .iter()
        .find(|s| s["id"] == "obsv")
        .unwrap_or_else(|| panic!("{status}"));
    assert_eq!(mine["procs"], 1, "{mine}");
    assert!(mine["memory_bytes"].as_u64().is_some(), "{mine}");
    assert!(mine["cpu_seconds"].as_f64().is_some(), "{mine}");
    let text = String::from_utf8_lossy(&stack.run(&["status"]).stdout).into_owned();
    assert!(text.contains("containment: full\n"), "{text}");
    assert!(text.contains("  s-obsv  cpu "), "{text}");
    assert!(text.contains("procs 1\n"), "{text}");

    // doctor, run by a shell that first enters `tools` like the env line does.
    let tools = session.tools.dir.display().to_string();
    let mut cmd = StdCommand::new("sh");
    cmd.arg("-c")
        .arg(format!(
            "echo $$ > '{tools}/cgroup.procs'; exec '{}' doctor --json",
            Sandbox::binary().display()
        ))
        .current_dir(&stack.sb.repo)
        .env_clear()
        .envs(stack.envs())
        .env(
            "XDG_RUNTIME_DIR",
            stack.manager_runtime.clone().expect("manager"),
        )
        .env("CLAUDE_PID", agent.to_string())
        .env("SLUICE_SESSION_LEAF", &tools)
        .stdin(Stdio::null());
    let out = cmd.output().expect("doctor");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let checks = report["checks"].as_array().expect("checks");
    let find = |name: &str| {
        checks
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("no {name}: {checks:?}"))
    };
    let session_check = find("session");
    assert_eq!(session_check["group"], "containment");
    assert_eq!(session_check["status"], "ok", "{session_check}");
    let detail = session_check["detail"].as_str().unwrap();
    assert!(
        detail.contains("this shell is in tools (s-obsv/tools)"),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("agent (pid {agent}) is in ui")),
        "{detail}"
    );
    assert_eq!(find("containment")["detail"], "full");
    assert!(
        find("slice")["detail"]
            .as_str()
            .unwrap()
            .contains(&format!("{} present", layout.slice))
    );
    assert!(
        find("memory-policy")["detail"]
            .as_str()
            .unwrap()
            .starts_with("memory policy")
    );
}
