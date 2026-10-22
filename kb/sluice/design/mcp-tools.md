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
- The server declares `instructions` for the model: use `verify` with the
  absolute working directory before committing; do not run test, lint or
  type-check commands through the shell; on `FAIL`, read the tail and fix;
  call `verify` again rather than re-running commands yourself; a passed
  tree is answered from the cache in milliseconds; `status`, `logs` and
  `gate_list` exist.

## Tools

All tools take `working_directory` (absolute path; the agent's cwd is not
known to the server) and resolve the repository from it. Results carry a
human-readable text block **and** `structured_content` (JSON); every tool
builds its `CallToolResult` by hand, with an `output_schema` derived from
the Rust result types for `verify`, `gate_list` and `logs`. The server
identifies itself to the daemon as `sluice-gate-mcp <version>`, forwards
`CLAUDE_CODE_SESSION_ID` as the job's `session`, never sets `hook`, and
re-reads the user config on every `verify` (so `[mcp] progress_interval`,
floored at 1 s, applies without a restart). `sluice mcp` exits 0 when the
client disconnects and 1 when the transport fails.

### `verify`

Parameters:

| Name | Type | Required | Meaning |
|---|---|---|---|
| `working_directory` | string | yes | Absolute path inside the repository (worktree). |
| `gates` | string[] | no | Gate names, run in this order (a repeated name runs twice, the second normally a hit). Default (or `[]`): gates whose `hooks` contain `pre-commit`, alphabetically; if none is tagged, every gate. |
| `timeout_seconds` | integer | no | Execution time limit per gate, overrides the gate's `timeout`; `0` means no timeout. Queue wait does not count. |
| `extra_args` | string[] | no | Appended to every selected gate's command and folded into the cache key (see [[sluice/design/cache-key.md]]), so a pass with arguments never answers a call without them. |

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
turns the cache off for the call (and skips the user config: default TTL,
tail and progress interval) but the gate still runs through the daemon;
`CI`, `SLUICE_CACHE=0`, `cache = false`, an unknown tree or a failed key
make the gate run uncached with `not_cached_reason` on a pass. A hit
sends the best-effort `hit` event when the daemon socket exists. Progress
messages: `[i/n] <gate>: submitting to the daemon`, `<gate>: queued,
position <p> (job <id>)`, `<gate>: running for <elapsed> (job <id>)`, with
`progress` = seconds since the call began; the first one is sent one
interval after the daemon phase begins. Cancellation before `queued` only
drops the connection; a cancel during the hit check or `ensure_daemon`
abandons that blocking step (a daemon start in progress still completes);
the result then starts with `sluice: the verification was cancelled; the
daemon subscription was dropped.` and the pending gate shows
`CANCELLED (job <id>)`.

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
`cancelled`; `job_id`, `log_path` and `exit_code` are always present
(`null` when absent, so a cached pass has `exit_code: null`); a cached
pass has no `duration_ms` (a daemon-side `cached` answer carries the
`done` job's id and prints `PASS (cached)` without the age);
`not_cached_reason`, `message` and `tail` (failing, timed-out and killed
gates) appear when relevant; the text tail is `--- last <n> lines of
<path> ---` with `n` the real count (`defaults.tail_lines` /
`cache.tail_lines`); the report carries `cancelled: true` after a
cancellation. A Sluice error on one gate (daemon unavailable, refused
before `queued`, connection lost) is recorded as that gate's `error`
result and the remaining gates still run; the call returns `is_error`.

A failing gate is a normal result with `is_error = true` (the model must
read it); only protocol-level problems (bad or relative
`working_directory`, no `.sluice.toml`, invalid user config, unreachable
daemon) are MCP errors (`invalid_params` / `internal_error`).

### `status`

No parameters beyond `working_directory` (optional). Returns the daemon
`StatusReport` (same schema as `sluice status --json`, or `"daemon": null`
when none runs; no `output_schema`, the report type is not schema-derived)
plus, when a directory is given, `cache: {repository, tree, tree_unknown,
gates: [{gate, state: hit|miss|uncached|error, passed_at?, job_id?,
last_run?, message?}]}` for its current tree (computed with caching
enabled whatever the environment; `gates` is empty and `tree_unknown`
set when the tree is unknown), and the text marks that repository's jobs
with `*`. A non-repository directory is `invalid_params`. Never starts
the daemon. Text mode mirrors `sluice status`.

### `gate_list`

`working_directory` → gates of `.sluice.toml` sorted by name: `name`,
`cmd` (trimmed), `weight`, `timeout_seconds` (0 = none), `hooks`, `cache`.
Text: one line per gate with the normalised command and a humantime
timeout; `no gates defined in .sluice.toml` when empty.

### `logs`

`job_id` (optional; default the daemon's newest job, or the greatest log
file name when no daemon answers) and `tail` (default 100; `0` becomes 1,
values over 2000 become 2000) → `LogsReport {job_id, path, lines,
omitted_bytes}`. Lines are the raw file (framing lines included); output
capped at 64 KiB, `omitted_bytes` counts what the cap dropped; the full
file stays on disk. Never starts the daemon; falls back to the log
directory on any daemon failure. An unknown job is an `is_error` text
result; a job id with unsafe characters is `invalid_params`.

## Agent policy integration (G7)

- Hooks keep using `sluice run --hook …`; the fail-fast message's last
  line is now `sluice: call the 'verify' MCP tool (or run 'sluice run
  <gate>') first, then retry the git command.`
- `docs/agents.md` ships: the MCP config snippet, a six-bullet `CLAUDE.md`
  section (call `verify` before committing, never run test, lint or
  type-check commands through the shell, …), the permission caveat (allow-rules
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
