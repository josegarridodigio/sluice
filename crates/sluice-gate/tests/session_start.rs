//! Acceptance tests of `sluice agent session-start` (task T5 of the
//! zero-config plan, ADR-0014): shims installed, `PATH` line appended once,
//! the three context variants and the fail-open behaviour.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output, Stdio};
use std::time::{Duration, Instant};

use common::Sandbox;
use serde_json::{Value, json};

fn shim_dir(sb: &Sandbox) -> PathBuf {
    sb.home.join(".local/share/sluice/shims")
}

fn env_file(sb: &Sandbox) -> PathBuf {
    sb.root.path().join("claude-env.sh")
}

/// Runs the hook with `input` on stdin and `CLAUDE_ENV_FILE` set.
fn hook(sb: &Sandbox, input: &str, extra: &[(&str, &str)]) -> Output {
    let mut cmd = StdCommand::new(Sandbox::binary());
    cmd.args(["agent", "session-start", "--plugin-version", "0.3.0"])
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env("CLAUDE_ENV_FILE", env_file(sb))
        .envs(extra.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn sluice");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(input.as_bytes());
    }
    child.wait_with_output().expect("wait")
}

fn input(cwd: &Path, source: &str) -> String {
    json!({
        "session_id": "s-1",
        "cwd": cwd,
        "hook_event_name": "SessionStart",
        "source": source,
    })
    .to_string()
}

/// The `additionalContext` of a successful run.
fn context(out: &Output) -> String {
    assert!(out.status.success(), "{out:?}");
    let value: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
    assert_eq!(value["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let text = value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext")
        .to_owned();
    assert!(
        text.chars().count() <= 1500,
        "{} chars",
        text.chars().count()
    );
    text
}

fn exe(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn installs_shims_and_appends_the_path_line_once() {
    let sb = Sandbox::standard();
    let first = hook(&sb, &input(&sb.repo, "startup"), &[]);
    context(&first);
    for name in ["npx", "ng", "cargo", "make"] {
        let shim = shim_dir(&sb).join(name);
        assert!(shim.is_file(), "missing shim {name}");
        assert_eq!(
            fs::metadata(&shim).unwrap().permissions().mode() & 0o111,
            0o111
        );
    }
    let again = hook(&sb, &input(&sb.repo, "resume"), &[]);
    context(&again);
    let env = fs::read_to_string(env_file(&sb)).unwrap();
    let dir = fs::canonicalize(shim_dir(&sb)).unwrap();
    let marker = format!("export PATH='{}':\"$PATH\"", dir.display());
    assert_eq!(env.matches(&marker).count(), 1, "{env}");
    assert_eq!(env.lines().count(), 1, "{env}");
    // The client log has one line per run.
    let log = fs::read_to_string(sb.home.join("state/sluice/logs/client.log")).unwrap();
    assert_eq!(log.matches("caller=agent hook=session-start").count(), 2);
    assert!(log.contains("decision=rules"), "{log}");
}

#[test]
fn a_subshell_npx_ng_build_reaches_the_daemon() {
    let sb = Sandbox::standard();
    context(&hook(&sb, &input(&sb.repo, "startup"), &[]));
    context(&hook(&sb, &input(&sb.repo, "resume"), &[]));
    let bin = sb.root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    exe(&bin.join("npx"), r#"echo "real npx $*""#);
    exe(&bin.join("ng"), r#"echo "real ng $*""#);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let before = sb.status_json()["totals"]["runs"].as_u64().unwrap_or(0);
    // Sourced twice, as after a resume; the line is guarded.
    let script = format!(
        ". '{0}'; . '{0}'; sh -c '(npx ng build)'",
        env_file(&sb).display()
    );
    let out = StdCommand::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env("PATH", path)
        .env("CLAUDECODE", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("real npx ng build"),
        "{out:?}"
    );
    assert_eq!(sb.status_json()["totals"]["runs"], before + 1);
}

#[test]
fn context_in_a_repository_with_a_config_has_the_rules_and_the_gates() {
    let sb = Sandbox::standard();
    let sub = sb.repo.join("sub");
    fs::create_dir(&sub).unwrap();
    let text = context(&hook(&sb, &input(&sub, "startup"), &[]));
    assert!(
        text.contains("Gates of this repository: fail, lint, slow, test."),
        "{text}"
    );
    for needle in [
        "`verify` MCP tool",
        "never retry one without it",
        "subshells, `$(...)`",
        "dev servers",
        "Never approve merge trains",
    ] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
}

#[test]
fn context_in_a_git_repository_without_a_config_offers_init() {
    let sb = Sandbox::standard();
    let repo = sb.root.path().join("plain-repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    let text = context(&hook(&sb, &input(&repo, "startup"), &[]));
    assert!(text.contains("sluice init --dry-run"), "{text}");
    assert!(text.contains("only after the user agrees"), "{text}");
    assert!(!text.contains("Gates of this repository"), "{text}");
}

#[test]
fn context_outside_git_is_one_line() {
    let sb = Sandbox::standard();
    let dir = sb.root.path().join("not-git");
    fs::create_dir(&dir).unwrap();
    let text = context(&hook(&sb, &input(&dir, "startup"), &[]));
    assert!(text.contains("not a git repository"), "{text}");
    assert!(text.len() < 300, "{text}");
}

#[test]
fn without_an_env_file_the_context_is_still_printed() {
    let sb = Sandbox::standard();
    let mut cmd = StdCommand::new(Sandbox::binary());
    cmd.args(["agent", "session-start", "--plugin-version", "0.3.0"])
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input(&sb.repo, "startup").as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    context(&out);
    assert!(!env_file(&sb).exists());
    assert!(shim_dir(&sb).join("npx").is_file());
}

#[test]
fn a_broken_environment_prints_nothing_and_exits_zero() {
    let sb = Sandbox::standard();
    // Invalid JSON, empty input, a non-object.
    for bad in ["not json", "", "[1,2]"] {
        let out = hook(&sb, bad, &[]);
        assert!(out.status.success(), "{bad}: {out:?}");
        assert!(out.stdout.is_empty(), "{bad}: {out:?}");
    }
    // A shim directory that cannot be created (its parent is a file).
    let file = sb.root.path().join("a-file");
    fs::write(&file, "x").unwrap();
    let blocked = file.join("shims");
    let out = hook(
        &sb,
        &input(&sb.repo, "startup"),
        &[("SLUICE_SHIM_DIR", blocked.to_str().unwrap())],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    // An env file that cannot be written (a directory).
    let out = hook(
        &sb,
        &input(&sb.repo, "startup"),
        &[("CLAUDE_ENV_FILE", sb.root.path().to_str().unwrap())],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn foreign_files_in_the_shim_dir_are_left_alone() {
    let sb = Sandbox::standard();
    fs::create_dir_all(shim_dir(&sb)).unwrap();
    let foreign = shim_dir(&sb).join("make");
    fs::write(&foreign, "mine\n").unwrap();
    context(&hook(&sb, &input(&sb.repo, "startup"), &[]));
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "mine\n");
}

#[test]
fn a_warm_run_takes_under_300_ms() {
    let sb = Sandbox::standard();
    context(&hook(&sb, &input(&sb.repo, "startup"), &[]));
    let start = Instant::now();
    let out = hook(&sb, &input(&sb.repo, "resume"), &[]);
    let took = start.elapsed();
    context(&out);
    assert!(took < Duration::from_millis(300), "{took:?}");
}

#[test]
fn plugin_test_session_start_script_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("plugin/hooks/test-session-start.sh");
    let out = StdCommand::new("sh")
        .arg(&script)
        .env("SLUICE_BIN", Sandbox::binary())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "test-session-start.sh failed:\n{stdout}"
    );
    assert!(
        stdout.contains("== real binary ("),
        "the real pass must run:\n{stdout}"
    );
    assert!(stdout.contains(", 0 failure(s)"), "{stdout}");
}

// ---------------------------------------------------------------------------
// Containment (task C3). Real cgroups in a `sluice-test-<pid>` slice; no CPU
// load, no memory limits; skipped when the host is not a full containment host.
// ---------------------------------------------------------------------------

use common::cgtree::{TestTree, cgroup_of};

/// The hook run against `tree` for the fake agent `agent`.
fn contained_hook(sb: &Sandbox, tree: &TestTree, session: &str, agent: u32) -> Output {
    let mut env = tree.env();
    env.push(("CLAUDE_PID", agent.to_string()));
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let body = json!({
        "session_id": session,
        "cwd": sb.repo,
        "hook_event_name": "SessionStart",
        "source": "startup",
    })
    .to_string();
    hook(sb, &body, &pairs)
}

fn env_lines_with(sb: &Sandbox, needle: &str) -> Vec<String> {
    fs::read_to_string(env_file(sb))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(needle))
        .map(str::to_owned)
        .collect()
}

fn client_log(sb: &Sandbox) -> String {
    fs::read_to_string(sb.home.join("state/sluice/logs/client.log")).unwrap_or_default()
}

fn nice_of_shell_after(script: &str) -> String {
    let out = StdCommand::new("sh")
        .arg("-c")
        .arg(format!("{script}; cat /proc/$$/stat"))
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout).into_owned();
    let after = &stat[stat.rfind(')').unwrap() + 1..];
    // Fields after the command name start at field 3 (state); nice is field 19.
    after.split_whitespace().nth(16).unwrap().to_owned()
}

#[test]
fn the_hook_moves_the_agent_and_its_children_into_ui_and_writes_the_line_once() {
    let Some(mut tree) = TestTree::new() else {
        return;
    };
    let sb = Sandbox::standard();
    let (agent, child) = tree.fake_agent();
    let started = Instant::now();
    let first = contained_hook(&sb, &tree, "sess-c3", agent);
    let took = started.elapsed();
    context(&first);
    assert!(first.stderr.is_empty(), "{first:?}");

    let session = tree.session_dir("sess-c3");
    let (ui, tools) = (session.join("ui"), session.join("tools"));
    assert!(ui.is_dir() && tools.is_dir(), "{session:?}");
    for pid in [agent, child] {
        assert!(
            cgroup_of(pid).ends_with("/s-sess-c3/ui"),
            "{pid}: {}",
            cgroup_of(pid)
        );
    }
    assert!(
        sluice_gate_core::cgroup::tree::has_oomd_avoid(&ui),
        "user.oomd_avoid on ui"
    );
    // Resume: same session, same line, still contained.
    let again = contained_hook(&sb, &tree, "sess-c3", agent);
    context(&again);
    let lines = env_lines_with(&sb, "SLUICE_SESSION_LEAF");
    assert_eq!(lines.len(), 1, "{lines:?}");
    let t = tools.display().to_string();
    assert_eq!(
        lines[0],
        format!(
            "export SLUICE_SESSION_LEAF='{t}'; if [ -w '{t}/cgroup.procs' ]; then echo $$ > '{t}/cgroup.procs'; else renice -n 19 $$ >/dev/null 2>&1; fi"
        )
    );
    // Even the run that creates the session stays inside the watchdog, and so
    // does a warm one.
    assert!(took < Duration::from_millis(300), "first run {took:?}");
    let t0 = Instant::now();
    context(&contained_hook(&sb, &tree, "sess-c3", agent));
    assert!(
        t0.elapsed() < Duration::from_millis(300),
        "{:?}",
        t0.elapsed()
    );
    let log = client_log(&sb);
    assert!(log.contains("contained=full"), "{log}");

    // Sourcing the env file moves a shell into `tools`, silently.
    let out = StdCommand::new("sh")
        .arg("-c")
        .arg(format!(
            ". '{}'; cat /proc/$$/cgroup",
            env_file(&sb).display()
        ))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.trim_end().ends_with("/s-sess-c3/tools"), "{text}");
    assert!(out.stderr.is_empty(), "{out:?}");
}

#[test]
fn a_shell_that_cannot_write_the_leaf_gets_nice_19_silently() {
    let Some(mut tree) = TestTree::new() else {
        return;
    };
    let sb = Sandbox::standard();
    let (agent, _) = tree.fake_agent();
    context(&contained_hook(&sb, &tree, "sess-ro", agent));
    let line = env_lines_with(&sb, "SLUICE_SESSION_LEAF").remove(0);
    // Point the line at a read-only stand-in for the leaf.
    let ro = sb.root.path().join("ro-leaf");
    fs::create_dir(&ro).unwrap();
    fs::write(ro.join("cgroup.procs"), "").unwrap();
    fs::set_permissions(ro.join("cgroup.procs"), fs::Permissions::from_mode(0o444)).unwrap();
    let tools = tree.session_dir("sess-ro").join("tools");
    let swapped = line.replace(&tools.display().to_string(), &ro.display().to_string());
    assert_ne!(swapped, line);
    let file = sb.root.path().join("ro-env.sh");
    fs::write(&file, format!("{swapped}\n")).unwrap();
    assert_eq!(
        nice_of_shell_after(&format!(". '{}'", file.display())),
        "19"
    );
    let out = StdCommand::new("sh")
        .arg("-c")
        .arg(format!(". '{}'", file.display()))
        .output()
        .unwrap();
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
}

#[test]
fn without_containment_the_hook_fails_open_and_adds_no_cgroup_line() {
    // The sandbox's runtime directory has no user manager: detection says off.
    let sb = Sandbox::standard();
    let out = hook(&sb, &input(&sb.repo, "startup"), &[]);
    context(&out);
    assert!(
        !fs::read_to_string(env_file(&sb))
            .unwrap()
            .contains("cgroup")
    );
    assert!(
        !fs::read_to_string(env_file(&sb))
            .unwrap()
            .contains("renice")
    );
    assert!(client_log(&sb).contains("contained=off"));
    // Disabled by config: nothing either, and still the context.
    let sb = Sandbox::standard();
    sb.write_user_config("[containment]\nenabled = false\n");
    context(&hook(&sb, &input(&sb.repo, "startup"), &[]));
    assert_eq!(
        fs::read_to_string(env_file(&sb)).unwrap().lines().count(),
        1
    );
    assert!(client_log(&sb).contains("contained=off"));
}

/// True when `/sys/fs/cgroup` is mounted read-only (as in Docker containers).
fn cgroupfs_read_only() -> bool {
    fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .find(|f| f.get(4) == Some(&"/sys/fs/cgroup"))
        .and_then(|f| f.get(5).map(|o| o.split(',').any(|x| x == "ro")))
        .unwrap_or(false)
}

#[test]
fn where_cpu_cannot_be_weighted_the_line_only_renices() {
    // No `systemctl` in PATH: containment is off but `nice` still applies.
    let sb = Sandbox::standard();
    let nopath = [("PATH", "/nonexistent")];
    context(&hook(&sb, &input(&sb.repo, "startup"), &nopath));
    let env = fs::read_to_string(env_file(&sb)).unwrap();
    if cgroupfs_read_only() {
        // Containers mount the cgroup filesystem read-only: detection says
        // off (read-only cgroup filesystem) and, by design (S4 matrix), the
        // hook adds no renice line there either.
        assert_eq!(env.lines().count(), 1, "{env}");
        assert!(!env.contains("renice"), "{env}");
        return;
    }
    assert_eq!(env.lines().count(), 2, "{env}");
    assert_eq!(env.lines().nth(1), Some("renice -n 19 $$ >/dev/null 2>&1"));
    // Twice: still once.
    context(&hook(&sb, &input(&sb.repo, "resume"), &nopath));
    assert_eq!(env_lines_with(&sb, "renice").len(), 1);
    assert_eq!(
        nice_of_shell_after(&format!(". '{}'", env_file(&sb).display())),
        "19"
    );
}

/// The leak the lead found: the hook's stopgap holder must never outlive its
/// scope or the test. The hook creates the scope (and a bounded
/// `sluice daemon holder --lifetime` inside `holder/`); after the tree is
/// dropped nothing may be left under the test's prefix.
#[test]
fn the_hooks_stopgap_holder_is_bounded_and_leaves_nothing_behind() {
    let Some(mut tree) = TestTree::bare() else {
        return;
    };
    let layout = tree.layout.clone();
    let sb = Sandbox::standard();
    let (agent, _) = tree.fake_agent();
    context(&contained_hook(&sb, &tree, "sess-leak", agent));
    let holders: Vec<String> = common::cgtree::procs_under(&layout.holder_dir())
        .into_iter()
        .map(|(_, p)| common::cgtree::cmdline_of(p))
        .collect();
    assert_eq!(
        holders,
        vec![format!(
            "sluice {}",
            sluice_gate_daemon::containment::stopgap_holder_args().join(" ")
        )],
        "the stopgap is the bounded holder, not a bare sleep"
    );
    drop(tree);
    let left = common::cgtree::live_leftovers(&layout);
    assert!(left.is_empty(), "{left:?}");
}

/// The stopgap holder ends by itself at the end of its lifetime.
#[test]
fn a_stopgap_holder_exits_when_its_lifetime_is_over() {
    let sb = Sandbox::standard();
    let mut child = StdCommand::new(Sandbox::binary())
        .args(["daemon", "holder", "--lifetime", "1"])
        .env_clear()
        .envs(sb.env())
        .stdin(Stdio::null())
        .spawn()
        .expect("holder starts");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status:?}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the holder outlived its lifetime");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
