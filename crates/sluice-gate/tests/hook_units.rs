//! Wave H2a: when git runs a husky or plain hook through Sluice's entry point,
//! Sluice runs the hook's units itself (queue, budget, cache) if it
//! understands the whole file, and delegates otherwise (ADR-0021).
//!
//! Fake tools named like real ones (`eslint`, `jest`, `lint-staged`) are
//! committed under `node_modules/.bin` (husky puts it first on `PATH`); they
//! append a line to the sandbox counter and exit with the code in their first
//! argument.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command as StdCommand, Output};

use common::{NO_AUTO_MAINTENANCE, Sandbox};
use serde_json::Value;

/// husky 9.1.7's `_/h`, verbatim in behaviour.
const HUSKY_H: &str = r#"[ "$HUSKY" = "2" ] && set -x
n=$(basename "$0")
s=$(dirname "$(dirname "$0")")/$n

[ ! -f "$s" ] && exit 0

if [ -f "$HOME/.huskyrc" ]; then
	echo "husky - '~/.huskyrc' is DEPRECATED, please move your code to ~/.config/husky/init.sh"
fi
i="${XDG_CONFIG_HOME:-$HOME/.config}/husky/init.sh"
[ -f "$i" ] && . "$i"

[ "${HUSKY-}" = "0" ] && exit 0

export PATH="node_modules/.bin:$PATH"
sh -e "$s" "$@"
c=$?

[ $c != 0 ] && echo "husky - $n script failed (code $c)"
[ $c = 127 ] && echo "husky - command not found in PATH=$PATH"
exit $c
"#;

/// A fake tool: logs its name, arguments and (for the index test) the git
/// index variable, then exits with its first argument when it is a number.
const FAKE_TOOL: &str = r#"#!/bin/sh
echo "$(basename "$0") $* index=${GIT_INDEX_FILE:+set}" >> "$SLUICE_TEST_COUNTER"
case "$1" in
  [0-9]*) exit "$1" ;;
esac
exit 0
"#;

fn exe(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn commit_all(sb: &Sandbox, message: &str) {
    sb.git(&["add", "-A"]);
    // The setup commit runs the repository's own hooks directly: not counted.
    sb.git(&["commit", "-q", "--no-verify", "-m", message]);
}

/// A repository with husky's layout (generated `_` directory, user hook files)
/// and the fake tools, committed before the entry point is switched on.
fn husky_repo(hooks: &[(&str, &str)]) -> Sandbox {
    let sb = Sandbox::standard();
    let dir = sb.repo.join(".husky");
    exe(&dir.join("_/h"), HUSKY_H);
    fs::write(dir.join("_/.gitignore"), "*\n").unwrap();
    for (hook, text) in hooks {
        exe(
            &dir.join("_").join(hook),
            "#!/usr/bin/env sh\n. \"$(dirname \"$0\")/h\"",
        );
        fs::write(dir.join(hook), text).unwrap();
    }
    for tool in ["eslint", "jest", "lint-staged", "tsc"] {
        exe(&sb.repo.join("node_modules/.bin").join(tool), FAKE_TOOL);
    }
    sb.git(&["config", "core.hooksPath", ".husky/_"]);
    commit_all(&sb, "husky");
    sb.sluice().args(["hooks", "on"]).assert().success();
    sb
}

fn hooks_dir(sb: &Sandbox) -> std::path::PathBuf {
    let common = sb.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    Path::new(&common).join("sluice/hooks")
}

/// Runs Sluice's script for `hook` as git would (no git environment).
fn run_hook(sb: &Sandbox, hook: &str, extra: &[(&str, &str)]) -> Output {
    StdCommand::new(hooks_dir(sb).join(hook))
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .envs(extra.iter().copied())
        .output()
        .expect("hook")
}

/// `git commit` with a changed file, through the entry point.
fn commit(sb: &Sandbox, extra: &[(&str, &str)]) -> Output {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    fs::write(sb.repo.join("change.txt"), format!("{nanos}\n")).unwrap();
    git(sb, &["add", "-A"], extra);
    git(sb, &["commit", "-q", "-m", "change"], extra)
}

fn git(sb: &Sandbox, args: &[&str], extra: &[(&str, &str)]) -> Output {
    StdCommand::new("git")
        .args(NO_AUTO_MAINTENANCE)
        .args(args)
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .envs(extra.iter().copied())
        .output()
        .expect("git")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn counter(sb: &Sandbox) -> Vec<String> {
    fs::read_to_string(&sb.counter)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn status(sb: &Sandbox, extra: &[(&str, &str)]) -> Value {
    let out = sb
        .sluice()
        .envs(extra.iter().copied())
        .args(["hooks", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

fn hook_entry<'a>(status: &'a Value, hook: &str) -> &'a Value {
    status["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["hook"] == hook)
        .unwrap_or_else(|| panic!("no hook {hook} in {status}"))
}

const RUNNING: &str = "unit(s) of the husky hook through the queue";

#[test]
fn units_run_through_the_daemon_and_the_same_tree_is_answered_from_the_cache() {
    let sb = husky_repo(&[("pre-commit", "eslint .\n")]);
    let first = commit(&sb, &[]);
    assert!(first.status.success(), "{}", text(&first));
    let log = text(&first);
    assert!(log.contains(RUNNING), "{log}");
    assert!(log.contains("husky:pre-commit#1 running (job"), "{log}");
    assert_eq!(
        counter(&sb),
        ["eslint . index="],
        "check ran once, sees no index"
    );
    assert_eq!(sb.marker_count(), 1);

    // The same working tree again: no run, one cached line.
    let second = git(&sb, &["commit", "-q", "--allow-empty", "-m", "again"], &[]);
    assert!(second.status.success(), "{}", text(&second));
    assert!(
        text(&second).contains("husky:pre-commit#1 PASS (cached"),
        "{}",
        text(&second)
    );
    assert_eq!(counter(&sb).len(), 1);

    // A different tree runs it again.
    let third = commit(&sb, &[]);
    assert!(third.status.success());
    assert_eq!(counter(&sb).len(), 2);
}

#[test]
fn a_mutating_unit_always_runs_and_runs_before_a_later_check_in_file_order() {
    let sb = husky_repo(&[("pre-commit", "lint-staged\neslint .\n")]);
    assert!(commit(&sb, &[]).status.success());
    assert_eq!(
        counter(&sb),
        ["lint-staged  index=set", "eslint . index="],
        "file order; the mutating unit keeps git's index variable, the check does not"
    );
    let again = git(&sb, &["commit", "-q", "--allow-empty", "-m", "again"], &[]);
    assert!(again.status.success(), "{}", text(&again));
    let lines = counter(&sb);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[2].starts_with("lint-staged"), "{lines:?}");
    assert!(text(&again).contains("husky:pre-commit#2 PASS (cached"));
}

#[test]
fn the_first_failing_unit_stops_the_hook_with_its_own_exit_code_and_is_not_cached() {
    let sb = husky_repo(&[("pre-commit", "eslint 7\njest ok\n")]);
    let out = run_hook(&sb, "pre-commit", &[]);
    assert_eq!(out.status.code(), Some(7), "{}", text(&out));
    assert!(text(&out).contains("husky - pre-commit script failed (code 7)"));
    assert!(text(&out).contains("FAIL exit=7"), "{}", text(&out));
    assert_eq!(counter(&sb), ["eslint 7 index="], "jest never ran");
    run_hook(&sb, "pre-commit", &[]);
    assert_eq!(counter(&sb).len(), 2, "a failure is never cached");
    assert_eq!(sb.marker_count(), 0);

    let refused = commit(&sb, &[]);
    assert!(!refused.status.success(), "git refuses the commit");
}

#[test]
fn a_unit_that_exits_3_is_its_own_failure_not_a_sluice_refusal() {
    let sb = husky_repo(&[("pre-commit", "eslint 3\n")]);
    let out = run_hook(&sb, "pre-commit", &[("CLAUDECODE", "1")]);
    let log = text(&out);
    assert_eq!(out.status.code(), Some(3), "{log}");
    assert!(log.contains("FAIL exit=3"), "{log}");
    assert!(!log.contains("has not passed on this tree"), "{log}");
    assert!(!log.contains("fail-fast"), "{log}");
    assert_eq!(counter(&sb).len(), 1, "an agent's hook still runs the unit");
}

#[test]
fn and_lists_follow_sh_e_a_non_final_failure_skips_the_rest_of_the_list_only() {
    let sb = husky_repo(&[
        ("pre-commit", "eslint 5 && jest ok\ntsc ok\n"),
        ("commit-msg", "jest ok && eslint 6\ntsc ok\n"),
    ]);
    let out = run_hook(&sb, "pre-commit", &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(counter(&sb), ["eslint 5 index=", "tsc ok index="]);

    let out = run_hook(&sb, "commit-msg", &[]);
    assert_eq!(out.status.code(), Some(6), "{}", text(&out));
    let lines = counter(&sb);
    assert_eq!(
        &lines[2..],
        ["jest ok index=", "eslint 6 index="],
        "{lines:?}"
    );
}

#[test]
fn a_trailing_exit_0_makes_the_hook_pass() {
    let sb = husky_repo(&[("pre-commit", "eslint 5 && jest ok\nexit 0\n")]);
    let out = run_hook(&sb, "pre-commit", &[]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
}

#[test]
fn a_hook_with_stdin_runs_as_units_only_when_every_unit_is_a_check() {
    let sb = husky_repo(&[("pre-push", "eslint .\njest ok\n")]);
    let out = run_hook(&sb, "pre-push", &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains(RUNNING), "{}", text(&out));
    assert_eq!(counter(&sb).len(), 2);

    // A mutating unit might read stdin: the whole hook is delegated.
    fs::write(sb.repo.join(".husky/pre-push"), "eslint .\nlint-staged\n").unwrap();
    let entry = status(&sb, &[]);
    let reason = hook_entry(&entry, "pre-push")["reason"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        reason.contains("stdin") && reason.contains("lint-staged"),
        "{reason}"
    );
    let before = counter(&sb).len();
    let out = run_hook(&sb, "pre-push", &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains(RUNNING), "{}", text(&out));
    assert_eq!(counter(&sb).len(), before + 2, "delegation still runs both");

    // A single unknown program is delegated too.
    fs::write(sb.repo.join(".husky/pre-push"), "./scripts/check.sh\n").unwrap();
    let entry = status(&sb, &[]);
    assert_eq!(hook_entry(&entry, "pre-push")["mode"], "delegate");
}

#[test]
fn a_hook_that_needs_a_shell_is_delegated_byte_for_byte() {
    let script = "if [ -z \"$CI\" ]; then\n  eslint 4\nfi\n";
    let sb = husky_repo(&[("pre-commit", script)]);
    let entry = status(&sb, &[]);
    assert_eq!(hook_entry(&entry, "pre-commit")["mode"], "delegate");
    let out = run_hook(&sb, "pre-commit", &[]);
    assert_eq!(out.status.code(), Some(4));
    let log = text(&out);
    assert!(!log.contains(RUNNING) && !log.contains("sluice:"), "{log}");
    assert!(log.contains("husky - pre-commit script failed (code 4)"));
    assert_eq!(counter(&sb).len(), 1);
}

#[test]
fn without_a_daemon_the_hook_is_delegated() {
    let sb = husky_repo(&[("pre-commit", "eslint .\n")]);
    let out = run_hook(
        &sb,
        "pre-commit",
        &[("SLUICE_SOCKET", "/dev/null/impossible")],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains(RUNNING), "{}", text(&out));
    assert_eq!(counter(&sb).len(), 1);
    assert_eq!(sb.marker_count(), 0);
}

#[test]
fn the_environment_opt_out_forces_delegation() {
    let sb = husky_repo(&[("pre-commit", "eslint .\n")]);
    let out = run_hook(&sb, "pre-commit", &[("SLUICE_HOOKS_DELEGATE", "1")]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains(RUNNING), "{}", text(&out));
    assert_eq!(counter(&sb).len(), 1);
    let entry = status(&sb, &[("SLUICE_HOOKS_DELEGATE", "1")]);
    let hook = hook_entry(&entry, "pre-commit");
    assert_eq!(hook["mode"], "delegate");
    assert!(
        hook["reason"]
            .as_str()
            .unwrap()
            .contains("SLUICE_HOOKS_DELEGATE")
    );
}

#[test]
fn status_lists_every_hook_with_its_units_and_kinds() {
    let sb = husky_repo(&[
        ("pre-commit", "lint-staged\nnpm run missing\n"),
        ("commit-msg", "commitlint --edit $1\n"),
    ]);
    let entry = status(&sb, &[]);
    let hook = hook_entry(&entry, "pre-commit");
    assert_eq!(hook["mode"], "units");
    assert_eq!(hook["units"][0]["kind"], "mutating");
    assert_eq!(hook["units"][0]["command"], "lint-staged");
    assert_eq!(hook["units"][1]["index"], 2);
    let msg = hook_entry(&entry, "commit-msg");
    assert_eq!(msg["mode"], "delegate");
    assert!(
        msg["reason"].as_str().unwrap().contains("arguments"),
        "{msg}"
    );

    let out = sb.sluice().args(["hooks", "status"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("hook pre-commit: 2 unit(s) run through the queue"),
        "{text}"
    );
    assert!(text.contains("hook commit-msg: delegated"), "{text}");
}

#[test]
fn plain_hooks_with_set_e_run_as_units_and_a_hook_that_calls_sluice_is_delegated() {
    let sb = Sandbox::standard();
    for tool in ["eslint", "jest"] {
        exe(&sb.repo.join("tools").join(tool), FAKE_TOOL);
    }
    exe(
        &sb.repo.join(".githooks/pre-commit"),
        "#!/bin/sh\nset -e\n./tools/eslint .\n./tools/jest ok\n",
    );
    sb.git(&["config", "core.hooksPath", ".githooks"]);
    commit_all(&sb, "hooks");
    sb.sluice().args(["hooks", "on"]).assert().success();
    let out = run_hook(&sb, "pre-commit", &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("unit(s) of the plain hooks hook"),
        "{}",
        text(&out)
    );
    assert_eq!(counter(&sb), ["eslint . index=", "jest ok index="]);
    assert!(text(&run_hook(&sb, "pre-commit", &[])).contains("plain:pre-commit#1 PASS (cached"));

    exe(
        &sb.repo.join(".githooks/pre-commit"),
        &format!(
            "#!/bin/sh\nset -e\n'{}' run --hook pre-commit test\n",
            Sandbox::binary().display()
        ),
    );
    let entry = status(&sb, &[]);
    assert!(
        hook_entry(&entry, "pre-commit")["reason"]
            .as_str()
            .unwrap()
            .contains("already runs sluice")
    );
}
