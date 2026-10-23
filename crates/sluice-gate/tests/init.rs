//! `sluice init`: stack detection, gates from existing hooks and hook
//! rewiring (zero-config plan task T3, ADR-0016).
//!
//! Every fixture under `tests/fixtures/init/<name>` is copied into a fresh
//! repository in a temporary directory with an isolated HOME.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
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
    let re = Regex::new(r"sluice run --hook (\S+) (\S+)").unwrap();
    re.captures_iter(text)
        .map(|c| (c[1].to_owned(), c[2].to_owned()))
        .collect()
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
        text.contains("+sluice run --hook pre-commit lint"),
        "{text}"
    );
    assert!(text.contains("-ng lint"), "{text}");
    check_init(&repo, true);
    assert_eq!(
        repo.read(".husky/pre-commit"),
        "npx pretty-quick --staged\nsluice run --hook pre-commit lint\n\
         sluice run --hook pre-commit test\n"
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
    assert_eq!(
        repo.read(".githooks/pre-commit"),
        "#!/bin/sh\nset -e\nsluice run --hook pre-commit lint\n\
         sluice run --hook pre-commit typecheck\n"
    );
    assert_eq!(
        repo.read(".githooks/pre-push"),
        "#!/bin/sh\nset -e\nsluice run --hook pre-push test\n"
    );
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
            "run: sluice run --hook pre-commit tsc",
        )
        .replace(
            "run: yarn test # the whole suite",
            "run: sluice run --hook pre-push test # the whole suite",
        );
    assert_eq!(after, expected);
    assert!(after.contains("run: npx prettier --write {staged_files}"));
    assert!(after.starts_with("# Hooks of the project.\n"));
    let config = repo.config();
    assert_eq!(config.gates["tsc"].cmd, "npx tsc --noEmit");
    assert_eq!(config.gates["test"].cmd, "yarn test");
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
    assert!(yaml.contains(
        "        entry: sluice run --hook pre-push pytest\n        language: system\n        \
         pass_filenames: false\n        stages: [pre-push]\n"
    ));
    assert!(yaml.contains(
        "        entry: sluice run --hook pre-commit pytest-2\n        pass_filenames: false\n        \
         language: system\n"
    ));
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
        "#!/bin/sh\r\n. \"$(dirname -- \"$0\")/_/husky.sh\"\r\n\r\n# lint first\r\n\
         sluice run --hook pre-commit lint || exit 1\r\nnpx commitlint --edit\r\n\
         sluice run --hook pre-commit jest\r\nsluice run --hook pre-commit test"
    );
    assert_eq!(mode(&repo, ".husky/pre-commit"), 0o755);
    assert_eq!(
        repo.read(".husky/pre-push"),
        "sluice run --hook pre-push test\n"
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
        "npx pretty-quick --staged\nsluice run --hook pre-commit lint\n\
         sluice run --hook pre-commit test\n"
    );
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
        text.contains("sluice run --hook pre-commit lint || exit $?"),
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
        "#!/bin/sh\nsluice run --hook pre-push test\n"
    );
    assert_eq!(
        fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!repo.dir.join(".githooks").exists());
    assert_eq!(repo.config().gates["test"].hooks, ["pre-push"]);
}
