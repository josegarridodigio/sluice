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
| `fail_fast` | boolean | no | Default `true`: the call returns at the first gate that fails, times out, errors or is killed, and cancels the gates still pending. `false` waits for every gate. |

Behaviour (0.3.0, ADR-0011 M4): the selected gates run **concurrently**
(one daemon connection per gate, submitted together; a `depends_on` is not
honoured in v1). With `fail_fast` (default) the first failing gate ends
the call: every pending gate's subscription is dropped with `cancel`
reason `early-verdict`, so the daemon kills (or drops from the queue) a job
nobody else waits for (`killed_by = early-verdict`), while a job another
client coalesced into keeps running (under `last_subscriber_gone = finish`
it completes and caches). Those gates report `cancelled` with message
`not finished: <gate> failed first` and the text line `CANCELLED (not
finished: <gate> failed first; job <id>)`; the call itself is not
`cancelled`. `fail_fast: false` waits for every gate and reports every
failure. `[mcp] verify_parallel = false` (or a single gate) restores the
pre-0.3.0 sequential loop: gates in the given order, **all of them run
even after one fails**, `fail_fast` ignored. Results are always listed in
the requested order. For each gate: hit → `PASS (cached, passed <age>
ago)`; miss → daemon `run` (coalescing applies). A gate that waited in the
queue gets ` (queued <time>: <reason>)` appended to its line.
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
result (with `fail_fast` it is the early verdict; otherwise the remaining
gates still run); the call returns `is_error`. Optional 0.3.0 fields:
`GateResult.queue_wait_ms` (this call's queue wait), `GateResult.wait_reason`
(dominant cause: `budget`, `psi`, `repo-cap`, `head-reserved`, `order`) and
`VerifyReport.wall_ms` (the whole call).

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
last_run?, message?, scope?: {files, total, hash}}]}` for its current tree
(`scope` only for a path-scoped gate with caching on; computed with caching
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

## Merge-queue tools (Phase 6)

Five tools, thin clients of the `queue-*` messages
([[sluice/design/protocol.md]] "Merge queue",
[[sluice/design/merge-queue.md]] sections 8, 9 and 12; change record
[[sluice/changes/phase-6-mcp.md]]). Code: `crates/sluice-gate-mcp/src/queue.rs`
(helpers, text, schema mirrors) and the five `#[tool]` methods in `lib.rs`.
Every request the tools send carries `via: mcp` (`queue-approve` always did;
`queue-enqueue`, `queue-retry`, `queue-cancel` through their optional
`via`), so `queue_events.actor` records `mcp`.

Common rules:

- `working_directory` is required and absolute (`invalid_params` otherwise,
  like the other tools). The repository comes from
  `sluice_gate_core::git::discover`; every git call goes through
  `sluice_gate_core::git::run` with `read_only = true`. Product code never
  touches the index or the working tree.
- The daemon is autostarted (`ensure_daemon`) and spoken to with the blocking
  client's `Client::request` on a blocking thread; the reply is returned as is,
  so an `error` message keeps its code. The queue tools go through the shared
  proto client, whose `hello` says `sluice-gate <version>` (only `verify`,
  `status` and `logs` use the `sluice-gate-mcp <version>` client of
  `daemon.rs`); the MCP server info is `sluice <version>`.
- Failures are **tool error results** (`is_error = true`, no structured
  content) whose text starts with the code and a colon: the daemon's own
  codes `conflict-state`, `not-allowed`, `stale-plan`, `duplicate-branch`,
  `not-supported` (also when `[queue] enabled = false`), `not-found`,
  `bad-request`, `internal`, plus codes minted by the server
  before or around the daemon call: `dirty-worktree`, `detached-head`,
  `unknown-branch`, `not-a-repository`, `git`, `unavailable` (daemon cannot be
  reached). Only a bad or relative `working_directory` or an unsafe
  `train_id`/`job_id` is an MCP error (`invalid_params`).
- The env sent to the daemon is the allowlist `SSH_AUTH_SOCK`,
  `SSH_AGENT_PID`, `GIT_SSH_COMMAND`, `GIT_ASKPASS`, `SSH_ASKPASS`, `HOME`,
  `PATH`, `TERM` (only variables that are present).
- Output schemas are derived from mirror types in `queue.rs` (`JobSchema`,
  `TrainSchema`, `GateSchema`, `StatusSchema`, `ApproveReport`) because the
  proto crate has no `schemars`. The structured content is the protocol view
  itself; fields the views skip when empty are optional in the mirrors, and
  `schema_tests::skipped_fields_are_never_required` covers them. A unit test
  deserialises the real views into the mirrors to keep both in step.

### `queue_enqueue`

Inputs: `working_directory`, `branch?` (default: the worktree's current
branch; a leading `refs/heads/` is stripped), `base?` (as typed:
`<remote>/<branch>` or a local branch; the daemon resolves it).

Before contacting the daemon it refuses: tracked changes in the worktree
(`git status --porcelain --untracked-files=no`, `dirty-worktree`; checked
even when `branch` names another branch), a detached HEAD without `branch`
(`detached-head`), and a branch with no local ref (`unknown-branch`). It
then sends `queue-enqueue {repo: git common dir, worktree: toplevel, branch,
head_sha: refs/heads/<branch>^{commit}, base, caller: agent, session:
$CLAUDE_CODE_SESSION_ID, env, via: mcp}`. A branch equal to the target, a
base that is not a branch and an already active branch are refused by the
daemon (`not-allowed`, `bad-request`, `duplicate-branch`). A job in
`conflict` or `failed` still holds its branch: the way back is
`queue_retry`, not a new `queue_enqueue`.

Text: `queued <branch> (job <id>) for <remote>/<target>, position N` (no
`position` when the job is not queued: `, state <state>`), then one line
telling the agent to stop and tell the human. `structured_content`: the job
view (`QueueJobView`: `job_id`, `branch`, `head_sha`, `remote`, `target`,
`state`, `position?`, `train_id?`, `caller`, `session?`, `reason?`,
`detail?`, `attempts`, timestamps).

### `queue_status`

Inputs: `working_directory`. Sends `queue-status {repo: common dir}`. Text:
one block per train (`train <id>  <remote>/<target>  base <sha7>  <STATE>`),
its members (`<branch>  job <id>  <state>`, queued position, `CONFLICT
<paths>` with `rebase '<branch>' onto <target>, run `verify`, then call
queue_retry with job_id <id>`, failure reasons and, for a `failed` job, `fix
and commit on '<branch>', run `verify`, then call queue_retry with job_id
<id>`), its gates (`test PASS 12.4s`, `lint CACHED`), and a
state line: `ready; a human approves with `sluice queue approve <id>``,
`approved; the daemon is pushing`, `landed <sha7>`, `push outcome unknown; a
human runs `sluice queue reconcile``; jobs outside any train follow under
`jobs outside a train:`. With no trains and no jobs the text is `the merge
queue is empty for this repository`. Conflict
paths are read from `detail.paths` (or a bare array). `structured_content` =
the `queue-status` payload, `schema: 1`, same as `sluice queue status --json`.

### `queue_approve`

Inputs: `working_directory`, `train_id`.

1. **Server-side gate**: the user config (`UserConfig::load`, read on every
   call) must have `[queue] allow_agent_approval = true`. Otherwise the call
   returns the error result `not-allowed: approval is reserved to humans
   (sluice queue approve); set [queue] allow_agent_approval = true in the
   user config to allow agents` **without contacting the daemon** (no
   autostart either). An unreadable user config also refuses (fail closed).
2. It fetches `queue-status` for the repository, finds the train
   (`not-found` otherwise), requires it to be `ready` with a `train_hash`
   (`conflict-state` otherwise) and sends `queue-approve {train_id,
   train_hash, via: mcp, env}`.
3. **Daemon-side gate**: the daemon repeats the check for `via: mcp` and
   answers `not-allowed` if the flag is off (the daemon's copy of the user
   config); a changed train gives `stale-plan`.
4. On `queue-approved`, while the train is `ready`/`approved`/`pushing`, it polls
   `queue-status` every second, up to the train's `push_timeout_ms` plus
   its `network_timeout_ms` (pre-push checks), two user-config `[queue]
   tick`s and 30 s (defaults 10 min and 60 s when the daemon does not report
   them; `approve_wait`), until the train is `landed`, `needs_reconcile`
   or disappears/`dissolved`; it sends a progress notification every
   `[mcp] progress_interval` when the request has a progress token.
   Cancelling the call stops the wait, never the push (error result
   `cancelled: stopped waiting; train <id> stays <state> and the daemon
   continues`).

Text: `landed <sha7> on <remote>/<target> (train <id>)`, or
`needs_reconcile: …`, `failed: train … was dissolved …`, `timeout: train …
is still …`. `is_error` is set unless it landed. `structured_content`
(`ApproveReport`): `{train_id, state, landed, train}` with the last known
train view.

### `queue_retry`

Inputs: `working_directory`, `job_id`. Fetches `queue-status` for the
repository (`not-found` when the job is not one of its jobs), re-reads
`refs/heads/<job branch>^{commit}` in that repository like `sluice queue
retry` (`unknown-branch` when the branch is gone) and sends `queue-retry
{job_id, head_sha, via: mcp}`. The daemon accepts only `conflict` and
`failed` jobs (`conflict-state` otherwise) and re-queues the job at the back
with the new head. Text: `requeued <branch> (job <id>) at <sha7> for
<target>, position N` and a line telling the agent to stop.
`structured_content`: the job view.

### `queue_cancel`

Inputs: `working_directory`, `job_id`. Checks that the job belongs to the
repository (`not-found`), then sends `queue-cancel {job_id, via: mcp}`.
Text: `<branch> (job <id>) is <state>`, or `cancel requested for …` when the
job is `pushing`/`needs_reconcile` (only recorded; honoured once nothing
landed is proven). `structured_content`: the job view.

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
- End to end with real sessions (`scripts/phase3-e2e.sh`, evidence in
  [[sluice/changes/phase-3-acceptance.md]]): two non-interactive Claude
  Code sessions in two worktrees call `verify` at once; one runs the gate,
  the other coalesces; an agent-invoked `pre-commit` on the verified tree
  hits; on a new tree it fails fast; with
  `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=60000` a 90 s gate completes only
  when progress notifications are sent. Auto-backgrounding (interactive
  sessions, 2 min) is documented by Claude Code and not exercised by the
  script.
