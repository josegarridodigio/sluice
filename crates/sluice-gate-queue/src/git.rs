//! Git calls of the queue, all through `sluice_gate_core::git` (the only
//! place that spawns `git`).
//!
//! Environment: the daemon's sanitised env with **every** `GIT_*` variable
//! removed, plus `LC_ALL=C` (git's messages are parsed) and
//! `GIT_TERMINAL_PROMPT=0` (the daemon has no terminal), plus the
//! allowlisted credentials variables of the client a call acts for.
//! Merge and ref operations add `-c core.hooksPath=/dev/null`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Context as _, Result};
use sluice_gate_core::git as core_git;

/// Environment map.
pub type EnvMap = BTreeMap<String, String>;

/// Credentials variables taken from the approving / enqueuing client.
pub const ENV_ALLOWLIST: &[&str] = &[
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "HOME",
    "PATH",
    "TERM",
];

/// Keeps only the [`ENV_ALLOWLIST`] variables of `env`.
#[must_use]
pub fn allowlisted(env: &EnvMap) -> EnvMap {
    env.iter()
        .filter(|(k, _)| ENV_ALLOWLIST.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Hooks disabled for merge and ref operations.
const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

/// A git runner bound to an environment.
#[derive(Debug, Clone)]
pub struct Git {
    env: EnvMap,
}

impl Git {
    /// From the daemon's sanitised env: drops every `GIT_*` variable and adds
    /// `LC_ALL=C` and `GIT_TERMINAL_PROMPT=0`.
    #[must_use]
    pub fn new(base: &EnvMap) -> Self {
        let mut env: EnvMap = base
            .iter()
            .filter(|(k, _)| !k.starts_with("GIT_"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        env.insert("LC_ALL".into(), "C".into());
        env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        Self { env }
    }

    /// The same runner with a client's allowlisted variables on top.
    #[must_use]
    pub fn with_client_env(&self, client: &EnvMap) -> Self {
        let mut env = self.env.clone();
        env.extend(allowlisted(client));
        Self { env }
    }

    /// The environment used for git.
    #[must_use]
    pub fn env(&self) -> &EnvMap {
        &self.env
    }

    fn repo_args<'a>(common: &'a str, args: &[&'a str], hooks: bool) -> Vec<&'a str> {
        let mut v: Vec<&str> = Vec::with_capacity(args.len() + 3);
        if !hooks {
            v.extend(NO_HOOKS);
        }
        v.push("--git-dir");
        v.push(common);
        v.extend(args);
        v
    }

    /// Runs `git --git-dir=<common> <args>` (hooks disabled); trimmed stdout.
    ///
    /// # Errors
    ///
    /// Spawn failure or non-zero exit.
    pub fn repo(&self, common: &Path, args: &[&str]) -> Result<String> {
        let c = common.to_string_lossy();
        let all = Self::repo_args(&c, args, false);
        core_git::run(&all, common, &self.env, false)
            .with_context(|| format!("git {}", args.join(" ")))
    }

    /// Like [`Git::repo`] but returns the raw output whatever the status.
    ///
    /// # Errors
    ///
    /// Spawn failure.
    pub fn repo_out(&self, common: &Path, args: &[&str]) -> Result<Output> {
        let c = common.to_string_lossy();
        let all = Self::repo_args(&c, args, false);
        core_git::run_output(&all, common, &self.env).context("spawning git")
    }

    /// Runs `git <args>` inside the worktree `wt` (hooks disabled).
    ///
    /// # Errors
    ///
    /// Spawn failure or non-zero exit.
    pub fn wt(&self, wt: &Path, args: &[&str]) -> Result<String> {
        let mut all: Vec<&str> = NO_HOOKS.to_vec();
        all.extend(args);
        core_git::run(&all, wt, &self.env, false)
            .with_context(|| format!("git {} in {}", args.join(" "), wt.display()))
    }

    /// Like [`Git::wt`] but returns the raw output whatever the status.
    ///
    /// # Errors
    ///
    /// Spawn failure.
    pub fn wt_out(&self, wt: &Path, args: &[&str]) -> Result<Output> {
        let mut all: Vec<&str> = NO_HOOKS.to_vec();
        all.extend(args);
        core_git::run_output(&all, wt, &self.env).context("spawning git")
    }

    /// Full SHA of `rev` (`rev-parse --verify --quiet <rev>^{commit}` for
    /// commits); `None` when it does not resolve.
    ///
    /// # Errors
    ///
    /// Spawn failure.
    pub fn resolve_commit(&self, common: &Path, rev: &str) -> Result<Option<String>> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.repo_out(common, &["rev-parse", "--verify", "--quiet", &spec])?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
    }

    /// SHA a full ref name points to, `None` when absent.
    ///
    /// # Errors
    ///
    /// Spawn failure.
    pub fn ref_sha(&self, common: &Path, refname: &str) -> Result<Option<String>> {
        let out = self.repo_out(common, &["rev-parse", "--verify", "--quiet", refname])?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
    }

    /// `git merge-base --is-ancestor a b`: `Some(bool)`, `None` when git
    /// cannot tell (missing objects).
    ///
    /// # Errors
    ///
    /// Spawn failure.
    pub fn is_ancestor(&self, common: &Path, a: &str, b: &str) -> Result<Option<bool>> {
        let out = self.repo_out(common, &["merge-base", "--is-ancestor", a, b])?;
        Ok(match out.status.code() {
            Some(0) => Some(true),
            Some(1) => Some(false),
            _ => None,
        })
    }

    /// `git ls-remote <remote> <refs…>`: map of ref name to SHA.
    ///
    /// # Errors
    ///
    /// The remote cannot be reached or git fails.
    pub fn ls_remote(
        &self,
        common: &Path,
        remote: &str,
        refs: &[&str],
    ) -> Result<BTreeMap<String, String>> {
        let mut args = vec!["ls-remote", remote];
        args.extend(refs);
        let out = self.repo(common, &args)?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (sha, name) = l.split_once('\t')?;
                Some((name.trim().to_owned(), sha.trim().to_owned()))
            })
            .collect())
    }

    /// Branches checked out in any worktree of the repository (short names).
    ///
    /// # Errors
    ///
    /// Git failure.
    pub fn checked_out_branches(&self, common: &Path) -> Result<Vec<String>> {
        let out = self.repo(common, &["worktree", "list", "--porcelain"])?;
        Ok(out
            .lines()
            .filter_map(|l| l.strip_prefix("branch refs/heads/"))
            .map(str::to_owned)
            .collect())
    }

    /// Removes a (locked) linked worktree; falls back to deleting the
    /// directory and pruning. Best effort.
    pub fn remove_worktree(&self, common: &Path, path: &Path) {
        let p = path.to_string_lossy();
        let removed = self
            .repo_out(common, &["worktree", "remove", "--force", "--force", &p])
            .is_ok_and(|o| o.status.success());
        if !removed {
            let _ = std::fs::remove_dir_all(path);
            let _ = self.repo_out(common, &["worktree", "prune"]);
        }
    }
}

/// Absolute per-worktree git dir of `wt`.
///
/// # Errors
///
/// Git failure.
pub fn worktree_git_dir(git: &Git, wt: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git.wt(
        wt,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_drops_git_variables_and_keeps_allowlist() {
        let mut base = EnvMap::new();
        base.insert("GIT_DIR".into(), "/x".into());
        base.insert("GIT_SSH_COMMAND".into(), "daemon".into());
        base.insert("PATH".into(), "/bin".into());
        let g = Git::new(&base);
        assert!(!g.env().contains_key("GIT_DIR"));
        assert!(!g.env().contains_key("GIT_SSH_COMMAND"));
        assert_eq!(g.env()["LC_ALL"], "C");
        let mut client = EnvMap::new();
        client.insert("GIT_SSH_COMMAND".into(), "ssh -i k".into());
        client.insert("SECRET_TOKEN".into(), "no".into());
        client.insert("GIT_DIR".into(), "/no".into());
        let g2 = g.with_client_env(&client);
        assert_eq!(g2.env()["GIT_SSH_COMMAND"], "ssh -i k");
        assert!(!g2.env().contains_key("SECRET_TOKEN"));
        assert!(!g2.env().contains_key("GIT_DIR"));
    }
}
