//! Wave H3: in a repository without `.sluice.toml` the check units of the
//! hooks are the gates of `sluice run`, `sluice gate list`, and the MCP
//! `verify` and `gate_list`, with the same cache entries as the hook runs
//! (ADR-0021 decision 7). A `verify` pass makes the commit's hook a cache hit
//! and the other way round, for husky, lefthook and pre-commit (fakes), also
//! through a real `git commit` (git puts its own directory first in the
//! hook's `PATH`, which must not change the key).

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output, Stdio};

use common::{NO_AUTO_MAINTENANCE, Sandbox};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};

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

const FAKE_TOOL: &str = r#"#!/bin/sh
echo "$(basename "$0") $* index=${GIT_INDEX_FILE:+set}" >> "$SLUICE_TEST_COUNTER"
exit 0
"#;

const FAKE_LEFTHOOK: &str = r#"#!/bin/sh
case "$1" in
  version) echo 2.2.2; exit 0 ;;
  dump) cat "$SLUICE_TEST_COUNTER.dump"; exit $? ;;
  run)
    shift
    echo "lefthook run $* index=${GIT_INDEX_FILE:+set}" >> "$SLUICE_TEST_COUNTER"
    exit 0 ;;
esac
exit 64
"#;

const FAKE_PYTHON: &str = r#"#!/bin/sh
shift
case "$1" in
  --version) echo "pre-commit 4.7.0"; exit 0 ;;
  run)
    echo "pre-commit $* index=${GIT_INDEX_FILE:+set}" >> "$SLUICE_TEST_COUNTER"
    exit 0 ;;
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

fn with_fakes(sb: &Sandbox) -> (String, String) {
    let path = std::env::var("PATH").unwrap_or_default();
    ("PATH".into(), format!("{}:{path}", bin_dir(sb).display()))
}

fn side(sb: &Sandbox, ext: &str) -> PathBuf {
    PathBuf::from(format!("{}.{ext}", sb.counter.display()))
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

fn hooks_dir(sb: &Sandbox) -> PathBuf {
    let common = sb.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    Path::new(&common).join("sluice/hooks")
}

fn git(sb: &Sandbox, args: &[&str]) -> Output {
    let (k, v) = with_fakes(sb);
    StdCommand::new("git")
        .args(NO_AUTO_MAINTENANCE)
        .args(args)
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env(k, v)
        .output()
        .expect("git")
}

/// Runs Sluice's script of `hook` without git around it.
fn run_hook(sb: &Sandbox, hook: &str) -> Output {
    let (k, v) = with_fakes(sb);
    StdCommand::new(hooks_dir(sb).join(hook))
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env(k, v)
        .output()
        .expect("hook")
}

/// `sluice <args>` in the repository with the fakes on `PATH`.
fn sluice(sb: &Sandbox, args: &[&str]) -> Output {
    let (k, v) = with_fakes(sb);
    sb.sluice().env(k, v).args(args).output().unwrap()
}

fn stage_change(sb: &Sandbox) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    fs::write(sb.repo.join("change.txt"), format!("{nanos}\n")).unwrap();
    sb.git(&["add", "change.txt"]);
}

/// Commits everything without running hooks.
fn commit_all(sb: &Sandbox, message: &str) {
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "--no-verify", "-m", message]);
}

/// The repository has no `.sluice.toml` (the deletion is committed, so the
/// working tree equals the index).
fn drop_config(sb: &Sandbox) {
    sb.git(&["rm", "-q", ".sluice.toml"]);
    sb.git(&["commit", "-q", "--no-verify", "-m", "no config"]);
}

fn gate_list(sb: &Sandbox) -> Value {
    let out = sluice(sb, &["gate", "list", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn gate_names(list: &Value) -> Vec<String> {
    list["gates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap().to_owned())
        .collect()
}

fn excluded_reason<'a>(list: &'a Value, unit_or_hook: &str) -> &'a str {
    list["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["unit"] == unit_or_hook || (e["unit"].is_null() && e["hook"] == unit_or_hook))
        .unwrap_or_else(|| panic!("{unit_or_hook} is not excluded in {list}"))["reason"]
        .as_str()
        .unwrap()
}

// -------------------------------------------------------------------- husky

fn husky_repo(hooks: &[(&str, &str)]) -> Sandbox {
    let sb = Sandbox::standard();
    let dir = sb.repo.join(".husky");
    exe(&dir.join("_/h"), HUSKY_H);
    fs::write(dir.join("_/.gitignore"), "*\n").unwrap();
    for (hook, body) in hooks {
        exe(
            &dir.join("_").join(hook),
            "#!/usr/bin/env sh\n. \"$(dirname \"$0\")/h\"",
        );
        fs::write(dir.join(hook), body).unwrap();
    }
    for tool in ["eslint", "jest", "lint-staged", "tsc"] {
        exe(&sb.repo.join("node_modules/.bin").join(tool), FAKE_TOOL);
    }
    sb.git(&["config", "core.hooksPath", ".husky/_"]);
    commit_all(&sb, "husky");
    drop_config(&sb);
    sb.sluice().args(["hooks", "on"]).assert().success();
    sb
}

#[test]
fn husky_units_are_gates_and_a_verify_pass_is_a_hit_for_the_commit_and_back() {
    let sb = husky_repo(&[
        ("pre-commit", "lint-staged\neslint .\n"),
        ("pre-push", "jest --ci\n"),
    ]);
    let list = gate_list(&sb);
    assert_eq!(
        gate_names(&list),
        ["husky:pre-commit#2", "husky:pre-push#1"],
        "{list}"
    );
    assert_eq!(list["gates"][0]["source"], "hook");
    assert_eq!(list["gates"][0]["manager"], "husky");
    assert!(
        excluded_reason(&list, "husky:pre-commit#1").contains("may change files"),
        "{list}"
    );

    // verify, then the commit.
    stage_change(&sb);
    let verified = sluice(&sb, &["run", "husky:pre-commit#2"]);
    assert!(verified.status.success(), "{}", text(&verified));
    assert_eq!(counter(&sb), ["eslint . index="]);
    let commit = git(&sb, &["commit", "-q", "-m", "x"]);
    assert!(commit.status.success(), "{}", text(&commit));
    assert!(
        text(&commit).contains("husky:pre-commit#2 PASS (cached"),
        "the hook hits what verify stored: {}",
        text(&commit)
    );
    assert_eq!(
        counter(&sb),
        ["eslint . index=", "lint-staged  index=set"],
        "only the mutating unit ran in the hook"
    );

    // The commit, then verify.
    stage_change(&sb);
    let commit = git(&sb, &["commit", "-q", "-m", "y"]);
    assert!(commit.status.success(), "{}", text(&commit));
    assert_eq!(counter(&sb).len(), 4, "the hook ran lint-staged and eslint");
    let verified = sluice(&sb, &["run", "husky:pre-commit#2"]);
    assert!(
        text(&verified).contains("PASS (cached"),
        "verify hits what the hook stored: {}",
        text(&verified)
    );
    assert_eq!(counter(&sb).len(), 4);
}

#[test]
fn verify_never_runs_a_mutating_unit() {
    let sb = husky_repo(&[("pre-commit", "lint-staged\neslint .\n")]);
    stage_change(&sb);
    let out = sluice(&sb, &["run", "husky:pre-commit#1"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("unknown gate"), "{}", text(&out));
    assert!(counter(&sb).is_empty(), "{:?}", counter(&sb));
}

#[test]
fn a_delegated_hook_is_listed_with_its_reason_and_yields_no_gate() {
    let sb = husky_repo(&[("pre-commit", "if [ -z \"$CI\" ]; then\n  npm test\nfi\n")]);
    let list = gate_list(&sb);
    assert!(gate_names(&list).is_empty(), "{list}");
    assert!(
        excluded_reason(&list, "pre-commit").contains("delegated"),
        "{list}"
    );
    let out = sluice(&sb, &["gate", "list"]);
    assert!(text(&out).contains("not gates:"), "{}", text(&out));
    // Neither a .sluice.toml nor a gate: `sluice run` keeps its old message.
    let run = sluice(&sb, &["run", "husky:pre-commit#1"]);
    assert!(
        text(&run).contains("no .sluice.toml found"),
        "{}",
        text(&run)
    );
}

#[test]
fn a_repository_without_hooks_or_config_keeps_the_missing_config_error() {
    let sb = Sandbox::standard();
    drop_config(&sb);
    let out = sluice(&sb, &["gate", "list"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("no .sluice.toml found"),
        "{}",
        text(&out)
    );
}

// ------------------------------------------------------------------ lefthook

const MARK: &str = "delegated-to-the-lefthook-script";

fn lefthook_repo(dump: &Value) -> Sandbox {
    let sb = Sandbox::standard();
    exe(&bin_dir(&sb).join("lefthook"), FAKE_LEFTHOOK);
    fs::write(side(&sb, "dump"), dump.to_string()).unwrap();
    for hook in ["pre-commit", "commit-msg"] {
        exe(
            &sb.repo.join(".git/hooks").join(hook),
            &format!("#!/bin/sh\n# LEFTHOOK\necho {MARK} >> \"$SLUICE_TEST_COUNTER\"\n"),
        );
    }
    drop_config(&sb);
    sb.sluice().args(["hooks", "on"]).assert().success();
    sb
}

const LEFTHOOK_RUN: &str =
    "lefthook run pre-commit --command unit --no-auto-install --skip-lfs index=";

#[test]
fn lefthook_units_are_gates_while_something_is_staged_and_share_the_cache_with_the_hook() {
    let sb = lefthook_repo(&json!({
        "pre-commit": {"commands": {"unit": {"run": "jest --ci"}}}
    }));
    // Nothing staged: lefthook would skip the command, so there is no gate.
    let list = gate_list(&sb);
    assert!(gate_names(&list).is_empty(), "{list}");
    assert!(
        excluded_reason(&list, "lefthook:pre-commit#1").contains("nothing is staged"),
        "{list}"
    );

    stage_change(&sb);
    assert_eq!(gate_names(&gate_list(&sb)), ["lefthook:pre-commit#1"]);
    let verified = sluice(&sb, &["run", "lefthook:pre-commit#1"]);
    assert!(verified.status.success(), "{}", text(&verified));
    assert_eq!(counter(&sb), [LEFTHOOK_RUN], "verify has no git variables");

    let commit = git(&sb, &["commit", "-q", "-m", "x"]);
    assert!(commit.status.success(), "{}", text(&commit));
    assert!(
        text(&commit).contains("lefthook:pre-commit#1 PASS (cached"),
        "{}",
        text(&commit)
    );
    assert_eq!(counter(&sb), [LEFTHOOK_RUN, MARK], "only commit-msg ran");

    // The hook, then verify.
    stage_change(&sb);
    let hook = run_hook(&sb, "pre-commit");
    assert!(hook.status.success(), "{}", text(&hook));
    let ran = counter(&sb).len();
    assert!(counter(&sb)[2].ends_with("index="), "{:?}", counter(&sb));
    let verified = sluice(&sb, &["run", "lefthook:pre-commit#1"]);
    assert!(
        text(&verified).contains("PASS (cached"),
        "{}",
        text(&verified)
    );
    assert_eq!(counter(&sb).len(), ran);
}

// ---------------------------------------------------------------- pre-commit

const STUB_MARK: &str = "delegated-to-the-pre-commit-stub";

fn pre_commit_repo() -> Sandbox {
    let sb = Sandbox::standard();
    let python = bin_dir(&sb).join("fakepython");
    exe(&python, FAKE_PYTHON);
    fs::write(
        sb.repo.join(".pre-commit-config.yaml"),
        "repos:\n- repo: local\n  hooks:\n  - id: unit\n    name: unit\n    language: system\n    \
         entry: jest --ci\n    pass_filenames: false\n    always_run: true\n",
    )
    .unwrap();
    commit_all(&sb, "config");
    for hook in ["pre-commit", "commit-msg"] {
        let stub = format!(
            "#!/usr/bin/env bash\n# File generated by pre-commit: https://pre-commit.com\n\
             # ID: 138fd403232d2ddd5efb44317e38bf03\n# start templated\nINSTALL_PYTHON={}\n\
             ARGS=(hook-impl --config=.pre-commit-config.yaml --hook-type={hook})\n\
             # end templated\necho {STUB_MARK} >> \"$SLUICE_TEST_COUNTER\"\n",
            python.display()
        );
        exe(&sb.repo.join(".git/hooks").join(hook), &stub);
    }
    drop_config(&sb);
    sb.sluice().args(["hooks", "on"]).assert().success();
    sb
}

const PRE_COMMIT_RUN: &str =
    "pre-commit run unit --hook-stage pre-commit --config=.pre-commit-config.yaml index=";

#[test]
fn pre_commit_units_are_gates_while_the_tree_equals_the_index_and_share_the_cache() {
    let sb = pre_commit_repo();
    stage_change(&sb);
    assert_eq!(gate_names(&gate_list(&sb)), ["pre-commit:pre-commit#1"]);

    // An unstaged change: pre-commit would stash it, so verify must not run it.
    fs::write(sb.repo.join("change.txt"), "unstaged\n").unwrap();
    let list = gate_list(&sb);
    assert!(
        excluded_reason(&list, "pre-commit:pre-commit#1").contains("unstaged changes"),
        "{list}"
    );
    let refused = sluice(&sb, &["run", "pre-commit:pre-commit#1"]);
    assert!(!refused.status.success());
    assert_eq!(counter(&sb), Vec::<String>::new());
    sb.git(&["add", "change.txt"]);

    let verified = sluice(&sb, &["run", "pre-commit:pre-commit#1"]);
    assert!(verified.status.success(), "{}", text(&verified));
    assert_eq!(counter(&sb), [PRE_COMMIT_RUN]);
    let commit = git(&sb, &["commit", "-q", "-m", "x"]);
    assert!(commit.status.success(), "{}", text(&commit));
    assert!(
        text(&commit).contains("pre-commit:pre-commit#1 PASS (cached"),
        "{}",
        text(&commit)
    );
    assert_eq!(counter(&sb), [PRE_COMMIT_RUN, STUB_MARK]);

    // The hook, then verify.
    stage_change(&sb);
    assert!(run_hook(&sb, "pre-commit").status.success());
    assert_eq!(counter(&sb).len(), 3);
    let verified = sluice(&sb, &["run", "pre-commit:pre-commit#1"]);
    assert!(
        text(&verified).contains("PASS (cached"),
        "{}",
        text(&verified)
    );
    assert_eq!(counter(&sb).len(), 3);
}

// ------------------------------------------------ a repository with .sluice.toml

fn plain_repo_with_config() -> Sandbox {
    let sb = Sandbox::new(
        r#"
[gates.unit]
cmd = "jest --ci"

[gates.lint]
cmd = "eslint ."
env = { LINT = "1" }
"#,
    );
    for tool in ["jest", "eslint"] {
        exe(&bin_dir(&sb).join(tool), FAKE_TOOL);
    }
    exe(
        &sb.repo.join(".githooks/pre-commit"),
        "#!/bin/sh\nset -e\njest --ci\neslint .\n",
    );
    sb.git(&["config", "core.hooksPath", ".githooks"]);
    commit_all(&sb, "hooks");
    sb.sluice().args(["hooks", "on"]).assert().success();
    sb
}

#[test]
fn gates_of_a_configured_repository_are_untouched_and_an_identical_unit_shares_their_cache() {
    let sb = plain_repo_with_config();
    let list = gate_list(&sb);
    assert_eq!(gate_names(&list), ["lint", "unit"], "{list}");
    assert!(list["excluded"].as_array().unwrap().is_empty(), "{list}");
    assert_eq!(list["gates"][0]["source"], "config");

    let status = sluice(&sb, &["hooks", "status", "--json"]);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    let units = status["hooks"][0]["units"].as_array().unwrap().clone();
    assert_eq!(
        units[0]["shares"], "cache shared with gate `unit`",
        "{units:?}"
    );
    let note = units[1]["shares"].as_str().unwrap();
    assert!(
        note.contains("not shared") && note.contains("environment variables"),
        "{note}"
    );

    // verify the gate, then the hook: the unit is the gate's cache hit.
    stage_change(&sb);
    let verified = sluice(&sb, &["run", "unit"]);
    assert!(verified.status.success(), "{}", text(&verified));
    assert_eq!(counter(&sb), ["jest --ci index="]);
    let commit = git(&sb, &["commit", "-q", "-m", "x"]);
    assert!(commit.status.success(), "{}", text(&commit));
    assert!(
        text(&commit).contains("unit PASS (cached"),
        "{}",
        text(&commit)
    );
    assert_eq!(
        counter(&sb),
        ["jest --ci index=", "eslint . index="],
        "jest did not run again, the unlike unit did"
    );

    // The hook, then verify the gate.
    stage_change(&sb);
    assert!(git(&sb, &["commit", "-q", "-m", "y"]).status.success());
    let verified = sluice(&sb, &["run", "unit"]);
    assert!(
        text(&verified).contains("PASS (cached"),
        "{}",
        text(&verified)
    );
}

// ------------------------------------------------------------------- doctor

#[test]
fn doctor_accepts_the_missing_config_when_the_hooks_go_through_sluice() {
    let sb = husky_repo(&[("pre-commit", "eslint .\n")]);
    let out = sluice(&sb, &["doctor", "--json"]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "repo-config")
        .unwrap();
    assert_eq!(check["status"], "ok", "{check}");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .contains("derived from the hooks"),
        "{check}"
    );

    // Entry point off: the missing file is still a failure.
    sb.sluice().args(["hooks", "off"]).assert().success();
    let out = sluice(&sb, &["doctor", "--json"]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "repo-config")
        .unwrap();
    assert_eq!(check["status"], "fail", "{check}");
}

// ---------------------------------------------------------------------- MCP

type Client = RunningService<RoleClient, ()>;

async fn connect(sb: &Sandbox) -> Client {
    let mut cmd = tokio::process::Command::new(Sandbox::binary());
    cmd.arg("mcp")
        .current_dir(&sb.home)
        .env_clear()
        .envs(sb.env());
    let (transport, _stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sluice mcp");
    ().serve(transport).await.expect("initialize")
}

async fn call(client: &Client, name: &'static str, args: &Value) -> CallToolResult {
    let Value::Object(map) = args.clone() else {
        panic!("arguments must be an object");
    };
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(map))
        .await
        .unwrap_or_else(|e| panic!("{name} failed: {e}"))
}

fn mcp_text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_gate_list_and_verify_use_the_hook_units_and_warm_the_commits_hook() {
    let sb = husky_repo(&[("pre-commit", "lint-staged\neslint .\n")]);
    let client = connect(&sb).await;
    let dir = sb.repo.display().to_string();

    let r = call(&client, "gate_list", &json!({"working_directory": dir})).await;
    let s = r.structured_content.clone().expect("structured");
    assert_eq!(s["gates"][0]["name"], "husky:pre-commit#2", "{s}");
    assert_eq!(s["gates"][0]["source"], "hook");
    assert_eq!(s["excluded"][0]["unit"], "husky:pre-commit#1", "{s}");
    assert!(mcp_text(&r).contains("not gates:"), "{}", mcp_text(&r));

    stage_change(&sb);
    let r = call(&client, "verify", &json!({"working_directory": dir})).await;
    assert_eq!(r.is_error, Some(false), "{}", mcp_text(&r));
    assert!(
        mcp_text(&r).starts_with("husky:pre-commit#2  PASS"),
        "{}",
        mcp_text(&r)
    );
    assert_eq!(counter(&sb), ["eslint . index="]);

    // The MCP server's PATH is the sandbox's, so is the commit's here.
    let commit = StdCommand::new("git")
        .args(NO_AUTO_MAINTENANCE)
        .args(["commit", "-q", "-m", "x"])
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .output()
        .unwrap();
    assert!(
        text(&commit).contains("husky:pre-commit#2 PASS (cached"),
        "{}",
        text(&commit)
    );

    // A mutating unit cannot be asked for.
    let r = client
        .call_tool(
            CallToolRequestParams::new("verify").with_arguments(
                json!({"working_directory": dir, "gates": ["husky:pre-commit#1"]})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(r.is_err() || r.unwrap().is_error == Some(true));
    assert_eq!(counter(&sb).len(), 2, "{:?}", counter(&sb));
    client.cancel().await.ok();
}
