#![allow(clippy::assert_is_empty, clippy::too_many_lines)]
//! Integration tests of the containment core (task C1) against the real
//! systemd user manager.
//!
//! Safety rules: no CPU load and no memory pressure (only `sleep`s, no
//! `MemoryHigh`/`MemoryMax`); only processes started here are moved or
//! killed; every unit is named `sluice-test-<pid>*` and stopped by a drop
//! guard. Tests that need the user manager skip (printing why) when it is
//! unreachable, for example in CI or in a container.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sluice_gate_core::cgroup::detect::{self, Outcome, Placement};
use sluice_gate_core::cgroup::systemd;
use sluice_gate_core::cgroup::tree::{
    PlacedBy, Tree, TreeSettings, has_oomd_avoid, read_usage, write_control,
};
use sluice_gate_core::cgroup::{Capabilities, CgroupError, Layout};
use sluice_gate_core::config::{ContainmentConfig, EnvMap, process_env};

mod common;

const SLUICE: &str = env!("CARGO_BIN_EXE_sluice");

/// Kills and reaps the processes this test started.
struct Kids(Vec<Child>);

impl Kids {
    fn sleeper(&mut self) -> u32 {
        let child = Command::new("sleep")
            .arg("600")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep starts");
        let pid = child.id();
        self.0.push(child);
        pid
    }

    fn kill_all(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.0.clear();
    }
}

impl Drop for Kids {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// Stops the test units, whatever happens to the test.
struct UnitGuard {
    tree: Tree,
}

impl Drop for UnitGuard {
    fn drop(&mut self) {
        let _ = self.tree.stop_all();
        // Nothing the test started may outlive its units.
        let left = common::cgtree::kill_leftovers(&self.tree.layout);
        if !left.is_empty() {
            eprintln!("cleanup: processes outlived their units: {left:?}");
        }
    }
}

fn procs(dir: &Path) -> Vec<u32> {
    fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn cgroup_of(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap_or_default()
        .trim()
        .trim_start_matches("0::")
        .to_owned()
}

fn read(dir: &Path, file: &str) -> String {
    fs::read_to_string(dir.join(file))
        .unwrap_or_else(|e| panic!("{}/{file}: {e}", dir.display()))
        .trim()
        .to_owned()
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `None` (after printing why) when the full containment tree cannot be
/// built here.
fn capabilities(env: &EnvMap) -> Option<Capabilities> {
    let caps = detect::detect(env);
    if caps.outcome != Outcome::Full || caps.placement != Placement::SelfMoveOk {
        println!(
            "skipped: needs a full containment host with the test inside the user manager: {} ({:?})",
            caps.summary(),
            caps.placement
        );
        return None;
    }
    Some(caps)
}

#[test]
fn tree_lifecycle_on_the_user_manager() {
    let env = process_env();
    let Some(caps) = capabilities(&env) else {
        return;
    };
    let uid = caps.facts.uid;
    let prefix = format!("sluice-test-{}", std::process::id());
    let cfg = ContainmentConfig::default();
    let pressure = detect::pressure_policy(&env, &cfg, uid).unwrap();
    let settings = TreeSettings::from_config(&cfg, &caps, pressure, 4);
    let layout = Layout::with_prefix(Path::new("/sys/fs/cgroup"), uid, &prefix);
    let tree = Tree::new(layout.clone(), settings.clone(), env.clone());
    let _units = UnitGuard { tree: tree.clone() };
    // The holder and the "daemon" keep their scopes alive (a scope ends with
    // its last process); the others are the movable ones.
    let mut infra = Kids(Vec::new());
    let mut kids = Kids(Vec::new());

    // Slice and agents scope (holder adopted), daemon scope.
    let holder = infra.sleeper();
    tree.ensure_agents_scope(holder).expect("agents scope");
    let daemon = infra.sleeper();
    tree.ensure_daemon_scope(daemon).expect("daemon scope");
    // Idempotent: a second call with the same processes changes nothing.
    tree.ensure_agents_scope(holder)
        .expect("agents scope again");
    tree.ensure_daemon_scope(daemon)
        .expect("daemon scope again");

    assert_eq!(procs(&layout.holder_dir()), [holder]);
    assert_eq!(procs(&layout.daemon_leaf_dir()), [daemon]);
    assert!(procs(&layout.agents_scope_dir()).is_empty());
    assert!(read(&layout.agents_scope_dir(), "cgroup.subtree_control").contains("cpu"));
    assert!(layout.jobs_dir().join("cgroup.procs").exists());

    // The computed directories are where systemd put the units.
    let shown = systemd::show(
        &env,
        &layout.agents_scope,
        &[
            "ControlGroup",
            "CPUWeight",
            "Delegate",
            "ManagedOOMMemoryPressure",
            "ManagedOOMMemoryPressureLimit",
        ],
    )
    .unwrap();
    assert_eq!(
        Path::new("/sys/fs/cgroup").join(shown["ControlGroup"].trim_start_matches('/')),
        layout.agents_scope_dir()
    );
    assert_eq!(shown["CPUWeight"], "30");
    assert_eq!(shown["Delegate"], "yes");
    if caps.oomd_policy_enabled(&cfg) {
        assert_eq!(shown["ManagedOOMMemoryPressure"], "kill");
        let limit: u64 = shown["ManagedOOMMemoryPressureLimit"].parse().unwrap();
        let pct = systemd::fixed_to_percent(limit);
        assert!(
            (pct - f64::from(pressure.limit_percent)).abs() < 0.01,
            "{pct} vs {pressure:?}"
        );
    } else {
        assert_eq!(shown["ManagedOOMMemoryPressure"], "auto");
    }
    let slice = systemd::show(&env, &layout.slice, &["CPUWeight", "ControlGroup"]).unwrap();
    assert_eq!(slice["CPUWeight"], "50");
    assert_eq!(
        Path::new("/sys/fs/cgroup").join(slice["ControlGroup"].trim_start_matches('/')),
        layout.slice_dir()
    );
    let daemon_unit = systemd::show(&env, &layout.daemon_scope, &["CPUWeight"]).unwrap();
    assert_eq!(daemon_unit["CPUWeight"], "100");

    // A session: leaves, weights, pids.max, the xattr.
    let session = tree.ensure_session("s-test1").expect("session");
    assert!(session.controllers.cpu && session.controllers.memory && session.controllers.pids);
    assert_eq!(read(&session.ui.dir, "cpu.weight"), "400");
    assert_eq!(read(&session.tools.dir, "cpu.weight"), "25");
    assert_eq!(read(&session.dir, "pids.max"), "4096");
    assert_eq!(
        read(&session.dir, "memory.max"),
        "max",
        "no memory limit by default"
    );
    assert_eq!(
        read(&session.dir, "memory.high"),
        "max",
        "no memory limit by default"
    );
    if has_oomd_avoid(&session.ui.dir) {
        assert!(!has_oomd_avoid(&session.tools.dir));
    } else {
        println!("user.oomd_avoid not supported here: skipped the xattr assertion");
    }

    // Weights and pids.max can be changed and read back (a second call of
    // ensure_session applies the new settings to the existing session).
    let mut changed = settings.clone();
    changed.ui_weight = 200;
    changed.tools_weight = 50;
    changed.pids_max = Some(100);
    let again = Tree::new(layout.clone(), changed, env.clone())
        .ensure_session("s-test1")
        .expect("session again");
    assert_eq!(again.ui.dir, session.ui.dir);
    assert_eq!(read(&session.ui.dir, "cpu.weight"), "200");
    assert_eq!(read(&session.tools.dir, "cpu.weight"), "50");
    assert_eq!(read(&session.dir, "pids.max"), "100");
    write_control(&session.tools.dir, "cpu.weight", "25").unwrap();
    assert_eq!(read(&session.tools.dir, "cpu.weight"), "25");

    // Direct move of a process this test started.
    let direct = kids.sleeper();
    assert_eq!(
        tree.move_pid(&session.tools, direct).unwrap(),
        PlacedBy::Direct
    );
    assert!(
        cgroup_of(direct).ends_with("/s-test1/tools"),
        "{}",
        cgroup_of(direct)
    );
    assert_eq!(procs(&session.tools.dir), [direct]);

    // cg-exec: the command is in the leaf from its first instruction.
    let mut exec = Command::new(SLUICE)
        .arg("cg-exec")
        .arg(&session.ui.dir)
        .args(["--", "sleep", "600"])
        .stdin(Stdio::null())
        .spawn()
        .expect("sluice cg-exec starts");
    let exec_pid = exec.id();
    wait_until("cg-exec to exec sleep", || {
        fs::read_to_string(format!("/proc/{exec_pid}/comm")).is_ok_and(|c| c.trim() == "sleep")
    });
    assert!(
        cgroup_of(exec_pid).ends_with("/s-test1/ui"),
        "{}",
        cgroup_of(exec_pid)
    );
    assert_eq!(procs(&session.ui.dir), [exec_pid]);

    // AttachProcessesToUnit works for a delegated leaf, and says no for an
    // inner node.
    let attached = kids.sleeper();
    systemd::attach_processes(&env, &layout.agents_scope, "/s-test1/tools", &[attached])
        .expect("attach to the tools leaf");
    assert!(cgroup_of(attached).ends_with("/s-test1/tools"));
    let refused =
        systemd::attach_processes(&env, &layout.agents_scope, "/s-test1", &[attached]).unwrap_err();
    assert!(matches!(refused, CgroupError::Command { .. }), "{refused}");
    assert!(cgroup_of(attached).ends_with("/s-test1/tools"));

    // move_pids reports what happened to each process, including one that
    // does not exist (a process that exited meanwhile).
    let more = kids.sleeper();
    let report = tree.move_pids(&session.ui, &[more, 0x7fff_fff0]);
    assert_eq!(report.direct, [more]);
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);

    // A job leaf of the daemon scope.
    let job = tree.job_leaf("j-test").expect("job leaf");
    let job_pid = kids.sleeper();
    tree.move_pid(&job, job_pid).unwrap();
    assert!(cgroup_of(job_pid).ends_with("/jobs/j-test"));

    // Usage readings.
    let usage = tree.usage(&session.dir);
    assert!(usage.cpu_usec.is_some());
    assert!(usage.memory_current.is_some());
    assert_eq!(usage.pids_current, Some(4), "{usage:?}");
    assert_eq!(usage.memory_events.oom_kill, 0);
    let tools_usage = read_usage(&session.tools.dir);
    assert_eq!(tools_usage.pids_current, Some(2));

    // Pruning: nothing while processes live, everything once they are gone.
    let r = tree.prune(Duration::ZERO);
    assert_eq!((r.sessions, r.jobs), (0, 0), "{r:?}");
    assert_eq!(r.busy, 2);
    assert!(session.dir.exists());
    let _ = exec.kill();
    let _ = exec.wait();
    kids.kill_all();
    wait_until("the test processes to leave their cgroups", || {
        procs(&session.dir.join("ui")).is_empty()
            && procs(&session.dir.join("tools")).is_empty()
            && procs(&layout.jobs_dir().join("j-test")).is_empty()
    });
    // The holder and the daemon still run, so the scopes stay; sessions and
    // jobs are pruned.
    let r = tree.prune(Duration::ZERO);
    assert_eq!((r.sessions, r.jobs, r.busy), (1, 1, 0), "{r:?}");
    assert!(!session.dir.exists());
    assert!(layout.holder_dir().exists(), "holder is not pruned");

    // Cleanup leaves nothing behind.
    tree.stop_all().expect("stop units");
    wait_until("the test units to disappear", || {
        systemd::list_units(&env, &format!("{prefix}*"))
            .unwrap()
            .is_empty()
    });
}

#[test]
fn unit_names_of_failed_creation_do_not_leak() {
    // A scope without a live pid is refused by the manager: the error is
    // typed and no unit is left.
    let env = process_env();
    let Some(caps) = capabilities(&env) else {
        return;
    };
    let prefix = format!("sluice-test-{}-neg", std::process::id());
    let layout = Layout::with_prefix(Path::new("/sys/fs/cgroup"), caps.facts.uid, &prefix);
    let cfg = ContainmentConfig::default();
    let settings = TreeSettings::from_config(
        &cfg,
        &caps,
        detect::pressure_policy(&env, &cfg, caps.facts.uid).unwrap(),
        4,
    );
    let tree = Tree::new(layout, settings, env.clone());
    let _units = UnitGuard { tree: tree.clone() };
    let err = tree.ensure_agents_scope(0x7fff_fff0).unwrap_err();
    assert!(
        matches!(err, CgroupError::Command { .. } | CgroupError::Io { .. }),
        "{err}"
    );
    tree.stop_all().unwrap();
    assert!(
        systemd::list_units(&env, &format!("{prefix}*"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn fail_open_without_a_user_manager() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = process_env();
    env.insert("XDG_RUNTIME_DIR".into(), dir.path().display().to_string());
    env.remove("DBUS_SESSION_BUS_ADDRESS");
    let caps = detect::detect(&env);
    assert!(!caps.usable(), "{}", caps.summary());
    let cfg = ContainmentConfig::default();
    let settings = TreeSettings::from_config(
        &cfg,
        &caps,
        detect::derive_pressure_policy(&detect::AncestorPolicy::default()),
        4,
    );
    let tree = Tree::new(
        Layout::with_prefix(
            Path::new("/sys/fs/cgroup"),
            caps.facts.uid,
            "sluice-test-nomanager",
        ),
        settings,
        env,
    );
    // Typed errors, no panic, no hang.
    assert!(tree.ensure_slice().is_err());
    assert!(tree.ensure_agents_scope(std::process::id()).is_err());
    assert!(tree.ensure_session("s-x").is_err());
}

#[test]
fn cg_exec_runs_the_command_even_when_the_move_fails() {
    // Missing leaf.
    let missing = Command::new(SLUICE)
        .args(["cg-exec", "/nonexistent/leaf", "--", "sh", "-c", "exit 7"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(7));

    // Unwritable leaf. A 0444 file is still writable for root (CI containers
    // run tests as root), so the target is a directory named `cgroup.procs`:
    // opening it for writing fails for everyone.
    let dir = tempfile::tempdir().unwrap();
    let procs_file = dir.path().join("cgroup.procs");
    fs::create_dir(&procs_file).unwrap();
    let out = Command::new(SLUICE)
        .arg("cg-exec")
        .arg(dir.path())
        .args(["--", "sh", "-c", "echo ran"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ran");
    assert!(
        procs_file.is_dir() && fs::read_dir(&procs_file).unwrap().next().is_none(),
        "nothing written"
    );

    // The reason is logged at debug level only.
    let quiet = Command::new(SLUICE)
        .args(["cg-exec", "/nonexistent/leaf", "--", "true"])
        .env_remove("SLUICE_LOG")
        .output()
        .unwrap();
    assert!(
        quiet.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&quiet.stderr)
    );
    let noisy = Command::new(SLUICE)
        .args(["cg-exec", "/nonexistent/leaf", "--", "true"])
        .env("SLUICE_LOG", "debug")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&noisy.stderr).contains("not moved into /nonexistent/leaf"),
        "{}",
        String::from_utf8_lossy(&noisy.stderr)
    );

    // A missing command is the shell's 127; no command at all is a usage error.
    let nf = Command::new(SLUICE)
        .args(["cg-exec", "/nonexistent/leaf", "--", "/nonexistent/cmd"])
        .output()
        .unwrap();
    assert_eq!(nf.status.code(), Some(127));
    let usage = Command::new(SLUICE)
        .args(["cg-exec", "/x"])
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
}

/// `oom_score_adj` of this test process, which `cg-exec` children inherit.
fn inherited_oom_score_adj() -> i64 {
    fs::read_to_string("/proc/self/oom_score_adj")
        .expect("procfs")
        .trim()
        .parse()
        .expect("a number")
}

/// What `sluice cg-exec <leaf> [--oom-score-adj N] -- cat /proc/self/oom_score_adj` prints.
fn adj_seen_by_child(leaf: &Path, adj: Option<&str>) -> i64 {
    let mut cmd = Command::new(SLUICE);
    cmd.arg("cg-exec").arg(leaf);
    if let Some(adj) = adj {
        cmd.args(["--oom-score-adj", adj]);
    }
    let out = cmd
        .args(["--", "cat", "/proc/self/oom_score_adj"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

#[test]
fn cg_exec_raises_the_oom_score_adj_and_never_lowers_it() {
    let inherited = inherited_oom_score_adj();
    // An unwritable leaf (a directory named `cgroup.procs`) does not stop
    // the adj: both steps fail open, independently.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("cgroup.procs")).unwrap();
    assert_eq!(
        adj_seen_by_child(dir.path(), Some("800")),
        inherited.max(800)
    );
    // A missing leaf, and a value below the inherited one: the inherited
    // value stays (an unprivileged process cannot lower its adj).
    assert_eq!(
        adj_seen_by_child(Path::new("/nonexistent/leaf"), Some("0")),
        inherited.max(0)
    );
    // The no-leaf spelling used when containment is unavailable.
    assert_eq!(
        adj_seen_by_child(Path::new("-"), Some("800")),
        inherited.max(800)
    );
    // Without the option nothing is touched.
    assert_eq!(adj_seen_by_child(Path::new("-"), None), inherited);
    // Out of range is a usage error, not a silent clamp.
    let bad = Command::new(SLUICE)
        .args(["cg-exec", "-", "--oom-score-adj", "1001", "--", "true"])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn hidden_from_help() {
    let out = Command::new(SLUICE).arg("--help").output().unwrap();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("cg-exec"));
}
