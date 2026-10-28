//! Git calls of the queue, all through `sluice_gate_core::git` (the only
//! place that spawns `git`).
//!
//! Environment: the daemon's sanitised env with **every** `GIT_*` variable
//! removed, plus `LC_ALL=C` (git's messages are parsed) and
//! `GIT_TERMINAL_PROMPT=0` (the daemon has no terminal), plus the
//! allowlisted credentials variables of the client a call acts for.
//! Merge and ref operations add `-c core.hooksPath=/dev/null`.
//!
//! Network calls (`fetch`, `ls-remote`) go to the remote's **push URL**
//! (`git remote get-url --push`, design section 2 "Endpoint binding") and
//! have a deadline (`[queue] network_timeout`): git runs in its own process
//! group, killed on expiry. User-derived positional arguments follow `--`
//! (or `--end-of-options` for `rev-parse`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use sluice_gate_core::config::{DEFAULT_NETWORK_TIMEOUT, invalid_remote_name};
use sluice_gate_core::git as core_git;

use crate::target::strip_userinfo;

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

/// A git runner bound to an environment and a network deadline.
#[derive(Debug, Clone)]
pub struct Git {
    env: EnvMap,
    network_timeout: Duration,
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
        Self {
            env,
            network_timeout: DEFAULT_NETWORK_TIMEOUT,
        }
    }

    /// The same runner with a client's allowlisted variables on top.
    #[must_use]
    pub fn with_client_env(&self, client: &EnvMap) -> Self {
        let mut env = self.env.clone();
        env.extend(allowlisted(client));
        Self {
            env,
            network_timeout: self.network_timeout,
        }
    }

    /// The same runner with another deadline for network calls.
    #[must_use]
    pub fn with_network_timeout(&self, timeout: Duration) -> Self {
        Self {
            env: self.env.clone(),
            network_timeout: timeout.max(Duration::from_millis(1)),
        }
    }

    /// Deadline of network calls.
    #[must_use]
    pub fn network_timeout(&self) -> Duration {
        self.network_timeout
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

    /// Paths of `wt` that differ from its `HEAD` (`git status --porcelain
    /// -z`; untracked, not ignored files too when `untracked`). Read from the
    /// raw NUL-separated output: a trimmed text output loses the leading
    /// space of a ` M path` line, and spaces or quotes in names survive.
    ///
    /// # Errors
    ///
    /// Spawn failure or a non-zero exit.
    pub fn status_paths(&self, wt: &Path, untracked: bool) -> Result<Vec<String>> {
        let mode = if untracked {
            "--untracked-files=normal"
        } else {
            "--untracked-files=no"
        };
        let out = self.wt_out(wt, &["status", "--porcelain", "-z", mode])?;
        if !out.status.success() {
            bail!(
                "git status in {}: {}",
                wt.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(porcelain_z_paths(&out.stdout))
    }

    /// Runs a network call `git --git-dir=<common> <args>` (hooks disabled)
    /// with the deadline; trimmed stdout.
    ///
    /// # Errors
    ///
    /// Spawn failure, non-zero exit or the deadline.
    ///
    /// The error never carries the userinfo of `url` (a password or token
    /// embedded in the remote URL): its text ends up in job details, train
    /// notes and the daemon log.
    fn net(&self, common: &Path, url: &str, args: &[&str]) -> Result<String> {
        let c = common.to_string_lossy();
        let all = Self::repo_args(&c, args, false);
        let out = core_git::run_output_deadline(&all, common, &self.env, self.network_timeout)
            .map_err(|e| anyhow!(hide_userinfo(&e.to_string(), url)))?;
        if !out.status.success() {
            let mut text = core_git::command_error(&all, &out).to_string();
            // `command_error` keeps git's `fatal:` / `error:` lines, but the
            // transport says why in its own words (`user@host: Permission
            // denied (publickey).`): keep its last lines too, so a refused
            // credential is recognised (`push::credentials_refused`).
            let stderr = String::from_utf8_lossy(&out.stderr);
            let others: Vec<&str> = stderr
                .lines()
                .map(str::trim)
                .filter(|l| {
                    !l.is_empty()
                        && !l.starts_with("fatal:")
                        && !l.starts_with("error:")
                        && !l.starts_with("hint:")
                })
                .collect();
            let others = &others[others.len().saturating_sub(3)..];
            if others.iter().any(|l| !text.contains(l)) {
                text.push('\n');
                text.push_str(&others.join("\n"));
            }
            return Err(anyhow!(hide_userinfo(&text, url)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// The push URL of `remote` (`git remote get-url --push`: `pushurl`, else
    /// `url`, with `insteadOf`/`pushInsteadOf` applied): where `git push
    /// <remote>` goes, hence the endpoint every network query of a train
    /// uses. Local (no network).
    ///
    /// # Errors
    ///
    /// An unacceptable name (leading `-`, whitespace), a remote that is not
    /// configured, or a remote with several push URLs (a train lands on
    /// exactly one endpoint).
    pub fn push_url(&self, common: &Path, remote: &str) -> Result<String> {
        if let Some(why) = invalid_remote_name(remote) {
            bail!("invalid remote name '{remote}': it {why}");
        }
        let out = self.repo_out(
            common,
            &["remote", "get-url", "--push", "--all", "--", remote],
        )?;
        if !out.status.success() {
            bail!("'{remote}' is not a configured remote of this repository");
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let urls: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        match urls.as_slice() {
            [url] => Ok((*url).to_owned()),
            [] => bail!("remote '{remote}' has no URL"),
            _ => bail!(
                "remote '{remote}' has {} push URLs; the merge queue pushes to exactly one",
                urls.len()
            ),
        }
    }

    /// `git fetch --no-tags --no-write-fetch-head <url> <refspec>` with the
    /// deadline. `FETCH_HEAD` of the repository is never touched.
    ///
    /// # Errors
    ///
    /// The fetch failed or timed out.
    pub fn fetch(&self, common: &Path, url: &str, refspec: &str) -> Result<()> {
        self.net(
            common,
            url,
            &[
                "fetch",
                "--no-tags",
                "--quiet",
                "--no-write-fetch-head",
                "--",
                url,
                refspec,
            ],
        )
        .with_context(|| format!("git fetch {} {refspec}", strip_userinfo(url)))?;
        Ok(())
    }

    /// Full SHA of `rev` (`rev-parse --verify --quiet <rev>^{commit}` for
    /// commits); `None` when it does not resolve.
    ///
    /// # Errors
    ///
    /// Spawn failure.
    pub fn resolve_commit(&self, common: &Path, rev: &str) -> Result<Option<String>> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.repo_out(
            common,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ],
        )?;
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
        let out = self.repo_out(
            common,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                refname,
            ],
        )?;
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

    /// `git ls-remote -- <url> <refs…>` with the deadline: map of ref name
    /// to SHA. `url` is the push URL ([`Git::push_url`]).
    ///
    /// # Errors
    ///
    /// The remote cannot be reached, git fails or the deadline expired.
    pub fn ls_remote(
        &self,
        common: &Path,
        url: &str,
        refs: &[&str],
    ) -> Result<BTreeMap<String, String>> {
        let mut args = vec!["ls-remote", "--", url];
        args.extend(refs);
        let out = self
            .net(common, url, &args)
            .with_context(|| format!("git ls-remote {}", strip_userinfo(url)))?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (sha, name) = l.split_once('\t')?;
                Some((name.trim().to_owned(), sha.trim().to_owned()))
            })
            .collect())
    }

    /// Full SHA of the tree of `rev` (`rev^{tree}`); `None` when it does not
    /// resolve.
    ///
    /// # Errors
    ///
    /// Spawn failure.
    pub fn tree_of(&self, common: &Path, rev: &str) -> Result<Option<String>> {
        let spec = format!("{rev}^{{tree}}");
        let out = self.repo_out(
            common,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ],
        )?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
    }

    /// Number of commits in `base..tip`; `None` when git cannot tell.
    ///
    /// # Errors
    ///
    /// Spawn failure.
    pub fn count_commits(&self, common: &Path, base: &str, tip: &str) -> Result<Option<u64>> {
        let range = format!("{base}..{tip}");
        let out = self.repo_out(common, &["rev-list", "--count", &range])?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
            .flatten())
    }

    /// Deletes `branch` on `url` only while it is still at `sha`
    /// (`push --force-with-lease=<ref>:<sha> :<ref>`), with the push
    /// deadline `timeout`. Never touches the index or the working tree.
    ///
    /// # Errors
    ///
    /// The remote refused, cannot be reached, or the deadline expired.
    pub fn delete_remote_branch(
        &self,
        common: &Path,
        url: &str,
        branch: &str,
        sha: &str,
        timeout: Duration,
    ) -> Result<()> {
        let reference = format!("refs/heads/{branch}");
        let lease = format!("--force-with-lease={reference}:{sha}");
        let delete = format!(":{reference}");
        self.with_network_timeout(timeout)
            .net(
                common,
                url,
                &["push", "--porcelain", &lease, "--", url, &delete],
            )
            .with_context(|| format!("git push --delete {branch} to {}", strip_userinfo(url)))?;
        Ok(())
    }

    /// The branch `HEAD` points to on `url` (`ls-remote --symref`, with the
    /// deadline); `None` when the remote does not say.
    ///
    /// # Errors
    ///
    /// The remote cannot be reached, git fails or the deadline expired.
    pub fn remote_head(&self, common: &Path, url: &str) -> Result<Option<String>> {
        let out = self.net(common, url, &["ls-remote", "--symref", "--", url, "HEAD"])?;
        Ok(out.lines().find_map(|l| {
            let rest = l.strip_prefix("ref: refs/heads/")?;
            Some(rest.split('\t').next()?.trim().to_owned())
        }))
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

/// `text` with every occurrence of `url` replaced by `url` without its
/// userinfo (`https://user:token@host/r` → `https://host/r`); `text`
/// unchanged when `url` has none.
#[must_use]
pub fn hide_userinfo(text: &str, url: &str) -> String {
    let bare = strip_userinfo(url);
    if url.is_empty() || bare == url {
        text.to_owned()
    } else {
        text.replace(url, &bare)
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

/// The paths of `git status --porcelain -z` output: `XY path` entries; a
/// rename or copy (`R`/`C` in either column) is followed by its source path,
/// which is skipped.
fn porcelain_z_paths(raw: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut entries = raw.split(|b| *b == 0).filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        let Some(path) = entry.get(3..) else { continue };
        paths.push(String::from_utf8_lossy(path).into_owned());
        if entry[..2].iter().any(|c| matches!(c, b'R' | b'C')) {
            let _ = entries.next();
        }
    }
    paths
}

#[cfg(test)]
mod tests {

    #[test]
    fn porcelain_z_paths_keep_every_character() {
        let raw = b" M a.txt\0M  dir/b c.txt\0R  new.txt\0old.txt\0?? u\0";
        assert_eq!(
            porcelain_z_paths(raw),
            ["a.txt", "dir/b c.txt", "new.txt", "u"]
        );
        assert_eq!(porcelain_z_paths(b""), Vec::<String>::new());
    }

    use super::*;

    #[test]
    fn userinfo_is_hidden_from_messages() {
        let url = "https://bot:hunter2@example.com/o/r.git";
        let text = format!("git ls-remote -- {url} refs/heads/main: exit status: 128");
        let out = hide_userinfo(&text, url);
        assert!(!out.contains("hunter2") && !out.contains("bot@"), "{out}");
        assert!(
            out.contains("https://example.com/o/r.git refs/heads/main"),
            "{out}"
        );
        assert_eq!(hide_userinfo("x /srv/r.git", "/srv/r.git"), "x /srv/r.git");
    }

    #[test]
    fn a_failed_network_call_does_not_reveal_the_url_password() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        let mut base = EnvMap::new();
        base.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        base.insert("HOME".into(), common.display().to_string());
        base.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
        let git = Git::new(&base).with_network_timeout(Duration::from_secs(20));
        git.repo(common, &["init", "-q", "--bare", "."]).unwrap();
        // Port 9 on localhost: refused at once, no network involved.
        let url = "http://bot:s3cr3t-in-url@127.0.0.1:9/r.git";
        let e = git
            .ls_remote(common, url, &["refs/heads/main"])
            .unwrap_err();
        let e2 = git
            .fetch(common, url, "+refs/heads/main:refs/x")
            .unwrap_err();
        for text in [format!("{e:#}"), format!("{e2:#}")] {
            assert!(!text.contains("s3cr3t-in-url"), "{text}");
            assert!(text.contains("http://127.0.0.1:9/r.git"), "{text}");
        }
    }

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
