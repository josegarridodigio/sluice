//! `sluice hooks on|off|status|run`: Sluice as the git hooks entry point of a
//! clone (ADR-0021, wave H1).
//!
//! Managers are replaced by fake ones that record argv, stdin, cwd and
//! environment and exit with a chosen code, placed where the real ones put
//! their files. Real tools run only when they are on `PATH`.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output, Stdio};

use common::{NO_AUTO_MAINTENANCE, Sandbox};
use serde_json::Value;

/// Records argv, cwd, stdin, an environment variable and `$0` to `$RECORD`
/// and exits with `$FAKE_EXIT`. The body of a fake hook (no shebang).
const RECORDER: &str = r#"{
  printf 'argv:'; for a in "$@"; do printf ' [%s]' "$a"; done; printf '\n'
  echo "cwd:$(pwd)"
  echo "stdin:$(cat)"
  echo "env:$FAKE_ENV"
  echo "self:$0"
} >> "$RECORD"
if [ -n "$FAKE_SIGNAL" ]; then kill -s "$FAKE_SIGNAL" $$; fi
exit "${FAKE_EXIT:-0}"
"#;

fn exe(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fake hook script.
fn fake_hook(path: &Path, marker: &str) {
    exe(path, &format!("#!/bin/sh\n# {marker}\n{RECORDER}"));
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// A `PATH` with only the tools hooks need, and no `sluice`: the scripts must
/// not depend on one being installed (an older one would not know `hooks`).
fn tools(sb: &Sandbox) -> PathBuf {
    let dir = sb.root.path().join("tools");
    if dir.exists() {
        return dir;
    }
    fs::create_dir_all(&dir).unwrap();
    for name in [
        "git", "sh", "cat", "tail", "env", "dirname", "basename", "kill", "sed", "sleep",
    ] {
        if let Some(real) = find_on_path(name) {
            symlink(real, dir.join(name)).unwrap();
        }
    }
    dir
}

fn record_file(sb: &Sandbox) -> PathBuf {
    sb.root.path().join("record")
}

fn record(sb: &Sandbox) -> String {
    fs::read_to_string(record_file(sb)).unwrap_or_default()
}

/// Runs git in the repository with the tools-only `PATH` and `extra` added.
fn git_out(sb: &Sandbox, args: &[&str], extra: &[(&str, &str)]) -> Output {
    StdCommand::new(find_on_path("git").unwrap())
        .args(NO_AUTO_MAINTENANCE)
        .args(args)
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env("PATH", tools(sb))
        .env("RECORD", record_file(sb))
        .envs(extra.iter().copied())
        .output()
        .expect("git")
}

fn git_ok(sb: &Sandbox, args: &[&str], extra: &[(&str, &str)]) {
    let out = git_out(sb, args, extra);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn common_dir(sb: &Sandbox) -> PathBuf {
    PathBuf::from(sb.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]))
}

fn hooks_dir(sb: &Sandbox) -> PathBuf {
    common_dir(sb).join("sluice/hooks")
}

fn on(sb: &Sandbox) {
    sb.sluice().args(["hooks", "on"]).assert().success();
}

fn script_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn status(sb: &Sandbox) -> Value {
    let out = sb
        .sluice()
        .args(["hooks", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Effective `core.hooksPath` (git's own answer), if any.
fn hooks_path(sb: &Sandbox) -> Option<String> {
    let out = git_out(sb, &["config", "--get", "core.hooksPath"], &[]);
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Runs a script of the hooks directory directly, as git would.
fn run_script(
    sb: &Sandbox,
    hook: &str,
    args: &[&str],
    stdin: &str,
    extra: &[(&str, &str)],
) -> Output {
    let mut child = StdCommand::new(hooks_dir(sb).join(hook))
        .args(args)
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env("PATH", tools(sb))
        .env("RECORD", record_file(sb))
        .envs(extra.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    child.wait_with_output().unwrap()
}

fn husky_stub(sb: &Sandbox, hooks: &[&str]) {
    let dir = sb.repo.join(".husky/_");
    for hook in hooks {
        exe(
            &dir.join(hook),
            "#!/usr/bin/env sh\n. \"$(dirname \"$0\")/h\"",
        );
    }
    // The real `h` runs the user's file; the fake one records the call.
    fs::write(dir.join("h"), RECORDER).unwrap();
    git_ok(sb, &["config", "core.hooksPath", ".husky/_"], &[]);
}

// ---------------------------------------------------------------- activation

#[test]
fn on_points_core_hooks_path_at_a_per_clone_directory_through_the_last_include() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".git/hooks/pre-commit"), "plain");
    fake_hook(&sb.repo.join(".git/hooks/pre-push"), "plain");
    on(&sb);
    let dir = hooks_dir(&sb);
    assert_eq!(hooks_path(&sb), Some(dir.display().to_string()));

    // One executable script per hook the repository has, and no other.
    assert_eq!(script_names(&dir), ["pre-commit", "pre-push"]);
    for name in script_names(&dir) {
        let mode = fs::metadata(dir.join(name)).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
    }
    let gitconfig = common_dir(&sb).join("sluice/hooks.gitconfig");
    assert!(
        fs::read_to_string(&gitconfig)
            .unwrap()
            .contains(&dir.display().to_string())
    );
    assert!(common_dir(&sb).join("sluice/hooks.json").is_file());

    // The include is the last `[include]` entry and its value is the last one read.
    let includes = sb.git(&["config", "--local", "--get-all", "include.path"]);
    assert_eq!(includes.lines().last(), Some(gitconfig.to_str().unwrap()));
    let origins = sb.git(&["config", "--show-origin", "--get-all", "core.hooksPath"]);
    let last = origins.lines().last().unwrap();
    assert!(last.contains("hooks.gitconfig"), "{origins}");

    // Nothing in the repository changed.
    assert_eq!(sb.git(&["status", "--porcelain"]), "");
}

#[test]
fn a_later_core_hooks_path_write_does_not_displace_it() {
    let sb = Sandbox::standard();
    on(&sb);
    let dir = hooks_dir(&sb).display().to_string();
    // What husky does on every install.
    git_ok(&sb, &["config", "core.hooksPath", ".husky/_"], &[]);
    let origins = sb.git(&["config", "--show-origin", "--get-all", "core.hooksPath"]);
    assert_eq!(origins.lines().count(), 2, "{origins}");
    assert_eq!(hooks_path(&sb), Some(dir));
    assert_eq!(status(&sb)["effective"], true);
    // The write landed in `[core]`, before the include.
    assert_eq!(status(&sb)["underlying"], ".husky/_");
}

#[test]
fn an_include_section_before_core_is_rebuilt_so_that_ours_is_last() {
    let sb = Sandbox::standard();
    let config = sb.repo.join(".git/config");
    fs::write(
        sb.repo.join(".git/extra.gitconfig"),
        "[color]\n\tui = never\n",
    )
    .unwrap();
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        format!("[include]\n\tpath = extra.gitconfig\n{text}"),
    )
    .unwrap();
    git_ok(&sb, &["config", "core.hooksPath", ".husky/_"], &[]);

    on(&sb);
    assert_eq!(hooks_path(&sb), Some(hooks_dir(&sb).display().to_string()));
    let includes = sb.git(&["config", "--local", "--get-all", "include.path"]);
    let lines: Vec<&str> = includes.lines().collect();
    assert_eq!(lines.len(), 2, "{includes}");
    assert_eq!(lines[0], "extra.gitconfig");
    assert!(lines[1].ends_with("hooks.gitconfig"), "{includes}");
    assert_eq!(sb.git(&["config", "--get", "color.ui"]), "never");
}

#[test]
fn on_twice_changes_nothing() {
    let sb = Sandbox::standard();
    on(&sb);
    let config = fs::read_to_string(sb.repo.join(".git/config")).unwrap();
    let state = fs::read_to_string(common_dir(&sb).join("sluice/hooks.json")).unwrap();
    on(&sb);
    assert_eq!(
        fs::read_to_string(sb.repo.join(".git/config")).unwrap(),
        config
    );
    assert_eq!(
        fs::read_to_string(common_dir(&sb).join("sluice/hooks.json")).unwrap(),
        state
    );
    assert_eq!(
        sb.git(&["config", "--local", "--get-all", "include.path"])
            .lines()
            .count(),
        1
    );
}

#[test]
fn a_linked_worktree_shares_the_activation() {
    let sb = Sandbox::standard();
    on(&sb);
    let wt = sb.root.path().join("wt");
    sb.git(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"]);
    let out = sb
        .sluice_in(&wt)
        .args(["hooks", "status", "--json"])
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["effective"], true, "{value}");
    assert_eq!(
        value["directory"].as_str().unwrap(),
        hooks_dir(&sb).display().to_string()
    );
}

#[test]
fn off_restores_the_previous_value_and_removes_what_on_made() {
    // A repository with its own value.
    let sb = Sandbox::standard();
    git_ok(&sb, &["config", "core.hooksPath", ".githooks"], &[]);
    let before = fs::read_to_string(sb.repo.join(".git/config")).unwrap();
    on(&sb);
    assert_ne!(hooks_path(&sb), Some(".githooks".to_owned()));
    sb.sluice().args(["hooks", "off"]).assert().success();
    assert_eq!(hooks_path(&sb), Some(".githooks".to_owned()));
    assert_eq!(
        fs::read_to_string(sb.repo.join(".git/config")).unwrap(),
        before
    );
    let sluice = common_dir(&sb).join("sluice");
    assert!(!sluice.join("hooks").exists());
    assert!(!sluice.join("hooks.gitconfig").exists());
    assert!(!sluice.join("hooks.json").exists());

    // A repository with none.
    let sb = Sandbox::standard();
    on(&sb);
    sb.sluice().args(["hooks", "off"]).assert().success();
    assert_eq!(hooks_path(&sb), None);
    // Off twice is fine.
    sb.sluice().args(["hooks", "off"]).assert().success();
    assert_eq!(sb.git(&["status", "--porcelain"]), "");
}

#[test]
fn status_reports_the_state_and_the_manager() {
    let sb = Sandbox::standard();
    let off = status(&sb);
    assert_eq!(off["active"], false);
    assert_eq!(off["effective"], false);
    assert_eq!(off["manager"], "none");

    husky_stub(&sb, &["pre-commit"]);
    on(&sb);
    let value = status(&sb);
    assert_eq!(value["active"], true);
    assert_eq!(value["effective"], true);
    assert_eq!(value["previous"], ".husky/_");
    assert_eq!(value["manager"], "husky");
    assert_eq!(value["problem"], Value::Null);
    let text = sb.sluice().args(["hooks", "status"]).output().unwrap();
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(text.contains("hooks entry point: on"), "{text}");
    assert!(text.contains("effective:       yes"), "{text}");
}

#[test]
fn a_displaced_activation_is_reported_and_on_repairs_it() {
    let sb = Sandbox::standard();
    on(&sb);
    // Someone removes the include (or rewrites the config wholesale).
    sb.git(&["config", "--local", "--unset-all", "include.path"]);
    let value = status(&sb);
    assert_eq!(value["active"], true);
    assert_eq!(value["effective"], false);
    assert!(value["problem"].as_str().unwrap().contains("include"));
    on(&sb);
    assert_eq!(status(&sb)["effective"], true);
}

// ------------------------------------------------------------------ delegation

#[test]
fn husky_hooks_run_with_the_same_argv_stdin_cwd_environment_and_exit_code() {
    let sb = Sandbox::standard();
    husky_stub(&sb, &["pre-push", "pre-commit"]);
    on(&sb);
    let out = run_script(
        &sb,
        "pre-push",
        &["origin", "ssh://example/repo with space"],
        "refs/heads/main abc refs/heads/main def\n",
        &[("FAKE_ENV", "kept"), ("FAKE_EXIT", "7")],
    );
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    let rec = record(&sb);
    assert!(
        rec.contains("argv: [origin] [ssh://example/repo with space]"),
        "{rec}"
    );
    assert!(
        rec.contains("stdin:refs/heads/main abc refs/heads/main def"),
        "{rec}"
    );
    assert!(rec.contains("env:kept"), "{rec}");
    assert!(rec.contains(&format!("cwd:{}", sb.repo.display())), "{rec}");
    // The stub was run where git's relative path points, so its `$0` is relative.
    assert!(rec.contains("self:.husky/_/pre-push"), "{rec}");
}

#[test]
fn a_husky_hook_without_a_stub_exits_zero_like_husky() {
    let sb = Sandbox::standard();
    husky_stub(&sb, &["pre-commit"]);
    on(&sb);
    // No stub, no script: git finds nothing to run, as under husky alone.
    assert_eq!(script_names(&hooks_dir(&sb)), ["pre-commit"]);
    assert_eq!(record(&sb), "");
}

#[test]
fn a_lefthook_script_in_the_default_hooks_directory_is_delegated() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".git/hooks/pre-commit"), "LEFTHOOK");
    on(&sb);
    assert_eq!(status(&sb)["manager"], "lefthook");
    let out = run_script(
        &sb,
        "pre-commit",
        &[],
        "",
        &[("FAKE_EXIT", "1"), ("FAKE_ENV", "e")],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let rec = record(&sb);
    assert!(rec.contains("argv:\n"), "{rec}");
    assert!(rec.contains("env:e"), "{rec}");
    assert!(
        rec.contains(&format!(
            "self:{}",
            sb.repo.join(".git/hooks/pre-commit").display()
        )),
        "{rec}"
    );
}

#[test]
fn a_pre_commit_stub_in_the_default_hooks_directory_is_delegated() {
    let sb = Sandbox::standard();
    fake_hook(
        &sb.repo.join(".git/hooks/commit-msg"),
        "File generated by pre-commit: https://pre-commit.com",
    );
    on(&sb);
    assert_eq!(status(&sb)["manager"], "pre-commit");
    let out = run_script(&sb, "commit-msg", &[".git/COMMIT_EDITMSG"], "", &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(record(&sb).contains("argv: [.git/COMMIT_EDITMSG]"));
}

#[test]
fn plain_hooks_run_from_core_hooks_path_and_from_the_default_directory() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".githooks/pre-commit"), "plain");
    fake_hook(&sb.repo.join(".git/hooks/commit-msg"), "plain");
    git_ok(&sb, &["config", "core.hooksPath", ".githooks"], &[]);
    on(&sb);
    assert_eq!(status(&sb)["manager"], "plain hooks");
    let out = run_script(&sb, "pre-commit", &[], "", &[("FAKE_EXIT", "5")]);
    assert_eq!(out.status.code(), Some(5), "{out:?}");
    assert!(record(&sb).contains("self:.githooks/pre-commit"));

    // The default directory is not consulted while core.hooksPath names another one
    // (git would not have run it either).
    fs::remove_file(record_file(&sb)).unwrap();
    assert_eq!(script_names(&hooks_dir(&sb)), ["pre-commit"]);

    // With no core.hooksPath the default directory is the one.
    sb.sluice().args(["hooks", "off"]).assert().success();
    git_ok(&sb, &["config", "--unset", "core.hooksPath"], &[]);
    on(&sb);
    let out = run_script(&sb, "commit-msg", &["m"], "", &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(record(&sb).contains("argv: [m]"));
}

#[test]
fn a_hook_file_that_is_not_executable_does_not_run() {
    let sb = Sandbox::standard();
    let path = sb.repo.join(".git/hooks/pre-commit");
    fake_hook(&path, "plain");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    on(&sb);
    assert_eq!(script_names(&hooks_dir(&sb)), Vec::<String>::new());
    assert_eq!(record(&sb), "");
}

#[test]
fn a_hook_killed_by_a_signal_is_seen_as_killed_by_the_same_signal() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".git/hooks/pre-commit"), "plain");
    on(&sb);
    let out = run_script(&sb, "pre-commit", &[], "", &[("FAKE_SIGNAL", "TERM")]);
    assert_eq!(out.status.signal(), Some(15), "{out:?}");
}

#[test]
fn git_commit_is_refused_by_a_failing_hook_and_allowed_by_a_passing_one() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".git/hooks/pre-commit"), "plain");
    on(&sb);
    fs::write(sb.repo.join("a.txt"), "changed\n").unwrap();
    sb.git(&["add", "a.txt"]);
    let refused = git_out(&sb, &["commit", "-q", "-m", "x"], &[("FAKE_EXIT", "1")]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(record(&sb).contains("argv:\n"));
    let allowed = git_out(&sb, &["commit", "-q", "-m", "x"], &[]);
    assert!(allowed.status.success(), "{allowed:?}");
    assert_eq!(sb.git(&["log", "-1", "--format=%s"]), "x");
}

#[test]
fn pre_push_stdin_and_arguments_reach_the_hook_through_git() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".git/hooks/pre-push"), "plain");
    on(&sb);
    let remote = sb.root.path().join("remote.git");
    sb.git_in(
        sb.root.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    sb.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    git_ok(&sb, &["push", "-q", "origin", "main"], &[]);
    let rec = record(&sb);
    assert!(
        rec.contains(&format!("argv: [origin] [{}]", remote.display())),
        "{rec}"
    );
    assert!(rec.contains("stdin:refs/heads/main "), "{rec}");
    assert!(rec.contains(&format!("cwd:{}", sb.repo.display())), "{rec}");
}

#[test]
fn a_missing_sluice_binary_still_runs_the_original_hook() {
    let sb = Sandbox::standard();
    // Activate with a private copy of the binary, then remove it: the script
    // finds no `sluice` (not at the recorded path, not in the tools-only PATH).
    let copy = sb.root.path().join("copy");
    fs::create_dir(&copy).unwrap();
    fs::copy(Sandbox::binary(), copy.join("sluice")).unwrap();
    fake_hook(&sb.repo.join(".git/hooks/pre-push"), "plain");
    let out = StdCommand::new(copy.join("sluice"))
        .args(["hooks", "on"])
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    fs::remove_dir_all(&copy).unwrap();

    let out = run_script(
        &sb,
        "pre-push",
        &["origin", "url"],
        "refs/heads/a 1 refs/heads/a 0\n",
        &[("FAKE_EXIT", "4")],
    );
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    let rec = record(&sb);
    assert!(rec.contains("argv: [origin] [url]"), "{rec}");
    assert!(rec.contains("stdin:refs/heads/a 1 refs/heads/a 0"), "{rec}");

    // A later `git config core.hooksPath` (husky's rewrite) is followed too.
    fs::remove_file(record_file(&sb)).unwrap();
    fake_hook(&sb.repo.join(".githooks/pre-push"), "plain");
    git_ok(&sb, &["config", "core.hooksPath", ".githooks"], &[]);
    let out = run_script(&sb, "pre-push", &["q"], "", &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(record(&sb).contains("self:.githooks/pre-push"));
}

#[test]
fn hooks_the_repository_does_not_have_get_no_script() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".git/hooks/pre-commit"), "plain");
    on(&sb);
    let names = script_names(&hooks_dir(&sb));
    assert_eq!(names, ["pre-commit"]);
    for frequent in [
        "reference-transaction",
        "post-index-change",
        "post-checkout",
    ] {
        assert!(!hooks_dir(&sb).join(frequent).exists(), "{frequent}");
    }
}

#[test]
fn on_mirrors_a_new_hook_and_doctor_flags_one_that_is_not_mirrored() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".git/hooks/pre-commit"), "plain");
    on(&sb);
    // A hook appears in the repository's hooks directory later.
    fake_hook(&sb.repo.join(".git/hooks/post-merge"), "plain");
    let check = doctor_hooks(&sb);
    assert_eq!(check["status"], "fail", "{check}");
    let detail = check["detail"].as_str().unwrap();
    assert!(detail.contains("post-merge"), "{detail}");
    assert!(detail.contains("sluice hooks on"), "{detail}");
    assert_eq!(status(&sb)["effective"], true);
    on(&sb);
    assert_eq!(script_names(&hooks_dir(&sb)), ["post-merge", "pre-commit"]);
    assert_eq!(doctor_hooks(&sb)["status"], "ok");
}

#[test]
fn hooks_run_keeps_the_mirror_in_line_with_the_repository() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".git/hooks/pre-commit"), "plain");
    on(&sb);
    fake_hook(&sb.repo.join(".git/hooks/post-merge"), "plain");
    // The next hook that runs adds the new script; the run itself is unchanged.
    let out = run_script(&sb, "pre-commit", &[], "", &[("FAKE_EXIT", "6")]);
    assert_eq!(out.status.code(), Some(6), "{out:?}");
    assert_eq!(script_names(&hooks_dir(&sb)), ["post-merge", "pre-commit"]);
    // A hook that vanished keeps its script until `on` prunes it: a hook run
    // sees one worktree, and another may still have the hook.
    fs::remove_file(sb.repo.join(".git/hooks/post-merge")).unwrap();
    let out = run_script(&sb, "pre-commit", &[], "", &[]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(script_names(&hooks_dir(&sb)), ["post-merge", "pre-commit"]);
    on(&sb);
    assert_eq!(script_names(&hooks_dir(&sb)), ["pre-commit"]);
}

#[test]
fn a_linked_worktree_without_the_relative_hooks_dir_keeps_the_scripts() {
    let sb = Sandbox::standard();
    husky_stub(&sb, &["pre-commit", "pre-push"]);
    on(&sb);
    // `.husky/_` is untracked: a fresh linked worktree has none.
    let wt = sb.root.path().join("wt");
    sb.git(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"]);
    let mut child = StdCommand::new(hooks_dir(&sb).join("pre-commit"))
        .current_dir(&wt)
        .env_clear()
        .envs(sb.env())
        .env("PATH", tools(&sb))
        .env("RECORD", record_file(&sb))
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn hook");
    assert!(child.wait().unwrap().success());
    assert_eq!(record(&sb), "", "no hook ran in the linked worktree");
    assert_eq!(script_names(&hooks_dir(&sb)), ["pre-commit", "pre-push"]);
    // `on` from that worktree prunes against every worktree, not only its own.
    sb.sluice_in(&wt).args(["hooks", "on"]).assert().success();
    assert_eq!(script_names(&hooks_dir(&sb)), ["pre-commit", "pre-push"]);
}

// -------------------------------------------------------- doctor, session start

fn doctor_hooks(sb: &Sandbox) -> Value {
    let out = sb.sluice().args(["doctor", "--json"]).output().unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "hooks")
        .unwrap()
        .clone()
}

#[test]
fn doctor_fails_for_a_manager_that_bypasses_sluice_and_passes_once_it_is_the_entry_point() {
    let sb = Sandbox::standard();
    husky_stub(&sb, &["pre-commit"]);
    fs::write(sb.repo.join(".husky/pre-commit"), "npm run lint\n").unwrap();
    let check = doctor_hooks(&sb);
    assert_eq!(check["status"], "fail", "{check}");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .contains("sluice hooks on")
    );

    on(&sb);
    let check = doctor_hooks(&sb);
    assert_eq!(check["status"], "ok", "{check}");

    // Displaced: a failure with the reason.
    sb.git(&["config", "--local", "--unset-all", "include.path"]);
    let check = doctor_hooks(&sb);
    assert_eq!(check["status"], "fail", "{check}");
    assert!(
        check["detail"].as_str().unwrap().contains("include"),
        "{check}"
    );
}

#[test]
fn doctor_fails_when_a_script_of_the_entry_point_is_missing() {
    let sb = Sandbox::standard();
    fake_hook(&sb.repo.join(".git/hooks/pre-push"), "plain");
    on(&sb);
    fs::remove_file(hooks_dir(&sb).join("pre-push")).unwrap();
    let check = doctor_hooks(&sb);
    assert_eq!(check["status"], "fail", "{check}");
    assert!(check["detail"].as_str().unwrap().contains("pre-push"));
}

fn session_start(sb: &Sandbox) -> Value {
    let input = serde_json::json!({"session_id": "s", "cwd": sb.repo, "source": "startup"});
    let mut child = StdCommand::new(Sandbox::binary())
        .args(["agent", "session-start", "--plugin-version", "0.6.3"])
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env("SLUICE_TEST_SESSION_WATCHDOG_MS", "5000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn session_start_restores_a_displaced_activation_and_never_activates_a_new_clone() {
    let sb = Sandbox::standard();
    // Never activated: stays so.
    let answer = session_start(&sb);
    let text = answer["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(!text.contains("hooks entry point"), "{text}");
    assert_eq!(hooks_path(&sb), None);
    assert!(!common_dir(&sb).join("sluice/hooks").exists());

    // Activated, healthy: nothing to say.
    on(&sb);
    let answer = session_start(&sb);
    let text = answer["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(!text.contains("hooks entry point"), "{text}");

    // Displaced: restored, and the agent is told.
    sb.git(&["config", "--local", "--unset-all", "include.path"]);
    assert_eq!(status(&sb)["effective"], false);
    let answer = session_start(&sb);
    let text = answer["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        text.contains("re-enabled its git hooks entry point"),
        "{text}"
    );
    assert_eq!(status(&sb)["effective"], true);
}

// ------------------------------------------------------------------------ init

fn husky_repo(sb: &Sandbox) -> String {
    husky_stub(sb, &["pre-commit"]);
    let hook = sb.repo.join(".husky/pre-commit");
    fs::write(&hook, "#!/bin/sh\nnpm run lint\n").unwrap();
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "husky"]);
    fs::read_to_string(hook).unwrap()
}

#[test]
fn init_hooks_entry_leaves_the_hook_files_alone_and_activates_the_entry_point() {
    let sb = Sandbox::standard();
    let original = husky_repo(&sb);
    let dry = sb
        .sluice()
        .args(["init", "--force", "--dry-run", "--hooks", "entry"])
        .output()
        .unwrap();
    let dry = String::from_utf8_lossy(&dry.stdout).into_owned();
    assert!(dry.contains("would run sluice hooks on"), "{dry}");
    assert_eq!(hooks_path(&sb), Some(".husky/_".to_owned()));

    sb.sluice()
        .args(["init", "--force", "--hooks", "entry"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(sb.repo.join(".husky/pre-commit")).unwrap(),
        original
    );
    assert_eq!(status(&sb)["effective"], true);
    assert_eq!(status(&sb)["underlying"], ".husky/_");
}

#[test]
fn init_by_default_still_rewires_and_mentions_the_entry_point() {
    let sb = Sandbox::standard();
    husky_repo(&sb);
    let out = sb
        .sluice()
        .args(["init", "--force", "--dry-run"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("sluice init --hooks entry"), "{text}");
    assert!(!text.contains("would run sluice hooks on"), "{text}");
}

// ------------------------------------------------------------- real managers

/// Runs only when `tool` is installed; says why otherwise.
fn have(tool: &str) -> bool {
    let found = find_on_path(tool).is_some();
    if !found {
        eprintln!("skipped: `{tool}` is not on PATH");
    }
    found
}

#[test]
fn real_lefthook_runs_through_the_entry_point() {
    if !have("lefthook") {
        return;
    }
    let sb = Sandbox::standard();
    fs::write(
        sb.repo.join("lefthook.yml"),
        "pre-commit:\n  commands:\n    mark:\n      run: echo ran-by-lefthook >> \"$RECORD\"\n",
    )
    .unwrap();
    let real = |args: &[&str]| {
        let out = StdCommand::new(find_on_path("lefthook").unwrap())
            .args(args)
            .current_dir(&sb.repo)
            .env_clear()
            .envs(sb.env())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    };
    real(&["install"]);
    on(&sb);
    fs::write(sb.repo.join("a.txt"), "changed\n").unwrap();
    sb.git(&["add", "a.txt"]);
    let lefthook_dir = find_on_path("lefthook")
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let path = format!("{}:{}", tools(&sb).display(), lefthook_dir.display());
    git_ok(
        &sb,
        &["commit", "-q", "-m", "x"],
        &[("PATH", path.as_str())],
    );
    assert!(record(&sb).contains("ran-by-lefthook"), "{}", record(&sb));
}

#[test]
fn real_pre_commit_runs_through_the_entry_point() {
    if !have("pre-commit") {
        return;
    }
    let sb = Sandbox::standard();
    fs::write(
        sb.repo.join(".pre-commit-config.yaml"),
        "repos:\n- repo: local\n  hooks:\n  - id: mark\n    name: mark\n    language: system\n    \
         entry: sh -c 'echo ran-by-pre-commit >> \"$RECORD\"'\n    pass_filenames: false\n    always_run: true\n",
    )
    .unwrap();
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "config"]);
    let tool = find_on_path("pre-commit").unwrap();
    // `pre-commit install` refuses while core.hooksPath is set: install first.
    let out = StdCommand::new(&tool)
        .arg("install")
        .current_dir(&sb.repo)
        .env_clear()
        .envs(sb.env())
        .env("PATH", std::env::var("PATH").unwrap())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    on(&sb);
    fs::write(sb.repo.join("a.txt"), "changed\n").unwrap();
    sb.git(&["add", "a.txt"]);
    let path = format!(
        "{}:{}",
        tools(&sb).display(),
        tool.parent().unwrap().display()
    );
    git_ok(
        &sb,
        &["commit", "-q", "-m", "x"],
        &[("PATH", path.as_str()), ("HOME", sb.home.to_str().unwrap())],
    );
    assert!(record(&sb).contains("ran-by-pre-commit"), "{}", record(&sb));
}
