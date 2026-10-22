---
title: "Change: Phase 6, protocol additions and daemon extension point for the merge queue"
tags: [sluice, change, phase-6, merge-queue]
---

# Change: Phase 6, protocol additions and daemon extension point (2026-10-22)

First building block of the local merge queue
([[sluice/design/merge-queue.md]] sections 8 and 10,
[[sluice/design/adr/0009-merge-queue.md]]): the wire types and the daemon
surface that `sluice-gate-queue` builds on. No queue logic lives in the
daemon, and the daemon crate does not depend on the queue crate.

## What changed

| Where | What |
|---|---|
| `sluice-gate-proto` `lib.rs` | `ClientMessage::{QueueEnqueue{id, EnqueueRequest (flattened)}, QueueStatus, QueueApprove, QueueRetry, QueueCancel, QueueReconcile}`; `ServerMessage::{QueueJob, QueueStatus (flattened QueueStatusReport), QueueApproved, QueueReconcile}`; types `EnqueueRequest`, `ApproveVia`, `QueueStatusReport`, `QueueJobView`, `QueueTrainView`, `TrainGate`, `QueueJobState`, `QueueTrainState`, `ReconcileDecision`, `ReconcileResult`, `QueueSummary`; consts `QUEUE_STATUS_SCHEMA_VERSION = 1`, `FEATURE_QUEUE = "queue"`; `ErrorCode::{ConflictState, NotAllowed, StalePlan, DuplicateBranch, NotSupported}`; `Welcome.features` (default empty, omitted when empty); `StatusReport.queue: Option<QueueSummary>` (omitted when `None`). `PROTOCOL_VERSION` stays 1. |
| `sluice-gate-proto` `client.rs` | `Client::features()`, `Client::supports(feature)`, `Client::request(&ClientMessage) -> ServerMessage` (single reply returned as is, `error` included). `check_welcome` now returns the features too. |
| `sluice-gate-daemon` `server.rs` + new `server/extension.rs` | `Extension` trait, `BoxFuture`, `ExtensionCtx`, `DaemonHandle`, `RunSubscription`, `LeaseFence` (+ `check`), `StoreError`; `DaemonConfig.user: UserConfig` and `DaemonConfig.extensions`; `UserConfig` re-exported from `server`. Lifecycle: `on_start` after orphan reconciliation and before `bind`; one `run` task per active extension; `shutdown` watch set when the accept loop ends; tasks awaited up to 10 s (`EXTENSION_GRACE`) after `drain`, then aborted, before the lease is released. `handshake` returns the client name and sends `features`; `queue-*` requests go to `dispatch_extension` (`not-supported` / `internal` fallback); `status` merges `status_summary()`. Heartbeat clears `lease_held` when the lease is lost. The sanitised daemon env is computed once at start (`git::child_env`, fallback: drop `GIT_*` and `CLAUDE*`). |
| `sluice-gate-daemon` `store.rs` | `pub(crate) Store::connection()` for `store_tx`. |
| `sluice-gate-daemon` `Cargo.toml` | `anyhow` (for `Extension::on_start`). |
| `sluice-gate` `daemon_cmd.rs` | passes `user` and an empty `extensions` (the queue is wired in later). |
| `executor` | `process_matches`, `proc_start_ticks`, `terminate_group` were already public; unchanged. |
| docs | [[sluice/design/protocol.md]] (handshake features, `status.queue`, section "Merge queue", error codes, versioning note), [[sluice/design/architecture.md]] section 5.5 and section 6. |

## Decisions taken here

- Queue requests and replies carry an `id` like every other request
  (`#[serde(default)]`, so a message without it parses with 0); the design
  listed only the payload fields.
- Boxed futures instead of `async_trait`: no new proc-macro dependency and
  the trait stays object-safe.
- `on_start` failure disables only that extension (gates keep working);
  its requests get `error{internal}` naming the cause, not `not-supported`
  (which would send the user into a stop/start loop).
- Job/train states and reconcile decisions are snake_case (as in the
  design and the `queue_*` tables) with an `Unknown` catch-all; the wire
  timestamps of the views are RFC 3339 like the rest of the protocol.
- `submit_run` events carry `id: 0`; a refused registration yields one
  `error{internal}`.

## Verification

Proto unit tests (round trip of every new message, spellings, a `welcome`
without `features`, optional `status.queue`), `tests/status_schema.rs`
(fixture unchanged; `queue` is purely additive), client tests (features
from `welcome`), and `crates/sluice-gate-daemon/tests/extension.rs` with a
fake extension: features in `welcome`, routing and reply, `not-supported`
without extensions and for unanswered messages, a failed `on_start`,
`status.queue`, `run` seeing `shutdown`, `submit_run` of `true`
(`queued`, `started`, `done{pass}`), dropping a subscription kills a
running `sleep 30` under the `cancel` policy, `store_tx` commit/rollback and
`LeaseFence::check`. `cargo fmt --all --check`, `cargo clippy --workspace
--all-targets -- -D warnings` and `cargo test --workspace` pass.
