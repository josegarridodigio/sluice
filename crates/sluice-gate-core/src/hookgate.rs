//! Gates derived from a repository's git hooks (ADR-0021, decision 7).
//!
//! A repository without `.sluice.toml` whose hooks Sluice can plan has no
//! gates of its own: the check units of its hooks are the gates. The planning
//! lives in the `sluice` binary (it knows the hook managers); everything that
//! only needs the result (the MCP server, `sluice run`, `sluice gate list`)
//! receives a [`HookGateSource`] and gets a [`HookGates`].

use std::sync::Arc;

use crate::config::{EnvMap, Gate};
use crate::git::RepoPaths;

/// A hook unit that `verify` and `sluice run` can run as a gate. Its [`Gate`]
/// is the very one the hook runner builds for the unit, so both derive the
/// same cache key.
#[derive(Debug, Clone)]
pub struct HookGate {
    /// The gate name, `<manager>:<hook>#<index>`: also the unit's name in the
    /// hook runner, and part of the cache key.
    pub name: String,
    /// The manager that owns the hook (`husky`, `lefthook`, `pre-commit`,
    /// `plain`).
    pub manager: String,
    /// The hook the unit belongs to.
    pub hook: String,
    /// The gate definition.
    pub gate: Gate,
}

/// A hook or unit that is not a gate, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookExclusion {
    /// The hook.
    pub hook: String,
    /// The unit's name; `None` when the whole hook is excluded.
    pub unit: Option<String>,
    /// The unit's command, when it is a unit.
    pub command: Option<String>,
    /// Why.
    pub reason: String,
}

/// What a repository's hooks contribute.
#[derive(Debug, Clone, Default)]
pub struct HookGates {
    /// Units that can be verified now.
    pub gates: Vec<HookGate>,
    /// Everything else, with the reason.
    pub excluded: Vec<HookExclusion>,
}

/// Derives the hook gates of a repository; supplied by the `sluice` binary.
/// The environment is the caller's, as for a hook run.
pub type HookGateSource = Arc<dyn Fn(&RepoPaths, &EnvMap) -> HookGates + Send + Sync>;
