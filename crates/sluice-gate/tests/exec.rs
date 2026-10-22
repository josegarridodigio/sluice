//! Acceptance tests of `sluice exec` and queue-only gates (alpha task A1;
//! test plan in `kb/sluice/design/command-control.md` section 7).
//!
//! Every test writes a user config with PSI admission off, so pressure from
//! other work on the machine cannot hold jobs back, and an explicit budget.

mod common;

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use predicates::prelude::*;
use sha2::{Digest, Sha256};

const WAIT: Duration = Duration::from_secs(30);

fn sandbox(budget: u32, extra_user_config: &str) -> Sandbox {
    let sb = Sandbox::new(
        r#"
[gates.build]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\"; while [ ! -f ignored.txt ]; do sleep 0.05; done"
mode = "queue-only"
weight = 1
hooks = ["pre-commit"]

[gates.quick]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\""
mode = "queue-only"
"#,
    );
    fs::write(
        sb.home.join(".config/sluice/config.toml"),
        format!(
            "version = 1\n[daemon]\nbudget = {budget}\n{extra_user_config}\n\
             [admission]\npsi_enabled = false\n"
        ),
    )
    .unwrap();
    sb
}

/// `sluice exec` as a std command (for tests that signal or overlap clients).
fn spawn_exec(sb: &Sandbox, dir: &Path, args: &[&str]) -> Child {
    StdCommand::new(Sandbox::binary())
        .arg("exec")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(sb.env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sluice exec")
}

fn wait_output(child: Child) -> Output {
    child.wait_with_output().expect("wait")
}

fn wait_for(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_pid(path: &Path) -> i32 {
    let mut pid = 0;
    wait_for(&format!("{}", path.display()), WAIT, || {
        pid = fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        pid > 0
    });
    pid
}

/// True while `pid` exists and is not a zombie.
fn alive(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X")
    })
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// ---------------------------------------------------------------------------
// Fidelity
// ---------------------------------------------------------------------------

#[test]
fn exit_codes_propagate_and_signal_deaths_map_to_128_plus_n() {
    let sb = sandbox(4, "");
    for code in [0, 1, 42, 255] {
        sb.sluice()
            .args(["exec", "--", "sh", "-c", &format!("exit {code}")])
            .assert()
            .code(code);
    }
    sb.sluice()
        .args(["exec", "--", "sh", "-c", "kill -KILL $$"])
        .assert()
        .code(137);
    sb.sluice()
        .args(["exec", "--", "no-such-command-anywhere"])
        .assert()
        .code(127)
        .stderr(predicate::str::contains(
            "sluice: exec: cannot run 'no-such-command-anywhere': command not found",
        ));
    fs::write(sb.repo.join("not-exec.sh"), "echo hi\n").unwrap();
    sb.sluice()
        .args(["exec", "--", "./not-exec.sh"])
        .assert()
        .code(126);
    sb.sluice()
        .args(["exec", "--timeout", "1s", "--", "sleep", "10"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains(
            "sluice: exec 'sleep 10' TIMEOUT after 1s",
        ));
}

#[test]
fn streams_binary_and_large_output_are_byte_exact() {
    let sb = sandbox(4, "");
    // Separate fds.
    let out = sb
        .sluice()
        .args(["exec", "-q", "--", "sh", "-c", "printf out; printf err >&2"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"out");
    assert_eq!(out.stderr, b"err");

    // 1 MiB of random binary.
    let mut random = vec![0u8; 1 << 20];
    fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut random)
        .unwrap();
    fs::write(sb.repo.join("random.bin"), &random).unwrap();
    let out = sb
        .sluice()
        .args(["exec", "-q", "--", "cat", "random.bin"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(sha(&out.stdout), sha(&random));

    // More than 10 MiB (12 MiB) through a pipeline.
    let n = 12 * 1024 * 1024;
    let out = sb
        .sluice()
        .args([
            "exec",
            "-q",
            "--",
            "sh",
            "-c",
            &format!("yes 0123456789abcdef | head -c {n}"),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout.len(), n);
    let expected: Vec<u8> = b"0123456789abcdef\n"
        .iter()
        .copied()
        .cycle()
        .take(n)
        .collect();
    assert_eq!(sha(&out.stdout), sha(&expected));
}

#[test]
fn order_within_each_stream_is_kept() {
    let sb = sandbox(4, "");
    let script = "i=1; while [ $i -le 200 ]; do echo o$i; echo e$i >&2; i=$((i+1)); done";
    let out = sb
        .sluice()
        .args(["exec", "-q", "--", "sh", "-c", script])
        .output()
        .unwrap();
    assert!(out.status.success());
    let expect = |p: &str| -> String {
        (1..=200).fold(String::new(), |mut s, i| {
            let _ = writeln!(s, "{p}{i}");
            s
        })
    };
    assert_eq!(String::from_utf8_lossy(&out.stdout), expect("o"));
    assert_eq!(String::from_utf8_lossy(&out.stderr), expect("e"));
}

#[test]
fn stdin_is_null_no_tty_env_and_cwd_are_the_callers() {
    let sb = sandbox(4, "");
    // `cat` reads /dev/null and exits at once even though our stdin is a pipe.
    let mut child = spawn_exec(&sb, &sb.repo, &["-q", "--", "cat"]);
    let _keep_stdin_open = child.stdin.take();
    let out = wait_output(child);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"");

    sb.sluice()
        .args(["exec", "-q", "--", "sh", "-c", "test -t 1 || test -t 0"])
        .assert()
        .code(1);

    let sub = sb.repo.join("sub dir");
    fs::create_dir(&sub).unwrap();
    let out = sb
        .sluice_in(&sub)
        .env("MY_VAR", "my value")
        .env("CLAUDECODE", "1")
        .args([
            "exec",
            "-q",
            "--",
            "sh",
            "-c",
            "echo \"$MY_VAR|$CLAUDECODE|$SLUICE_JOB\"; pwd",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let first: Vec<&str> = lines.next().unwrap().split('|').collect();
    assert_eq!(first[0], "my value");
    assert_eq!(first[1], "1");
    assert!(!first[2].is_empty(), "SLUICE_JOB is set: {text}");
    assert_eq!(
        fs::canonicalize(lines.next().unwrap()).unwrap(),
        fs::canonicalize(&sub).unwrap()
    );
    // The job's output is in `sluice logs <job>` (streams merged).
    sb.sluice()
        .args(["logs", first[2]])
        .assert()
        .success()
        .stdout(predicate::str::contains("my value|1|"));
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// A running job whose leader waits on a background grandchild.
fn start_group(sb: &Sandbox, tag: &str) -> (Child, i32, i32) {
    let leader = sb.repo.join(format!("{tag}.leader"));
    let grandchild = sb.repo.join(format!("{tag}.gc"));
    let script = format!(
        "sleep 300 & echo $! > '{}'; echo $$ > '{}'; wait",
        grandchild.display(),
        leader.display()
    );
    let child = spawn_exec(sb, &sb.repo, &["--", "sh", "-c", &script]);
    let leader = read_pid(&leader);
    let grandchild = read_pid(&grandchild);
    (child, leader, grandchild)
}

#[test]
fn sigint_and_sigterm_are_forwarded_to_the_whole_group() {
    let sb = sandbox(4, "");
    for (signal, code) in [(Signal::SIGINT, 130), (Signal::SIGTERM, 143)] {
        let (client, leader, grandchild) = start_group(&sb, &format!("{signal:?}"));
        assert!(alive(leader) && alive(grandchild));
        let sent = Instant::now();
        kill(Pid::from_raw(i32::try_from(client.id()).unwrap()), signal).unwrap();
        let out = wait_output(client);
        assert_eq!(out.status.code(), Some(code), "{signal:?}: {out:?}");
        wait_for("the group to die", Duration::from_secs(6), || {
            !alive(leader) && !alive(grandchild)
        });
        assert!(sent.elapsed() < Duration::from_secs(6));
    }
}

#[test]
fn a_signal_while_queued_drops_the_request() {
    let sb = sandbox(1, "");
    let (blocker, leader, _) = start_group(&sb, "blocker");
    let queued = spawn_exec(&sb, &sb.repo, &["--label", "waiting", "--", "true"]);
    wait_for("the queued exec", WAIT, || {
        sb.status_json()["queued"]
            .as_array()
            .is_some_and(|q| q.iter().any(|j| j["label"] == "waiting"))
    });
    kill(
        Pid::from_raw(i32::try_from(queued.id()).unwrap()),
        Signal::SIGINT,
    )
    .unwrap();
    let out = wait_output(queued);
    assert_eq!(out.status.code(), Some(130), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("sluice: exec 'waiting' interrupted while queued"),
        "{out:?}"
    );
    wait_for("the queue to empty", WAIT, || {
        sb.status_json()["queued"]
            .as_array()
            .is_some_and(Vec::is_empty)
    });
    kill(
        Pid::from_raw(i32::try_from(blocker.id()).unwrap()),
        Signal::SIGTERM,
    )
    .unwrap();
    let _ = wait_output(blocker);
    wait_for("the blocker to die", Duration::from_secs(10), || {
        !alive(leader)
    });
}

#[test]
fn a_killed_client_kills_its_job_even_under_finish() {
    let sb = sandbox(4, "last_subscriber_gone = \"finish\"\n");
    let (mut client, leader, grandchild) = start_group(&sb, "orphan");
    client.kill().unwrap();
    let _ = client.wait();
    wait_for("the group to die", Duration::from_secs(10), || {
        !alive(leader) && !alive(grandchild)
    });
}

// ---------------------------------------------------------------------------
// Budget, status, coalescing
// ---------------------------------------------------------------------------

/// Leaders of running jobs tagged `tag`, found by scanning `/proc`.
fn proc_count(tag: &str) -> usize {
    let Ok(entries) = fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().parse::<u32>().is_ok())
        .filter(|e| {
            fs::read(e.path().join("cmdline")).is_ok_and(|c| {
                // `sh -c <script> <tag>`: NUL-separated, with a trailing NUL.
                let args: Vec<&[u8]> = c
                    .strip_suffix(b"\0")
                    .unwrap_or(&c)
                    .split(|b| *b == 0)
                    .collect();
                let is_sh = args.first().is_some_and(|a| a.ends_with(b"sh"));
                is_sh && args.last().is_some_and(|a| *a == tag.as_bytes())
            })
        })
        .count()
}

#[test]
fn twenty_concurrent_execs_respect_the_budget() {
    let sb = sandbox(8, "");
    let log = sb.root.path().join("concurrency.log");
    let tag = format!("sluice-a1-{}", std::process::id());
    let script = format!(
        "echo start >> '{log}'; sleep 0.6; echo end >> '{log}'",
        log = log.display()
    );
    let children: Vec<Child> = (0..20)
        .map(|i| {
            spawn_exec(
                &sb,
                &sb.repo,
                &[
                    "--weight",
                    "2",
                    "--label",
                    &format!("job {i}"),
                    "--",
                    "sh",
                    "-c",
                    &script,
                    &tag,
                ],
            )
        })
        .collect();
    let mut peak_proc = 0;
    let mut saw_exec_status = false;
    let mut saw_budget_wait = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    while fs::read_to_string(&log)
        .unwrap_or_default()
        .matches("end")
        .count()
        < 20
    {
        assert!(Instant::now() < deadline, "the 20 jobs did not finish");
        peak_proc = peak_proc.max(proc_count(&tag));
        if !saw_exec_status || !saw_budget_wait {
            let status = sb.status_json();
            let running = status["running"].as_array().cloned().unwrap_or_default();
            let queued = status["queued"].as_array().cloned().unwrap_or_default();
            saw_exec_status |= running.iter().any(|j| {
                j["kind"] == "exec"
                    && j["label"].as_str().is_some_and(|l| l.starts_with("job "))
                    && j["gate"]
                        .as_str()
                        .is_some_and(|g| g.starts_with("exec:job "))
            });
            saw_budget_wait |= queued
                .iter()
                .any(|j| j["kind"] == "exec" && j["reason"] == "budget");
            assert!(status["budget"]["used"].as_u64().unwrap_or(0) <= 8);
        }
        thread::sleep(Duration::from_millis(30));
    }
    for child in children {
        let out = wait_output(child);
        assert!(out.status.success(), "{out:?}");
    }
    let mut current = 0i32;
    let mut peak = 0;
    for line in fs::read_to_string(&log).unwrap().lines() {
        current += if line == "start" { 1 } else { -1 };
        peak = peak.max(current);
    }
    assert!(peak <= 4, "counter peak {peak} > 4");
    assert!(peak_proc <= 4, "/proc peak {peak_proc} > 4");
    assert!(peak_proc >= 1, "the /proc probe never saw a job");
    assert!(saw_exec_status, "status never showed a running exec job");
    assert!(
        saw_budget_wait,
        "status never showed an exec job waiting for budget"
    );
}

#[test]
fn coalescing_needs_a_key_and_the_same_command() {
    let sb = sandbox(8, "");
    let count = sb.root.path().join("coalesce.count");
    let go = sb.root.path().join("coalesce.go");
    // The job waits for `go`, created once all three requests joined it, so
    // a slow client start under load cannot make this test flaky.
    let script = format!(
        "echo run >> '{}'; while [ ! -f '{}' ]; do sleep 0.05; done; echo shared",
        count.display(),
        go.display()
    );
    let runs = || {
        fs::read_to_string(&count)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let three: Vec<Child> = (0..3)
        .map(|_| {
            spawn_exec(
                &sb,
                &sb.repo,
                &["--coalesce-key", "k", "--", "sh", "-c", &script],
            )
        })
        .collect();
    wait_for("three subscribers", WAIT, || {
        sb.status_json()["running"]
            .as_array()
            .is_some_and(|r| r.iter().any(|j| j["subscribers"] == 3))
    });
    fs::write(&go, "").unwrap();
    let outs: Vec<Output> = three.into_iter().map(wait_output).collect();
    for out in &outs {
        assert!(out.status.success(), "{out:?}");
        assert_eq!(out.stdout, b"shared\n");
    }
    assert_eq!(runs(), 1, "three requests with one key ran once");
    assert_eq!(sb.status_json()["totals"]["coalesced"], 2);

    // Same key, different argv: two executions.
    let other = format!("{script}; true");
    let a = spawn_exec(
        &sb,
        &sb.repo,
        &["--coalesce-key", "k", "--", "sh", "-c", &script],
    );
    let b = spawn_exec(
        &sb,
        &sb.repo,
        &["--coalesce-key", "k", "--", "sh", "-c", &other],
    );
    assert!(wait_output(a).status.success() && wait_output(b).status.success());
    assert_eq!(runs(), 3);

    // No key: never coalesced.
    let a = spawn_exec(&sb, &sb.repo, &["--", "sh", "-c", &script]);
    let b = spawn_exec(&sb, &sb.repo, &["--", "sh", "-c", &script]);
    assert!(wait_output(a).status.success() && wait_output(b).status.success());
    assert_eq!(runs(), 5);
}

#[test]
fn nested_exec_inside_a_job_runs_directly() {
    // Budget 1: queueing the inner exec would wait forever for the outer one.
    let sb = sandbox(1, "");
    let script = format!(
        "'{}' exec -- sh -c 'echo inner $SLUICE_JOB'; echo outer",
        Sandbox::binary().display()
    );
    let out = sb
        .sluice()
        .timeout(Duration::from_secs(60))
        .args(["exec", "-q", "--", "sh", "-c", &script])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("inner ") && text.ends_with("outer\n"),
        "{text}"
    );
    assert_eq!(sb.status_json()["totals"]["runs"], 1, "no second job");
}

#[test]
fn daemon_stop_force_kills_with_143() {
    let sb = sandbox(4, "");
    let (client, leader, _) = start_group(&sb, "stopped");
    sb.sluice()
        .args(["daemon", "stop", "--force"])
        .assert()
        .success();
    let out = wait_output(client);
    assert_eq!(out.status.code(), Some(143), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("KILLED: the daemon stopped (job "),
        "{out:?}"
    );
    wait_for("the group to die", Duration::from_secs(10), || {
        !alive(leader)
    });
}

// ---------------------------------------------------------------------------
// Fallbacks
// ---------------------------------------------------------------------------

const PROBE: &str = "printf 'a\\0b'; echo err >&2; exit 7";

fn probe(sb: &Sandbox, socket: Option<&str>) -> Output {
    let mut cmd = sb.sluice();
    if let Some(s) = socket {
        cmd.env("SLUICE_SOCKET", s);
    }
    cmd.args(["exec", "--", "sh", "-c", PROBE])
        .output()
        .unwrap()
}

#[test]
fn without_a_daemon_the_command_runs_in_process_with_the_same_result() {
    let sb = sandbox(1, "");
    let daemon = probe(&sb, None);
    let local = probe(&sb, Some("/dev/null/impossible"));
    assert_eq!(daemon.status.code(), Some(7));
    assert_eq!(local.status.code(), Some(7));
    assert_eq!(daemon.stdout, local.stdout);
    assert_eq!(daemon.stdout, b"a\0b");
    let stderr = String::from_utf8_lossy(&local.stderr);
    assert!(
        stderr.contains("sluice: daemon unavailable (") && stderr.contains("in-process"),
        "{stderr}"
    );
    assert!(stderr.ends_with("err\n"), "{stderr}");

    // The in-process path holds the budget semaphore: with budget 1 two
    // concurrent runs never overlap.
    let log = sb.root.path().join("inproc.log");
    let script = format!(
        "echo start >> '{l}'; sleep 0.5; echo end >> '{l}'",
        l = log.display()
    );
    let mut cmds: Vec<Child> = (0..2)
        .map(|_| {
            StdCommand::new(Sandbox::binary())
                .args(["exec", "--", "sh", "-c", &script])
                .current_dir(&sb.repo)
                .env_clear()
                .envs(sb.env())
                .env("SLUICE_SOCKET", "/dev/null/impossible")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for c in &mut cmds {
        assert!(c.wait().unwrap().success());
    }
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "start\nend\nstart\nend\n"
    );
}

/// A fake daemon on `socket`: answers `hello` with a welcome announcing
/// `features`, then answers any request with `error{bad-request}`.
fn fake_daemon(socket: &Path, features: &'static str) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            thread::spawn(move || {
                let mut reader = BufReader::new(&stream);
                let mut writer = &stream;
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let welcome = format!(
                    "{{\"type\":\"welcome\",\"protocol\":1,\"daemon\":\"sluice-gate 0.2.0\",\
                     \"instance\":\"fake\",\"pid\":1,\"started_at\":\"t\"{features}}}\n"
                );
                let _ = writer.write_all(welcome.as_bytes());
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let _ = writer.write_all(
                        b"{\"type\":\"error\",\"id\":1,\"code\":\"bad-request\",\
                          \"message\":\"unknown variant `exec`\"}\n",
                    );
                }
            });
        }
    })
}

#[test]
fn an_old_daemon_means_in_process() {
    let sb = sandbox(2, "");
    let dir = sb.root.path().join("fake");
    fs::create_dir(&dir).unwrap();
    // A v0.2 daemon: no features at all.
    let old: PathBuf = dir.join("old.sock");
    let _old = fake_daemon(&old, "");
    let out = probe(&sb, Some(old.to_str().unwrap()));
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    assert_eq!(out.stdout, b"a\0b");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not support exec"),
        "{out:?}"
    );
    // A daemon that claims `exec` but refuses the message before `queued`.
    let liar = dir.join("liar.sock");
    let _liar = fake_daemon(&liar, ",\"features\":[\"exec\"]");
    let out = probe(&sb, Some(liar.to_str().unwrap()));
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    assert_eq!(out.stdout, b"a\0b");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("running 'sh -c' in-process"),
        "{out:?}"
    );
}

// ---------------------------------------------------------------------------
// Queue-only gates
// ---------------------------------------------------------------------------

#[test]
fn queue_only_gates_always_run_and_never_touch_the_cache() {
    let sb = sandbox(4, "");
    for _ in 0..2 {
        sb.sluice()
            .args(["run", "quick"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("quick PASS")
                    .and(predicate::str::contains("cached").not()),
            );
    }
    assert_eq!(sb.runs(), 2);
    assert_eq!(sb.marker_count(), 0);
    assert_eq!(sb.miss_count(), 0);
    // A failure leaves no miss record either.
    sb.write_config("[gates.bad]\ncmd = \"exit 3\"\nmode = \"queue-only\"\ncache = false\n");
    sb.sluice().args(["run", "bad"]).assert().code(3);
    assert_eq!(sb.miss_count(), 0);
    // `cache = true` is refused with a clear message.
    sb.write_config("[gates.bad]\ncmd = \"true\"\nmode = \"queue-only\"\ncache = true\n");
    sb.sluice()
        .args(["run", "bad"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "gates.bad: mode = \"queue-only\" never caches; remove cache = true",
        ));
}

#[test]
fn simultaneous_identical_queue_only_requests_coalesce() {
    let sb = sandbox(4, "");
    let spawn = || {
        StdCommand::new(Sandbox::binary())
            .args(["run", "build"])
            .current_dir(&sb.repo)
            .env_clear()
            .envs(sb.env())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (spawn(), spawn());
    // The gate waits for the (git-ignored) `ignored.txt`, created once both
    // requests share the job.
    wait_for("two subscribers", WAIT, || {
        sb.status_json()["running"]
            .as_array()
            .is_some_and(|r| r.iter().any(|j| j["subscribers"] == 2))
    });
    fs::write(sb.repo.join("ignored.txt"), "").unwrap();
    let (a, b) = (wait_output(a), wait_output(b));
    assert!(a.status.success() && b.status.success(), "{a:?} {b:?}");
    assert_eq!(sb.runs(), 1, "one run for two simultaneous requests");
    assert_eq!(sb.marker_count(), 0);
    // A later request runs again.
    sb.sluice().args(["run", "build"]).assert().success();
    assert_eq!(sb.runs(), 2);
    assert_eq!(sb.marker_count(), 0);
}

#[test]
fn an_agent_hook_waits_for_a_queue_only_gate_instead_of_failing_fast() {
    let sb = sandbox(4, "");
    fs::write(sb.repo.join("ignored.txt"), "").unwrap();
    sb.sluice()
        .env("CLAUDECODE", "1")
        .args(["run", "--hook", "pre-commit", "build"])
        .assert()
        .success();
    assert_eq!(sb.runs(), 1);
}
