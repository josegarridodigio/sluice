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
    /// Keep git's own variables (`GIT_INDEX_FILE`, ...) in the unit's
    /// environment. Mutating units always do; a manager that asks git itself
    /// what is staged (lefthook, pre-commit) needs them even for a check.
    pub git_env: bool,
    /// Commands whose output joins the cache key (tool version, effective
    /// configuration). Only checks use them.
    pub key_inputs: Vec<String>,
    /// Files (relative to the worktree root) whose content joins the key.
    pub key_files: Vec<PathBuf>,
}

impl Unit {
    /// The unit's name in logs, `status` and the cache key:
    /// `<manager>:<hook>#<index>`.
    pub fn name(&self) -> String {
        format!("{}:{}#{}", self.manager.slug(), self.hook, self.index)
    }
}

/// What must hold when the hook runs for its units to behave exactly like the
/// manager's own whole-hook run; when it does not, the hook is delegated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition {
    /// lefthook's `pre-commit`: something is staged (a command that finds no
    /// staged file is skipped, which `--command` reports as a failure) and no
    /// file is partially staged (lefthook hides the unstaged half while it
    /// runs).
    StagedNotPartial,
    /// pre-commit's commit stages: no tracked file differs from the index
    /// (`pre-commit run` stashes unstaged changes on every invocation).
    WorktreeEqualsIndex,
}

/// What to run for a hook: lists of units joined by `&&` (one element for a
/// plain command), executed in order with `sh -e` semantics ([`super::runner`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub lists: Vec<Vec<Unit>>,
    /// The file ends with `exit 0`: the hook's status is 0 unless an earlier
    /// line already ended it.
    pub exit_ok: bool,
    /// lefthook and pre-commit run every unit even after a failure and fail
    /// at the end: a unit that exits 1 does not stop the hook. Any other
    /// code (a crash, pre-commit's 3) ends it at once with that code.
    pub keep_going: bool,
    /// Checked just before the first unit starts.
    pub precondition: Option<Precondition>,
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
