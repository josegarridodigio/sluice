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
