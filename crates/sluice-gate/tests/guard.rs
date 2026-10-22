//! `sluice guard` end to end: the built binary with hook JSON on stdin, and
//! the plugin's `plugin/hooks/test-guard.sh` (shell fallback and Rust passes).
//! Test plan: `kb/sluice/design/command-control.md` section 7, A2.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::{Value, json};

struct Env {
    root: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for d in ["repo/sub", "home"] {
            fs::create_dir_all(root.path().join(d)).unwrap();
        }
        fs::write(
            root.path().join("repo/.sluice.toml"),
            "version = 1\n[gates.test]\ncmd = \"cargo test --workspace\"\n",
        )
        .unwrap();
        Self { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn guard(&self) -> Command {
        let mut cmd = Command::cargo_bin("sluice").expect("binary built by cargo");
        cmd.arg("guard")
            .arg("--hook")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.path("home"))
            .env("CLAUDE_PROJECT_DIR", self.path("repo"))
            .env("SLUICE_CONFIG", self.path("home/sluice.toml"))
            // A loaded CI machine must not hit the 500 ms watchdog.
            .env("SLUICE_TEST_GUARD_WATCHDOG_MS", "20000");
        cmd
    }

    /// Runs the guard; asserts exit 0 and an empty stderr-independent result.
    fn answer(&self, input: &str, envs: &[(&str, &str)]) -> Option<Value> {
        let mut cmd = self.guard();
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.write_stdin(input).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "guard must always exit 0");
        let stdout = String::from_utf8(out.stdout).unwrap();
        if stdout.is_empty() {
            return None;
        }
        assert!(
            stdout.ends_with('\n') && stdout.matches('\n').count() == 1,
            "{stdout:?}"
        );
        Some(serde_json::from_str(&stdout).unwrap())
    }

    fn hook(cwd: &Path, command: &str, mode: &str) -> String {
        json!({
            "session_id": "abc",
            "transcript_path": "/dev/null",
            "cwd": cwd,
            "permission_mode": mode,
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {
                "command": command,
                "description": "Build it",
                "timeout": 120_000,
                "run_in_background": false
            }
        })
        .to_string()
    }
}

#[test]
fn rewrites_a_heavy_command_with_allow_in_bypass_mode() {
    let env = Env::new();
    let v = env
        .answer(
            &Env::hook(
                &env.path("repo/sub"),
                "make -j4 2>&1 | tail -3",
                "bypassPermissions",
            ),
            &[],
        )
        .unwrap();
    let h = &v["hookSpecificOutput"];
    assert_eq!(h["hookEventName"], "PreToolUse");
    assert_eq!(h["permissionDecision"], "allow");
    assert_eq!(
        h["updatedInput"],
        json!({
            "command": "sluice exec --label make -- make -j4 2>&1 | tail -3",
            "description": "Build it",
            "timeout": 120_000,
            "run_in_background": false
        })
    );
    assert!(
        h["additionalContext"]
            .as_str()
            .unwrap()
            .starts_with("Sluice ran this through its shared queue as: `sluice exec")
    );
}

#[test]
fn rewrites_a_gate_with_ask_in_default_mode() {
    let env = Env::new();
    let v = env
        .answer(
            &Env::hook(
                &env.path("repo"),
                "cargo test --workspace -- --nocapture",
                "default",
            ),
            &[],
        )
        .unwrap();
    let h = &v["hookSpecificOutput"];
    assert_eq!(h["permissionDecision"], "ask");
    assert_eq!(
        h["updatedInput"]["command"],
        "sluice run test -v -- -- --nocapture"
    );
    assert!(
        h["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("sluice run test -v")
    );
    assert!(
        h["additionalContext"]
            .as_str()
            .unwrap()
            .contains("as gate 'test'")
    );
}

#[test]
fn respects_deny_rules_and_keeps_the_legacy_deny() {
    let env = Env::new();
    fs::create_dir_all(env.path("repo/.claude")).unwrap();
    fs::write(
        env.path("repo/.claude/settings.json"),
        r#"{"permissions":{"deny":["Bash(cargo test:*)"]}}"#,
    )
    .unwrap();
    let v = env
        .answer(
            &Env::hook(
                &env.path("repo"),
                "cargo test --workspace",
                "bypassPermissions",
            ),
            &[],
        )
        .unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(
        v["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .contains("gates=[\"test\"]")
    );
    // A heavy command with nothing to deny it is still rewritten.
    let v = env
        .answer(
            &Env::hook(&env.path("repo"), "cargo build", "bypassPermissions"),
            &[],
        )
        .unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["updatedInput"]["command"],
        "sluice exec --label cargo-build -- cargo build"
    );
}

#[test]
fn denies_approve_and_stays_silent_otherwise() {
    let env = Env::new();
    let v = env
        .answer(
            &Env::hook(
                &env.path("home"),
                "sluice queue approve t-1 --yes",
                "bypassPermissions",
            ),
            &[],
        )
        .unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecisionReason"],
        "Approval is reserved to humans."
    );
    for input in [
        String::new(),
        "{not json".to_owned(),
        "\u{0}\u{1}binary".to_owned(),
        Env::hook(&env.path("repo"), "ls -la", "default"),
        Env::hook(
            &env.path("repo"),
            "cat <<EOF\nmake\nEOF",
            "bypassPermissions",
        ),
        Env::hook(&env.path("repo"), "sh -c 'make'", "bypassPermissions"),
        Env::hook(&env.path("home"), "make", "plan"),
    ] {
        assert_eq!(env.answer(&input, &[]), None, "{input}");
    }
}

#[test]
fn guard_env_switches() {
    let env = Env::new();
    let input = Env::hook(&env.path("home"), "make", "bypassPermissions");
    assert_eq!(env.answer(&input, &[("SLUICE_GUARD", "0")]), None);
    let v = env.answer(&input, &[("SLUICE_GUARD", "deny")]).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecisionReason"],
        "This machine shares its CPU through Sluice: run `sluice exec --label make -- make` instead (same output and exit code)."
    );
    // A broken user config disables rewrites, never the hook.
    fs::write(
        env.path("home/sluice.toml"),
        "version = 1\n[agent]\nnope = 1\n",
    )
    .unwrap();
    assert_eq!(env.answer(&input, &[]), None);
}

#[test]
fn rewritten_gate_command_parses_as_sluice_run() {
    // `sluice run <gate> -v -- -- --nocapture` must hand `-- --nocapture` to
    // the gate as extra arguments; `--explain` shows them without running.
    let out = Command::cargo_bin("sluice")
        .unwrap()
        .args(["run", "test", "-v", "--explain", "--", "--", "--nocapture"])
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("unexpected argument"), "{text}");
}

#[test]
fn plugin_test_guard_script_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = root.join("plugin/hooks/test-guard.sh");
    let bin = assert_cmd::cargo::cargo_bin("sluice");
    let out = StdCommand::new("sh")
        .arg(&script)
        .env("SLUICE_BIN", &bin)
        .env("SLUICE_TEST_GUARD_WATCHDOG_MS", "20000")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "test-guard.sh failed:\n{stdout}");
    assert!(
        stdout.contains("== Rust guard ("),
        "the Rust pass must run:\n{stdout}"
    );
    assert!(stdout.contains(", 0 failure(s)"), "{stdout}");
}
