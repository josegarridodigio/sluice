//! `sluice queue ...`: client-side checks, the not-supported path against a
//! real daemon, and the request/reply flows against a scripted fake daemon.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use common::Sandbox;
use predicates::prelude::*;
use serde_json::{Value, json};

#[test]
fn enqueue_on_a_dirty_worktree_fails_without_contacting_the_daemon() {
    let sb = Sandbox::standard();
    fs::write(sb.repo.join("a.txt"), "changed\n").unwrap();
    sb.sluice()
        .args(["queue", "enqueue"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::starts_with("sluice: ").and(predicate::str::contains("uncommitted")),
        );
    assert!(!sb.socket().exists(), "the daemon must not be started");

    // Staged changes count too; untracked files do not.
    sb.git(&["add", "a.txt"]);
    sb.sluice()
        .args(["queue", "enqueue"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("uncommitted"));
    sb.git(&["checkout", "-q", "--", "."]);
    sb.git(&["reset", "-q", "--hard"]);
    fs::write(sb.repo.join("untracked.txt"), "x\n").unwrap();
    // Untracked files do not count as dirty: the request reaches the daemon,
    // which refuses it for another reason (this repository has no remote).
    sb.sluice()
        .args(["queue", "enqueue"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("uncommitted").not());
    assert!(sb.socket().exists(), "the daemon was contacted");
}

#[test]
fn enqueue_on_a_detached_head_fails_without_contacting_the_daemon() {
    let sb = Sandbox::standard();
    sb.git(&["checkout", "-q", "--detach"]);
    sb.sluice()
        .args(["queue", "enqueue"])
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("sluice: ").and(predicate::str::contains("detached")));
    assert!(!sb.socket().exists(), "the daemon must not be started");
}

#[test]
fn a_daemon_with_the_queue_disabled_answers_not_supported() {
    let sb = Sandbox::standard();
    std::fs::write(
        sb.home.join(".config/sluice/config.toml"),
        "version = 1\n[queue]\nenabled = false\n",
    )
    .expect("user config");
    for args in [
        &["queue", "enqueue"][..],
        &["queue", "status"],
        &["queue", "status", "--json"],
        &["queue", "reconcile"],
        &["queue", "cancel", "j-1"],
    ] {
        sb.sluice().args(args).assert().code(1).stderr(
            predicate::str::starts_with("sluice: ")
                .and(predicate::str::contains("disabled in the user config")),
        );
    }
}

#[test]
fn approve_without_a_terminal_requires_yes() {
    let sb = Sandbox::standard();
    sb.sluice()
        .args(["queue", "approve", "t-1-abcdef"])
        .assert()
        .code(2)
        .stderr(predicate::str::starts_with("sluice: ").and(predicate::str::contains("--yes")));
    assert!(!sb.socket().exists(), "the daemon must not be started");
}

#[test]
fn every_subcommand_documents_its_arguments() {
    let sb = Sandbox::standard();
    let cases: [(&[&str], &[&str]); 7] = [
        (
            &["queue"],
            &[
                "enqueue",
                "status",
                "approve",
                "retry",
                "cancel",
                "reconcile",
            ],
        ),
        (
            &["queue", "enqueue"],
            &["[BRANCH]", "--base", "<remote>/<branch>", "detached HEAD"],
        ),
        (&["queue", "status"], &["--json", "schema"]),
        (
            &["queue", "approve"],
            &[
                "<TRAIN_ID>",
                "--yes",
                "Land this train? [y/N]",
                "stale-plan",
            ],
        ),
        (&["queue", "retry"], &["<JOB_ID>", "rebasing"]),
        (&["queue", "cancel"], &["<JOB_ID>", "pushing"]),
        (
            &["queue", "reconcile"],
            &["needs_reconcile", "never pushes"],
        ),
    ];
    for (cmd, needles) in cases {
        let out = sb.sluice().args(cmd).arg("--help").output().unwrap();
        assert!(out.status.success(), "{cmd:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        for n in needles {
            assert!(text.contains(n), "{cmd:?} --help lacks {n:?}:\n{text}");
        }
    }
}

// ------------------------------------------------------------ fake daemon

type Handler = dyn Fn(&Value) -> Value + Send + Sync;

/// A scripted daemon that announces the queue feature and answers every
/// request with `handler`; requests are recorded.
struct FakeDaemon {
    socket: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    _dir: tempfile::TempDir,
}

impl FakeDaemon {
    fn start(handler: Arc<Handler>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = Arc::clone(&handler);
                let log = Arc::clone(&log);
                thread::spawn(move || {
                    let mut writer = stream.try_clone().unwrap();
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let welcome = json!({"type":"welcome","protocol":1,"daemon":"fake","instance":"i",
                        "pid":1,"started_at":"2026-01-01T00:00:00Z","features":["queue"]});
                    let _ = writeln!(writer, "{welcome}");
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    log.lock().unwrap().push(request.clone());
                    let _ = writeln!(writer, "{}", handler(&request));
                });
            }
        });
        Self {
            socket,
            requests,
            _dir: dir,
        }
    }

    fn sent(&self, kind: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["type"] == kind)
            .cloned()
            .collect()
    }
}

fn error(code: &str, message: &str) -> Value {
    json!({"type":"error","id":1,"code":code,"message":message})
}

fn job(id: &str, branch: &str, state: &str) -> Value {
    json!({"job_id":id,"repo":"/r/.git","worktree":"/r","branch":branch,"head_sha":"abc",
        "remote":"origin","target":"main","state":state,"caller":"human","cancel_requested":false,
        "attempts":1,"enqueued_at":"t","updated_at":"t","position":2})
}

fn train(state: &str) -> Value {
    json!({"train_id":"t-1-aaaaaa","repo":"/r/.git","remote":"origin","target":"main",
        "base_sha":"4e1c9a0ffff","state":state,"members":["j1"],"merge_sha":"3b7d2f1eeee",
        "gates":[{"gate":"test","status":"cached"}],"train_hash":"hash1",
        "created_at":"t","updated_at":"t"})
}

fn run_with(fake: &FakeDaemon, sb: &Sandbox) -> assert_cmd::Command {
    let mut cmd = sb.sluice();
    cmd.env("SLUICE_SOCKET", &fake.socket);
    cmd
}

#[test]
fn enqueue_sends_the_facts_and_prints_the_position() {
    let sb = Sandbox::standard();
    sb.git(&["checkout", "-q", "-b", "feat/login"]);
    let head = sb.git(&["rev-parse", "HEAD"]);
    let fake = FakeDaemon::start(Arc::new(|r| {
        assert_eq!(r["type"], "queue-enqueue");
        json!({"type":"queue-job","id":1,"job":job("j-9","feat/login","queued")})
    }));
    run_with(&fake, &sb)
        .args(["queue", "enqueue", "--base", "origin/release"])
        .env("CLAUDE_CODE_SESSION_ID", "sess-1")
        .env("SSH_AUTH_SOCK", "/tmp/agent")
        .env("SECRET_TOKEN", "nope")
        .assert()
        .success()
        .stdout("sluice: queued feat/login (job j-9) for origin/main, position 2\n");
    let sent = &fake.sent("queue-enqueue")[0];
    assert_eq!(sent["branch"], "feat/login");
    assert_eq!(sent["head_sha"], head.as_str());
    assert_eq!(sent["base"], "origin/release");
    assert_eq!(sent["caller"], "human");
    assert_eq!(sent["session"], "sess-1");
    assert_eq!(sent["env"]["SSH_AUTH_SOCK"], "/tmp/agent");
    assert!(sent["env"].get("SECRET_TOKEN").is_none());
    assert!(sent["repo"].as_str().unwrap().contains(".git"));
}

#[test]
fn refused_states_exit_4_and_other_daemon_errors_exit_1() {
    let sb = Sandbox::standard();
    for (code, exit) in [
        ("duplicate-branch", 4),
        ("conflict-state", 4),
        ("not-allowed", 4),
        ("stale-plan", 4),
        ("internal", 1),
        ("not-supported", 1),
    ] {
        let fake = FakeDaemon::start(Arc::new(move |_| error(code, "refused: because")));
        run_with(&fake, &sb)
            .args(["queue", "enqueue"])
            .assert()
            .code(exit)
            .stderr("sluice: refused: because\n");
    }
}

#[test]
fn status_json_is_the_payload_and_text_shows_the_hint() {
    let sb = Sandbox::standard();
    let fake = FakeDaemon::start(Arc::new(|_| {
        json!({"type":"queue-status","id":1,"schema":1,
            "trains":[train("ready")],"jobs":[job("j1","feat/login","ready")]})
    }));
    let out = run_with(&fake, &sb)
        .args(["queue", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["trains"][0]["train_id"], "t-1-aaaaaa");
    run_with(&fake, &sb)
        .args(["queue", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("READY (awaiting approval)").and(
            predicate::str::contains("approve with: sluice queue approve t-1-aaaaaa"),
        ));
}

#[test]
fn approve_yes_sends_the_displayed_hash_and_prints_the_landing() {
    let sb = Sandbox::standard();
    let fake = FakeDaemon::start(Arc::new(|r| match r["type"].as_str().unwrap() {
        "queue-status" => json!({"type":"queue-status","id":1,"schema":1,
            "trains":[train("ready")],"jobs":[job("j1","feat/login","ready")]}),
        "queue-approve" => json!({"type":"queue-approved","id":1,"train":train("landed")}),
        other => panic!("unexpected {other}"),
    }));
    run_with(&fake, &sb)
        .args(["queue", "approve", "t-1-aaaaaa", "--yes"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("landing commit 3b7d2f1").and(predicate::str::contains(
                "sluice: landed 3b7d2f1 on origin/main (train t-1-aaaaaa)",
            )),
        );
    let sent = &fake.sent("queue-approve")[0];
    assert_eq!(sent["train_hash"], "hash1");
    assert_eq!(sent["via"], "cli");
}

#[test]
fn approve_polls_while_the_push_is_asynchronous() {
    let sb = Sandbox::standard();
    let polls = Arc::new(Mutex::new(0_u32));
    let fake = FakeDaemon::start(Arc::new(move |r| match r["type"].as_str().unwrap() {
        "queue-status" => {
            let mut n = polls.lock().unwrap();
            *n += 1;
            // First call: the plan; second: still pushing; third: landed.
            let state = match *n {
                1 => "ready",
                2 => "pushing",
                _ => "landed",
            };
            json!({"type":"queue-status","id":1,"schema":1,
                "trains":[train(state)],"jobs":[job("j1","feat/login","landed")]})
        }
        _ => json!({"type":"queue-approved","id":1,"train":train("approved")}),
    }));
    run_with(&fake, &sb)
        .args(["queue", "approve", "t-1-aaaaaa", "--yes"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("approved; pushing").and(predicate::str::contains(
                "sluice: landed 3b7d2f1 on origin/main",
            )),
        );
}

#[test]
fn reconcile_prints_one_line_per_train() {
    let sb = Sandbox::standard();
    let fake = FakeDaemon::start(Arc::new(|_| {
        json!({"type":"queue-reconcile","id":1,"results":[
            {"train_id":"t-1","decision":"landed","reason":"target_is_merge_sha"},
            {"train_id":"t-2","decision":"refused","reason":"push_in_progress"}]})
    }));
    run_with(&fake, &sb)
        .args(["queue", "reconcile"])
        .assert()
        .success()
        .stdout("train t-1: landed (target_is_merge_sha)\ntrain t-2: refused (push_in_progress)\n");
}

#[test]
fn retry_rereads_the_branch_head() {
    let sb = Sandbox::standard();
    sb.git(&["checkout", "-q", "-b", "feat/ui"]);
    let common = sb.repo.join(".git").canonicalize().unwrap();
    let head = sb.git(&["rev-parse", "HEAD"]);
    let mut conflicted = job("j2", "feat/ui", "conflict");
    conflicted["repo"] = json!(common.display().to_string());
    let fake = FakeDaemon::start(Arc::new(move |r| match r["type"].as_str().unwrap() {
        "queue-status" => {
            json!({"type":"queue-status","id":1,"schema":1,"trains":[],"jobs":[conflicted.clone()]})
        }
        _ => json!({"type":"queue-job","id":1,"job":job("j2","feat/ui","queued")}),
    }));
    run_with(&fake, &sb)
        .args(["queue", "retry", "j2"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "sluice: requeued feat/ui (job j2)",
        ));
    let sent = &fake.sent("queue-retry")[0];
    assert_eq!(sent["job_id"], "j2");
    assert_eq!(sent["head_sha"], head.as_str());
}
