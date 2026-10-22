//! Phase 1 acceptance tests for `sluice run`, `cache` and `logs`.

mod common;

use std::fs;
use std::time::Instant;

use common::Sandbox;
use predicates::prelude::*;

#[test]
fn first_run_executes_then_hits_and_other_worktree_shares_the_hit() {
    let sb = Sandbox::standard();
    sb.sluice()
        .args(["run", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test PASS").and(predicate::str::contains("(job ")));
    assert_eq!(sb.runs(), 1);
    assert_eq!(sb.marker_count(), 1);

    sb.sluice()
        .args(["run", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test PASS (cached"));
    assert_eq!(sb.runs(), 1);

    // A second worktree with the same content hits the marker written from the first.
    let wt = sb.root.path().join("wt");
    sb.git(&[
        "worktree",
        "add",
        "-q",
        wt.to_str().unwrap(),
        "-b",
        "feature",
    ]);
    sb.sluice_in(&wt)
        .args(["run", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cached"));
    assert_eq!(sb.runs(), 1);
}

#[test]
fn tracked_and_untracked_changes_invalidate_but_ignored_files_do_not() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "test"]).assert().success();
    assert_eq!(sb.runs(), 1);

    fs::write(sb.repo.join("a.txt"), "changed\n").unwrap();
    sb.sluice().args(["run", "test"]).assert().success();
    assert_eq!(sb.runs(), 2, "an unstaged change must invalidate");

    fs::write(sb.repo.join("new.txt"), "new\n").unwrap();
    sb.sluice().args(["run", "test"]).assert().success();
    assert_eq!(sb.runs(), 3, "an untracked file must invalidate");

    fs::write(sb.repo.join("ignored.txt"), "ignored\n").unwrap();
    sb.sluice()
        .args(["run", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cached"));
    assert_eq!(sb.runs(), 3, "an ignored file must not invalidate");

    // Different gates have different keys on the same tree.
    sb.sluice().args(["run", "lint"]).assert().success();
    assert_eq!(sb.runs(), 4);
}

#[test]
fn real_index_and_object_store_are_never_touched() {
    let sb = Sandbox::standard();
    fs::write(
        sb.repo.join("untracked.txt"),
        "blob that must not reach .git/objects\n",
    )
    .unwrap();
    let index = sb.repo.join(".git/index");
    let before = fs::read(&index).unwrap();
    let mtime_before = fs::metadata(&index).unwrap().modified().unwrap();
    let objects_before = list_objects(&sb);

    sb.sluice().args(["run", "test"]).assert().success(); // miss: snapshot twice + run
    sb.sluice().args(["run", "test"]).assert().success(); // hit: snapshot once

    assert_eq!(fs::read(&index).unwrap(), before, "index bytes changed");
    assert_eq!(
        fs::metadata(&index).unwrap().modified().unwrap(),
        mtime_before,
        "index mtime changed"
    );
    assert!(!sb.repo.join(".git/index.lock").exists());
    assert_eq!(
        list_objects(&sb),
        objects_before,
        "objects were written to .git/objects"
    );
}

fn list_objects(sb: &Sandbox) -> Vec<String> {
    let mut out = Vec::new();
    let root = sb.repo.join(".git/objects");
    for entry in walkdir(&root) {
        out.push(entry);
    }
    out.sort();
    out
}

fn walkdir(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.filter_map(Result::ok) {
            let p = e.path();
            if p.is_dir() {
                out.extend(walkdir(&p));
            } else {
                out.push(p.display().to_string());
            }
        }
    }
    out
}

#[test]
fn works_inside_real_pre_commit_and_pre_push_hooks() {
    let sb = Sandbox::standard();
    sb.install_hooks(&["lint", "test"], &["test"]);

    // First commit: both gates run inside pre-commit (hook env present).
    fs::write(sb.repo.join("b.txt"), "b\n").unwrap();
    sb.git(&["add", "b.txt"]);
    sb.git(&["commit", "-q", "-m", "second"]);
    assert_eq!(
        sb.runs(),
        2,
        "lint and test must run once each inside pre-commit"
    );
    assert_eq!(sb.marker_count(), 2);

    // Pushing the same tree to a bare remote: pre-push hits the cache.
    let remote = sb.root.path().join("remote.git");
    sb.git_in(
        sb.root.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    sb.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    sb.git(&["push", "-q", "origin", "main"]);
    assert_eq!(
        sb.runs(),
        2,
        "pre-push must hit the marker written at commit time"
    );

    // A partial commit: GIT_INDEX_FILE points at a temporary index; sluice must not break.
    fs::write(sb.repo.join("c.txt"), "c\n").unwrap();
    fs::write(sb.repo.join("a.txt"), "dirty\n").unwrap();
    sb.git(&["add", "c.txt"]);
    sb.git(&["commit", "-q", "-m", "third", "--", "c.txt"]);
    assert_eq!(sb.runs(), 4, "a new tree runs both gates again");

    // Markers record the committed tree inside pre-commit.
    let any_marker = fs::read_dir(sb.markers_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .find(|s| s.contains("\"committed_tree\":\"") || s.contains("\"committed_tree\": \""))
        .expect("a marker with committed_tree");
    assert!(any_marker.contains("\"gate\""));
}

#[test]
fn hook_environment_variables_from_a_subdirectory_still_hit() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "test"]).assert().success();
    assert_eq!(sb.runs(), 1);
    let sub = sb.repo.join("sub");
    fs::create_dir_all(&sub).unwrap();
    // Untracked empty dirs are invisible to git, so the tree is unchanged.
    sb.sluice_in(&sub)
        .env("GIT_DIR", sb.repo.join(".git"))
        .env("GIT_PREFIX", "sub/")
        .env("GIT_INDEX_FILE", sb.repo.join(".git/index"))
        .args(["run", "--hook", "pre-commit", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cached"));
    assert_eq!(sb.runs(), 1);
}

#[test]
fn ci_disables_the_cache_unless_sluice_cache_is_set_and_disable_bypasses() {
    let sb = Sandbox::standard();
    sb.sluice()
        .env("CI", "true")
        .args(["run", "test"])
        .assert()
        .success();
    sb.sluice()
        .env("CI", "true")
        .args(["run", "test"])
        .assert()
        .success();
    assert_eq!(sb.runs(), 2, "CI must run every time");
    assert_eq!(sb.marker_count(), 0, "CI must not write markers");

    sb.sluice()
        .env("CI", "true")
        .env("SLUICE_CACHE", "1")
        .args(["run", "test"])
        .assert()
        .success();
    sb.sluice()
        .env("CI", "true")
        .env("SLUICE_CACHE", "1")
        .args(["run", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cached"));
    assert_eq!(sb.runs(), 3);

    sb.sluice()
        .env("SLUICE_DISABLE", "1")
        .args(["run", "test"])
        .assert()
        .success()
        .stderr(predicate::str::contains("SLUICE_DISABLE"));
    assert_eq!(sb.runs(), 4, "SLUICE_DISABLE runs directly");
}

#[test]
fn agent_in_hook_fails_fast_on_miss_and_hits_on_pass() {
    let sb = Sandbox::standard();
    sb.sluice()
        .env("CLAUDECODE", "1")
        .args(["run", "--hook", "pre-commit", "test"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("has not passed on this tree").and(
            predicate::str::contains("call the 'verify' MCP tool (or run 'sluice run test') first"),
        ));
    assert_eq!(sb.runs(), 0, "fail-fast must not run the gate");

    // With the cache off the gate can never be cached, so fail-fast must not refuse.
    sb.sluice()
        .env("CLAUDECODE", "1")
        .env("CI", "true")
        .args(["run", "--hook", "pre-commit", "test"])
        .assert()
        .success();
    assert_eq!(sb.runs(), 1);
    sb.sluice()
        .env("CLAUDECODE", "1")
        .args(["run", "--hook", "pre-commit", "test"])
        .assert()
        .code(3);
    assert_eq!(sb.runs(), 1);

    // Outside a hook the agent may run the gate (it is the Phase 1 way to verify).
    sb.sluice()
        .env("CLAUDECODE", "1")
        .args(["run", "test"])
        .assert()
        .success();
    assert_eq!(sb.runs(), 2);

    sb.sluice()
        .env("CLAUDECODE", "1")
        .args(["run", "--hook", "pre-commit", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cached"));

    // SLUICE_AGENT=0 overrides detection: a human waits and runs.
    fs::write(sb.repo.join("a.txt"), "x\n").unwrap();
    sb.sluice()
        .env("CLAUDECODE", "1")
        .env("SLUICE_AGENT", "0")
        .args(["run", "--hook", "pre-commit", "test"])
        .assert()
        .success();
    assert_eq!(sb.runs(), 3);

    // run-unqueued policy lets the agent run inside the hook.
    sb.write_config(
        r#"
[defaults]
agent_hook_policy = "run-unqueued"

[gates.test]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\""
"#,
    );
    sb.sluice()
        .env("CLAUDECODE", "1")
        .args(["run", "--hook", "pre-commit", "test"])
        .assert()
        .success();
    assert_eq!(sb.runs(), 4);
}

#[test]
fn explain_prints_decisions_without_running() {
    let sb = Sandbox::standard();
    sb.sluice()
        .env("CLAUDECODE", "1")
        .args(["run", "--hook", "pre-commit", "--explain", "test"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("caller:       Agent")
                .and(predicate::str::contains("lookup:       MISS"))
                .and(predicate::str::contains("key:          ")),
        );
    assert_eq!(sb.runs(), 0);
}

#[test]
fn repo_tail_lines_override_the_user_default() {
    let sb = Sandbox::new(
        r#"
[defaults]
tail_lines = 2

[gates.fail]
cmd = "echo one; echo two; echo three; exit 1"
"#,
    );
    sb.sluice().args(["run", "fail"]).assert().code(1).stderr(
        predicate::str::contains("last 2 lines")
            .and(predicate::str::contains("three"))
            .and(predicate::str::contains("one").not()),
    );
}

#[test]
fn disable_works_without_a_user_config_but_needs_the_gate() {
    let sb = Sandbox::standard();
    fs::write(
        sb.home.join(".config/sluice/config.toml"),
        "this is not toml",
    )
    .unwrap();
    sb.sluice()
        .env("SLUICE_DISABLE", "1")
        .args(["run", "test"])
        .assert()
        .success();
    assert_eq!(sb.runs(), 1);
    sb.sluice()
        .env("SLUICE_DISABLE", "1")
        .args(["run", "nope"])
        .assert()
        .code(1);
}

#[test]
fn a_gate_that_modifies_the_tree_is_not_cached() {
    let sb = Sandbox::new(
        r#"
[gates.fix]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\"; echo fixed >> a.txt"
"#,
    );
    sb.sluice()
        .args(["run", "fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "not cached: the gate modified the tree",
        ));
    assert_eq!(sb.marker_count(), 0);
    assert_eq!(sb.miss_count(), 1);
    sb.sluice().args(["run", "fix"]).assert().success();
    assert_eq!(sb.runs(), 2);
}

#[test]
fn failure_returns_the_gate_exit_code_with_log_tail_and_records_a_miss() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "fail"]).assert().code(7).stderr(
        predicate::str::contains("fail FAIL exit=7")
            .and(predicate::str::contains("boom"))
            .and(predicate::str::contains("more")),
    );
    assert_eq!(sb.marker_count(), 0);
    assert_eq!(sb.miss_count(), 1);

    sb.sluice()
        .args(["cache", "status"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("fail")
                .and(predicate::str::contains("MISS (last run"))
                .and(predicate::str::contains("exit 7")),
        );

    // The log holds the full interleaved output and `sluice logs` prints it.
    sb.sluice()
        .args(["logs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("boom").and(predicate::str::contains("# exit: 7")));
}

#[test]
fn timeout_kills_the_gate_and_exits_124() {
    let sb = Sandbox::standard();
    let start = Instant::now();
    sb.sluice()
        .args(["run", "slow"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("TIMEOUT"));
    assert!(
        start.elapsed().as_secs() < 4,
        "timeout must be enforced promptly"
    );
}

#[test]
fn budget_of_one_serialises_concurrent_runs() {
    let sb = Sandbox::new(
        r#"
[gates.a]
cmd = "sleep 1"
[gates.b]
cmd = "sleep 1"
"#,
    );
    fs::write(
        sb.home.join(".config/sluice/config.toml"),
        "version = 1\n[daemon]\nbudget = 1\n",
    )
    .unwrap();
    let start = Instant::now();
    let a = {
        let mut cmd = sb.sluice();
        cmd.args(["run", "a"]);
        std::thread::spawn(move || cmd.assert().success())
    };
    let b = {
        let mut cmd = sb.sluice();
        cmd.args(["run", "b"]);
        std::thread::spawn(move || cmd.assert().success())
    };
    a.join().unwrap();
    b.join().unwrap();
    assert!(
        start.elapsed().as_millis() >= 1900,
        "two one-second gates under budget 1 must not overlap (took {:?})",
        start.elapsed()
    );
}

#[test]
fn cache_clear_removes_markers() {
    let sb = Sandbox::standard();
    sb.sluice().args(["run", "test"]).assert().success();
    sb.sluice().args(["run", "lint"]).assert().success();
    assert_eq!(sb.marker_count(), 2);
    sb.sluice()
        .args(["cache", "clear", "--gate", "lint"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed 1 cache entry"));
    assert_eq!(sb.marker_count(), 1);
    sb.sluice()
        .args(["cache", "status", "--all"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("test             HIT")
                .and(predicate::str::contains("lint             MISS")),
        );
    sb.sluice().args(["cache", "clear"]).assert().success();
    assert_eq!(sb.marker_count(), 0);
}

#[test]
fn key_inputs_and_key_files_change_the_key() {
    let sb = Sandbox::new(
        r#"
[gates.test]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\""
key_inputs = ["cat \"$SLUICE_TEST_KEY\" 2>/dev/null || true"]
key_files = ["ignored.txt"]
"#,
    );
    let key_input = sb.root.path().join("key-input");
    let run = |sb: &Sandbox| {
        sb.sluice()
            .env("SLUICE_TEST_KEY", &key_input)
            .args(["run", "test"])
            .assert()
            .success();
    };
    run(&sb);
    run(&sb);
    assert_eq!(sb.runs(), 1);
    fs::write(&key_input, "v2").unwrap();
    run(&sb);
    assert_eq!(sb.runs(), 2, "a changed key_input output must invalidate");
    fs::write(sb.repo.join("ignored.txt"), "secret\n").unwrap();
    run(&sb);
    assert_eq!(sb.runs(), 3, "a declared ignored key_file must invalidate");
}

#[test]
fn extra_arguments_reach_the_gate() {
    let sb = Sandbox::new(
        r#"
[gates.echo]
cmd = "echo"
cache = false
"#,
    );
    sb.sluice()
        .args(["run", "echo", "--", "hello", "world"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not cached"));
    sb.sluice()
        .args(["logs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}

#[test]
fn unknown_gate_and_missing_config_are_reported() {
    let sb = Sandbox::standard();
    sb.sluice()
        .args(["run", "nope"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown gate 'nope'"));
    let empty = sb.root.path().join("empty");
    fs::create_dir_all(&empty).unwrap();
    sb.sluice_in(&empty)
        .args(["run", "test"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not a git repository"));
}

#[test]
fn compound_commands_work_without_extra_arguments() {
    let sb = Sandbox::new(
        r#"
[gates.loop]
cmd = "for i in 1 2; do echo run >> \"$SLUICE_TEST_COUNTER\"; done"
"#,
    );
    sb.sluice().args(["run", "loop"]).assert().success();
    assert_eq!(sb.runs(), 2);
}

#[test]
fn a_zero_timeout_means_no_timeout_in_process() {
    let sb = Sandbox::new(
        r#"
[gates.zero]
cmd = "sleep 0.3"
timeout = "0s"
"#,
    );
    // Unreachable socket: the in-process fallback runs the gate.
    sb.sluice()
        .env("SLUICE_SOCKET", "/dev/null/impossible")
        .args(["run", "zero"])
        .assert()
        .success()
        .stdout(predicate::str::contains("zero PASS"));
}

#[test]
fn the_state_and_logs_dirs_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::standard();
    sb.sluice()
        .env("SLUICE_SOCKET", "/dev/null/impossible")
        .args(["run", "test"])
        .assert()
        .success();
    let state = sb.home.join("state/sluice");
    for dir in [state.clone(), state.join("logs")] {
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
}
