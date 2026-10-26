//! Resolution of the landing target (design section 2) and the endpoint
//! binding of a remote.

use std::path::Path;

use sha2::{Digest, Sha256};
use sluice_gate_core::config::{QueueConfig, REPO_CONFIG_FILE, RepoConfig, invalid_remote_name};

use crate::git::Git;

/// Where a train lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Remote name; empty for a local branch.
    pub remote: String,
    /// Branch name (short).
    pub branch: String,
}

impl Target {
    /// True for a local branch (landed by `update-ref`).
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.remote.is_empty()
    }

    /// `remote/branch` or `branch`.
    #[must_use]
    pub fn display(&self) -> String {
        if self.is_local() {
            self.branch.clone()
        } else {
            format!("{}/{}", self.remote, self.branch)
        }
    }
}

/// Why a base was refused. `code` is the machine-readable token placed
/// first in the error message (`invalid_base`, `invalid_remote`, `tag`,
/// `sha`, `head`, `own_branch`, `target_checked_out`, `unknown_base`,
/// `no_default_target`, `invalid_config`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct TargetError {
    /// Token.
    pub code: &'static str,
    /// Human-readable explanation.
    pub message: String,
}

impl TargetError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// True for refusals by policy (own branch, checked-out target) as
    /// opposed to a base that is not a branch at all.
    #[must_use]
    pub fn is_policy(&self) -> bool {
        matches!(self.code, "own_branch" | "target_checked_out")
    }
}

/// `[queue]` defaults of the configuration found at `rev` (the enqueued
/// head: only used to pick the default target, never for gates); the
/// built-in defaults when the file is absent.
///
/// # Errors
///
/// `invalid_config` when the file exists at `rev` but does not parse: an
/// older binary (or a typo) would otherwise silently pick the wrong target.
pub fn defaults_at(git: &Git, common: &Path, rev: &str) -> Result<QueueConfig, TargetError> {
    let spec = format!("{rev}:{REPO_CONFIG_FILE}");
    let Ok(text) = git.repo(common, &["show", &spec]) else {
        return Ok(QueueConfig::default());
    };
    RepoConfig::parse(&text, Path::new(REPO_CONFIG_FILE))
        .map(|c| c.queue)
        .map_err(|e| {
            TargetError::new(
                "invalid_config",
                format!(
                    "{REPO_CONFIG_FILE} of the branch does not parse ({e:#}); fix it or pass --base"
                ),
            )
        })
}

fn remotes(git: &Git, common: &Path) -> Vec<String> {
    git.repo(common, &["remote"])
        .map(|s| s.lines().map(|l| l.trim().to_owned()).collect())
        .unwrap_or_default()
}

/// True when `remote` has the branch: a remote-tracking ref exists, or
/// `ls-remote` of its push URL lists it.
fn remote_has_branch(git: &Git, common: &Path, remote: &str, branch: &str) -> bool {
    let tracking = format!("refs/remotes/{remote}/{branch}");
    if git.ref_sha(common, &tracking).ok().flatten().is_some() {
        return true;
    }
    let head = format!("refs/heads/{branch}");
    git.push_url(common, remote)
        .and_then(|url| git.ls_remote(common, &url, &[&head]))
        .is_ok_and(|m| m.contains_key(&head))
}

/// The remote's HEAD branch: `refs/remotes/<remote>/HEAD`, else
/// `ls-remote --symref <push url> HEAD`.
fn remote_head_branch(git: &Git, common: &Path, remote: &str) -> Option<String> {
    let sym = format!("refs/remotes/{remote}/HEAD");
    if let Ok(full) = git.repo(common, &["symbolic-ref", "--quiet", "--", &sym]) {
        let prefix = format!("refs/remotes/{remote}/");
        if let Some(b) = full.strip_prefix(&prefix) {
            return Some(b.to_owned());
        }
    }
    let url = git.push_url(common, remote).ok()?;
    git.remote_head(common, &url).ok().flatten()
}

/// Refuses a remote that git would read as an option, or that is not a
/// configured remote of the repository (`invalid_remote`).
fn check_remote(git: &Git, common: &Path, remote: &str) -> Result<(), TargetError> {
    if let Some(why) = invalid_remote_name(remote) {
        return Err(TargetError::new(
            "invalid_remote",
            format!("the remote name '{remote}' {why}"),
        ));
    }
    if !remotes(git, common).iter().any(|r| r == remote) {
        return Err(TargetError::new(
            "invalid_remote",
            format!("'{remote}' is not a configured remote of this repository"),
        ));
    }
    Ok(())
}

fn looks_like_sha(s: &str) -> bool {
    (4..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolves `base` (as typed; `None` = `[queue] remote`/`target` of
/// `defaults`) for a job of `job_branch` in the repository `common`.
///
/// Order: `HEAD` refused; `refs/heads/x`, `refs/remotes/r/x` and
/// `refs/tags/x` spelled out are honoured; `<remote>/<branch>` when the
/// first segment names a configured remote; otherwise the name is tried as a
/// branch of `defaults.remote`, then as a local branch; a tag or a SHA of
/// that name is refused. Then: the job's own branch and a local branch
/// checked out in any worktree are refused.
///
/// # Errors
///
/// [`TargetError`].
pub fn resolve(
    git: &Git,
    common: &Path,
    job_branch: &str,
    base: Option<&str>,
    defaults: &QueueConfig,
) -> Result<Target, TargetError> {
    let target = match base.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            let remote = defaults.remote.clone();
            check_remote(git, common, &remote)?;
            let branch = match &defaults.target {
                Some(t) => t.clone(),
                None => remote_head_branch(git, common, &remote).ok_or_else(|| {
                    TargetError::new(
                        "no_default_target",
                        format!(
                            "cannot tell the default branch of remote '{remote}'; \
                             pass --base or set [queue] target"
                        ),
                    )
                })?,
            };
            Target { remote, branch }
        }
        Some(spec) => resolve_spec(git, common, spec, &defaults.remote)?,
    };
    if target.branch == job_branch {
        return Err(TargetError::new(
            "own_branch",
            format!(
                "the target {} is the queued branch itself",
                target.display()
            ),
        ));
    }
    if target.is_local() {
        let checked = git.checked_out_branches(common).map_err(|e| {
            TargetError::new("invalid_base", format!("cannot list worktrees: {e:#}"))
        })?;
        if checked.contains(&target.branch) {
            return Err(TargetError::new(
                "target_checked_out",
                format!(
                    "local branch '{}' is checked out in a worktree; updating it would desync \
                     that worktree",
                    target.branch
                ),
            ));
        }
    }
    Ok(target)
}

fn resolve_spec(
    git: &Git,
    common: &Path,
    spec: &str,
    default_remote: &str,
) -> Result<Target, TargetError> {
    if spec == "HEAD" || spec == "@" || spec.ends_with("/HEAD") {
        return Err(TargetError::new(
            "head",
            format!("'{spec}' is not a branch"),
        ));
    }
    if spec.starts_with("refs/tags/") {
        return Err(TargetError::new(
            "tag",
            format!("'{spec}' is a tag, not a branch"),
        ));
    }
    if let Some(b) = spec.strip_prefix("refs/heads/") {
        if git.ref_sha(common, spec).ok().flatten().is_some() {
            return Ok(Target {
                remote: String::new(),
                branch: b.to_owned(),
            });
        }
        return Err(TargetError::new(
            "unknown_base",
            format!("no local branch '{b}'"),
        ));
    }
    let spec = spec.strip_prefix("refs/remotes/").unwrap_or(spec);
    let known = remotes(git, common);
    if let Some((r, b)) = spec.split_once('/')
        && known.iter().any(|k| k == r)
        && !b.is_empty()
    {
        if remote_has_branch(git, common, r, b) {
            return Ok(Target {
                remote: r.to_owned(),
                branch: b.to_owned(),
            });
        }
        return Err(TargetError::new(
            "unknown_base",
            format!("remote '{r}' has no branch '{b}'"),
        ));
    }
    if known.iter().any(|k| k == default_remote)
        && remote_has_branch(git, common, default_remote, spec)
    {
        return Ok(Target {
            remote: default_remote.to_owned(),
            branch: spec.to_owned(),
        });
    }
    if git
        .ref_sha(common, &format!("refs/heads/{spec}"))
        .ok()
        .flatten()
        .is_some()
    {
        return Ok(Target {
            remote: String::new(),
            branch: spec.to_owned(),
        });
    }
    if git
        .ref_sha(common, &format!("refs/tags/{spec}"))
        .ok()
        .flatten()
        .is_some()
    {
        return Err(TargetError::new(
            "tag",
            format!("'{spec}' is a tag, not a branch"),
        ));
    }
    if looks_like_sha(spec) && git.resolve_commit(common, spec).ok().flatten().is_some() {
        return Err(TargetError::new(
            "sha",
            format!("'{spec}' is a commit, not a branch"),
        ));
    }
    Err(TargetError::new(
        "unknown_base",
        format!("'{spec}' is neither a branch of '{default_remote}' nor a local branch"),
    ))
}

/// Strips the userinfo of a URL (`scheme://user:pw@host/...` →
/// `scheme://host/...`); scp-like and local forms are returned unchanged.
#[must_use]
pub fn strip_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{scheme}://{host}{path}")
}

/// The endpoint of a target: `(push URL, endpoint hash)`. Endpoint
/// binding: `sha256(git remote get-url --push <remote>)` with the userinfo
/// stripped (the URL `git push <remote>` uses, `pushurl` and
/// `pushInsteadOf` included); for a local target `("",
/// sha256("local:" + common dir))`.
///
/// # Errors
///
/// An invalid or unknown remote, or several push URLs ([`Git::push_url`]).
pub fn endpoint(git: &Git, common: &Path, remote: &str) -> anyhow::Result<(String, String)> {
    if remote.is_empty() {
        let material = format!("local:{}", common.display());
        return Ok((
            String::new(),
            hex::encode(Sha256::digest(material.as_bytes())),
        ));
    }
    let url = git.push_url(common, remote)?;
    let hash = hex::encode(Sha256::digest(strip_userinfo(&url).as_bytes()));
    Ok((url, hash))
}

/// The endpoint hash alone (see [`endpoint`]).
///
/// # Errors
///
/// As [`endpoint`].
pub fn endpoint_hash(git: &Git, common: &Path, remote: &str) -> anyhow::Result<String> {
    endpoint(git, common, remote).map(|(_, h)| h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_stripped() {
        assert_eq!(
            strip_userinfo("https://user:secret@example.com/o/r.git"),
            "https://example.com/o/r.git"
        );
        assert_eq!(strip_userinfo("ssh://git@host:22/r"), "ssh://host:22/r");
        assert_eq!(
            strip_userinfo("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
        assert_eq!(strip_userinfo("/srv/r.git"), "/srv/r.git");
        assert_eq!(strip_userinfo("https://example.com"), "https://example.com");
    }

    #[test]
    fn sha_shape() {
        assert!(looks_like_sha("deadbeef"));
        assert!(!looks_like_sha("main"));
        assert!(!looks_like_sha("abc"));
    }
}
