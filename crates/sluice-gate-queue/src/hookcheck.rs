//! A merged tree whose `.sluice.toml` asks the `pre-push` hook for gates the
//! train does not verify (ADR-0018 section 3 and its review correction).
//!
//! The train's push runs the repository's hook, which reads the **merged**
//! worktree's `.sluice.toml`; the daemon vouches only for gates the train
//! verified. A member that adds a `pre-push` gate (the train runs the target's
//! configuration, not the member's) would therefore always end in a push-time
//! refusal after a human approved. The runner looks for it at build time.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use sluice_gate_core::config::{REPO_CONFIG_FILE, RepoConfig};

/// What is wrong with the merged configuration.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    /// The merged `.sluice.toml` does not parse (the parse error).
    Invalid(String),
    /// It asks the `pre-push` hook for these gates, which the train does not
    /// verify.
    Unverified(Vec<String>),
}

/// The text of the merged worktree's `.sluice.toml`; `None` when it has none
/// (the hook then reads nothing). Blocking.
///
/// # Errors
///
/// A read failure other than a missing file.
pub(crate) fn read_merged(worktree: &Path) -> Result<Option<String>> {
    let path = worktree.join(REPO_CONFIG_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Compares the gates the merged configuration `text` asks of the `pre-push`
/// hook with the gate names the train `verified`.
pub(crate) fn check(text: &str, verified: &[String]) -> Option<Problem> {
    let parsed = match RepoConfig::parse(text, &PathBuf::from(REPO_CONFIG_FILE)) {
        Ok(c) => c,
        Err(e) => return Some(Problem::Invalid(format!("{e:#}"))),
    };
    let missing: Vec<String> = parsed
        .gates_for_hook("pre-push")
        .into_iter()
        .map(|(n, _)| n.to_owned())
        .filter(|n| !verified.contains(n))
        .collect();
    (!missing.is_empty()).then_some(Problem::Unverified(missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: &str = "version = 1\n[gates.test]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n\
                       [gates.lint]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n";

    #[test]
    fn only_pre_push_gates_the_train_did_not_verify_are_reported() {
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(check(TWO, &names(&["test", "lint"])), None);
        assert_eq!(
            check(TWO, &names(&["test"])),
            Some(Problem::Unverified(names(&["lint"])))
        );
        assert_eq!(
            check(TWO, &[]),
            Some(Problem::Unverified(names(&["lint", "test"])))
        );
        // A gate without the hook is not the hook's business.
        let plain = "version = 1\n[gates.test]\ncmd = \"true\"\n";
        assert_eq!(check(plain, &[]), None);
    }

    #[test]
    fn an_unparsable_config_is_reported_with_the_error() {
        let Some(Problem::Invalid(why)) = check("version = [", &[]) else {
            panic!("expected an invalid config");
        };
        assert_ne!(why, "");
    }

    #[test]
    fn a_missing_file_is_no_config() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_merged(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join(REPO_CONFIG_FILE), "version = 1\n").unwrap();
        assert!(read_merged(dir.path()).unwrap().is_some());
    }
}
