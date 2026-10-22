---
title: "Change: Phase 6, merge-queue MCP tools"
tags: [sluice, change, phase-6, merge-queue, mcp]
---

# Change: Phase 6, merge-queue MCP tools (2026-10-22)

The MCP server gains `queue_enqueue`, `queue_status` and `queue_approve`, thin
clients of the `queue-*` messages added in
[[sluice/changes/phase-6-daemon-extension-point.md]]. Spec:
[[sluice/design/merge-queue.md]] sections 9 and 12; behaviour documented in
[[sluice/design/mcp-tools.md]] ("Merge-queue tools").

## What changed

| Where | What |
|---|---|
| `sluice-gate-mcp` `src/queue.rs` (new) | `enqueue_facts` (discover, dirty check, branch and SHA with read-only `git::run`), `request` (autostart, blocking `Client::request`, `error{code}` becomes `code: message`), `fetch_status`, `send_approval` (status, train hash, `queue-approve via mcp`), `allowlisted_env`, `ENV_ALLOWLIST`, `NOT_ALLOWED_TEXT`, text renderers `enqueue_text`, `render_status`, `approve_text`, schema mirrors `JobSchema`, `GateSchema`, `TrainSchema`, `StatusSchema`, `ApproveReport`. |
| `sluice-gate-mcp` `src/lib.rs` | Tools `queue_enqueue`, `queue_status`, `queue_approve` (+ `Server::approve`, the 1 s polling loop up to `APPROVE_WAIT` = 10 min with progress notifications), params `QueueEnqueueParams`, `QueueStatusParams`, `QueueApproveParams`, helper `tool_error`; `INSTRUCTIONS` gains the queue guidance. |
| `sluice-gate` `tests/mcp.rs` | Six integration tests (below). |
| docs | `kb/sluice/design/mcp-tools.md`, `docs/agents.md` (tool table, CLAUDE.md lines, deny rule `Bash(sluice queue approve:*)`). |

## Decisions taken here

- `queue_approve` checks `UserConfig.queue.allow_agent_approval` first, with the
  user config read on every call, and returns `not-allowed` before any daemon
  contact (no autostart); an unreadable config refuses too. The daemon repeats
  the check for `via: mcp`.
- Daemon and local refusals are tool error results (`is_error`) starting with
  the code, not MCP errors, so the model reads them. Local codes:
  `dirty-worktree`, `detached-head`, `unknown-branch`, `not-a-repository`,
  `git`, `unavailable`.
- The dirty check applies to the worktree even when `branch` names another
  branch (simple and conservative; the CLI should do the same).
- Output schemas use mirror types because `sluice-gate-proto` has no
  `schemars` (adding it would touch a `Cargo.toml`); the structured content is
  the protocol view as serialised, and a unit test deserialises real views
  into the mirrors.
- The instructions say `retry` is CLI-only for now: after a conflict the agent
  rebases and enqueues again.
- Conflict paths are read from `detail.paths` or a bare array in the job's
  `detail`; the queue crate must write one of those shapes.

## Verification

- Unit (`cargo test -p sluice-gate-mcp`): tool list with descriptions and
  output schemas, `skipped_fields_are_never_required` extended with the new
  output types and optional view fields, instructions content, enqueue and
  status text, env allowlist, mirrors accept the protocol views.
- Integration (`cargo test -p sluice-gate --test mcp queue`), against the
  built binary and a daemon without the queue extension: the three tools are
  listed with descriptions and instructions; `queue_approve` refused by default
  with the exact `not-allowed` text and no daemon socket created; with
  `[queue] allow_agent_approval = true` it reaches the daemon and returns
  `not-supported`; `queue_enqueue` on a dirty worktree is refused with no
  daemon started; detached HEAD and unknown branch refused; `queue_enqueue` and
  `queue_status` on a clean repository return `not-supported`.
- Not covered here (needs the queue extension): the approval polling loop, the
  rendered text of real trains against a real daemon.
- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test -p sluice-gate-mcp`, `cargo test -p sluice-gate --test mcp`: see
  the task report.
