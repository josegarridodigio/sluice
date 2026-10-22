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
//! - [`extension`]: [`QueueExtension`], the `Extension` implementation.
//!
//! Design: `kb/sluice/design/merge-queue.md`.

pub mod extension;
pub mod git;
pub mod model;
pub mod push;
pub mod reconcile;
pub mod runner;
pub mod store;
pub mod target;

pub use extension::{QueueExtension, QueueOptions};
