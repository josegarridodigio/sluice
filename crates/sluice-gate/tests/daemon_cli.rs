//! Phase 2 acceptance tests: `sluice run` through the daemon, `status`,
//! `daemon stop` and the in-process fallback.

mod common;

use std::fs;
use std::process::{Child, Command as StdCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;
use predicates::prelude::*;

/// Polls `cond` every 50 ms for up to `limit`.
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

/// True when a process whose command line contains `pattern` exists.
fn pgrep(pattern: &str) -> bool {
    StdCommand::new("pgrep")
        .args(["-f", pattern])
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Spawns `sluice run <gate>` in the background.
fn spawn_run(sb: &Sandbox, gate: &str) -> Child {
    StdCommand::new(Sandbox::binary())
        .args(["run", gate])
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sluice run")
}

#[test]
fn a_miss_starts_the_daemon_and_a_hit_only_sends_an_event() {
    let sb = Sandbox::standard();
    sb.sluice()
        .args(["run", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test PASS").and(predicate::str::contains("(job ")))
        .stderr(predicate::str::contains("test queued (position"));
    assert_eq!(sb.runs(), 1);
    assert!(sb.socket().exists(), "the daemon must have been started");
    let status = sb.status_json();
    assert_eq!(status["totals"]["runs"], 1, "{status}");

    sb.sluice()
        .args(["run", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test PASS (cached"))
        .stderr(predicate::str::contains("queued").not());
    assert_eq!(sb.runs(), 1);
    assert!(
        wait_for(
            Duration::from_secs(3),
            || sb.status_json()["totals"]["hits"] == 1
        ),
        "the hit event must reach the daemon"
    );
    assert_eq!(sb.status_json()["totals"]["runs"], 1);

    sb.sluice()
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 runs").and(predicate::str::contains("1 hits")));
}

#[test]
fn six_concurrent_runs_on_one_tree_run_the_gate_once() {
    let sb = Sandbox::new(
        r#"
[gates.test]
cmd = "sleep 0.5; echo run >> \"$SLUICE_TEST_COUNTER\""
"#,
    );
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let mut cmd = sb.sluice();
            cmd.args(["run", "test"]);
            thread::spawn(move || cmd.assert().success())
        })
        .collect();
    for h in handles {
        h.join().expect("client thread");
    }
    assert_eq!(
        sb.runs(),
        1,
        "six clients on one tree must run the gate once"
    );
}

#[test]
fn the_budget_limits_overlap() {
    let gate = |name: &str| {
        format!(
            "[gates.{name}]\ncmd = \"echo start $(date +%s%N) >> ts; sleep 0.5; echo end $(date +%s%N) >> ts\"\ncache = false\n"
        )
    };
    let sb = Sandbox::new(&format!("{}{}{}", gate("a"), gate("b"), gate("c")));
    fs::write(sb.repo.join(".gitignore"), "ignored.txt\nts\n").unwrap();
    fs::write(
        sb.home.join(".config/sluice/config.toml"),
        "version = 1\n[daemon]\nbudget = 2\n",
    )
    .unwrap();
    let handles: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|g| {
            let mut cmd = sb.sluice();
            cmd.args(["run", g]);
            thread::spawn(move || cmd.assert().success())
        })
        .collect();
    for h in handles {
        h.join().expect("client thread");
    }
    let ts = fs::read_to_string(sb.repo.join("ts")).unwrap();
    let mut events: Vec<(u128, i32)> = ts
        .lines()
        .map(|l| {
            let (kind, t) = l.split_once(' ').unwrap();
            (t.parse().unwrap(), if kind == "start" { 1 } else { -1 })
        })
        .collect();
    assert_eq!(events.len(), 6, "{ts}");
    // Ends sort before starts at the same instant.
    events.sort_unstable();
    let (mut running, mut max) = (0, 0);
    for (_, delta) in events {
        running += delta;
        max = max.max(running);
    }
    assert!(
        max <= 2,
        "at most two gates may overlap under budget 2:\n{ts}"
    );
}

#[test]
fn killing_the_client_kills_the_gate_and_the_daemon_survives() {
    let sb = Sandbox::new(
        r#"
[gates.long]
cmd = "sleep 41.25"
"#,
    );
    let mut child = spawn_run(&sb, "long");
    assert!(
        wait_for(Duration::from_secs(10), || pgrep("sleep 41.25")),
        "the gate never started"
    );
    thread::sleep(Duration::from_millis(200));
    child.kill().expect("kill client");
    let _ = child.wait();
    assert!(
        wait_for(Duration::from_secs(7), || !pgrep("sleep 41.25")),
        "the gate must die once its last client is gone"
    );
    assert_eq!(
        sb.status_json()["schema"],
        1,
        "the daemon must still answer"
    );
}

#[test]
fn daemon_stop_without_and_with_running_jobs() {
    let sb = Sandbox::new(
        r#"
[gates.long]
cmd = "sleep 42.75"
"#,
    );
    sb.sluice()
        .args(["daemon", "stop"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no daemon running"));
    sb.sluice()
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no daemon running"));
    let out = sb.sluice().args(["status", "--json"]).output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["schema"], 1);
    assert!(json["daemon"].is_null());

    let child = spawn_run(&sb, "long");
    assert!(
        wait_for(Duration::from_secs(10), || pgrep("sleep 42.75")),
        "the gate never started"
    );
    let start = Instant::now();
    sb.sluice()
        .args(["daemon", "stop", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("daemon stopping (1 running jobs)"));
    assert!(start.elapsed() < Duration::from_secs(7));
    assert!(
        wait_for(Duration::from_secs(1), || !pgrep("sleep 42.75")),
        "stop --force must kill the gate"
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(143), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("long KILLED"));
    sb.sluice()
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no daemon running"));
}

#[test]
fn an_unreachable_daemon_falls_back_to_an_in_process_run() {
    let sb = Sandbox::standard();
    sb.sluice()
        .env("SLUICE_SOCKET", "/dev/null/impossible")
        .args(["run", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test PASS"))
        .stderr(predicate::str::contains("running in-process"));
    assert_eq!(sb.runs(), 1);
    assert_eq!(sb.marker_count(), 1);
}

#[test]
fn status_json_has_schema_one() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "lint"]).assert().success();
    let status = sb.status_json();
    assert_eq!(status["schema"], 1);
    assert!(status["daemon"]["pid"].as_u64().is_some_and(|p| p > 0));
    assert!(status["budget"]["total"].as_u64().is_some_and(|t| t >= 1));
}

#[test]
fn a_second_daemon_refuses_to_start() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "lint"]).assert().success();
    sb.sluice()
        .args(["daemon", "--foreground"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("already running"));
}
