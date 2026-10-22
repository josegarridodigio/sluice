//! Debugging and observability: `client.log`, `daemon restart`, log level in
//! `sluice status`, `daemon.log` rotation, `sluice doctor` and its bundle.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;
use predicates::prelude::*;

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

fn state_dir(sb: &Sandbox) -> std::path::PathBuf {
    sb.home.join("state/sluice")
}

fn client_log(sb: &Sandbox) -> Vec<String> {
    fs::read_to_string(state_dir(sb).join("logs/client.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn field<'a>(line: &'a str, key: &str) -> &'a str {
    let prefix = format!("{key}=");
    line.split(' ')
        .find_map(|w| w.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {key} in {line}"))
}

fn write_user_config(sb: &Sandbox, body: &str) {
    fs::write(
        sb.home.join(".config/sluice/config.toml"),
        format!("version = 1\n{body}"),
    )
    .expect("user config");
}

fn daemon_pid(sb: &Sandbox) -> u64 {
    sb.status_json()["daemon"]["pid"].as_u64().expect("pid")
}

// ------------------------------------------------------------- client.log

#[test]
fn client_log_records_miss_hit_fail_fast_and_disabled() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "test"]).assert().success();
    sb.sluice().args(["run", "test"]).assert().success();
    sb.sluice()
        .env("CLAUDECODE", "1")
        .args(["run", "--hook", "pre-commit", "lint"])
        .assert()
        .code(3);
    sb.sluice()
        .env("SLUICE_DISABLE", "1")
        .args(["run", "test"])
        .assert()
        .success();

    let lines = client_log(&sb);
    assert_eq!(lines.len(), 4, "{lines:#?}");
    let expect = [
        ("test", "-", "miss-run", "0", "human"),
        ("test", "-", "hit", "0", "human"),
        ("lint", "pre-commit", "miss-fail-fast", "3", "agent"),
        ("test", "-", "disabled", "0", "human"),
    ];
    for (line, (gate, hook, decision, exit, caller)) in lines.iter().zip(expect) {
        let ts = line.split(' ').next().unwrap();
        assert!(ts.contains('T') && ts.ends_with('Z'), "{line}");
        assert_eq!(field(line, "gate"), gate, "{line}");
        assert_eq!(field(line, "hook"), hook, "{line}");
        assert_eq!(field(line, "decision"), decision, "{line}");
        assert_eq!(field(line, "exit"), exit, "{line}");
        assert_eq!(field(line, "caller"), caller, "{line}");
        assert!(field(line, "pid").parse::<u32>().is_ok(), "{line}");
        assert!(field(line, "ms").parse::<u64>().is_ok(), "{line}");
        assert!(
            line.ends_with(&format!("cwd={}", sb.repo.display())),
            "{line}"
        );
    }
    // Tree hashes are short hex for the first three; the miss and the hit share one.
    let tree = |i: usize| field(&lines[i], "tree");
    assert_eq!(tree(0).len(), 12);
    assert!(tree(0).bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(tree(0), tree(1));
    assert_eq!(tree(2).len(), 12);
}

#[test]
fn client_log_records_errors_and_failing_gates() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "nope"]).assert().code(1);
    sb.sluice().args(["run", "fail"]).assert().code(7);
    let lines = client_log(&sb);
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert_eq!(field(&lines[0], "decision"), "error");
    assert_eq!(field(&lines[0], "exit"), "1");
    assert_eq!(field(&lines[1], "decision"), "miss-run");
    assert_eq!(field(&lines[1], "exit"), "7");
}

#[test]
fn client_log_rotates_at_the_cap_keeping_one_generation() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "test"]).assert().success();
    for _ in 0..12 {
        sb.sluice()
            .env("SLUICE_TEST_CLIENT_LOG_CAP", "400")
            .args(["run", "test"])
            .assert()
            .success();
    }
    let logs = state_dir(&sb).join("logs");
    let current = fs::metadata(logs.join("client.log")).expect("client.log");
    let rotated = fs::metadata(logs.join("client.log.1")).expect("client.log.1");
    assert!(current.len() <= 400 + 400, "{}", current.len());
    assert!(rotated.len() > 400, "{}", rotated.len());
    assert!(!logs.join("client.log.2").exists());
}

#[test]
fn client_log_never_breaks_the_command() {
    let sb = Sandbox::standard();
    // A directory where the log file should be: appending fails, the run must not.
    fs::create_dir_all(state_dir(&sb).join("logs/client.log")).unwrap();
    sb.sluice().args(["run", "test"]).assert().success();
    sb.sluice().args(["run", "test"]).assert().success();
}

// ------------------------------------------------------------ daemon.log

#[test]
fn daemon_log_is_rotated_at_start() {
    let sb = Sandbox::standard();
    write_user_config(&sb, "[debug]\ndaemon_log_max_mib = 1\n");
    let state = state_dir(&sb);
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("daemon.log"), vec![b'x'; 1024 * 1024 + 10]).unwrap();
    fs::write(state.join("daemon.log.1"), "older").unwrap();
    sb.sluice().arg("daemon").assert().success();
    assert!(wait_for(Duration::from_secs(10), || sb.socket().exists()));
    let rotated = fs::metadata(state.join("daemon.log.1")).expect("daemon.log.1");
    assert!(rotated.len() > 1024 * 1024);
    assert_eq!(
        fs::read_to_string(state.join("daemon.log.2")).unwrap(),
        "older"
    );
    let fresh = fs::metadata(state.join("daemon.log")).expect("daemon.log");
    assert!(fresh.len() < 100_000, "{}", fresh.len());
}

// ------------------------------------------------------- daemon restart

#[test]
fn restart_replaces_the_daemon_and_debug_is_reflected_in_status() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "test"]).assert().success();
    let first = daemon_pid(&sb);
    assert_eq!(sb.status_json()["daemon"]["log_level"], "info");

    sb.sluice()
        .args(["daemon", "restart", "--debug"])
        .assert()
        .success()
        .stdout(predicate::str::contains("log level debug"));
    let second = daemon_pid(&sb);
    assert_ne!(first, second);
    assert_eq!(sb.status_json()["daemon"]["log_level"], "debug");
    sb.sluice().arg("status").assert().success().stdout(
        predicate::str::contains("log:      level debug, ")
            .and(predicate::str::contains("daemon.log")),
    );

    // A plain restart goes back to the configured level.
    write_user_config(&sb, "[debug]\nlevel = \"warn\"\n");
    sb.sluice().args(["daemon", "restart"]).assert().success();
    assert_eq!(sb.status_json()["daemon"]["log_level"], "warn");
    // SLUICE_LOG beats the config.
    sb.sluice()
        .env("SLUICE_LOG", "trace")
        .args(["daemon", "restart"])
        .assert()
        .success();
    assert_eq!(sb.status_json()["daemon"]["log_level"], "trace");
}

#[test]
fn restart_without_a_daemon_just_starts_one() {
    let sb = Sandbox::standard();
    sb.sluice().args(["daemon", "restart"]).assert().success();
    assert!(sb.socket().exists());
}

#[test]
fn restart_refuses_while_gates_run_unless_forced() {
    let sb = Sandbox::new(
        r#"
[gates.sleepy]
cmd = "sleep 20"
"#,
    );
    let mut run = StdCommand::new(Sandbox::binary())
        .args(["run", "sleepy"])
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn run");
    assert!(
        wait_for(Duration::from_secs(10), || sb.socket().exists()
            && sb.status_json()["running"]
                .as_array()
                .is_some_and(|r| r.len() == 1)),
        "the gate never started"
    );
    let pid = daemon_pid(&sb);
    sb.sluice()
        .args(["daemon", "restart"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("1 gate(s) are running"));
    assert_eq!(daemon_pid(&sb), pid, "the daemon must be untouched");

    sb.sluice()
        .args(["daemon", "restart", "--force"])
        .assert()
        .success();
    assert_ne!(daemon_pid(&sb), pid);
    let _ = run.wait();
}

// ----------------------------------------------------------------- doctor

fn doctor_json(sb: &Sandbox) -> (i32, serde_json::Value) {
    let out = sb
        .sluice()
        .args(["doctor", "--json"])
        .output()
        .expect("doctor");
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
        .unwrap_or_else(|| panic!("no check {name} in {report}"))
}

#[test]
fn doctor_on_a_healthy_sandbox_exits_zero_with_the_documented_shape() {
    let sb = Sandbox::standard();
    sb.install_hooks(&["lint", "test"], &["test"]);
    sb.sluice().args(["run", "test"]).assert().success();
    let (code, report) = doctor_json(&sb);
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(report["schema"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    for c in report["checks"].as_array().unwrap() {
        assert!(c["name"].is_string() && c["detail"].is_string(), "{c}");
        assert!(["ok", "warn", "fail"].contains(&c["status"].as_str().unwrap()));
    }
    for name in [
        "git",
        "psi",
        "state-dir",
        "runtime-dir",
        "daemon",
        "daemon-version",
        "lease",
        "user-config",
        "repo-config",
        "hooks",
        "markers",
        "path",
    ] {
        check(&report, name);
    }
    assert_eq!(check(&report, "daemon")["status"], "ok");
    assert_eq!(check(&report, "daemon-version")["status"], "ok");
    assert_eq!(check(&report, "lease")["status"], "ok");
    assert_eq!(check(&report, "hooks")["status"], "ok");
    assert_eq!(check(&report, "runtime-dir")["status"], "ok");

    sb.sluice()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("[ok").and(predicate::str::contains("git")));
}

#[test]
fn doctor_fails_and_names_a_broken_repo_config() {
    let sb = Sandbox::standard();
    sb.write_config("[gates.test\ncmd = \n");
    let (code, report) = doctor_json(&sb);
    assert_eq!(code, 1);
    assert_eq!(report["ok"], false);
    let c = check(&report, "repo-config");
    assert_eq!(c["status"], "fail");
    assert!(
        c["detail"].as_str().unwrap().contains(".sluice.toml"),
        "{c}"
    );
}

#[test]
fn doctor_fails_on_a_broken_user_config_and_warns_without_hooks() {
    let sb = Sandbox::standard();
    write_user_config(&sb, "[debug]\nlevel = \"a=b=c\"\n");
    let (code, report) = doctor_json(&sb);
    assert_eq!(code, 1);
    let c = check(&report, "user-config");
    assert_eq!(c["status"], "fail");
    assert!(c["detail"].as_str().unwrap().contains("config.toml"), "{c}");
    assert_eq!(check(&report, "hooks")["status"], "warn");
}

#[test]
fn doctor_fails_on_a_stale_socket() {
    // A socket file that nothing answers on is a failure.
    let sb = Sandbox::standard();
    fs::create_dir_all(sb.socket().parent().unwrap()).unwrap();
    std::os::unix::net::UnixListener::bind(sb.socket())
        .map(drop)
        .unwrap();
    let (code, report) = doctor_json(&sb);
    assert_eq!(code, 1, "{report:#}");
    assert_eq!(check(&report, "daemon")["status"], "fail");
}

#[test]
fn doctor_bundle_has_the_expected_files_and_redacts_secrets() {
    let sb = Sandbox::standard();
    write_user_config(&sb, "");
    sb.sluice().args(["run", "test"]).assert().success();
    sb.sluice().args(["run", "fail"]).assert().code(7);

    // Plant a secret and the home path in the daemon log.
    let planted = format!(
        "GITHUB_TOKEN=ghp_planted123 working in {}/project\n\
         remote https://bot:hunter2@example.com/o/r.git\n",
        sb.home.display()
    );
    let daemon_log = state_dir(&sb).join("daemon.log");
    let mut text = fs::read_to_string(&daemon_log).unwrap_or_default();
    text.push_str(&planted);
    fs::write(&daemon_log, text).unwrap();

    let out_dir = tempfile::tempdir().unwrap();
    let bundle = out_dir.path().join("b.tar.gz");
    let out = sb
        .sluice()
        .args(["doctor", "--bundle"])
        .arg(&bundle)
        .output()
        .expect("doctor --bundle");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&bundle.display().to_string()), "{stderr}");
    assert!(stderr.contains("review"), "{stderr}");

    let listing = StdCommand::new("tar")
        .arg("-tzf")
        .arg(&bundle)
        .output()
        .expect("tar -t");
    let listing = String::from_utf8_lossy(&listing.stdout).into_owned();
    for needle in [
        "/doctor.json",
        "/version.txt",
        "/user-config.toml",
        "/repo-sluice.toml",
        "/status.json",
        "/queue-status.json",
        "/logs/daemon.log",
        "/logs/client.log",
        "/logs/jobs/",
    ] {
        assert!(listing.contains(needle), "{needle} missing from\n{listing}");
    }

    let extract = tempfile::tempdir().unwrap();
    let status = StdCommand::new("tar")
        .arg("-xzf")
        .arg(&bundle)
        .arg("-C")
        .arg(extract.path())
        .status()
        .unwrap();
    assert!(status.success());
    let mut files = Vec::new();
    collect_files(extract.path(), &mut files);
    let home = sb.home.display().to_string();
    for f in &files {
        let content = fs::read_to_string(f).unwrap_or_default();
        assert!(
            !content.contains(&home),
            "{} leaks the home path",
            f.display()
        );
        assert!(!content.contains("ghp_planted123"), "{}", f.display());
        assert!(!content.contains("hunter2"), "{}", f.display());
    }
    let daemon = files
        .iter()
        .find(|f| f.ends_with("logs/daemon.log"))
        .expect("daemon.log in the bundle");
    let daemon = fs::read_to_string(daemon).unwrap();
    assert!(daemon.contains("GITHUB_TOKEN=<redacted>"), "{daemon}");
    assert!(daemon.contains("working in ~/project"), "{daemon}");
    assert!(daemon.contains("https://example.com/o/r.git"), "{daemon}");
}

fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}
