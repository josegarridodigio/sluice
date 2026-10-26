//! `--trust-member-config` (ADR-0018 section 6): reading the member's own
//! `.sluice.toml` for a trusted train, and the diff a human reviews before
//! approving it.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};
use sluice_gate_core::config::{REPO_CONFIG_FILE, RepoConfig};

use crate::git::Git;

/// `config_hash` of a member that has no `.sluice.toml`.
pub(crate) const CONFIG_ABSENT: &str = "absent";

/// Largest `config_diff` kept, in bytes; the rest is cut with a note.
pub(crate) const MAX_DIFF_BYTES: usize = 64 * 1024;

/// The member's configuration for a trusted train.
#[derive(Debug)]
pub(crate) struct MemberConfig {
    /// Parsed `.sluice.toml` of the member's head; `None` when it has none
    /// (no gates, the defaults).
    pub(crate) config: Option<RepoConfig>,
    /// `sha256` of the blob (lowercase hex), or [`CONFIG_ABSENT`].
    pub(crate) hash: String,
}

/// `git show <head>:.sluice.toml`, hashed and parsed. Blocking. A file absent
/// at `head` (`git ls-tree` prints nothing) is "no config"; a git error is an
/// error, never "no gates".
///
/// # Errors
///
/// A git failure (the message carries it) or an invalid config (the message
/// names the parse error).
pub(crate) fn read_member_config(git: &Git, common: &Path, head: &str) -> Result<MemberConfig> {
    let spec = format!("{head}:{REPO_CONFIG_FILE}");
    let listed = git
        .repo(common, &["ls-tree", head, "--", REPO_CONFIG_FILE])
        .with_context(|| format!("cannot look for {REPO_CONFIG_FILE} at {head}"))?;
    if listed.trim().is_empty() {
        return Ok(MemberConfig {
            config: None,
            hash: CONFIG_ABSENT.to_owned(),
        });
    }
    let out = git.repo_out(common, &["show", &spec])?;
    if !out.status.success() {
        anyhow::bail!(
            "cannot read {spec}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let hash = hex::encode(Sha256::digest(&out.stdout));
    let text = String::from_utf8_lossy(&out.stdout);
    let config = RepoConfig::parse(&text, &PathBuf::from(&spec))
        .context("invalid .sluice.toml on the branch")?;
    Ok(MemberConfig {
        config: Some(config),
        hash,
    })
}

/// Cuts `text` to [`MAX_DIFF_BYTES`] (at a character boundary) and says so.
pub(crate) fn cap(mut text: String) -> String {
    if text.len() <= MAX_DIFF_BYTES {
        return text;
    }
    let mut end = MAX_DIFF_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n... (truncated at 64 KiB: read the file in the branch)\n");
    text
}

/// The change a trusted train makes to the judges:
/// `git diff <base>...<head> -- .sluice.toml`, or, when the base has no such file, a
/// note and the whole member file. Never fails: a problem is the text.
/// Blocking.
pub(crate) fn config_diff(git: &Git, common: &Path, base: &str, head: &str) -> String {
    let base_spec = format!("{base}:{REPO_CONFIG_FILE}");
    let base_has = git
        .repo_out(common, &["cat-file", "-e", &base_spec])
        .is_ok_and(|o| o.status.success());
    if base_has {
        let range = format!("{base}...{head}");
        let args = [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            range.as_str(),
            "--",
            REPO_CONFIG_FILE,
        ];
        return match git.repo_out(common, &args) {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout).into_owned();
                if text.trim().is_empty() {
                    "(the branch does not change .sluice.toml)\n".to_owned()
                } else {
                    cap(text)
                }
            }
            Ok(o) => format!(
                "(cannot compute the diff: {})\n",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => format!("(cannot compute the diff: {e:#})\n"),
        };
    }
    let head_spec = format!("{head}:{REPO_CONFIG_FILE}");
    match git.repo_out(common, &["show", &head_spec]) {
        Ok(o) if o.status.success() => cap(format!(
            "(the base has no .sluice.toml; this is the whole file of the branch)\n{}",
            String::from_utf8_lossy(&o.stdout)
        )),
        Ok(_) => "(neither the base nor the branch has a .sluice.toml)\n".to_owned(),
        Err(e) => format!("(cannot read the file: {e:#})\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_diff_is_cut_at_a_character_boundary_with_a_note() {
        let short = "+gate\n".to_owned();
        assert_eq!(cap(short.clone()), short);
        // Multi-byte characters straddling the limit must not split.
        let long = "é".repeat(MAX_DIFF_BYTES);
        let cut = cap(long);
        assert!(cut.ends_with("read the file in the branch)\n"), "{cut}");
        assert!(cut.len() < MAX_DIFF_BYTES + 100);
        assert!(cut.contains("truncated at 64 KiB"));
    }

    /// A repository with one commit holding `files`; returns its directory
    /// guard, the git wrapper, the git dir and the commit.
    fn repo_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Git, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let mut env = std::collections::BTreeMap::new();
        env.insert("PATH".to_owned(), std::env::var("PATH").unwrap_or_default());
        env.insert("HOME".to_owned(), dir.path().display().to_string());
        env.insert("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned());
        let git = Git::new(&env);
        let work = dir.path().to_path_buf();
        git.wt(&work, &["init", "-q", "."]).unwrap();
        for (name, text) in files {
            std::fs::write(work.join(name), text).unwrap();
        }
        git.wt(&work, &["add", "-A"]).unwrap();
        git.wt(
            &work,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "c",
            ],
        )
        .unwrap();
        let head = git.wt(&work, &["rev-parse", "HEAD"]).unwrap();
        let common = work.join(".git");
        (dir, git, common, head)
    }

    #[test]
    fn an_absent_file_is_no_config_but_a_git_error_is_an_error() {
        let (_dir, git, common, head) = repo_with(&[("a.txt", "a\n")]);
        let m = read_member_config(&git, &common, &head).unwrap();
        assert!(m.config.is_none());
        assert_eq!(m.hash, CONFIG_ABSENT);
        let (_d2, git2, common2, head2) = repo_with(&[(REPO_CONFIG_FILE, "version = 1\n")]);
        let m = read_member_config(&git2, &common2, &head2).unwrap();
        assert!(m.config.is_some());
        // A head that does not exist is a git error, not "no gates".
        let missing = "0".repeat(40);
        let e = read_member_config(&git, &common, &missing).unwrap_err();
        assert!(format!("{e:#}").contains("cannot look for"), "{e:#}");
    }
}
