//! Chaos regressions found by the alpha harness (`scripts/alpha/s4_chaos.py`):
//! a daemon killed by SIGKILL, unwritable logs and what `doctor` reports about
//! damaged storage.

mod common;

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use predicates::prelude::*;

/// Generous: the suite runs on loaded shared machines.
const LIMIT: Duration = Duration::from_secs(20);

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

/// Fields after the command name of `/proc/<pid>/stat` (index 0 is the state).
fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_owned).collect())
}

/// Live (non-zombie) processes whose command line contains `pattern`.
fn live(pattern: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmd) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
        let zombie = stat_fields(pid).is_none_or(|f| f.first().is_some_and(|s| s == "Z"));
        if cmd.contains(pattern) && !zombie && pid != std::process::id() {
            out.push(pid);
        }
    }
    out
}

/// The gate reaper of daemon `daemon` (`sluice daemon reaper --daemon <pid>`).
fn reaper_of(daemon: u32) -> Option<u32> {
    live(&format!("daemon reaper --daemon {daemon} "))
        .into_iter()
        .next()
}

fn sigkill(pid: u32) {
    let _ = kill(Pid::from_raw(i32::try_from(pid).unwrap()), Signal::SIGKILL);
}

/// A `sluice run` client whose stderr is collected while it runs, so a test
/// can wait for a progress line (`<gate> running (job …)`).
struct Client {
    child: Child,
    stderr: Arc<Mutex<String>>,
    reader: thread::JoinHandle<()>,
}

impl Client {
    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Exit status, stdout and the complete stderr.
    fn finish(mut self) -> (ExitStatus, String, String) {
        let mut stdout = String::new();
        if let Some(mut out) = self.child.stdout.take() {
            out.read_to_string(&mut stdout).unwrap();
        }
        let status = self.child.wait().unwrap();
        self.reader.join().unwrap();
        let stderr = self.stderr.lock().unwrap().clone();
        (status, stdout, stderr)
    }
}

fn spawn_run(sb: &Sandbox, gate: &str, path: Option<&str>) -> Client {
    let mut cmd = StdCommand::new(Sandbox::binary());
    cmd.args(["run", gate])
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    let mut child = cmd.spawn().expect("spawn sluice run");
    let mut pipe = child.stderr.take().unwrap();
    let stderr = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&stderr);
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    Client {
        child,
        stderr,
        reader,
    }
}

fn daemon_pid(sb: &Sandbox) -> u32 {
    let pid = sb.status_json()["daemon"]["pid"]
        .as_u64()
        .expect("a daemon");
    u32::try_from(pid).unwrap()
}

/// Starts `sluice run long` for a gate that forks a background `sleep`
/// next to a foreground one (`marker` is part of both command lines) and
/// waits until all three processes (`sh` and both sleeps) run and the
/// client was told the job started (the daemon announces a start only
/// after it registered the group with its reaper and recorded it in the
/// store, so the test never kills the daemon in the middle of a spawn);
/// returns the client and the daemon pid.
fn start_forking_gate(sb: &Sandbox, marker: &str) -> (Client, u32) {
    let client = spawn_run(sb, "long", None);
    assert!(
        wait_for(LIMIT, || live(&format!("sleep {marker}")).len() == 3
            && client.stderr().contains("sluice: long running (job")),
        "the gate and its child never started: {}",
        client.stderr()
    );
    (client, daemon_pid(sb))
}

/// PSI admission is off: these tests are about daemon death, and on a
/// loaded machine CPU pressure would keep the gate queued past `LIMIT`.
fn forking_gate(marker: &str) -> Sandbox {
    let sb = Sandbox::new(&format!(
        "[gates.long]\ncmd = \"sleep {marker}1 & sleep {marker}2; wait\"\n"
    ));
    fs::write(
        sb.home.join(".config/sluice/config.toml"),
        "version = 1\n[admission]\npsi_enabled = false\n",
    )
    .unwrap();
    sb
}

#[test]
fn a_sigkilled_daemon_takes_its_gates_and_their_children_with_it() {
    let sb = forking_gate("314.2");
    let (client, daemon) = start_forking_gate(&sb, "314.2");
    let reaper = reaper_of(daemon).expect("the daemon starts a reaper");
    sigkill(daemon);
    assert!(
        wait_for(LIMIT, || live("sleep 314.2").is_empty()),
        "gate processes survived the daemon: {:?}",
        live("sleep 314.2")
    );
    assert!(
        wait_for(LIMIT, || stat_fields(reaper)
            .is_none_or(|f| f.first().is_some_and(|s| s == "Z"))),
        "the reaper exits once it cleaned up"
    );
    let (status, stdout, stderr) = client.finish();
    assert_eq!(status.code(), Some(143), "{stderr}");
    assert!(
        stderr.contains("the daemon died while running long; it was killed"),
        "{stderr}"
    );
    assert!(!stdout.contains("PASS"));
    assert_eq!(sb.marker_count(), 0, "an interrupted run is never cached");
}

/// Regression (CI flake of the test above): the daemon used to register a
/// gate with its reaper only after `renice` and `ionice` had run on it, and
/// a daemon killed while those helpers ran left the gate behind. A slow
/// `ionice` shim holds the daemon in that window; the gate, already
/// running, must still be reaped.
#[test]
fn a_daemon_killed_while_it_sets_gate_priorities_still_reaps_the_gate() {
    let sb = forking_gate("316.2");
    let shims = sb.root.path().join("slow-ionice-bin");
    fs::create_dir_all(&shims).unwrap();
    let shim = shims.join("ionice");
    fs::write(&shim, "#!/bin/sh\n# slow-ionice-316\nsleep 3\n").unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        shims.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let client = spawn_run(&sb, "long", Some(&path));
    let shim_cmd = shim.display().to_string();
    assert!(
        wait_for(LIMIT, || live("sleep 316.2").len() == 3
            && !live(&shim_cmd).is_empty()),
        "the gate never started next to the slow ionice: {}",
        client.stderr()
    );
    let daemon = daemon_pid(&sb);
    reaper_of(daemon).expect("the daemon starts a reaper");
    sigkill(daemon);
    assert!(
        wait_for(LIMIT, || live("sleep 316.2").is_empty()),
        "gate processes survived the daemon: {:?}",
        live("sleep 316.2")
    );
    let (status, _, stderr) = client.finish();
    assert_eq!(status.code(), Some(143), "{stderr}");
    assert!(wait_for(LIMIT, || live(&shim_cmd).is_empty()));
}

#[test]
fn a_restarted_daemon_kills_the_gates_a_dead_daemon_left_behind() {
    let sb = forking_gate("315.2");
    let (client, daemon) = start_forking_gate(&sb, "315.2");
    // Without its reaper the dead daemon's gates live on until the next
    // daemon reconciles them.
    let reaper = reaper_of(daemon).expect("a reaper");
    sigkill(reaper);
    assert!(wait_for(LIMIT, || reaper_of(daemon).is_none()));
    sigkill(daemon);
    let (status, _, stderr) = client.finish();
    assert_eq!(status.code(), Some(143), "{stderr}");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        live("sleep 315.2").len(),
        3,
        "the orphaned gate must still be running before the restart"
    );
    sb.sluice().arg("daemon").assert().success();
    assert!(
        wait_for(LIMIT, || live("sleep 315.2").is_empty()),
        "the restarted daemon must kill the orphaned group: {:?}",
        live("sleep 315.2")
    );
    // The row is closed right after the group is gone.
    let db = sb.home.join("state/sluice/sluice.db");
    let newest = || {
        sluice_gate_daemon::store::Store::open_read_only(&db)
            .ok()?
            .newest_job()
            .ok()?
    };
    assert!(
        wait_for(LIMIT, || newest().is_some_and(|r| r.outcome.is_some())),
        "the interrupted job is closed: {:?}",
        newest()
    );
    let row = newest().expect("the interrupted job");
    assert_eq!(row.gate, "long");
    assert_eq!(row.outcome.as_deref(), Some("killed"));
    let status = sb.status_json();
    assert_eq!(status["running"].as_array().map(Vec::len), Some(0));
}

/// Makes `dir` read-only for the test's duration; `None` when running as
/// root (permissions are not enforced, nothing to prove).
struct ReadOnly(PathBuf);

impl ReadOnly {
    fn new(dir: &Path) -> Option<Self> {
        fs::create_dir_all(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).unwrap();
        let guard = Self(dir.to_path_buf());
        fs::write(dir.join("probe"), "").is_err().then_some(guard)
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
    }
}

/// Runs the passing and the failing gate with an unwritable `logs/`.
fn unwritable_logs(sb: &Sandbox, extra_env: &[(&str, &str)]) {
    let Some(_ro) = ReadOnly::new(&sb.home.join("state/sluice/logs")) else {
        return;
    };
    let out = sb
        .sluice()
        .envs(extra_env.iter().copied())
        .args(["run", "test"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("test PASS"));
    assert_eq!(stderr.matches("Permission denied").count(), 1, "{stderr}");
    assert!(stderr.contains("kept in memory"), "{stderr}");
    assert_eq!(sb.runs(), 1, "the gate ran");
    assert_eq!(sb.marker_count(), 1, "a real pass is cached");
    sb.sluice()
        .envs(extra_env.iter().copied())
        .args(["run", "fail"])
        .assert()
        .code(7)
        .stderr(predicate::str::contains("boom").and(predicate::str::contains("more")));
    assert_eq!(sb.marker_count(), 1, "a failure never writes a marker");
}

#[test]
fn an_unwritable_logs_dir_does_not_stop_a_daemon_run() {
    let sb = Sandbox::standard();
    unwritable_logs(&sb, &[]);
}

#[test]
fn an_unwritable_logs_dir_does_not_stop_an_in_process_run() {
    let sb = Sandbox::standard();
    unwritable_logs(&sb, &[("SLUICE_SOCKET", "/dev/null/impossible")]);
}

fn doctor_json(sb: &Sandbox) -> (i32, serde_json::Value) {
    let out = sb.sluice().args(["doctor", "--json"]).output().unwrap();
    let value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    (out.status.code().unwrap_or(-1), value)
}

fn check<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no {name} check in {report}"))
}

#[test]
fn doctor_fails_on_an_unwritable_markers_dir() {
    let sb = Sandbox::standard();
    sb.sluice()
        .env("SLUICE_SOCKET", "/dev/null/impossible")
        .args(["run", "test"])
        .assert()
        .success();
    let (code, report) = doctor_json(&sb);
    assert_eq!(check(&report, "markers")["status"], "ok", "{report}");
    assert_eq!(check(&report, "database")["status"], "ok", "{report}");
    assert_eq!(code, 0, "{report}");
    let Some(_ro) = ReadOnly::new(&sb.markers_dir()) else {
        return;
    };
    let (code, report) = doctor_json(&sb);
    let markers = check(&report, "markers");
    assert_eq!(markers["status"], "fail", "{report}");
    assert!(
        markers["detail"]
            .as_str()
            .unwrap()
            .contains("is not writable"),
        "{markers}"
    );
    assert_eq!(code, 1);
}

#[test]
fn doctor_warns_about_damaged_markers() {
    let sb = Sandbox::standard();
    sb.sluice()
        .env("SLUICE_SOCKET", "/dev/null/impossible")
        .args(["run", "test"])
        .assert()
        .success();
    let dir = sb.markers_dir();
    let good: PathBuf = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "json"))
        .unwrap();
    let bytes = fs::read(&good).unwrap();
    let truncated = dir.join(format!("{}.json", "1".repeat(64)));
    fs::write(&truncated, &bytes[..25]).unwrap();
    fs::write(dir.join(format!("{}.json", "2".repeat(64))), [0xfe_u8; 99]).unwrap();
    fs::write(dir.join(format!("{}.json", "3".repeat(64))), &bytes).unwrap();
    fs::write(dir.join(".tmp-1-1"), "half written").unwrap();
    let (code, report) = doctor_json(&sb);
    let markers = check(&report, "markers");
    assert_eq!(markers["status"], "warn", "{report}");
    let detail = markers["detail"].as_str().unwrap();
    assert!(detail.contains("3 damaged file(s)"), "{detail}");
    assert!(
        detail.contains(&truncated.display().to_string()),
        "{detail}"
    );
    assert_eq!(code, 0, "a warning does not fail doctor");
}

#[test]
fn doctor_fails_on_a_database_the_daemon_cannot_open() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "test"]).assert().success();
    sb.sluice().args(["daemon", "stop"]).assert().success();
    let db = sb.home.join("state/sluice/sluice.db");
    let mut bytes = fs::read(&db).unwrap();
    bytes[..100].fill(0xff);
    fs::write(&db, bytes).unwrap();
    for ext in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{ext}", db.display()));
    }
    let (code, report) = doctor_json(&sb);
    let database = check(&report, "database");
    assert_eq!(database["status"], "fail", "{report}");
    assert!(
        database["detail"]
            .as_str()
            .unwrap()
            .contains("the daemon cannot open"),
        "{database}"
    );
    assert_eq!(code, 1);
}
