---
title: "Change: Phase 6, the `sluice queue` subcommands"
tags: [sluice, change, phase-6, merge-queue, cli]
---

# Change: Phase 6, `sluice queue` CLI (2026-10-22)

Client side of the local merge queue ([[sluice/design/merge-queue.md]] sections 8
and 9) on top of the protocol additions of
[[sluice/changes/phase-6-daemon-extension-point.md]]. No queue logic lives here: each
subcommand is a thin client of one `queue-*` message.

## What changed

| Where | What |
|---|---|
| `crates/sluice-gate/src/queue.rs` (new) | `enqueue`, `status`, `approve`, `retry`, `cancel`, `reconcile`; `Repo` (git facts through `sluice_gate_core::git`: branch, head, dirty check on tracked files); `send` (autostart via `ensure_daemon`, `Client::supports(FEATURE_QUEUE)`, error code to exit code); `filter_env` + `ENV_ALLOWLIST`; `render_status` / `render_train` / `render_plan` (human formatter); `wait_for_landing` (1 s poll of `queue-status`); `Failure` / `finish` (exit codes 1, 2, 4). |
| `crates/sluice-gate/src/main.rs` | `Command::Queue` and `QueueCommand` with the help texts; dispatch to `queue::*`. |
| `crates/sluice-gate-proto/src/client.rs` | `Client::request_timeout(msg, Option<Duration>)`; `request` now delegates to it with the 10 s reply timeout. Needed because `queue-approve`, `queue-enqueue` and `queue-reconcile` may fetch or push and outlive the 10 s read timeout. |
| `crates/sluice-gate/tests/queue_cli.rs` (new) | see Verification. |
| `docs/merge-queue.md`, `README.md` | user guide and a short "Merge queue" section. |

## Behaviour

- Exit codes: 0 ok; 1 error (also `not-supported`, printed as the daemon's message);
  2 usage (also `approve` without a TTY and without `--yes`, checked before contacting the
  daemon); 4 refused by state (`conflict-state`, `not-allowed`, `stale-plan`,
  `duplicate-branch`). Messages start with `sluice: `.
- `enqueue` checks the detached HEAD and the dirty tracked files before any daemon contact. The
  request carries `caller` from `agent::detect`, `session` from `CLAUDE_CODE_SESSION_ID` and
  the allowlisted env (`SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `GIT_SSH_COMMAND`, `GIT_ASKPASS`,
  `SSH_ASKPASS`, `HOME`, `PATH`, `TERM`; only those present).
- A daemon whose `welcome` lacks the `queue` feature is reported client-side with the same
  message the daemon uses for `not-supported`.
- `approve` fetches `queue-status`, requires the train to be `ready` with a hash, prints the plan,
  asks on a TTY, sends `queue-approve {train_hash, via: cli}`. A reply with the train still
  `approved`/`pushing` prints `approved; pushing...` and polls every second until the train is
  `landed`, `needs_reconcile` or dissolved/gone (a dissolved train with a failed member is
  reported as failed).
- `status` and `reconcile` are scoped to the current repository when run inside one; `approve`
  and `retry` look jobs and trains up globally by id. `retry` requires the job to belong to the
  current repository and re-reads `refs/heads/<branch>`.

## Decisions and assumptions

- The conflict `detail` of a job is not specified by the protocol; the formatter accepts an
  array of paths or `{"paths": [...]}`, and for failures `{"gate", "job_id"}` or a string.
- A train has no `failed` state in the protocol, so "failed" after approval is derived from the
  members' jobs.

## Verification

- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test -p sluice-gate`: all clean; `cargo test -p sluice-gate` passes (19 unit, 12 queue_cli, plus the existing suites).
- Unit tests in `queue.rs`: ready train with the approve hint, conflict member with paths and
  retry hint, `needs_reconcile` hint, queued position, empty queue, env allowlist, queued line.
- `tests/queue_cli.rs`: dirty worktree (staged and unstaged; untracked ignored) and detached
  HEAD exit 1 without starting the daemon; `enqueue`/`status`/`reconcile`/`cancel` against the
  real daemon print the `not-supported` message and exit 1; `approve` without TTY exits 2;
  `--help` of every subcommand; a scripted fake daemon covers the request payload, the exit codes
  of every error code, `status --json`, `approve` (hash, `via`, polling) , `reconcile`, `retry`.
