//! A disposable containment tree on the real systemd user manager, for the
//! tests of the session hook and the shims (tasks C3 and C4).
//!
//! Safety: no CPU load, no memory limit or pressure; every unit is named
//! `sluice-test-<pid>*` and stopped by `Drop`; only processes the test
//! started are moved. Tests skip (printing why) when the host is not a full
//! containment host (CI, containers, no user manager).
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sluice_gate_core::cgroup::detect::{self, Outcome, Placement};
use sluice_gate_core::cgroup::tree::{Tree, TreeSettings};
use sluice_gate_core::cgroup::{Layout, session_dir_name};
use sluice_gate_core::config::{ContainmentConfig, process_env};

/// The test tree: `sluice-test-<pid>.slice` with its agents scope.
pub struct TestTree {
    pub tree: Tree,
    pub layout: Layout,
    pub prefix: String,
    /// The user manager's runtime directory (the sandbox fakes its own).
    pub runtime_dir: String,
    kids: Vec<Child>,
}

impl TestTree {
    /// The tree with its agents scope, or `None` (after printing why).
    pub fn new() -> Option<Self> {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let env = process_env();
        let caps = detect::detect(&env);
        if caps.outcome != Outcome::Full || caps.placement != Placement::SelfMoveOk {
            println!(
                "skipped: needs a full containment host with the test inside the user manager: {} ({:?})",
                caps.summary(),
                caps.placement
            );
            return None;
        }
        let uid = caps.facts.uid;
        // One prefix per tree: tests of one process run in parallel and must
        // not share (or stop) each other's units.
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let prefix = format!("sluice-test-{}-t{n}", std::process::id());
        let cfg = ContainmentConfig::default();
        let pressure = detect::pressure_policy(&env, &cfg, uid).ok()?;
        let settings = TreeSettings::from_config(&cfg, &caps, pressure, 4);
        let layout = Layout::with_prefix(Path::new("/sys/fs/cgroup"), uid, &prefix);
        let tree = Tree::new(layout.clone(), settings, env.clone());
        let runtime_dir = detect::runtime_dir(&env, uid).display().to_string();
        let mut this = Self {
            tree,
            layout,
            prefix,
            runtime_dir,
            kids: Vec::new(),
        };
        let holder = this.sleeper().id();
        this.tree.ensure_agents_scope(holder).expect("agents scope");
        Some(this)
    }

    /// Starts a `sleep` that `Drop` kills.
    pub fn sleeper(&mut self) -> &Child {
        let child = Command::new("sleep")
            .arg("120")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep starts");
        self.kids.push(child);
        self.kids.last().unwrap()
    }

    /// Starts a fake agent: a shell with one `sleep` child.
    pub fn fake_agent(&mut self) -> (u32, u32) {
        let child = Command::new("sh")
            .args(["-c", "sleep 120 & wait"])
            .stdin(Stdio::null())
            .spawn()
            .expect("sh starts");
        let agent = child.id();
        self.kids.push(child);
        let children = format!("/proc/{agent}/task/{agent}/children");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = fs::read_to_string(&children)
                .ok()
                .and_then(|t| t.split_whitespace().next().and_then(|p| p.parse().ok()))
            {
                return (agent, pid);
            }
            assert!(Instant::now() < deadline, "the fake agent's child");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Cgroup directory of the session `id` (`s-<id>`).
    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.layout.session_dir(&session_dir_name(Some(id), 0))
    }

    /// Environment entries that point a command at this tree.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("XDG_RUNTIME_DIR", self.runtime_dir.clone()),
            ("SLUICE_TEST_CONTAINMENT_PREFIX", self.prefix.clone()),
        ]
    }
}

impl Drop for TestTree {
    fn drop(&mut self) {
        // The scope's own stop kills what the hook moved into it.
        let _ = self.tree.stop_all();
        for c in &mut self.kids {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// `/proc/<pid>/cgroup` without the `0::` prefix.
pub fn cgroup_of(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap_or_default()
        .trim()
        .trim_start_matches("0::")
        .to_owned()
}
