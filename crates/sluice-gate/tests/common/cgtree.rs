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
        Self::build(true)
    }

    /// The tree without its agents scope: the session hook has to create it
    /// (and its stopgap holder) itself.
    pub fn bare() -> Option<Self> {
        Self::build(false)
    }

    fn build(with_scope: bool) -> Option<Self> {
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
        if with_scope {
            let holder = this.sleeper().id();
            this.tree.ensure_agents_scope(holder).expect("agents scope");
        }
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
        // Whatever is still in the test's own tree now outlived its unit.
        let left = kill_leftovers(&self.layout);
        if !left.is_empty() {
            eprintln!(
                "cleanup: processes outlived {}: {left:?}",
                self.layout.slice
            );
        }
    }
}

/// Every `(cgroup dir, pid)` listed in `cgroup.procs` of `root` and its
/// descendants.
pub fn procs_under(root: &Path) -> Vec<(PathBuf, u32)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(text) = fs::read_to_string(dir.join("cgroup.procs")) {
            out.extend(
                text.lines()
                    .filter_map(|l| l.trim().parse::<u32>().ok())
                    .map(|p| (dir.clone(), p)),
            );
        }
        if let Ok(entries) = fs::read_dir(&dir) {
            stack.extend(
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path()),
            );
        }
    }
    out
}

/// `true` for a zombie (it only waits for its parent; nothing to kill).
fn is_zombie(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|t| {
            t.rfind(')')
                .map(|i| t[i + 1..].trim_start().starts_with('Z'))
        })
        .unwrap_or(true)
}

/// The command line of a process, words joined by spaces.
pub fn cmdline_of(pid: u32) -> String {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            String::from_utf8_lossy(&b)
                .trim_end_matches('\0')
                .replace('\0', " ")
        })
        .unwrap_or_default()
}

/// Kills (SIGKILL) the live processes left in the test's own tree (only a
/// layout named `sluice-test-*` is ever touched, and only paths below its
/// slice directory) and waits for them to go. Returns what it had to kill as
/// `"<pid> <command line>"`.
pub fn kill_leftovers(layout: &Layout) -> Vec<String> {
    assert!(
        layout.slice.starts_with("sluice-test-"),
        "never clean up a production tree: {}",
        layout.slice
    );
    let root = layout.slice_dir();
    let mut killed = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let alive: Vec<u32> = procs_under(&root)
            .into_iter()
            .map(|(_, p)| p)
            .filter(|p| !is_zombie(*p))
            .collect();
        if alive.is_empty() || Instant::now() >= deadline {
            return killed;
        }
        for pid in alive {
            let line = format!("{pid} {}", cmdline_of(pid));
            if let Ok(raw) = i32::try_from(pid) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(raw),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            if !killed.contains(&line) {
                killed.push(line);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Live processes still in the test's tree (nothing is killed).
pub fn live_leftovers(layout: &Layout) -> Vec<String> {
    procs_under(&layout.slice_dir())
        .into_iter()
        .filter(|(_, p)| !is_zombie(*p))
        .map(|(d, p)| format!("{p} {} in {}", cmdline_of(p), d.display()))
        .collect()
}

/// `/proc/<pid>/cgroup` without the `0::` prefix.
pub fn cgroup_of(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .unwrap_or_default()
        .trim()
        .trim_start_matches("0::")
        .to_owned()
}
