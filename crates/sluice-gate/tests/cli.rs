//! Smoke tests for the `sluice` command surface.

use assert_cmd::Command;
use predicates::prelude::*;

fn sluice() -> Command {
    Command::cargo_bin("sluice").expect("binary built by cargo")
}

#[test]
fn help_lists_every_top_level_command() {
    let assert = sluice().arg("--help").assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 help");
    for command in [
        "run", "mcp", "daemon", "status", "cache", "init", "hook", "logs",
    ] {
        assert!(
            out.contains(command),
            "help output should mention '{command}'"
        );
    }
}

#[test]
fn version_prints_crate_version() {
    sluice()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unimplemented_command_exits_with_70() {
    sluice()
        .args(["mcp"])
        .assert()
        .code(70)
        .stderr(predicate::str::contains("not implemented"));
}

#[test]
fn unknown_hook_manager_is_rejected() {
    sluice()
        .args(["hook", "print", "nope"])
        .assert()
        .failure()
        .code(2);
}

fn in_dir(dir: &tempfile::TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
    sluice().current_dir(dir.path()).args(args).assert()
}

#[test]
fn hook_print_without_config_shows_example() {
    let dir = tempfile::tempdir().expect("tempdir");
    in_dir(&dir, &["hook", "print", "husky"])
        .success()
        .stdout(predicate::str::contains("# No .sluice.toml found here"))
        .stdout(predicate::str::contains(
            "sluice run --hook pre-commit test || exit $?",
        ));
}

#[test]
fn init_creates_config_and_refuses_overwrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    in_dir(&dir, &["init"])
        .success()
        .stdout(predicate::str::contains("sluice: wrote .sluice.toml"))
        .stdout(predicate::str::contains(
            "git config core.hooksPath .githooks",
        ));
    assert!(dir.path().join(".sluice.toml").is_file());
    in_dir(&dir, &["init"])
        .code(1)
        .stderr(predicate::str::contains("already exists; not overwriting"));
    in_dir(&dir, &["init", "--force"]).success();
}

#[test]
fn hook_print_after_init_uses_config_hooks() {
    let dir = tempfile::tempdir().expect("tempdir");
    in_dir(&dir, &["init"]).success();
    let assert = in_dir(&dir, &["hook", "print", "pre-commit"]).success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8");
    assert!(!out.contains("No .sluice.toml"));
    assert_eq!(out.matches("id: sluice-lint").count(), 1);
    assert_eq!(out.matches("id: sluice-test").count(), 2);
    assert_eq!(out.matches("stages: [pre-commit]").count(), 2);
    assert_eq!(out.matches("stages: [pre-push]").count(), 1);
}

#[test]
fn hook_print_lefthook_and_plain_have_expected_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    in_dir(&dir, &["init"]).success();
    in_dir(&dir, &["hook", "print", "lefthook"])
        .success()
        .stdout(predicate::str::contains("pre-push:\n  commands:"))
        .stdout(predicate::str::contains(
            "run: sluice run --hook pre-commit lint",
        ));
    in_dir(&dir, &["hook", "print", "plain"])
        .success()
        .stdout(predicate::str::contains("#!/bin/sh"))
        .stdout(predicate::str::contains("chmod +x .githooks/*"));
}
