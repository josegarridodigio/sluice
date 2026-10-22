---
title: "Change: Phase 6, the merge queue crate (sluice-gate-queue)"
tags: [sluice, change, phase-6, merge-queue]
---

# Change: Phase 6, the merge queue crate (2026-10-22)

Core of the local merge queue ([[sluice/design/merge-queue.md]] sections 2
to 7, 10, 11, 14), built on the daemon extension point
([[sluice/changes/phase-6-daemon-extension-point.md]]). The CLI
(`sluice queue …`) and the MCP tools are separate changes.

## What changed

| Where | What |
|---|---|
| new crate `crates/sluice-gate-queue` | `model` (`JOB_TRANSITIONS`, `TRAIN_TRANSITIONS`, `job_transition_allowed`, `train_transition_allowed`, state spellings, `TrainHashInput`, `train_hash`); `store` (`init` with `queue_schema` v1, `JobRow`, `TrainRow`, `MarkerRow`, `TrainSettings`, `Writer` (fenced by `LeaseFence::check`; `insert_job`, `job_to`, `train_to`, `dissolve`, `insert_train`, `insert_marker`, `set_marker_pid`, `delete_marker`, `request_cancel`, `adopt_trains`), `Refusal` + `refuse`/`refusal`, reads `job`, `jobs`, `jobs_in_state`, `train`, `trains`, `train_jobs`, `marker`, `position`, `events`); `target` (`resolve`, `Target`, `TargetError`, `defaults_at`, `endpoint_hash`, `strip_userinfo`); `git` (`Git` runner, `ENV_ALLOWLIST`, `allowlisted`, `worktree_git_dir`); `push` (`push_args`, `spawn`, `PushProcess::wait`, `PushEnd`, `classify`, `PushVerdict`, `land_local`); `reconcile` (`ReconcileInput`, `decide`, `Decision`, `TrainFacts`, `gather`); `runner` (`Core`, `run`, build, gates, push orchestration, `read_config`); `extension` (`QueueExtension`, `QueueOptions`, `on_start` recovery and merge-dir sweep, request handlers, `status_summary`). |
| workspace `Cargo.toml` | member and workspace dependency `sluice-gate-queue`. |
| `sluice-gate-daemon` `server/extension.rs` | `DaemonHandle::store_tx_durable` (one transaction committed with `synchronous = FULL`). |
| `sluice-gate-core` `git.rs` | `git::command` (builds a `git` `Command`, for the push spawned in its own process group) and `git::run_output` (raw output whatever the status). |
| `sluice-gate` `daemon_cmd.rs`, `Cargo.toml` | `extensions: vec![Arc::new(QueueExtension::new(QueueOptions::default()))]`. |
| KB | [[sluice/design/merge-queue.md]] updated to the implementation (sections 2–6, 10, 12, new 15); [[sluice/design/architecture.md]] crate layout, `store_tx_durable` in 5.5, new 5.6. |

## Deviations from the design, and why

- **Durable marker commit**: SQLite refuses `PRAGMA synchronous` inside a
  transaction, so `store_tx` cannot do it; added
  `DaemonHandle::store_tx_durable` instead of fsyncing by hand.
- **Extra columns** (`queue_schema` stays v1, tables are new):
  `queue_jobs.solo` (the split flag of step 7), `queue_jobs.env` (the
  enqueuer's allowlist, for fetches and `ls-remote`),
  `queue_trains.members` (claimed members in order, for the view and the
  hash), `queue_trains.settings` (`push_timeout`, `audit_ref` of the base).
- **Logical refusals** travel as `rusqlite::Error::ToSqlConversionFailure`
  wrapping `store::Refusal`, so `store_tx` rolls back; `store::refusal`
  recovers them and the extension maps them to `not-found`,
  `conflict-state`, `duplicate-branch`, `stale-plan`, `not-allowed`,
  `internal`.
- **Extra transitions** (all recorded in `model.rs` and section 3):
  `merging → failed` (internal error), `approved → failed`
  (`target_checked_out` at landing), `pushing → cancelled` (definitive
  rejection with a pending cancel); `needs_reconcile` jobs, like `pushing`,
  only record `cancel_requested`.
- **Default target**: `[queue] remote`/`target` are read from the enqueued
  head's `.sluice.toml` (only to pick the default target; gates still come
  from the base). The design did not say where the defaults come from
  before a base exists.
- **Own branch** is refused for remote targets too (target branch name ==
  job branch).
- **Endpoint changed at push time** dissolves the train (rebuilt and
  re-approved against the new URL) instead of a bare refusal; reconcile
  still refuses `endpoint_changed`.
- **Hook-declined detection** is a heuristic on Git's output (exit 1, no
  ref line, no `fatal:`, `failed to push some refs`); `LC_ALL=C` is forced
  for every queue git call so the parsing is locale independent.
  Permission denied before any ref line (SSH auth) counts as a definitive
  policy rejection.
- **Merge identity fallback** `Sluice merge queue <sluice@localhost>` when
  git has no committer identity (the daemon may run without one).
- **`enabled = false`**: `welcome.features` still lists `queue` (the trait's
  `features()` is static); requests get `not-supported` naming the setting.
- **Base without `.sluice.toml`**: no gates; invalid config or failed fetch:
  jobs stay `queued`, logged, retried each tick (no new job state).
- **Reconcile row 7** cancels the members with a pending cancel and
  re-queues the others.

## Verification

- Unit tests (32): transition tables, state spellings vs serde, train hash
  determinism and sensitivity (every field, member order, gate key order
  irrelevant), store schema idempotence and fail-closed on a newer
  `queue_schema`, lease fencing, duplicate branch, CAS refusals,
  `dissolve` with a pending cancel, `claim_token` fencing and `adopt_trains`,
  marker round trip, every reconcile row (1–10, pure), porcelain fixtures
  (success, `stale info`, `non-fast-forward`, `[remote rejected]`,
  permission denied, pre-push hook declined, network failure, truncated,
  signal, stale audit ref), push arguments, env allowlist, userinfo
  stripping.
- `tests/target.rs` (4): remote-tracking, local, bare name (remote first),
  `refs/…` spellings, default via `origin/HEAD` and `[queue] target`;
  refusals tag, SHA (full and short), `HEAD`, own branch, checked-out branch
  (linked worktree and main worktree), unknown; endpoint hash follows
  `remote set-url`.
- `tests/queue.rs` (11, real daemon via `server::run` + local bare remote,
  ~4.5 s total): conflict + ready + wrong hash `stale-plan` + MCP
  `not-allowed` + approval → `landed`, remote target == S, audit ref
  present, pin ref and merge worktree gone; MCP approval when allowed,
  fast-forward S, `config_changed`, `status.queue`; marked tree → `cached`
  with no run (counter file); base moved → dissolved, rebuilt, old approval
  `stale-plan`; declining `pre-push` → jobs `failed` `pre_push_hook` with
  the hook output; hanging `pre-push` (FIFO) + `push_timeout = "2s"` →
  `needs_reconcile`, reconcile → `ready`, approval voided, pin ref gone;
  marker without a push across a daemon restart → `needs_reconcile` →
  `ready` → re-approved → landed, and the same with a pending cancel →
  `cancelled`; local target lands by `update-ref` with the audit ref and
  refuses a checked-out branch and `HEAD`; a failing member splits the
  train (A lands alone, B `failed` `gate_failed` with log path);
  `enabled = false` → `not-supported`.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace` pass.

## Open points

- `[queue] allow_agent_approval` in `.sluice.toml` is a config **error**
  today (`QueueConfig` has `deny_unknown_fields`), while section 11 says it
  is ignored with a warning; left for the config owner.
- Not covered by tests: `endpoint_changed`, lease loss during a push,
  reconcile rows 5/6 against a real remote, SIGKILL of the daemon while a
  push blocks (acceptance test 2 uses the marker-without-push variant).
