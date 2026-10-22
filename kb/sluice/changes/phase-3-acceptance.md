---
title: "Phase 3 acceptance evidence"
tags: [sluice, acceptance, phase-3]
---

# Phase 3 acceptance evidence (2026-10-22)

Automated: `crates/sluice-gate/tests/mcp.rs` (rmcp child-process client
against the built `sluice mcp`): gate_list; verify miss then hit; the fail
gate (exit 7, tail); status with cache state; logs; bad requests; progress
notifications at a 1 s interval; cancellation kills the gate; extra_args
miss then hit; daemon unavailable gives per-gate errors; two parallel
verify calls coalesce; a gate after a failure still runs; timeout_seconds;
logs limits and unknown job; plus `schema_tests::skipped_fields_are_never_required`.

End to end with real Claude Code sessions: `scripts/phase3-e2e.sh`
(Claude Code 2.1.308 via `claude -p --model sonnet`, Sluice 0.1.0 release
build, machine: 12th Gen Intel i7-1280P, 20 logical CPUs, Linux 7.0). The
script runs in a private HOME/XDG tree with a scratch clone of this
repository whose `test` gate is `sleep 90`, one extra worktree with the
same unstaged change, and `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=60000`.

## Run with progress notifications every 15 s (`scripts/phase3-e2e.sh 90 15s`)

| Step | Output (trimmed) | Result |
|---|---|---|
| Two sessions call `verify` 5 s apart | `sluice status` at 21 s: `running: 1 … weight 2 21.6s 2 subscriber(s)`, `totals: … 1 coalesced` | one job, two subscribers |
| Session A (main worktree) | 97 s; result `{"all_passed":true,…,"results":[{"cached":false,"coalesced":false,"duration_ms":90003,"exit_code":0,"gate":"test","job_id":"1792685926228-…","status":"pass"}]}` | ran the gate |
| Session B (worktree) | 91 s; same job id with `"coalesced":true` | coalesced |
| Gate executions (counter file) | 1 | one run for two requests |
| Agent-invoked `pre-commit` on the verified tree (`CLAUDECODE=1 git commit`) | `sluice: test PASS (cached, passed 3s ago)`, `commit ok` | cache hit in milliseconds |
| Agent-invoked `pre-commit` on a new tree | the three fail-fast lines (`… call the 'verify' MCP tool (or run 'sluice run test') first …`), exit 3 | fail-fast |
| `sluice status --json` totals | `runs 1, passes 1, hits 1, coalesced 1, run_ms_total 90018, time_saved_ms 180021` | |

A 90 s gate completed under a 60 s idle window: Claude Code sent a
progress token and reset its idle timer on every notification.

## Run with progress effectively disabled (`scripts/phase3-e2e.sh 90 10m`)

| Step | Output (trimmed) | Result |
|---|---|---|
| Session B | aborted after 67 s: `MCP server "sluice" tool "verify" sent no response or progress for 60s; aborting. If this server is configured in your MCP settings, set a per-server "timeout" (ms) …` | idle window enforced |
| Session A | returned after 35 s with `Connection closed` (single occurrence, cause not identified; the daemon log shows its subscription alive until B's abort) | |
| Daemon log | `last subscriber gone, cancelling job=…` then `job done … outcome="killed"` 67 s after start | `last_subscriber_gone = cancel` applied |
| Agent-invoked `pre-commit` afterwards | `'test' has not passed on this tree (last run failed 1s ago)`, exit 3 | the killed run left a miss record |

Conclusion: progress notifications are what keep a long `verify` alive in
Claude Code; the default `[mcp] progress_interval = "15s"` is well under
the default 30-minute idle window and under any reasonable override.

## What the script does not cover

Interactive auto-backgrounding after 2 minutes (documented by Claude Code
for interactive sessions only) was not exercised; the non-interactive
sessions simply blocked. The first run of the script lost its outputs to
an unrelated `rm -rf /tmp/tmp.*` during the session and was repeated; the
numbers above are from the repeat.

Related: [[sluice/changes/phase-3-mcp.md]],
[[sluice/research/claude-code-facts.md]],
[[sluice/design/adr/0005-mcp-verify-blocking-or-handle.md]].
