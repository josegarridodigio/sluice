//! The unit model of the hook runner (ADR-0021, wave H2a).
//!
//! A hook resolves to either [`Plan::Delegate`] (run it untouched, with the
//! reason) or a [`Program`]: ordered lists of [`Unit`]s. Managers produce
//! programs through an adapter ([`super::plan`] for husky and plain hooks);
//! the runner ([`super::runner`]) does not care which manager made them.

use std::path::PathBuf;

use super::delegate::Manager;

/// Whether a unit may change files or the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// May change files or the index (or is not known to be read-only): it
    /// always runs and its result is never cached.
    Mutating,
    /// Only reads the working tree: its pass is cached by what it saw.
    Check,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mutating => "mutating",
            Self::Check => "check",
        }
    }
}

/// One command of a hook, run as a job of the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub manager: Manager,
    pub hook: String,
    /// Position among the hook's units, from 1, in file order.
    pub index: usize,
    /// The command as `sh -c` runs it (the words as written).
    pub command: String,
    /// The words after quote removal.
    pub argv: Vec<String>,
    /// Working directory (the worktree root, where git runs hooks).
    pub cwd: PathBuf,
    /// Entries the manager puts before `PATH` (husky: `node_modules/.bin`,
    /// relative, resolved against the working directory).
    pub path_prepend: Vec<String>,
    pub kind: Kind,
}

impl Unit {
    /// The unit's name in logs, `status` and the cache key:
    /// `<manager>:<hook>#<index>`.
    pub fn name(&self) -> String {
        format!("{}:{}#{}", self.manager.slug(), self.hook, self.index)
    }
}

/// What to run for a hook: lists of units joined by `&&` (one element for a
/// plain command), executed in order with `sh -e` semantics ([`super::runner`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub lists: Vec<Vec<Unit>>,
    /// The file ends with `exit 0`: the hook's status is 0 unless an earlier
    /// line already ended it.
    pub exit_ok: bool,
}

impl Program {
    pub fn unit_count(&self) -> usize {
        self.lists.iter().map(Vec::len).sum()
    }

    /// Every unit in file order.
    pub fn units(&self) -> impl Iterator<Item = &Unit> {
        self.lists.iter().flatten()
    }
}

/// What the runner does with a hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Run the hook untouched (H1); the reason is shown by `hooks status`.
    Delegate(String),
    /// Run these units.
    Units(Program),
}
