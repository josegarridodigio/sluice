//! Socket path fallback: a runtime directory too deep for `sun_path` moves
//! the daemon socket to the short `/tmp/sluice-<uid>/daemon.sock`.
//!
//! The tests never touch the real `/tmp/sluice-<uid>`: the test-only
//! `SLUICE_TEST_SOCKET_FALLBACK_DIR` points the fallback base at a short
//! temporary directory of their own (`/tmp/sa-*`).

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use common::Sandbox;
use predicates::prelude::*;
use tempfile::TempDir;

const FALLBACK_ENV: &str = "SLUICE_TEST_SOCKET_FALLBACK_DIR";

/// A sandbox whose `XDG_RUNTIME_DIR` is longer than 120 bytes, with the
/// socket fallback base redirected to a short private temporary directory.
struct LongRuntime {
    sb: Sandbox,
    runtime: PathBuf,
    base: TempDir,
}

impl LongRuntime {
    fn new(config: &str) -> Self {
        let sb = Sandbox::new(config);
        let runtime = sb.home.join(format!("runtime-{}", "x".repeat(120)));
        fs::create_dir_all(&runtime).expect("mkdir runtime");
        assert!(runtime.as_os_str().len() > 120);
        let base = tempfile::Builder::new()
            .prefix("sa-")
            .tempdir_in("/tmp")
            .expect("short tempdir");
        Self { sb, runtime, base }
    }

    fn envs(&self) -> [(&'static str, String); 2] {
        [
            ("XDG_RUNTIME_DIR", self.runtime.display().to_string()),
            (FALLBACK_ENV, self.base.path().display().to_string()),
        ]
    }

    fn sluice(&self) -> Command {
        let mut cmd = self.sb.sluice();
        cmd.envs(self.envs());
        cmd
    }

    fn fallback_dir(&self) -> PathBuf {
        let uid = fs::metadata("/proc/self").expect("uid").uid();
        self.base.path().join(format!("sluice-{uid}"))
    }

    fn socket(&self) -> PathBuf {
        self.fallback_dir().join("daemon.sock")
    }

    fn status_json(&self) -> serde_json::Value {
        let out = self
            .sluice()
            .args(["status", "--json"])
            .output()
            .expect("status");
        assert!(out.status.success(), "status failed: {out:?}");
        serde_json::from_slice(&out.stdout).expect("status --json is JSON")
    }

    fn doctor_check(&self, name: &str) -> serde_json::Value {
        let out = self
            .sluice()
            .args(["doctor", "--json"])
            .output()
            .expect("doctor");
        let report: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("doctor --json is JSON");
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no check {name} in {report}"))
    }
}

impl Drop for LongRuntime {
    fn drop(&mut self) {
        if self.socket().exists() {
            let _ = StdCommand::new(Sandbox::binary())
                .args(["daemon", "stop", "--force"])
                .current_dir(&self.sb.home)
                .env_clear()
                .envs(self.sb.env())
                .envs(self.envs())
                .output();
        }
    }
}

fn wait_for(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    cond()
}

#[test]
fn a_long_runtime_dir_goes_through_the_daemon_on_the_fallback_socket() {
    let lr = LongRuntime::new(
        r#"
[gates.test]
cmd = "sleep 1; echo run >> \"$SLUICE_TEST_COUNTER\""
"#,
    );
    let spawn = || {
        StdCommand::new(Sandbox::binary())
            .args(["run", "test"])
            .current_dir(&lr.sb.repo)
            .env_clear()
            .envs(lr.sb.env())
            .envs(lr.envs())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sluice run")
    };
    let first = spawn();
    assert!(
        wait_for(Duration::from_secs(5), || lr.socket().exists()),
        "the daemon must listen on the fallback socket {}",
        lr.socket().display()
    );
    let second = spawn();
    assert!(
        wait_for(Duration::from_secs(5), || {
            lr.status_json()["running"]
                .as_array()
                .is_some_and(|r| r.iter().any(|j| j["gate"] == "test"))
        }),
        "status must show the running job"
    );
    for child in [first, second] {
        let out = child.wait_with_output().expect("wait");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{out:?}");
        assert!(!stderr.contains("in-process"), "{stderr}");
        assert!(stderr.contains("queued (position"), "{stderr}");
    }
    assert_eq!(lr.sb.runs(), 1, "two identical runs must coalesce");

    let mode = fs::metadata(lr.fallback_dir())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    let status = lr.status_json();
    assert_eq!(
        status["daemon"]["socket"],
        lr.socket().display().to_string()
    );
    lr.sluice().arg("status").assert().success().stdout(
        predicate::str::contains(lr.socket().display().to_string())
            .and(predicate::str::contains("socket:   fallback in use")),
    );
    let log = fs::read_to_string(lr.sb.home.join("state/sluice/daemon.log")).unwrap();
    assert!(
        log.contains("socket path fallback in use")
            && log.contains(&lr.socket().display().to_string()),
        "{log}"
    );

    let check = lr.doctor_check("socket-path");
    assert_eq!(check["status"], "ok", "{check}");
    let detail = check["detail"].as_str().unwrap();
    assert!(
        detail.contains("fallback in use") && detail.contains("107-byte"),
        "{detail}"
    );
    assert_eq!(lr.doctor_check("daemon")["status"], "ok");
}

#[test]
fn a_world_writable_fallback_dir_is_refused() {
    let lr = LongRuntime::new("[gates.test]\ncmd = \"echo run >> \\\"$SLUICE_TEST_COUNTER\\\"\"\n");
    let dir = lr.fallback_dir();
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    lr.sluice().args(["run", "test"]).assert().code(1).stderr(
        predicate::str::contains("group or world writable")
            .and(predicate::str::contains(dir.display().to_string()))
            .and(predicate::str::contains("in-process").not()),
    );
    assert_eq!(lr.sb.runs(), 0, "no silent in-process run");
    assert!(!lr.socket().exists());
    let check = lr.doctor_check("socket-path");
    assert_eq!(check["status"], "fail", "{check}");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .contains("group or world writable")
    );
    let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o777, "the refused directory is left untouched");
}

#[test]
fn a_symlinked_fallback_dir_is_refused() {
    let lr = LongRuntime::new("[gates.test]\ncmd = \"true\"\n");
    let target = lr.base.path().join("elsewhere");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&target, lr.fallback_dir()).unwrap();
    lr.sluice().args(["run", "test"]).assert().code(1).stderr(
        predicate::str::contains("symlink").and(predicate::str::contains(
            lr.fallback_dir().display().to_string(),
        )),
    );
    lr.sluice()
        .args(["daemon", "--foreground"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("symlink"));
    assert!(!target.join("daemon.sock").exists());
}

#[test]
fn an_over_long_sluice_socket_is_a_clear_error() {
    let sb = Sandbox::standard();
    let long = format!("/tmp/{}/d.sock", "s".repeat(110));
    sb.sluice()
        .env("SLUICE_SOCKET", &long)
        .args(["run", "test"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("SLUICE_SOCKET is")
                .and(predicate::str::contains("107-byte"))
                .and(predicate::str::contains("in-process").not()),
        );
    assert_eq!(sb.runs(), 0);
    let out = sb
        .sluice()
        .env("SLUICE_SOCKET", &long)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "socket-path")
        .unwrap();
    assert_eq!(check["status"], "fail", "{check}");
    let detail = check["detail"].as_str().unwrap();
    assert!(
        detail.contains("SLUICE_SOCKET is") && detail.contains("longer than"),
        "{detail}"
    );
}

/// Alpha review finding 12: `doctor` (checks and bundle) never connects to
/// a socket planted in an unsafe fallback directory.
#[test]
fn doctor_never_connects_through_an_unsafe_fallback_dir() {
    let lr = LongRuntime::new("[gates.test]\ncmd = \"true\"\n");
    let dir = lr.fallback_dir();
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    let planted = std::os::unix::net::UnixListener::bind(lr.socket()).unwrap();
    planted.set_nonblocking(true).unwrap();
    let check = lr.doctor_check("daemon");
    assert_eq!(check["status"], "fail", "{check}");
    assert!(
        check["detail"].as_str().unwrap().contains("not contacted"),
        "{check}"
    );
    let out_dir = tempfile::tempdir().unwrap();
    let bundle = out_dir.path().join("b.tar.gz");
    let out = lr
        .sluice()
        .args(["doctor", "--bundle"])
        .arg(&bundle)
        .output()
        .expect("doctor --bundle");
    assert!(bundle.exists(), "{out:?}");
    match planted.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("doctor connected to the planted socket: {other:?}"),
    }
}
