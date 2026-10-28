//! Sluice local merge queue (Phase 6), a daemon extension.
//!
//! Agents enqueue committed branches; the runner (a task inside `sluice
//! daemon`, fenced by the daemon's lease) merges them onto the target in a
//! disposable worktree, verifies the combined tree through the daemon's
//! scheduler and cache, waits for a human approval bound to a train hash,
//! and lands the train with a crash-safe atomic push (or a compare-and-swap
//! ref update for a local target).
//!
//! Modules:
//! - [`model`]: states, transition tables, train hash.
//! - [`store`]: the `queue_*` tables; fenced compare-and-swap writes.
//! - [`target`]: resolution of `--base`, endpoint binding.
//! - [`git`]: git calls (through `sluice_gate_core::git`) and env allowlist.
//! - [`push`]: push arguments, process handling, porcelain classification.
//! - [`reconcile`]: the reconcile decision table and its queries.
//! - [`runner`]: the runner loop, train build, gates, push orchestration.
//! - [`toolenv`]: the environment trains run their tools in (ADR-0020).
//! - `prepare`: `[queue] prepare`, the prepare job and the base probe.
//! - `hookcheck`: a merged config asking the `pre-push` hook for unverified gates.
//! - `trust`: the member's own configuration for `--trust-member-config`.
//! - `vouch`: the answer to the repository's `pre-push` hook during the train's push.
//! - [`pr`]: the reconcile tables of `land = "pull-request"` (ADR-0025).
//! - [`extension`]: [`QueueExtension`], the `Extension` implementation.
//!
//! Design: `kb/sluice/design/merge-queue.md`.

mod awaiting;
pub mod extension;
pub mod git;
mod hookcheck;
pub mod model;
pub mod pr;
mod prepare;
pub mod push;
pub mod reconcile;
pub mod runner;
pub mod store;
pub mod target;
pub mod toolenv;
mod trust;
mod vouch;

pub use extension::{QueueExtension, QueueOptions};
