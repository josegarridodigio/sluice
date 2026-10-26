//! `sluice init`: stack detection, gates from existing hooks and hook
//! rewiring (zero-config plan task T3, ADR-0016).
//!
//! Every fixture under `tests/fixtures/init/<name>` is copied into a fresh
//! repository in a temporary directory with an isolated HOME.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Output};

use assert_cmd::Command;
use regex::Regex;
use sluice_gate_core::config::{RepoConfig, Weight, starter_repo_config};
use tempfile::TempDir;

/// A repository made from a fixture.
struct Repo {
    _root: TempDir,
    home: PathBuf,
    dir: PathBuf,
}

fn env_for(home: &Path) -> Vec<(String, String)> {
    vec![
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".into(), home.display().to_string()),
        (
            "XDG_DATA_HOME".into(),
            home.join("data").display().to_string(),
        ),
        (
            "XDG_STATE_HOME".into(),
            home.join("state").display().to_string(),
        ),
        (
            "XDG_RUNTIME_DIR".into(),
            home.join("runtime").display().to_string(),
        ),
        (
            "XDG_CONFIG_HOME".into(),
            home.join(".config").display().to_string(),
        ),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_AUTHOR_NAME".into(), "Test".into()),
        ("GIT_AUTHOR_EMAIL".into(), "test@example.com".into()),
        ("GIT_COMMITTER_NAME".into(), "Test".into()),
        ("GIT_COMMITTER_EMAIL".into(), "test@example.com".into()),
    ]
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

impl Repo {
    /// Copies the fixture, lets `prepare` adjust it, then commits it.
    fn with(name: &str, prepare: impl FnOnce(&Path)) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        for d in ["runtime", ".config/sluice", "data", "state"] {
            fs::create_dir_all(home.join(d)).unwrap();
        }
        fs::write(
            home.join(".config/sluice/config.toml"),
            "version = 1\n[admission]\npsi_enabled = false\n",
        )
        .unwrap();
        let dir = root.path().join("repo");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/init")
            .join(name);
        copy_tree(&fixture, &dir);
        let repo = Self {
            _root: root,
            home,
            dir,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        prepare(&repo.dir);
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "fixture", "--no-verify"]);
        repo
    }

    fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    fn git(&self, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
            .args(args)
            .current_dir(&self.dir)
            .env_clear()
            .envs(env_for(&self.home))
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn git_ok(&self, args: &[&str]) -> bool {
        StdCommand::new("git")
            .args(args)
            .current_dir(&self.dir)
            .env_clear()
            .envs(env_for(&self.home))
            .output()
            .unwrap()
            .status
            .success()
    }

    fn sluice_in(&self, dir: &Path, args: &[&str]) -> Output {
        Command::cargo_bin("sluice")
            .unwrap()
            .current_dir(dir)
            .env_clear()
            .envs(env_for(&self.home))
            .args(args)
            .output()
            .unwrap()
    }

    fn sluice(&self, args: &[&str]) -> Output {
        self.sluice_in(&self.dir, args)
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.dir.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn config(&self) -> RepoConfig {
        let path = self.dir.join(".sluice.toml");
        RepoConfig::parse(&fs::read_to_string(&path).unwrap(), &path).expect("config parses")
    }

    /// Every file but `.git/`: relative path to bytes and mode.
    fn snapshot(&self) -> BTreeMap<String, (Vec<u8>, u32)> {
        fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, (Vec<u8>, u32)>) {
            for entry in fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                let meta = fs::symlink_metadata(&path).unwrap();
                if meta.is_dir() {
                    walk(&path, root, out);
                } else if meta.is_file() {
                    out.insert(
                        path.strip_prefix(root).unwrap().display().to_string(),
                        (fs::read(&path).unwrap(), meta.permissions().mode()),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.dir, &self.dir, &mut out);
        out
    }

    /// The hook files `init` may edit.
    fn hook_files(&self) -> BTreeMap<String, Vec<u8>> {
        self.snapshot()
            .into_iter()
            .filter(|(path, _)| {
                path.starts_with(".husky/")
                    || path.starts_with(".githooks/")
                    || path == "lefthook.yml"
                    || path == ".pre-commit-config.yaml"
            })
            .map(|(path, (bytes, _))| (path, bytes))
            .collect()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The `(hook, gate)` pairs of every `sluice run --hook H G` in `text`.
fn wired(text: &str) -> BTreeSet<(String, String)> {
    let re = Regex::new(r"sluice run --hook ([\w-]+) ([\w-]+)").unwrap();
    re.captures_iter(text)
        .map(|c| (c[1].to_owned(), c[2].to_owned()))
        .collect()
}

/// A hook line that runs the gate through Sluice and falls back to `original`.
fn fb(hook: &str, gate: &str, original: &str) -> String {
    format!(
        "if command -v sluice >/dev/null 2>&1; then sluice run --hook {hook} {gate}; \
         else {original}; fi"
    )
}

/// The same inside `sh -c '...'`, as a pre-commit `entry:` has it.
fn fb_sh(hook: &str, gate: &str, original: &str) -> String {
    format!("sh -c '{}'", fb(hook, gate, original))
}

/// Every `.sluice-tmp-` file under `dir`, `.git/` included.
fn temp_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            out.extend(temp_files(&path));
        } else if path.to_string_lossy().contains("sluice-tmp") {
            out.push(path);
        }
    }
    out
}

/// The checks every fixture must pass; returns the repository after `init`.
fn check_init(repo: &Repo, expect_hooks: bool) {
    // `--dry-run` writes nothing: same files, same index state, same config.
    let before = repo.snapshot();
    let porcelain = repo.git(&["status", "--porcelain"]);
    let hooks_path_set = repo.git_ok(&["config", "--get", "core.hooksPath"]);
    let dry = repo.sluice(&["init", "--dry-run"]);
    assert!(dry.status.success(), "dry run: {}", stderr(&dry));
    assert!(stdout(&dry).contains("dry run"), "{}", stdout(&dry));
    assert_eq!(before, repo.snapshot(), "dry run changed the tree");
    assert_eq!(porcelain, repo.git(&["status", "--porcelain"]));
    assert_eq!(
        hooks_path_set,
        repo.git_ok(&["config", "--get", "core.hooksPath"]),
        "dry run set core.hooksPath"
    );

    // The real run succeeds and the result parses.
    let run = repo.sluice(&["init"]);
    assert!(run.status.success(), "init: {}", stderr(&run));
    let config = repo.config();
    assert!(
        stdout(&run).contains("sluice: wrote .sluice.toml"),
        "{}",
        stdout(&run)
    );
    // Nothing is staged or committed.
    let index = repo.git(&["diff", "--cached", "--name-only"]);
    assert!(index.is_empty(), "init staged something: {index}");

    // The dry run showed the same hook edits the real run made.
    let edits = |out: &Output| {
        stdout(out)
            .lines()
            .filter(|l| l.starts_with("--- ") || l.starts_with("+++ "))
            .count()
    };
    assert_eq!(edits(&dry), edits(&run));

    // `doctor` has no failure.
    let doctor = repo.sluice(&["doctor", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&doctor.stdout)
        .unwrap_or_else(|e| panic!("doctor: {e}: {}", stderr(&doctor)));
    assert!(
        report["checks"].as_array().is_some_and(|c| c
            .iter()
            .any(|c| c["name"] == "repo-config" && c["status"] == "ok")),
        "doctor did not read the config: {report}"
    );
    let failures: Vec<&serde_json::Value> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["status"] == "fail")
        .collect();
    assert!(failures.is_empty(), "doctor failures: {failures:?}");

    // `hook print` and the hook files name the same gates.
    let files = repo.hook_files();
    let from_files: BTreeSet<(String, String)> = files
        .values()
        .flat_map(|bytes| wired(&String::from_utf8_lossy(bytes)))
        .collect();
    let expected: BTreeSet<(String, String)> = config
        .gates
        .iter()
        .flat_map(|(name, gate)| {
            gate.hooks
                .iter()
                .map(|h| (h.clone(), name.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    for manager in ["husky", "lefthook", "pre-commit", "plain"] {
        let printed = repo.sluice(&["hook", "print", manager]);
        assert!(printed.status.success());
        assert_eq!(wired(&stdout(&printed)), expected, "hook print {manager}");
    }
    if expect_hooks {
        assert_eq!(from_files, expected, "hook files and config disagree");
    }

    // A second run is idempotent: no hook and no config changes.
    let after = repo.snapshot();
    let again = repo.sluice(&["init", "--force"]);
    assert!(again.status.success(), "second init: {}", stderr(&again));
    assert_eq!(after, repo.snapshot(), "the second init changed the tree");
    assert!(
        !stdout(&again).contains("\n+++ "),
        "the second init edited a hook:\n{}",
        stdout(&again)
    );
}

fn mode(repo: &Repo, rel: &str) -> u32 {
    fs::metadata(repo.dir.join(rel))
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn angular_husky_keeps_the_formatter_and_wires_lint_and_test() {
    let repo = Repo::new("angular-husky");
    let dry = repo.sluice(&["init", "--dry-run"]);
    let text = stdout(&dry);
    assert!(
        text.contains(&format!("+{}", fb("pre-commit", "lint", "ng lint"))),
        "{text}"
    );
    assert!(text.contains("-ng lint"), "{text}");
    check_init(&repo, true);
    assert_eq!(
        repo.read(".husky/pre-commit"),
        format!(
            "npx pretty-quick --staged\n{}\n{}\n",
            fb("pre-commit", "lint", "ng lint"),
            fb("pre-commit", "test", "ng test --watch=false")
        )
    );
    let config = repo.config();
    assert_eq!(
        config.gates.keys().map(String::as_str).collect::<Vec<_>>(),
        ["lint", "test"]
    );
    assert_eq!(config.gates["lint"].cmd, "npx ng lint");
    assert_eq!(config.gates["lint"].weight, Weight::Fixed(2));
    assert_eq!(config.gates["test"].cmd, "npx ng test --watch=false");
    assert_eq!(config.gates["test"].weight, Weight::Auto);
    assert_eq!(config.gates["test"].timeout.as_secs(), 30 * 60);
    assert_eq!(config.gates["lint"].timeout.as_secs(), 10 * 60);
    assert!(
        config.gates["lint"]
            .key_inputs
            .iter()
            .any(|k| k == "node --version")
    );
    // No hook manager was added next to husky.
    assert!(!repo.dir.join(".githooks").exists());
    assert!(!repo.git_ok(&["config", "--get", "core.hooksPath"]));
}

#[test]
fn node_pnpm_without_hooks_gets_githooks_and_a_local_hooks_path() {
    let repo = Repo::new("node-pnpm");
    check_init(&repo, true);
    let config = repo.config();
    assert_eq!(config.gates["lint"].cmd, "pnpm run lint");
    assert_eq!(config.gates["typecheck"].cmd, "pnpm run typecheck");
    assert_eq!(config.gates["test"].cmd, "pnpm test");
    assert_eq!(
        config.gates["test"].env.get("CI").map(String::as_str),
        Some("1")
    );
    assert_eq!(config.gates["test"].weight, Weight::Auto);
    assert!(
        config.gates["test"]
            .key_inputs
            .iter()
            .any(|k| k == "pnpm --version")
    );
    // The merge queue's fresh worktree has no node_modules: prepare installs them.
    assert_eq!(
        config.queue.prepare.as_deref(),
        Some("pnpm install --frozen-lockfile --prefer-offline")
    );
    let text = repo.read(".sluice.toml");
    assert!(text.contains("# The merge queue verifies each train in a fresh worktree"));
    assert!(text.find("[queue]").unwrap() < text.find("[gates.").unwrap());
    assert_eq!(
        repo.read(".githooks/pre-commit"),
        "#!/bin/sh\nset -e\ncommand -v sluice >/dev/null 2>&1 || exit 0\n\
         sluice run --hook pre-commit lint\nsluice run --hook pre-commit typecheck\n"
    );
    assert_eq!(
        repo.read(".githooks/pre-push"),
        "#!/bin/sh\nset -e\ncommand -v sluice >/dev/null 2>&1 || exit 0\n\
         sluice run --hook pre-push test\n"
    );
    // A clone without Sluice can still commit.
    let bare_path = tempfile::tempdir().unwrap();
    let status = StdCommand::new("/bin/sh")
        .args(["-e", ".githooks/pre-commit"])
        .current_dir(&repo.dir)
        .env_clear()
        .env("PATH", bare_path.path())
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(mode(&repo, ".githooks/pre-commit"), 0o755);
    assert_eq!(mode(&repo, ".githooks/pre-push"), 0o755);
    assert_eq!(
        repo.git(&["config", "--get", "core.hooksPath"]),
        ".githooks"
    );
}

#[test]
fn node_yarn_lefthook_is_edited_textually() {
    let repo = Repo::new("node-yarn-lefthook");
    let before = repo.read("lefthook.yml");
    check_init(&repo, true);
    let after = repo.read("lefthook.yml");
    let expected = before
        .replace(
            "run: npx tsc --noEmit",
            &format!("run: {}", fb("pre-commit", "tsc", "npx tsc --noEmit")),
        )
        .replace(
            "run: yarn test # the whole suite",
            &format!(
                "run: {} # the whole suite",
                fb("pre-push", "test", "yarn test")
            ),
        );
    assert_eq!(after, expected);
    assert!(after.contains("run: npx prettier --write {staged_files}"));
    assert!(after.starts_with("# Hooks of the project.\n"));
    let config = repo.config();
    assert_eq!(config.gates["tsc"].cmd, "npx tsc --noEmit");
    assert_eq!(config.gates["test"].cmd, "yarn test");
    // No `.yarnrc.yml`: classic yarn.
    assert_eq!(
        config.queue.prepare.as_deref(),
        Some("yarn install --frozen-lockfile")
    );
    // `lint` is detected but no hook runs it.
    assert_eq!(config.gates["lint"].hooks.len(), 0);
    assert!(!repo.dir.join(".githooks").exists());
}

#[test]
fn rust_plain_gets_the_cookbook_gates() {
    let repo = Repo::new("rust-plain");
    check_init(&repo, true);
    let config = repo.config();
    assert_eq!(
        config.gates.keys().map(String::as_str).collect::<Vec<_>>(),
        ["clippy", "fmt", "test"]
    );
    assert_eq!(config.gates["fmt"].cmd, "cargo fmt --check");
    // Cargo fetches into a shared cache: nothing to prepare.
    assert!(config.queue.prepare.is_none());
    assert!(!repo.read(".sluice.toml").contains("[queue]"));
    assert_eq!(config.gates["fmt"].weight, Weight::Fixed(1));
    assert_eq!(config.gates["clippy"].weight, Weight::Auto);
    assert_eq!(config.gates["test"].weight, Weight::Auto);
    for name in ["clippy", "test"] {
        assert_eq!(
            config.gates[name]
                .env
                .get("CARGO_BUILD_JOBS")
                .map(String::as_str),
            Some("4")
        );
    }
    assert!(
        config.gates["test"]
            .key_inputs
            .iter()
            .any(|k| k == "rustc --version")
    );
    assert_eq!(config.gates["fmt"].hooks, ["pre-commit"]);
    assert_eq!(config.gates["test"].hooks, ["pre-push"]);
    assert_eq!(
        repo.git(&["config", "--get", "core.hooksPath"]),
        ".githooks"
    );
}

#[test]
fn python_precommit_wires_local_hooks_and_prefers_the_virtualenv() {
    let repo = Repo::new("python-precommit");
    check_init(&repo, true);
    let yaml = repo.read(".pre-commit-config.yaml");
    // The hook of a remote repository is not ours to touch.
    assert!(yaml.contains("      - id: ruff\n        entry: pytest -q\n"));
    assert!(yaml.contains(&format!(
        "        entry: {}\n        language: system\n        \
         pass_filenames: false\n        stages: [pre-push]\n",
        fb_sh("pre-push", "pytest", "pytest -q")
    )));
    assert!(yaml.contains(&format!(
        "        entry: {}\n        pass_filenames: false\n        \
         language: system\n",
        fb_sh("pre-commit", "pytest-2", "python -m pytest -x")
    )));
    let config = repo.config();
    assert_eq!(config.gates["pytest"].cmd, "pytest -q");
    assert_eq!(config.gates["pytest-2"].cmd, "python -m pytest -x");
    assert_eq!(
        config.gates["ruff"].cmd,
        ".venv/bin/ruff check . && .venv/bin/ruff format --check ."
    );
    assert_eq!(config.gates["mypy"].cmd, ".venv/bin/mypy .");
    assert!(
        config.gates["mypy"]
            .key_inputs
            .iter()
            .any(|k| k == ".venv/bin/python --version")
    );
    assert!(!repo.dir.join(".githooks").exists());
}

#[test]
fn go_gets_vet_and_test() {
    let repo = Repo::new("go");
    check_init(&repo, true);
    let config = repo.config();
    assert!(config.queue.prepare.is_none());
    assert_eq!(config.gates["vet"].cmd, "go vet ./...");
    assert_eq!(config.gates["vet"].weight, Weight::Fixed(2));
    assert_eq!(config.gates["test"].cmd, "go test -p 4 ./...");
    assert_eq!(config.gates["test"].weight, Weight::Auto);
    assert!(
        config.gates["test"]
            .key_inputs
            .iter()
            .any(|k| k == "go version")
    );
}

#[test]
fn an_empty_repository_gets_the_starter_template() {
    let repo = Repo::new("empty");
    check_init(&repo, false);
    assert_eq!(repo.read(".sluice.toml"), starter_repo_config());
    let out = repo.sluice(&["init", "--force", "--dry-run"]);
    assert!(stdout(&out).contains("nothing detected"));
    assert!(stdout(&out).contains("git config core.hooksPath .githooks"));
    assert!(!repo.dir.join(".githooks").exists());
}

/// Husky hooks with CRLF line endings and no final newline.
fn crlf_without_final_newline(dir: &Path) {
    let path = dir.join(".husky/pre-commit");
    let text = fs::read_to_string(&path).unwrap().replace('\n', "\r\n");
    fs::write(&path, text.trim_end_matches("\r\n")).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
}

#[test]
fn mixed_husky_keeps_crlf_modes_and_the_missing_final_newline() {
    let repo = Repo::with("mixed", crlf_without_final_newline);
    check_init(&repo, true);
    assert_eq!(
        repo.read(".husky/pre-commit"),
        format!(
            "#!/bin/sh\r\n. \"$(dirname -- \"$0\")/_/husky.sh\"\r\n\r\n# lint first\r\n\
             {} || exit 1\r\nnpx commitlint --edit\r\n{}\r\n{}",
            fb("pre-commit", "lint", "ng lint"),
            fb("pre-commit", "jest", "CI=true npx jest --ci"),
            fb("pre-commit", "test", "ng test --watch=false")
        )
    );
    assert_eq!(mode(&repo, ".husky/pre-commit"), 0o755);
    assert_eq!(
        repo.read(".husky/pre-push"),
        format!("{}\n", fb("pre-push", "test", "ng test --watch=false"))
    );
    let config = repo.config();
    // The same command in two hooks is one gate in both.
    assert_eq!(config.gates["test"].hooks, ["pre-commit", "pre-push"]);
    assert_eq!(config.gates["test"].cmd, "pnpm exec ng test --watch=false");
    // Leading variables move into the gate's environment.
    assert_eq!(config.gates["jest"].cmd, "npx jest --ci");
    assert_eq!(
        config.gates["jest"].env.get("CI").map(String::as_str),
        Some("true")
    );
}

#[test]
fn init_outside_a_repository_is_an_error() {
    let repo = Repo::new("empty");
    let outside = tempfile::tempdir().unwrap();
    let out = repo.sluice_in(outside.path(), &["init"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not inside a git repository"));
    assert!(!outside.path().join(".sluice.toml").exists());
}

#[test]
fn init_writes_at_the_root_from_a_subdirectory() {
    let repo = Repo::new("go");
    let sub = repo.dir.join("pkg/inner");
    fs::create_dir_all(&sub).unwrap();
    let out = repo.sluice_in(&sub, &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(repo.dir.join(".sluice.toml").is_file());
    assert!(!sub.join(".sluice.toml").exists());
}

#[test]
fn an_existing_config_needs_force_and_usage_errors_exit_2() {
    let repo = Repo::new("go");
    assert!(repo.sluice(&["init"]).status.success());
    let refused = repo.sluice(&["init"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(stderr(&refused).contains("already exists; not overwriting"));
    let refused = repo.sluice(&["init", "--dry-run"]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(repo.sluice(&["init", "--nope"]).status.code(), Some(2));
}

#[test]
fn a_symlinked_hook_inside_the_repository_is_edited_at_its_target() {
    let repo = Repo::with("angular-husky", |dir| {
        fs::create_dir_all(dir.join("tools")).unwrap();
        fs::rename(dir.join(".husky/pre-commit"), dir.join("tools/pre-commit")).unwrap();
        symlink("../tools/pre-commit", dir.join(".husky/pre-commit")).unwrap();
    });
    let out = repo.sluice(&["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        fs::symlink_metadata(repo.dir.join(".husky/pre-commit"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        repo.read("tools/pre-commit"),
        format!(
            "npx pretty-quick --staged\n{}\n{}\n",
            fb("pre-commit", "lint", "ng lint"),
            fb("pre-commit", "test", "ng test --watch=false")
        )
    );
    assert_eq!(temp_files(&repo.dir), Vec::<PathBuf>::new());
}

#[test]
fn a_symlinked_hook_outside_the_repository_is_left_alone_and_a_snippet_is_printed() {
    let outside = tempfile::tempdir().unwrap();
    let shared = outside.path().join("pre-commit");
    let original = "npx pretty-quick --staged\nng lint\nng test --watch=false\n";
    fs::write(&shared, original).unwrap();
    let target = shared.clone();
    let repo = Repo::with("angular-husky", move |dir| {
        fs::remove_file(dir.join(".husky/pre-commit")).unwrap();
        symlink(&target, dir.join(".husky/pre-commit")).unwrap();
    });
    let out = repo.sluice(&["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&shared).unwrap(), original);
    let text = stdout(&out);
    assert!(text.contains("could not edit .husky/pre-commit"), "{text}");
    assert!(
        text.contains(&format!(
            "{} || exit $?",
            fb("pre-commit", "lint", "ng lint")
        )),
        "{text}"
    );
    // The gates still exist.
    assert!(repo.config().gates.contains_key("lint"));
}

#[test]
fn hooks_of_the_git_dir_are_not_replaced_by_a_hooks_path() {
    let repo = Repo::new("rust-plain");
    let hook = repo.dir.join(".git/hooks/commit-msg");
    fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    let out = repo.sluice(&["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!repo.dir.join(".githooks").exists());
    assert!(!repo.git_ok(&["config", "--get", "core.hooksPath"]));
    let text = stdout(&out);
    assert!(text.contains("already has hooks of its own"), "{text}");
    assert!(
        text.contains("git config core.hooksPath .githooks"),
        "{text}"
    );
}

#[test]
fn a_plain_git_dir_hook_with_a_heavy_command_is_rewired() {
    let repo = Repo::new("rust-plain");
    let hook = repo.dir.join(".git/hooks/pre-push");
    fs::write(&hook, "#!/bin/sh\ncargo test --workspace\n").unwrap();
    let mut perms = fs::metadata(&hook).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&hook, perms).unwrap();
    let out = repo.sluice(&["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(&hook).unwrap(),
        format!(
            "#!/bin/sh\n{}\n",
            fb("pre-push", "test", "cargo test --workspace")
        )
    );
    assert_eq!(
        fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!repo.dir.join(".githooks").exists());
    assert_eq!(repo.config().gates["test"].hooks, ["pre-push"]);
}

/// A directory with fake `ng` and `npx` (and `sluice` when asked) that only
/// use shell builtins and append what they ran to `marker`.
fn fake_bin(dir: &Path, with_sluice: bool) -> PathBuf {
    let bin = dir.join(if with_sluice {
        "bin-with"
    } else {
        "bin-without"
    });
    fs::create_dir_all(&bin).unwrap();
    let marker = dir.join("marker");
    let script = |name: &str, body: &str| {
        let path = bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    };
    let log = |who: &str| format!("echo \"{who} $*\" >> '{}'", marker.display());
    script("ng", &format!("{}\nexit ${{NG_EXIT:-0}}", log("ng")));
    script("npx", "exit 0");
    if with_sluice {
        script(
            "sluice",
            &format!("{}\nexit ${{SLUICE_EXIT:-0}}", log("sluice")),
        );
    }
    bin
}

/// Runs the hook script with `sh -e` and only `bin` in `PATH`.
fn run_hook(repo: &Repo, bin: &Path, rel: &str, vars: &[(&str, &str)]) -> Option<i32> {
    StdCommand::new("/bin/sh")
        .args(["-e", rel])
        .current_dir(&repo.dir)
        .env_clear()
        .env("PATH", bin)
        .envs(vars.iter().copied())
        .status()
        .unwrap()
        .code()
}

#[test]
fn a_rewired_husky_hook_runs_the_original_command_without_sluice() {
    let repo = Repo::new("angular-husky");
    assert!(repo.sluice(&["init"]).status.success());
    let marker = repo.dir.parent().unwrap().join("marker");

    // Sluice absent: the original commands run, and their status is the hook's.
    let bin = fake_bin(repo.dir.parent().unwrap(), false);
    assert_eq!(run_hook(&repo, &bin, ".husky/pre-commit", &[]), Some(0));
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "ng lint\nng test --watch=false\n"
    );
    fs::remove_file(&marker).unwrap();
    assert_eq!(
        run_hook(&repo, &bin, ".husky/pre-commit", &[("NG_EXIT", "3")]),
        Some(3)
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), "ng lint\n");
    fs::remove_file(&marker).unwrap();

    // Sluice present: it runs the gates instead of the original commands.
    let bin = fake_bin(repo.dir.parent().unwrap(), true);
    assert_eq!(run_hook(&repo, &bin, ".husky/pre-commit", &[]), Some(0));
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "sluice run --hook pre-commit lint\nsluice run --hook pre-commit test\n"
    );
    fs::remove_file(&marker).unwrap();
    assert_eq!(
        run_hook(&repo, &bin, ".husky/pre-commit", &[("SLUICE_EXIT", "4")]),
        Some(4)
    );
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "sluice run --hook pre-commit lint\n"
    );
}

/// Turns every fallback line back into the bare 0.4.0 form.
fn bare_form(text: &str) -> String {
    let re = Regex::new(
        r"(?:sh -c ')?if command -v sluice >/dev/null 2>&1; then (sluice run --hook [\w-]+ [\w-]+); else [^']*?; fi'?",
    )
    .unwrap();
    re.replace_all(text, "$1").into_owned()
}

#[test]
fn init_force_upgrades_the_bare_lines_of_0_4_0_once() {
    for (fixture, files) in [
        ("angular-husky", vec![".husky/pre-commit"]),
        ("mixed", vec![".husky/pre-commit", ".husky/pre-push"]),
        ("node-yarn-lefthook", vec!["lefthook.yml"]),
        ("python-precommit", vec![".pre-commit-config.yaml"]),
    ] {
        let repo = Repo::new(fixture);
        assert!(repo.sluice(&["init"]).status.success(), "{fixture}");
        let fallback: Vec<String> = files.iter().map(|f| repo.read(f)).collect();
        for (file, text) in files.iter().zip(&fallback) {
            let bare = bare_form(text);
            assert_ne!(&bare, text, "{fixture}: nothing was bare");
            assert!(!bare.contains("command -v sluice"), "{fixture}: {bare}");
            fs::write(repo.dir.join(file), bare).unwrap();
        }
        // The dry run shows the upgrade and writes nothing.
        let before = repo.snapshot();
        let dry = repo.sluice(&["init", "--force", "--dry-run"]);
        assert!(stdout(&dry).contains("\n+++ "), "{fixture}");
        assert_eq!(before, repo.snapshot(), "{fixture}: dry run wrote");

        let out = repo.sluice(&["init", "--force"]);
        assert!(out.status.success(), "{fixture}: {}", stderr(&out));
        let upgraded: Vec<String> = files.iter().map(|f| repo.read(f)).collect();
        for text in &upgraded {
            assert!(text.contains("else "), "{fixture}: {text}");
        }
        match fixture {
            // The gate's command is the original one.
            "python-precommit" => assert_eq!(upgraded, fallback, "{fixture}"),
            // The gate's environment (`CI=1`, added by detection) comes along.
            "node-yarn-lefthook" => assert_eq!(
                upgraded[0],
                fallback[0].replace("else yarn test;", "else CI=1 yarn test;")
            ),
            _ => {}
        }
        // A second `--force` changes nothing.
        let after = repo.snapshot();
        let again = repo.sluice(&["init", "--force"]);
        assert!(again.status.success(), "{fixture}");
        assert_eq!(after, repo.snapshot(), "{fixture}: second force changed it");
        assert!(!stdout(&again).contains("\n+++ "), "{fixture}");
    }
}

#[test]
fn the_upgrade_uses_the_gate_command_and_environment() {
    let repo = Repo::new("angular-husky");
    assert!(repo.sluice(&["init"]).status.success());
    fs::write(
        repo.dir.join(".husky/pre-commit"),
        "npx pretty-quick --staged\nsluice run --hook pre-commit lint\n\
         sluice run --hook pre-commit test\n",
    )
    .unwrap();
    assert!(repo.sluice(&["init", "--force"]).status.success());
    assert_eq!(
        repo.read(".husky/pre-commit"),
        format!(
            "npx pretty-quick --staged\n{}\n{}\n",
            fb("pre-commit", "lint", "npx ng lint"),
            fb("pre-commit", "test", "npx ng test --watch=false")
        )
    );
}

#[test]
fn a_command_that_cannot_be_nested_is_left_alone_and_printed() {
    let repo = Repo::with("python-precommit", |dir| {
        let path = dir.join(".pre-commit-config.yaml");
        let text = fs::read_to_string(&path).unwrap();
        // The second `entry: pytest -q` is the local hook.
        let (head, tail) = text.split_once("      - id: pytest\n").unwrap();
        let tail = tail.replacen("entry: pytest -q", "entry: pytest -k 'slow or fast'", 1);
        fs::write(&path, format!("{head}      - id: pytest\n{tail}")).unwrap();
    });
    let out = repo.sluice(&["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let yaml = repo.read(".pre-commit-config.yaml");
    assert!(
        yaml.contains("        entry: pytest -k 'slow or fast'\n"),
        "{yaml}"
    );
    let text = stdout(&out);
    assert!(
        text.contains("could not edit .pre-commit-config.yaml (the command cannot be nested"),
        "{text}"
    );
    assert!(
        text.contains("entry: sluice run --hook pre-push pytest\n"),
        "{text}"
    );
}

#[test]
fn files_are_replaced_atomically_keeping_mode_and_symlinks() {
    let repo = Repo::with("angular-husky", |dir| {
        let path = dir.join(".husky/pre-commit");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    });
    let hook = repo.dir.join(".husky/pre-commit");
    let before = fs::metadata(&hook).unwrap();
    assert!(repo.sluice(&["init"]).status.success());
    let after = fs::metadata(&hook).unwrap();
    assert_ne!(before.ino(), after.ino(), "the hook was rewritten in place");
    assert_eq!(after.permissions().mode() & 0o777, 0o750);
    assert_eq!(temp_files(&repo.dir), Vec::<PathBuf>::new());

    // The config is replaced the same way on `--force`.
    let config = repo.dir.join(".sluice.toml");
    let before = fs::metadata(&config).unwrap().ino();
    assert!(repo.sluice(&["init", "--force"]).status.success());
    assert_ne!(before, fs::metadata(&config).unwrap().ino());
    assert_eq!(
        fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(temp_files(&repo.dir), Vec::<PathBuf>::new());
}

#[test]
fn a_symlinked_hook_keeps_its_link_and_the_target_is_replaced() {
    let repo = Repo::with("angular-husky", |dir| {
        fs::create_dir_all(dir.join("tools")).unwrap();
        fs::rename(dir.join(".husky/pre-commit"), dir.join("tools/pre-commit")).unwrap();
        symlink("../tools/pre-commit", dir.join(".husky/pre-commit")).unwrap();
    });
    let target = repo.dir.join("tools/pre-commit");
    let before = fs::metadata(&target).unwrap().ino();
    assert!(repo.sluice(&["init"]).status.success());
    let link = repo.dir.join(".husky/pre-commit");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        Path::new("../tools/pre-commit")
    );
    assert_ne!(before, fs::metadata(&target).unwrap().ino());
    assert_eq!(temp_files(&repo.dir), Vec::<PathBuf>::new());
}
