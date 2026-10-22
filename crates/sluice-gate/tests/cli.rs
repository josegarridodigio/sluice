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
        .args(["run", "test"])
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
