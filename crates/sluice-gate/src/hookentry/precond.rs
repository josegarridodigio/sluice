//! The run-time conditions of a unit program ([`Precondition`]), read from
//! `git status` in the environment git gave the hook (so `git commit -a` and
//! `git commit <paths>`, which use a temporary index, are seen as they are).
//!
//! A false condition never fails a hook: it makes the whole hook a
//! delegation, decided before the first unit starts.

use std::path::Path;

use sluice_gate_core::config::EnvMap;
use sluice_gate_core::git;

use super::units::Precondition;

impl Precondition {
    /// For `hooks status`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::StagedNotPartial => "something is staged and no file is partially staged",
            Self::WorktreeEqualsIndex => "no tracked file has unstaged changes",
        }
    }
}

/// Checks `pre` for the repository at `root`.
///
/// # Errors
///
/// Why the units must not be used now (or that git could not be asked).
pub fn check(pre: Precondition, root: &Path, env: &EnvMap) -> Result<(), String> {
    let out = git::run_output(
        &["--no-optional-locks", "status", "--porcelain", "-uno"],
        root,
        env,
    )
    .map_err(|e| format!("cannot read the git status: {e}"))?;
    if !out.status.success() {
        return Err("git status failed".to_owned());
    }
    evaluate(pre, &String::from_utf8_lossy(&out.stdout))
}

/// `status` is `git status --porcelain` (version 1): `XY path` per line, `X`
/// the index and `Y` the worktree.
fn evaluate(pre: Precondition, status: &str) -> Result<(), String> {
    let entries: Vec<(char, char)> = status
        .lines()
        .filter_map(|l| {
            let mut c = l.chars();
            Some((c.next()?, c.next()?))
        })
        .collect();
    match pre {
        Precondition::StagedNotPartial => {
            // The kinds of change lefthook's `--diff-filter=ACMRD` counts.
            if !entries.iter().any(|(x, _)| "ACMRD".contains(*x)) {
                return Err("nothing is staged, so lefthook would skip the command".to_owned());
            }
            if entries.iter().any(|(x, y)| *x != ' ' && *y != ' ') {
                return Err(
                    "a file is partially staged (lefthook hides its unstaged half while it runs)"
                        .to_owned(),
                );
            }
            Ok(())
        }
        Precondition::WorktreeEqualsIndex => {
            if entries.iter().any(|(_, y)| *y != ' ') {
                return Err(
                    "tracked files have unstaged changes (pre-commit stashes them on every run)"
                        .to_owned(),
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lefthook_needs_staged_files_and_no_partially_staged_one() {
        let pre = Precondition::StagedNotPartial;
        assert!(evaluate(pre, "M  a.txt\nA  b.txt\n").is_ok());
        assert!(
            evaluate(pre, "M  a.txt\n M other.txt\n").is_ok(),
            "unrelated change"
        );
        assert!(evaluate(pre, "").unwrap_err().contains("nothing is staged"));
        assert!(
            evaluate(pre, " M a.txt\n")
                .unwrap_err()
                .contains("nothing is staged")
        );
        assert!(
            evaluate(pre, "MM a.txt\n")
                .unwrap_err()
                .contains("partially staged")
        );
        assert!(
            evaluate(pre, "D  gone.txt\n").is_ok(),
            "deleted counts as staged"
        );
    }

    #[test]
    fn pre_commit_needs_a_worktree_equal_to_the_index() {
        let pre = Precondition::WorktreeEqualsIndex;
        assert!(evaluate(pre, "").is_ok());
        assert!(evaluate(pre, "M  a.txt\nA  b.txt\n").is_ok());
        assert!(evaluate(pre, " M a.txt\n").is_err());
        assert!(evaluate(pre, "MM a.txt\n").is_err());
        assert!(evaluate(pre, " A intent.txt\n").is_err(), "intent to add");
        assert!(evaluate(pre, "UU conflict.txt\n").is_err());
    }
}
