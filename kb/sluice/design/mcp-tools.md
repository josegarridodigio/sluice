---
title: MCP tools
tags: [sluice, design, mcp, phase-3]
---

# MCP tools (`sluice mcp`, Phase 3)

Status: implemented in Phase 3 (`sluice-gate-mcp`), see [[sluice/changes/phase-3-mcp.md]]. Decisions: [[sluice/design/adr/0005-mcp-verify-blocking-or-handle.md]].
Facts relied on: [[sluice/research/claude-code-facts.md]]. Library notes:
[[sluice/research/references/rmcp.md]]. Related:
[[sluice/design/protocol.md]], [[sluice/design/architecture.md]] section 7.

## Process model

- `sluice mcp` is an MCP server over stdio built with `rmcp` (features
  `server`, `macros`, `transport-io`, `schemars`). Claude Code spawns it per
  session from its MCP config:

  ```json
  {"mcpServers": {"sluice": {"command": "sluice", "args": ["mcp"]}}}
  ```

- It is a thin client: `verify` performs the same hit check as `sluice run`
  (`sluice_gate_core::lookup`, shared with the CLI, no daemon) and forwards
  misses to the daemon, autostarting it when needed (`ensure_daemon` from
  the proto client on a blocking thread). The miss itself is spoken over a
  tokio Unix socket by the server's own small async client (`daemon.rs`),
  because the blocking client cannot be interrupted from another task;
  `status`, `logs` and the hit event use the blocking client. It never
  runs a gate in-process (no fallback: an unavailable daemon is an MCP
  error, because the agent's shell is the wrong place to run suites).
- Logging only to stderr (`tracing_subscriber` with `with_ansi(false)`),
  never to stdout. `SLUICE_LOG` sets the filter for the MCP process.
- The server declares `instructions` for the model: use `verify` before
  committing; do not run test or lint commands through the shell; on
  `FAIL`, read the tail and fix; call `verify` again rather than re-running
  commands yourself.

## Tools

All tools take `working_directory` (absolute path; the agent's cwd is not
known to the server) and resolve the repository from it. Results carry a
human-readable text block **and** `structured_content` (JSON) with an
`output_schema` derived from the Rust types (`Json<T>` where the text
summary is not needed, hand-built `CallToolResult` for `verify`).

### `verify`

Parameters:

| Name | Type | Required | Meaning |
|---|---|---|---|
| `working_directory` | string | yes | Absolute path inside the repository (worktree). |
| `gates` | string[] | no | Gate names. Default: gates whose `hooks` contain `pre-commit`; if none is tagged, every gate. |
| `timeout_seconds` | integer | no | Execution time limit per gate, overrides the gate's `timeout`. Queue wait does not count. |
| `extra_args` | string[] | no | Appended to every selected gate's command. |

Behaviour: gates run sequentially in the given order (a `depends_on` is
not honoured in v1) and **all selected gates run even after one fails**.
For each gate: hit → `PASS (cached, passed <age> ago)`; miss → daemon
`run` (coalescing applies); the tool blocks until every gate finished.
Progress notifications every `[mcp] progress_interval` (default 15 s)
carrying the gate, queue position or elapsed time and the job id, when
the client sent a `progressToken`. Cancellation (`CancellationToken`,
which rmcp fires on `notifications/cancelled` and on transport EOF) sends
`cancel` to the daemon and closes the connection, so the daemon applies
`last_subscriber_gone`; the remaining gates are skipped and the result is
`is_error` with `cancelled: true`. The caller kind is always `agent`
(`Caller::Agent`), independent of environment detection. `SLUICE_DISABLE`
turns the cache off for the call but the gate still runs through the
daemon.

Text result (one line per gate, then the tail of the first failing log):

```
test  PASS 32.1s (job 1761…)
lint  PASS (cached, passed 2h ago)
e2e   FAIL exit=1 12.3s (job 1761…) log=/home/u/.local/state/sluice/logs/1761….log
--- last 40 lines of /home/u/.local/state/sluice/logs/1761….log ---
…
```

`structured_content` (`VerifyReport`, with an `output_schema`):

```json
{"repository":"/abs","results":[
  {"gate":"test","status":"pass","cached":false,"duration_ms":32100,"job_id":"…","log_path":"/…","exit_code":0,"coalesced":false},
  {"gate":"lint","status":"pass","cached":true,"job_id":"<job that wrote the marker>","log_path":null,"exit_code":0,"coalesced":false},
  {"gate":"e2e","status":"fail","cached":false,"exit_code":1,"duration_ms":12300,"job_id":"…","log_path":"/…","coalesced":false,"tail":["…"]}],
 "all_passed":false}
```

`status` is one of `pass`, `fail`, `timeout`, `killed`, `error`,
`cancelled`; a cached pass has no `duration_ms`; `not_cached_reason`,
`message` and `tail` (failing gates only) appear when relevant; the report
carries `cancelled: true` after a cancellation.

A failing gate is a normal result with `is_error = true` (the model must
read it); only protocol-level problems (bad or relative
`working_directory`, no `.sluice.toml`, invalid user config, unreachable
daemon) are MCP errors (`invalid_params` / `internal_error`).

### `status`

No parameters beyond `working_directory` (optional). Returns the daemon
`StatusReport` (same schema as `sluice status --json`, or `"daemon": null`
when none runs; no `output_schema`, the report type is not schema-derived)
plus, when a directory is given, `cache: {repository, tree, tree_unknown,
gates: [{gate, state: hit|miss|uncached|error, …}]}` for its current tree
(as `sluice cache status`), and the text marks that repository's jobs with
`*`. Text mode mirrors `sluice status`.

### `gate_list`

`working_directory` → gates of `.sluice.toml`: name, command, weight,
timeout, hooks, cache flag. Text: one line per gate.

### `logs`

`job_id` (optional; default newest) and `tail` (default 100, max 2000) →
`LogsReport {job_id, path, lines, omitted_bytes}`. Output capped at 64 KiB
with an omission marker; the full file stays on disk. Never starts the
daemon; falls back to the log directory when the daemon is absent. An
unknown job is an `is_error` result; a job id with unsafe characters is
`invalid_params`.

## Agent policy integration (G7)

- Hooks keep using `sluice run --hook …`; the fail-fast message's last
  line is now `sluice: call the 'verify' MCP tool (or run 'sluice run
  <gate>') first, then retry the git command.`
- `docs/agents.md` ships: the MCP config snippet, a `CLAUDE.md` section
  ("Before committing, call the `verify` tool. Do not run test, lint or
  type-check commands through Bash."), the permission caveat (allow-rules
  such as `Bash(npm test:*)` and `bypassPermissions` let agents bypass the
  queue; a deny rule for the raw command is the enforcement option), and
  how to add other agents' environment markers.

## Acceptance (Phase 3)

- Automated: an rmcp client test spawns `sluice mcp`, calls `gate_list`,
  `verify` (miss then hit), `status` and `logs` against a throwaway
  repository and a daemon on a temporary socket; a cancelled `verify`
  leaves no gate process.
- Manual (documented with output): two Claude Code agents in two
  worktrees call `verify`; one runs, the other coalesces; later commits
  hit; a miss in an agent hook fails fast; a gate longer than 2 minutes
  survives auto-backgrounding; with `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`
  lowered to 60 s, progress notifications keep a 3-minute gate alive.
