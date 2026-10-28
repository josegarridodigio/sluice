//! Wave H2b: when git runs a lefthook or pre-commit hook through Sluice's
//! entry point, Sluice runs the manager's units itself (each one through the
//! manager's own CLI, in the queue, with the cache) if it understands the
//! whole hook, and delegates otherwise (ADR-0021).
//!
//! The managers are fake executables that print what Sluice asks for
//! (`lefthook dump`, `version`) and append the arguments of every `run` to the
//! sandbox counter, so the tests can check argv, order, caching and exit codes
//! without the real tools. The real tools run only when they are on `PATH`
//! (the CI installs them).

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};

use common::{NO_AUTO_MAINTENANCE, Sandbox};
use serde_json::Value;

/// A fake `lefthook`: `version` and `dump` answer from files next to the
/// counter, `run` records its arguments and exits with the code listed for
/// its `--command` in `<counter>.codes` (lines `<name> <code>`).
const FAKE_LEFTHOOK: &str = r#"#!/bin/sh
case "$1" in
  version) cat "$SLUICE_TEST_COUNTER.version" 2>/dev/null || echo 2.2.2; exit 0 ;;
  dump) cat "$SLUICE_TEST_COUNTER.dump"; exit $? ;;
  run)
    shift
    echo "lefthook run $* index=${GIT_INDEX_FILE:+set}" >> "$SLUICE_TEST_COUNTER"
    job=""; prev=""
    for a in "$@"; do [ "$prev" = "--command" ] && job="$a"; prev="$a"; done
    code=$(sed -n "s/^$job //p" "$SLUICE_TEST_COUNTER.codes" 2>/dev/null | head -1)
    exit "${code:-0}" ;;
esac
exit 64
"#;

/// The Python of a pre-commit stub: `-mpre_commit --version` and `run`,
/// recorded like the fake lefthook, codes keyed by hook id.
const FAKE_PYTHON: &str = r#"#!/bin/sh
shift
case "$1" in
  --version) echo "pre-commit 4.7.0"; exit 0 ;;
  run)
    echo "pre-commit $* index=${GIT_INDEX_FILE:+set}" >> "$SLUICE_TEST_COUNTER"
    code=$(sed -n "s/^$2 //p" "$SLUICE_TEST_COUNTER.codes" 2>/dev/null | head -1)
    exit "${code:-0}" ;;
esac
exit 64
"#;

fn exe(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn bin_dir(sb: &Sandbox) -> PathBuf {
    sb.root.path().join("bin")
}

/// Files next to the counter that the fakes read.
fn side(sb: &Sandbox, ext: &str) -> PathBuf {
    PathBuf::from(format!("{}.{ext}", sb.counter.display()))
}

fn set_codes(sb: &Sandbox, codes: &str) {
    fs::write(side(sb, "codes"), codes).unwrap();
}

/// `PATH` with the fakes first, as extra environment.
fn with_fakes(sb: &Sandbox) -> (String, String) {
    let path = std::env::var("PATH").unwrap_or_default();
    ("PATH".into(), format!("{}:{path}", bin_dir(sb).display()))
}

fn hooks_dir(sb: &Sandbox) -> PathBuf {
    let common = sb.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    Path::new(&common).join("sluice/hooks")
}

fn commit_all(sb: &Sandbox, message: &str) {
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "--no-verify", "-m", message]);
}

/// Stages a new file, so the index differs from HEAD (a `pre-commit` hook
/// runs with something to commit).
fn stage_change(sb: &Sandbox) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    fs::write(sb.repo.join("change.txt"), format!("{nanos}\n")).unwrap();
    sb.git(&["add", "change.txt"]);
}

fn run_hook(sb: &Sandbox, hook: &str, extra: &[(&str, &str)]) -> Output {
    let (k, v) = with_fakes(sb);
    StdCommand::new(hooks_dir(sb).join(hook))
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env(k, v)
        .envs(extra.iter().copied())
        .output()
        .expect("hook")
}

fn git(sb: &Sandbox, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let (k, v) = with_fakes(sb);
    StdCommand::new("git")
        .args(NO_AUTO_MAINTENANCE)
        .args(args)
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env(k, v)
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
    let (k, v) = with_fakes(sb);
    let out = sb
        .sluice()
        .env(k, v)
        .envs(extra.iter().copied())
        .args(["hooks", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

fn hook_entry(status: &Value, hook: &str) -> Value {
    status["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["hook"] == hook)
        .unwrap_or_else(|| panic!("no hook {hook} in {status}"))
        .clone()
}

/// The reason a hook is delegated; panics when it runs as units.
fn reason(sb: &Sandbox, hook: &str) -> String {
    let entry = hook_entry(&status(sb, &[]), hook);
    assert_eq!(entry["mode"], "delegate", "{entry}");
    entry["reason"].as_str().unwrap().to_owned()
}

// ------------------------------------------------------------------ lefthook

const MARK: &str = "delegated-to-the-lefthook-script";

/// A repository whose hooks are lefthook's (a generated-looking script that
/// records when it is reached, i.e. when Sluice delegates) and a fake
/// `lefthook` that dumps `dump`.
fn lefthook_repo(dump: &Value) -> Sandbox {
    let sb = Sandbox::standard();
    exe(&bin_dir(&sb).join("lefthook"), FAKE_LEFTHOOK);
    fs::write(side(&sb, "dump"), dump.to_string()).unwrap();
    for hook in ["pre-commit", "commit-msg", "post-rewrite", "pre-push"] {
        exe(
            &sb.repo.join(".git/hooks").join(hook),
            &format!("#!/bin/sh\n# LEFTHOOK\necho {MARK} >> \"$SLUICE_TEST_COUNTER\"\n"),
        );
    }
    sb.sluice().args(["hooks", "on"]).assert().success();
    stage_change(&sb);
    sb
}

fn argv(hook: &str, job: &str) -> String {
    format!("lefthook run {hook} --command {job} --no-auto-install --skip-lfs index=")
}

const RUNNING_LEFTHOOK: &str = "unit(s) of the lefthook hook through the queue";

#[test]
fn lefthook_commands_run_as_units_with_exact_argv_and_only_checks_are_cached() {
    let sb = lefthook_repo(&serde_json::json!({
        "pre-commit": {"parallel": true, "commands": {
            "fmt": {"run": "prettier --write src"},
            "unit": {"run": "jest --ci", "env": {"CI": "1"}, "timeout": "5m"},
        }}
    }));
    let out = run_hook(&sb, "pre-commit", &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains(RUNNING_LEFTHOOK), "{}", text(&out));
    assert_eq!(
        counter(&sb),
        [argv("pre-commit", "fmt"), argv("pre-commit", "unit")]
    );
    assert_eq!(sb.marker_count(), 1, "only the check is recorded");

    let again = run_hook(&sb, "pre-commit", &[]);
    assert!(again.status.success(), "{}", text(&again));
    assert!(
        text(&again).contains("lefthook:pre-commit#2 PASS (cached"),
        "{}",
        text(&again)
    );
    assert_eq!(
        counter(&sb).len(),
        3,
        "the formatter ran again, the check did not"
    );
    assert!(counter(&sb)[2].contains("--command fmt"));
}

#[test]
fn a_change_of_the_effective_configuration_is_a_different_cache_entry() {
    let sb = lefthook_repo(&serde_json::json!({
        "pre-commit": {"commands": {"unit": {"run": "jest --ci"}}}
    }));
    assert!(run_hook(&sb, "pre-commit", &[]).status.success());
    fs::write(
        side(&sb, "dump"),
        r#"{"pre-commit":{"commands":{"unit":{"run":"jest --ci --bail"}}}}"#,
    )
    .unwrap();
    assert!(run_hook(&sb, "pre-commit", &[]).status.success());
    assert_eq!(counter(&sb).len(), 2, "the dump is part of the key");
}

#[test]
fn jobs_keep_their_order_then_commands_by_priority_number_and_name() {
    let sb = lefthook_repo(&serde_json::json!({
        "pre-commit": {
            "jobs": [{"name": "b", "run": "jest"}, {"name": "a", "run": "tsc"}],
            "commands": {
                "z": {"run": "vitest"},
                "10-y": {"run": "mocha"},
                "2-x": {"run": "eslint ."},
            },
        }
    }));
    assert!(run_hook(&sb, "pre-commit", &[]).status.success());
    let jobs: Vec<String> = counter(&sb)
        .iter()
        .map(|l| {
            l.split("--command ")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(jobs, ["b", "a", "2-x", "10-y", "z"]);
}

#[test]
fn a_failing_command_does_not_stop_the_others_and_the_hook_exits_1() {
    let sb = lefthook_repo(&serde_json::json!({
        "pre-commit": {"commands": {
            "a": {"run": "eslint ."}, "b": {"run": "jest"}, "c": {"run": "tsc"},
        }}
    }));
    set_codes(&sb, "a 1\n");
    let out = run_hook(&sb, "pre-commit", &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(counter(&sb).len(), 3, "sequential and keeps going");
    // A failure is never cached: the next run executes `a` again.
    run_hook(&sb, "pre-commit", &[]);
    let lines = counter(&sb);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[3].contains("--command a"));
}

#[test]
fn another_exit_code_ends_the_hook_at_once_and_a_3_is_the_units_own() {
    let sb = lefthook_repo(&serde_json::json!({
        "pre-commit": {"commands": {"a": {"run": "eslint ."}, "b": {"run": "jest"}}}
    }));
    set_codes(&sb, "a 3\n");
    let out = run_hook(&sb, "pre-commit", &[("CLAUDECODE", "1")]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(!text(&out).contains("has not passed on this tree"));
    assert_eq!(counter(&sb).len(), 1, "b never ran");
}

#[test]
fn what_lefthook_would_skip_or_a_per_command_run_would_change_is_delegated() {
    let cases = [
        (r#"{"pre-commit":{"jobs":[{"run":"jest"}]}}"#, "no name"),
        (
            r#"{"pre-commit":{"scripts":{"a.sh":{"runner":"bash"}}}}"#,
            "`scripts`",
        ),
        (
            r#"{"pre-commit":{"setup":[{"run":"x"}],"commands":{"a":{"run":"jest"}}}}"#,
            "`setup`",
        ),
        (
            r#"{"pre-commit":{"piped":true,"commands":{"a":{"run":"jest"}}}}"#,
            "`piped`",
        ),
        (
            r#"{"pre-commit":{"follow":true,"commands":{"a":{"run":"jest"}}}}"#,
            "`follow`",
        ),
        (
            r#"{"pre-commit":{"exclude_tags":["x"],"commands":{"a":{"run":"jest"}}}}"#,
            "`exclude_tags`",
        ),
        (
            r#"{"pre-commit":{"skip":["merge"],"commands":{"a":{"run":"jest"}}}}"#,
            "`skip`",
        ),
        (
            r#"{"pre-commit":{"commands":{"a":{"run":"jest","only":[{"ref":"main"}]}}}}"#,
            "`only`",
        ),
        (
            r#"{"pre-commit":{"commands":{"a":{"run":"jest {staged_files}"}}}}"#,
            "template",
        ),
        (
            r#"{"pre-commit":{"commands":{"a":{"run":"jest","glob":"*.ts"}}}}"#,
            "`glob`",
        ),
        (
            r#"{"pre-commit":{"commands":{"a":{"run":"jest","stage_fixed":true}}}}"#,
            "`stage_fixed`",
        ),
        (
            r#"{"pre-commit":{"commands":{"a":{"run":"jest","use_stdin":true}}}}"#,
            "`use_stdin`",
        ),
        (
            r#"{"pre-commit":{"commands":{"a":{"run":"jest","interactive":true}}}}"#,
            "`interactive`",
        ),
        (
            r#"{"pre-commit":{"jobs":[{"name":"g","group":{"jobs":[{"run":"x"}]}}]}}"#,
            "`group`",
        ),
        (
            r#"{"pre-commit":{"commands":{"a":{"run":"jest"}}},"remotes":[{"git_url":"u"}]}"#,
            "`remotes`",
        ),
        (
            r#"{"pre-commit":{"commands":{"a":{"run":"jest"}}},"rc":"~/.rc"}"#,
            "`rc`",
        ),
        (
            r#"{"pre-commit":{"commands":{"1-a":{"run":"jest"},"1-b":{"run":"tsc"}}}}"#,
            "order",
        ),
        (
            r#"{"pre-commit":{"commands":{"a":{"run":"jest"}},"nonsense":1}}"#,
            "unknown option",
        ),
        (
            r#"{"pre-push":{"commands":{"a":{"run":"jest"}}}}"#,
            "files to push",
        ),
        (
            r#"{"commit-msg":{"commands":{"a":{"run":"x {1}"}}}}"#,
            "template",
        ),
        (r#"{"pre-commit":{}}"#, "no job"),
        (
            r#"{"post-rewrite":{"commands":{"a":{"run":"./scripts/x.sh"}}}}"#,
            "stdin",
        ),
    ];
    let sb = lefthook_repo(&serde_json::json!({}));
    for (dump, expected) in cases {
        fs::write(side(&sb, "dump"), dump).unwrap();
        let hook = if dump.contains("\"pre-push\"") {
            "pre-push"
        } else if dump.contains("\"commit-msg\"") {
            "commit-msg"
        } else if dump.contains("\"post-rewrite\"") {
            "post-rewrite"
        } else {
            "pre-commit"
        };
        let why = reason(&sb, hook);
        assert!(why.contains(expected), "{dump}: {why}");
    }
}

#[test]
fn a_hook_that_receives_stdin_runs_as_units_when_every_unit_is_a_check() {
    let sb = lefthook_repo(&serde_json::json!({
        "post-rewrite": {"commands": {"a": {"run": "jest"}}}
    }));
    let entry = hook_entry(&status(&sb, &[]), "post-rewrite");
    assert_eq!(entry["mode"], "units", "{entry}");
    assert_eq!(entry["units"][0]["kind"], "check");
}

#[test]
fn lefthook_is_delegated_when_it_cannot_be_found_asked_or_is_not_a_known_version() {
    let sb = lefthook_repo(&serde_json::json!({
        "pre-commit": {"commands": {"a": {"run": "jest"}}}
    }));
    assert_eq!(hook_entry(&status(&sb, &[]), "pre-commit")["mode"], "units");

    // The hook script's other ways to find the binary are not modelled.
    let path = std::env::var("PATH").unwrap_or_default();
    let entry = hook_entry(&status(&sb, &[("PATH", path.as_str())]), "pre-commit");
    assert!(
        entry["reason"].as_str().unwrap().contains("not on PATH"),
        "{entry}"
    );
    let out = run_hook(&sb, "pre-commit", &[("PATH", path.as_str())]);
    assert!(text(&out).is_empty() || !text(&out).contains(RUNNING_LEFTHOOK));
    assert_eq!(counter(&sb), [MARK], "the lefthook script ran, untouched");

    fs::write(side(&sb, "version"), "1.9.0\n").unwrap();
    assert!(reason(&sb, "pre-commit").contains("not a version"));
    fs::remove_file(side(&sb, "version")).unwrap();

    fs::remove_file(side(&sb, "dump")).unwrap();
    assert!(reason(&sb, "pre-commit").contains("dump failed"));
    fs::write(side(&sb, "dump"), "not json").unwrap();
    assert!(reason(&sb, "pre-commit").contains("JSON"));
}

#[test]
fn lefthook_environment_switches_delegate() {
    let sb = lefthook_repo(&serde_json::json!({
        "pre-commit": {"commands": {"a": {"run": "jest"}}}
    }));
    for (var, value, expected) in [
        ("LEFTHOOK", "0", "disables"),
        ("LEFTHOOK_EXCLUDE", "a", "skip"),
        ("LEFTHOOK_BIN", "/nonexistent/lefthook", "LEFTHOOK_BIN"),
    ] {
        let entry = hook_entry(&status(&sb, &[(var, value)]), "pre-commit");
        assert!(
            entry["reason"]
                .as_str()
                .unwrap_or_default()
                .contains(expected),
            "{var}: {entry}"
        );
    }
}

#[test]
fn lefthook_units_are_delegated_when_nothing_is_staged_or_a_file_is_partially_staged() {
    let sb = lefthook_repo(&serde_json::json!({
        "pre-commit": {"commands": {"a": {"run": "jest"}}}
    }));
    // Partially staged: lefthook would hide the unstaged half while it runs.
    fs::write(sb.repo.join("change.txt"), "more\n").unwrap();
    let out = run_hook(&sb, "pre-commit", &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains(RUNNING_LEFTHOOK), "{}", text(&out));
    assert_eq!(counter(&sb), [MARK]);

    // Nothing staged: lefthook skips every command ("no job matching").
    sb.git(&["reset", "-q", "--hard"]);
    let out = run_hook(&sb, "pre-commit", &[]);
    assert!(!text(&out).contains(RUNNING_LEFTHOOK), "{}", text(&out));
    assert_eq!(counter(&sb), [MARK, MARK]);
}

#[test]
fn a_commit_runs_the_lefthook_units_and_a_failure_refuses_it() {
    let sb = lefthook_repo(&serde_json::json!({
        "pre-commit": {"commands": {"a": {"run": "eslint ."}}}
    }));
    let ok = git(&sb, &["commit", "-q", "-m", "x"], &[]);
    assert!(ok.status.success(), "{}", text(&ok));
    // The commit-msg script (not in the configuration) is lefthook's own.
    let lines = counter(&sb);
    assert!(
        lines[0].ends_with("index=set"),
        "git's variables reach the unit: {lines:?}"
    );
    assert_eq!(lines.len(), 2, "{lines:?}");

    stage_change(&sb);
    set_codes(&sb, "a 1\n");
    let refused = git(&sb, &["commit", "-q", "-m", "y"], &[]);
    assert!(!refused.status.success(), "{}", text(&refused));
}

// ---------------------------------------------------------------- pre-commit

const STUB_MARK: &str = "delegated-to-the-pre-commit-stub";

fn stub(python: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n# File generated by pre-commit: https://pre-commit.com\n\
         # ID: 138fd403232d2ddd5efb44317e38bf03\n# start templated\nINSTALL_PYTHON={python}\n\
         ARGS=(hook-impl --config=.pre-commit-config.yaml --hook-type=pre-commit)\n\
         # end templated\necho {STUB_MARK} >> \"$SLUICE_TEST_COUNTER\"\n"
    )
}

/// A repository with `config` committed, pre-commit's stub for `pre-commit`
/// and `pre-push` pointing at a fake Python, and the entry point on.
fn pre_commit_repo(config: &str) -> Sandbox {
    let sb = Sandbox::standard();
    let python = bin_dir(&sb).join("fakepython");
    exe(&python, FAKE_PYTHON);
    fs::write(sb.repo.join(".pre-commit-config.yaml"), config).unwrap();
    commit_all(&sb, "config");
    for hook in ["pre-commit", "pre-push", "commit-msg"] {
        let text = stub(&python.display().to_string())
            .replace("--hook-type=pre-commit", &format!("--hook-type={hook}"));
        exe(&sb.repo.join(".git/hooks").join(hook), &text);
    }
    sb.sluice().args(["hooks", "on"]).assert().success();
    stage_change(&sb);
    sb
}

fn pc_argv(id: &str) -> String {
    format!("pre-commit run {id} --hook-stage pre-commit --config=.pre-commit-config.yaml index=")
}

fn local_hook(id: &str, entry: &str, extra: &str) -> String {
    format!("  - id: {id}\n    name: {id}\n    language: system\n    entry: {entry}\n{extra}")
}

fn config(hooks: &[String]) -> String {
    format!("repos:\n- repo: local\n  hooks:\n{}", hooks.concat())
}

const CHECK_EXTRA: &str = "    pass_filenames: false\n    always_run: true\n";

#[test]
fn pre_commit_hooks_become_units_and_only_always_run_local_checks_are_cached() {
    let cfg = format!(
        "repos:\n- repo: local\n  hooks:\n{}{}{}\
         - repo: https://example.invalid/hooks\n  rev: v1.0.0\n  hooks:\n  \
         - id: black\n    alias: fmt\n    stages: [commit]\n",
        local_hook("unit", "jest --ci", CHECK_EXTRA),
        local_hook("fixer", "prettier --write src", CHECK_EXTRA),
        local_hook("files", "jest", "")
    );
    let sb = pre_commit_repo(&cfg);
    let out = run_hook(&sb, "pre-commit", &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("unit(s) of the pre-commit hook through the queue"),
        "{}",
        text(&out)
    );
    assert_eq!(
        counter(&sb),
        [
            pc_argv("unit"),
            pc_argv("fixer"),
            pc_argv("files"),
            pc_argv("black")
        ]
    );
    assert_eq!(sb.marker_count(), 1, "only the always_run check is cached");

    let again = run_hook(&sb, "pre-commit", &[]);
    assert!(
        text(&again).contains("pre-commit:pre-commit#1 PASS (cached"),
        "{}",
        text(&again)
    );
    assert_eq!(counter(&sb).len(), 7, "three of four ran again");
}

#[test]
fn a_remote_hook_with_stages_in_the_config_is_an_uncached_unit_and_without_them_the_hook_is_delegated()
 {
    let remote = |stages: &str| {
        format!(
            "repos:\n- repo: https://example.invalid/hooks\n  rev: v1.0.0\n  hooks:\n  - id: black\n{stages}"
        )
    };
    let sb = pre_commit_repo(&remote("    stages: [pre-commit]\n"));
    let entry = hook_entry(&status(&sb, &[]), "pre-commit");
    assert_eq!(entry["mode"], "units", "{entry}");
    assert_eq!(entry["units"][0]["kind"], "mutating");

    let sb = pre_commit_repo(&remote(""));
    assert!(reason(&sb, "pre-commit").contains("does not say at which stages"));
    let out = run_hook(&sb, "pre-commit", &[]);
    assert!(out.status.success());
    assert_eq!(counter(&sb), [STUB_MARK], "pre-commit's own stub ran");
}

#[test]
fn what_pre_commit_would_run_differently_per_hook_is_delegated() {
    let one = |extra: &str| config(&[local_hook("a", "jest", extra)]);
    let cases = [
        (
            format!("fail_fast: true\n{}", one("")),
            "pre-commit",
            "fail_fast",
        ),
        (
            format!("minimum_pre_commit_version: '9.0.0'\n{}", one("")),
            "pre-commit",
            "newer",
        ),
        (
            "repos:\n- repo: meta\n  hooks:\n  - id: identity\n".to_owned(),
            "pre-commit",
            "meta",
        ),
        (
            config(&[local_hook("a", "jest", ""), local_hook("a", "tsc", "")]),
            "pre-commit",
            "more than one",
        ),
        (
            config(&[
                local_hook("a", "jest", ""),
                local_hook("b", "tsc", "    alias: a\n"),
            ]),
            "pre-commit",
            "more than one",
        ),
        (one("    fail_fast: true\n"), "pre-commit", "fail_fast"),
        (one("    always_run: yes\n"), "pre-commit", "plain boolean"),
        (one("    surprise: 1\n"), "pre-commit", "unknown key"),
        (
            format!("mystery: 1\n{}", one("")),
            "pre-commit",
            "unknown key",
        ),
        (
            format!("x: &x {{a: 1}}\n{}", one("    <<: *x\n")),
            "pre-commit",
            "merge keys",
        ),
        (
            format!("default_stages: [pre-push]\n{}", one("")),
            "pre-commit",
            "no hook",
        ),
        (one(""), "pre-push", "stdin"),
        (one(""), "commit-msg", "arguments"),
    ];
    for (cfg, hook, expected) in cases {
        let sb = pre_commit_repo(&cfg);
        let why = reason(&sb, hook);
        assert!(why.contains(expected), "{cfg}: {why}");
    }
}

#[test]
fn legacy_stage_names_and_default_stages_place_hooks_at_the_git_hook_stage() {
    let cfg = format!(
        "default_stages: [commit]\n{}",
        config(&[
            local_hook("a", "jest", ""),
            local_hook("b", "tsc", "    stages: [push]\n")
        ])
    );
    let sb = pre_commit_repo(&cfg);
    let entry = hook_entry(&status(&sb, &[]), "pre-commit");
    assert_eq!(entry["units"].as_array().unwrap().len(), 1, "{entry}");
    assert_eq!(entry["requires"], "no tracked file has unstaged changes");
}

#[test]
fn pre_commit_is_delegated_when_skip_is_set_or_the_launcher_cannot_be_found() {
    let sb = pre_commit_repo(&config(&[local_hook("a", "jest", CHECK_EXTRA)]));
    let entry = hook_entry(&status(&sb, &[("SKIP", "a")]), "pre-commit");
    assert!(
        entry["reason"].as_str().unwrap().contains("SKIP"),
        "{entry}"
    );
    let out = run_hook(&sb, "pre-commit", &[("SKIP", "a")]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(counter(&sb), [STUB_MARK]);

    // INSTALL_PYTHON gone and no `pre-commit` on PATH: the stub itself fails.
    let stub_path = sb.repo.join(".git/hooks/pre-commit");
    let text = fs::read_to_string(&stub_path).unwrap().replace(
        &bin_dir(&sb).join("fakepython").display().to_string(),
        "/nonexistent/python",
    );
    exe(&stub_path, &text);
    assert!(reason(&sb, "pre-commit").contains("not found the way its script finds it"));

    // With `pre-commit` on PATH it is the launcher (as the stub would use it).
    exe(
        &bin_dir(&sb).join("pre-commit"),
        &FAKE_PYTHON.replacen("shift\n", "", 1),
    );
    let entry = hook_entry(&status(&sb, &[]), "pre-commit");
    assert_eq!(entry["mode"], "units", "{entry}");
    assert!(
        entry["units"][0]["command"]
            .as_str()
            .unwrap()
            .contains("bin/pre-commit run a"),
        "{entry}"
    );
}

#[test]
fn pre_commit_units_are_delegated_while_tracked_files_have_unstaged_changes() {
    let sb = pre_commit_repo(&config(&[local_hook("a", "jest", CHECK_EXTRA)]));
    fs::write(sb.repo.join("a.txt"), "edited\n").unwrap();
    let out = run_hook(&sb, "pre-commit", &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains("through the queue"), "{}", text(&out));
    assert_eq!(
        counter(&sb),
        [STUB_MARK],
        "pre-commit stashes once, as before"
    );

    sb.git(&["checkout", "--", "a.txt"]);
    assert!(text(&run_hook(&sb, "pre-commit", &[])).contains("through the queue"));
}

#[test]
fn pre_commit_exit_codes_are_the_units_own_and_one_failure_does_not_stop_the_rest() {
    let sb = pre_commit_repo(&config(&[
        local_hook("a", "jest", CHECK_EXTRA),
        local_hook("b", "tsc", CHECK_EXTRA),
        local_hook("c", "eslint .", CHECK_EXTRA),
    ]));
    set_codes(&sb, "a 1\n");
    let out = run_hook(&sb, "pre-commit", &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert_eq!(counter(&sb).len(), 3);
    assert_eq!(sb.marker_count(), 2, "b and c passed, a is not cached");

    // pre-commit's own 3 (an internal error) ends the hook and is propagated
    // as the unit's code, never as a Sluice refusal.
    set_codes(&sb, "a 3\n");
    let out = run_hook(&sb, "pre-commit", &[("CLAUDECODE", "1")]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(!text(&out).contains("has not passed on this tree"));
    assert_eq!(
        counter(&sb).len(),
        4,
        "only `a` ran, `b` and `c` were cached"
    );
}

// ----------------------------------------------------------------- real tools

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// A real tool, run in the repository with the sandbox environment.
fn real(sb: &Sandbox, tool: &Path, args: &[&str]) -> Output {
    StdCommand::new(tool)
        .args(args)
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env("PATH", std::env::var("PATH").unwrap())
        .output()
        .unwrap()
}

#[test]
fn real_lefthook_commands_run_as_units() {
    let Some(tool) = find_on_path("lefthook") else {
        eprintln!("skipped: `lefthook` is not on PATH");
        return;
    };
    let sb = Sandbox::standard();
    let version = String::from_utf8_lossy(&real(&sb, &tool, &["version"]).stdout).into_owned();
    if !version.trim().starts_with("2.") {
        eprintln!("skipped: lefthook {} is not 2.x", version.trim());
        return;
    }
    fs::write(
        sb.repo.join("lefthook.yml"),
        "pre-commit:\n  parallel: true\n  commands:\n    mark:\n      \
         run: echo ran-by-lefthook-unit >> \"$RECORD\"\n",
    )
    .unwrap();
    commit_all(&sb, "config");
    let out = real(&sb, &tool, &["install"]);
    assert!(out.status.success(), "{out:?}");
    sb.sluice().args(["hooks", "on"]).assert().success();
    stage_change(&sb);
    let record = sb.root.path().join("record");
    let path = std::env::var("PATH").unwrap();
    let out = git(
        &sb,
        &["commit", "-q", "-m", "x"],
        &[
            ("PATH", path.as_str()),
            ("RECORD", record.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains(RUNNING_LEFTHOOK), "{}", text(&out));
    let recorded = fs::read_to_string(&record).unwrap_or_default();
    assert!(recorded.contains("ran-by-lefthook-unit"), "{recorded}");
}

#[test]
fn real_pre_commit_local_hooks_run_as_units() {
    let Some(tool) = find_on_path("pre-commit") else {
        eprintln!("skipped: `pre-commit` is not on PATH");
        return;
    };
    let sb = Sandbox::standard();
    fs::write(
        sb.repo.join(".pre-commit-config.yaml"),
        "repos:\n- repo: local\n  hooks:\n  - id: mark\n    name: mark\n    language: system\n    \
         entry: sh -c 'echo ran-by-pre-commit-unit >> \"$RECORD\"'\n    pass_filenames: false\n    \
         always_run: true\n",
    )
    .unwrap();
    commit_all(&sb, "config");
    // `pre-commit install` refuses while core.hooksPath is set: install first.
    let out = real(&sb, &tool, &["install"]);
    assert!(out.status.success(), "{out:?}");
    sb.sluice().args(["hooks", "on"]).assert().success();
    stage_change(&sb);
    let record = sb.root.path().join("record");
    let path = std::env::var("PATH").unwrap();
    let out = git(
        &sb,
        &["commit", "-q", "-m", "x"],
        &[
            ("PATH", path.as_str()),
            ("RECORD", record.to_str().unwrap()),
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("unit(s) of the pre-commit hook through the queue"),
        "{}",
        text(&out)
    );
    let recorded = fs::read_to_string(&record).unwrap_or_default();
    assert!(recorded.contains("ran-by-pre-commit-unit"), "{recorded}");
}
