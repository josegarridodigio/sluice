//! Throwaway repositories for the `sluice run` acceptance tests.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

/// A temporary HOME / XDG layout plus one repository with a `.sluice.toml`.
pub struct Sandbox {
    pub root: TempDir,
    pub home: PathBuf,
    pub repo: PathBuf,
    /// File the test gates append to, so tests can count real runs.
    pub counter: PathBuf,
}

impl Sandbox {
    /// Creates the sandbox with the given `.sluice.toml` body (`version = 1` is prepended).
    pub fn new(config: &str) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join("home");
        let repo = root.path().join("repo");
        let counter = root.path().join("counter");
        for d in [&home, &repo, &home.join(".config/sluice")] {
            fs::create_dir_all(d).expect("mkdir");
        }
        fs::create_dir_all(home.join("runtime")).expect("mkdir runtime");
        fs::write(&counter, "").expect("counter");
        let sb = Self {
            root,
            home,
            repo,
            counter,
        };
        sb.git(&["init", "-q", "-b", "main"]);
        sb.git(&["config", "user.name", "Test"]);
        sb.git(&["config", "user.email", "test@example.com"]);
        sb.git(&["config", "commit.gpgsign", "false"]);
        fs::write(sb.repo.join(".gitignore"), "ignored.txt\n").expect("gitignore");
        fs::write(sb.repo.join("a.txt"), "a\n").expect("a.txt");
        sb.write_config(config);
        sb.git(&["add", "-A"]);
        sb.git(&["commit", "-q", "-m", "init"]);
        sb
    }

    /// Standard sandbox: gates `test` (appends to the counter) and `fail`.
    pub fn standard() -> Self {
        Self::new(
            r#"
[gates.test]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\""
hooks = ["pre-commit", "pre-push"]

[gates.lint]
cmd = "echo lint >> \"$SLUICE_TEST_COUNTER\""
hooks = ["pre-commit"]

[gates.fail]
cmd = "echo boom; echo more >&2; exit 7"

[gates.slow]
cmd = "sleep 5"
timeout = "1s"
"#,
        )
    }

    pub fn write_config(&self, body: &str) {
        fs::write(
            self.repo.join(".sluice.toml"),
            format!("version = 1\n{body}"),
        )
        .expect("config");
    }

    /// Runs git in the repository (or another directory).
    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(self.env())
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.repo, args)
    }

    /// Environment every command runs with: isolated HOME/XDG dirs, no inherited GIT_*/CLAUDE*.
    pub fn env(&self) -> Vec<(String, String)> {
        let path = std::env::var("PATH").unwrap_or_default();
        vec![
            ("PATH".into(), path),
            ("HOME".into(), self.home.display().to_string()),
            (
                "XDG_STATE_HOME".into(),
                self.home.join("state").display().to_string(),
            ),
            (
                "XDG_RUNTIME_DIR".into(),
                self.home.join("runtime").display().to_string(),
            ),
            (
                "XDG_CONFIG_HOME".into(),
                self.home.join(".config").display().to_string(),
            ),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_AUTHOR_NAME".into(), "Test".into()),
            ("GIT_AUTHOR_EMAIL".into(), "test@example.com".into()),
            ("GIT_COMMITTER_NAME".into(), "Test".into()),
            ("GIT_COMMITTER_EMAIL".into(), "test@example.com".into()),
            (
                "SLUICE_TEST_COUNTER".into(),
                self.counter.display().to_string(),
            ),
        ]
    }

    /// `sluice` ready to run in `dir` with the sandbox environment.
    pub fn sluice_in(&self, dir: &Path) -> Command {
        let mut cmd = Command::cargo_bin("sluice").expect("binary");
        cmd.current_dir(dir).env_clear().envs(self.env());
        cmd
    }

    pub fn sluice(&self) -> Command {
        self.sluice_in(&self.repo)
    }

    /// Number of real gate runs so far.
    pub fn runs(&self) -> usize {
        fs::read_to_string(&self.counter)
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// Absolute path of the built `sluice` binary (for hooks).
    pub fn binary() -> PathBuf {
        assert_cmd::cargo::cargo_bin("sluice")
    }

    /// Installs plain hooks under `.githooks/` that call the built binary.
    pub fn install_hooks(&self, pre_commit: &[&str], pre_push: &[&str]) {
        let dir = self.repo.join(".githooks");
        fs::create_dir_all(&dir).expect("hooks dir");
        for (hook, gates) in [("pre-commit", pre_commit), ("pre-push", pre_push)] {
            let mut script = String::from("#!/bin/sh\nset -e\n");
            for gate in gates {
                let _ = writeln!(
                    script,
                    "'{}' run --hook {hook} {gate}",
                    Self::binary().display()
                );
            }
            let path = dir.join(hook);
            fs::write(&path, script).expect("hook");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        self.git(&["config", "core.hooksPath", ".githooks"]);
    }

    pub fn markers_dir(&self) -> PathBuf {
        self.repo.join(".git/sluice/markers")
    }

    pub fn marker_count(&self) -> usize {
        fs::read_dir(self.markers_dir()).map_or(0, |d| {
            d.filter_map(Result::ok)
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    Path::new(&n)
                        .extension()
                        .is_some_and(|x| x.eq_ignore_ascii_case("json"))
                        && !n.ends_with(".miss.json")
                })
                .count()
        })
    }

    pub fn miss_count(&self) -> usize {
        fs::read_dir(self.markers_dir()).map_or(0, |d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().ends_with(".miss.json"))
                .count()
        })
    }
}
