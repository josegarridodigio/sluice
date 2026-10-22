---
title: "ADR-0005: The MCP verify tool blocks (with progress) instead of returning a handle"
tags: [sluice, adr, mcp]
---

# ADR-0005: The MCP `verify` tool blocks, with progress notifications

Status: accepted (2026-10-22, Phase-0 answer Q13).

## Context

Agents must be able to wait for a gate that may queue and run for 20 minutes
without hitting a shell timeout. Two proven shapes exist:

- **Blocking call**: the tool returns when the gate finishes. Simple for the
  model (one turn), but depends on the client's tool-call timeout.
- **Bounded wait + handle** (block/agent-task-queue): the tool returns within
  30 s with a job id; the model polls a `task_status` tool. Robust against
  any client timeout, but the model has to drive a polling loop and often
  re-runs commands when confused.

Facts gathered in [[sluice/research/claude-code-facts.md]]:

- Claude Code applies no practical wall-clock limit to MCP calls (default
  about 28 h) and a 30-minute idle window for stdio servers that **progress
  notifications reset**.
- After 2 minutes an interactive session moves the call to a background
  task and delivers the result later; the call is not cancelled.
- rmcp imposes no server-side handler timeout, exposes a
  `CancellationToken` per request and `notify_progress`
  ([[sluice/research/references/rmcp.md]]).
- MCP Tasks support in Claude Code is undocumented.

## Decision

`verify` is a blocking tool in v1:

1. It accepts a `CancellationToken`; cancellation (client gave up, session
   exited) unsubscribes the request from the job. The job itself follows
   the daemon's last-subscriber policy (cancel by default).
2. It sends `notifications/progress` every 15 s (configurable) while queued
   or running, with queue position, elapsed time and the job id, when the
   client supplied a `progressToken`. If no token was supplied it still
   blocks; the 30-minute idle window is then the effective limit and the
   tool description says so.
3. The result is a text block (one line per gate, log tail on failure) plus
   `structured_content` with per-gate status, duration, job id and log path.
4. A companion read-only `status` tool shows queued and running jobs, so a
   model whose call was backgrounded can still look.

Tasks (SEP-2663) are deferred. The gate runner future is kept independent
of rmcp types so it can later be handed to `TaskManager::spawn` if Claude
Code declares the extension.

## Consequences

- Progress notifications go out only when the client sent a
  `progressToken`; whether Claude Code does is undocumented
  ([[sluice/research/claude-code-facts.md]]), so the 30-minute idle window
  is the practical limit until the manual test confirms it.

- Correctness depends on Claude Code's documented idle-window behaviour;
  the manual end-to-end test in Phase 3 must include a gate longer than
  2 minutes (backgrounding) and, with a lowered
  `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`, confirm that progress keeps the call
  alive.
- The MCP process holds one task per in-flight `verify`; cheap.
- If the facts change (a future Claude Code version enforces a short
  per-call limit), the fallback is the handle pattern, which only adds two
  tools (`verify_start`, `verify_status`) on top of the same daemon protocol.
