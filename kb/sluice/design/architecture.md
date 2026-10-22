---
title: Architecture
tags: [sluice, design]
---

# Architecture (living document)

Status: draft for Phase-0 review. Changes to this document are recorded as
ADRs under `sluice/design/adr/`.

Inputs: [[sluice/research/problem-statement.md]],
[[sluice/research/references/gate-cache.md]],
[[sluice/research/references/agent-task-queue.md]],
[[sluice/research/references/rmcp.md]].

## 1. One binary, several modes

```
sluice run <gate> [-- extra args]   # thin client: hooks, terminal, scripts
sluice mcp                          # thin client: MCP stdio server spawned by Claude Code
sluice daemon                       # the brain: one per user, Unix socket
sluice status [--json]              # queue, in-flight, cache stats, time saved
sluice cache status|clear [--gate]  # inspect / clear markers for the current repo
sluice init                         # write a starter .sluice.toml and print hook snippets
sluice hook print <manager>         # husky | lefthook | pre-commit | plain
sluice logs [<job-id>]              # show job logs
```

Process topology:

```
 git hook ──▶ sluice run ──┐                       ┌──▶ gate process group
 terminal ──▶ sluice run ──┼──(Unix socket)──▶ daemon ─┼──▶ gate process group
 Claude   ──▶ sluice mcp ──┘   JSON lines, versioned   └──▶ ... (bounded by budget)
                 │
                 └── cache-hit fast path: no daemon involved
```

Three invariants:

1. **The hit path never needs the daemon.** The client computes the key and
   checks the marker file itself (target < 30 ms). Only misses talk to the
   daemon.
2. **Every expensive run goes through the daemon** (except the explicit
   `run-unqueued` escape hatch), so budget, coalescing and PSI apply globally.
3. **Sluice never touches the user's index or working tree.**

## 2. Crate layout (proposed, see [[sluice/design/adr/0001-rust-edition-and-workspace.md]])

```
crates/
  sluice-gate-core/     # config, cache key, markers, git tree computation. Sync, no tokio.
  sluice-gate-proto/    # IPC message types (serde), versioned.
  sluice-gate-daemon/   # scheduler, coalescing, executor, PSI, SQLite.
  sluice-gate-mcp/      # rmcp server; thin client of the daemon.
  sluice-gate/          # the `sluice` binary; subcommands; thin client; integration tests.
docs/
kb/                     # Pando knowledge base (design history, ADRs, change log)
refs/                   # reference repositories (gitignored)
```

Crate names on crates.io use the `sluice-gate` prefix
([[sluice/design/adr/0003-crate-naming.md]]). Target platform: Linux only
(Phase-0 answer Q4).

## 3. Gates and configuration

`.sluice.toml` at the repository root defines named gates (shell commands)
with weight, timeout, extra key inputs, env. Details and the frozen format
live in [[sluice/design/config-format.md]].

User-level `~/.config/sluice/config.toml`: global budget, PSI thresholds, log
retention, nice/ionice, per-repo overrides, agent detection.

## 4. Cache key and markers

Summarised here; authoritative in [[sluice/design/cache-key.md]].

- Tree hash from a private copy of the index (`GIT_INDEX_FILE` pointed at a
  temp copy, `git add -A`, `git write-tree`). The real index is never used.
- Inside hooks, git exports `GIT_INDEX_FILE`, `GIT_DIR`, `GIT_WORK_TREE`,
  `GIT_PREFIX`; Sluice reads them explicitly and decides per hook type.
- Key = SHA-256 over (tree, gate name, normalised command, sorted env,
  `key_inputs` outputs, `key_files` hashes, cache-format version).
- Markers under `$(git rev-parse --git-common-dir)/sluice/markers/` so all
  worktrees share them. JSON content. Only successful, non-mutating runs
  write a marker. Default TTL 30 days.

## 4b. Directories

| Purpose | Path | Notes |
|---|---|---|
| Markers and miss records | `<git-common-dir>/sluice/markers/` | shared by all worktrees of a repo |
| Scratch snapshots, daemon socket, locks, semaphore slots | `$XDG_RUNTIME_DIR/sluice/` (fallback `/tmp/sluice-<uid>/`) | mode `0700`; stale `snap-*` swept after 1 h |
| Job logs, SQLite database | `$XDG_STATE_HOME/sluice/` (fallback `~/.local/state/sluice/`) | `logs/<job-id>.log`, `sluice.db` (Phase 2) |
| User config | `$SLUICE_CONFIG` or `$XDG_CONFIG_HOME/sluice/config.toml` (fallback `~/.config/sluice/config.toml`) | optional |

Job ids are `<unix-ms>-<pid>-<random hex>`; they are unique per machine
and sort chronologically.

## 5. Daemon

### 5.1 Lifecycle

- Socket: `$XDG_RUNTIME_DIR/sluice/daemon.sock`; fallback
  `/tmp/sluice-<uid>/daemon.sock` with mode `0700` on the directory.
- Autostart: a client that cannot connect takes an `flock` on
  `<runtime-dir>/daemon.lock`, re-checks the socket, spawns
  `sluice daemon` detached (new session, stdio to a log file), waits for the
  socket with a short backoff, releases the lock. Double start is impossible
  because the daemon itself holds the same lock for its whole life.
- Idle exit: none in v1 (Q10); `sluice daemon stop` stops it.
- `systemd --user` socket activation: later, not v1.

### 5.2 Request handling

For each `Run` request (repo, gate, key, cwd, env snapshot, policy, client kind):

1. Look up the **in-flight map** `key → broadcast::Sender<Outcome>`. If
   present, subscribe and stream the outcome when it arrives (coalescing, G3).
2. Otherwise create the entry and enqueue a **job** with the gate's weight.
3. The **scheduler** admits jobs when `used + weight <= budget` and PSI is
   below thresholds. Policy is documented in [[sluice/design/scheduler.md]]
   (FIFO with a no-starvation rule for heavy gates; to be recorded as an ADR).
4. The **executor** runs the command in the requester's working directory,
   in a new process group, with `nice`/`ionice` applied, stdout+stderr
   streamed to `<data-dir>/logs/<job-id>.log`, enforcing the gate timeout and
   killing the whole group on timeout or cancel.
5. On success with an unchanged tree, the daemon writes the marker. On
   failure it returns the exit code and the log tail.
6. Outcome is broadcast to all subscribers; the in-flight entry is removed.

### 5.3 Client disconnects and crashes

- A job keeps running while it has at least one subscriber. When the last
  subscriber disconnects: cancel (default) or let it finish and cache the
  result (configurable; "finish" is useful because the result is cacheable
  and likely requested again).
- On daemon start, reconcile: kill process groups recorded in SQLite as
  running whose daemon instance id differs (orphans from a crashed daemon).
  Ideas taken from agent-task-queue's zombie protection, see
  [[sluice/research/references/agent-task-queue.md]].

### 5.4 Persistence

SQLite (`rusqlite`, bundled) at `<data-dir>/sluice.db`: jobs table
(id, key, repo, gate, state, timestamps, wait/run durations, exit code, log
path, coalesced count, daemon instance id), plus aggregate metrics for
`sluice status` (hit count, coalesced count, estimated time saved = sum of
previous run durations for hits and coalesced waits).

Markers stay as files inside `.git`: the hit path must not depend on the DB.

## 6. IPC protocol

Newline-delimited JSON over the Unix socket, with a `version` field in the
hello handshake. Messages: `Hello`, `Run`, `Progress` (optional, for MCP
progress notifications), `Outcome`, `Status`, `Cancel`, `Logs`. Authoritative
in [[sluice/design/protocol.md]] (Phase 2).

## 7. MCP server

`sluice mcp` uses `rmcp` with stdio transport. It is a thin client: it
performs the same hit check as the CLI, and forwards misses to the daemon,
blocking as long as needed (no shell timeout applies to MCP tool calls).
Tools: `verify`, `status`, `gate_list`, `logs`. Details and the decision on
the MCP Tasks extension in [[sluice/design/mcp-tools.md]].

## 8. Hook integration and agent policy

Hooks call `sluice run --hook <hook> <gate>` ([[sluice/design/adr/0006-hook-flag.md]]).
On a hit: exit 0 with one line. On a miss the behaviour depends on who
invoked the hook:

| Caller | Default policy | Behaviour |
|---|---|---|
| agent | `fail-fast` | exit 3 with an actionable message pointing to `sluice run <gate>` and the `verify` MCP tool |
| agent | `wait` | wait in the queue (risky under Bash timeouts) |
| agent | `run-unqueued` | run immediately outside the queue |
| human | `wait` | wait in the queue, show progress |

Agent detection: see [[sluice/design/adr/0004-agent-detection.md]].

## 9. Output contract (G8)

- Success: one line on stdout, e.g. `sluice: test PASS 32.1s (job 1761…)`
  or `sluice: test PASS (cached, passed 2h ago)`; a pass that could not be
  cached appends `(not cached: <reason>)`.
- Failure: one line on stderr `sluice: test FAIL exit=1 12.3s (job …) log=<path>`
  followed by the last N lines of the log (default 40, `cache.tail_lines`).
- Timeout: `sluice: test TIMEOUT after 20m (job …) log=<path>` + tail, exit 124.
- Exit codes of `sluice run`: 0 pass or hit; the gate's exit code on
  failure; 3 refused by `fail-fast`; 124 timeout; 143 gate killed by a
  signal; 1 Sluice error; 2 usage error.
- `-v/--verbose` streams the gate output to stderr while it runs; the log
  file always has everything.
- `--json` variants (Phase 2, `status`); the MCP tool returns the same text block.

## 9b. Phase 1 execution (no daemon yet)

Until the daemon exists, `sluice run` itself runs the gate: it takes
`weight` slots (all or nothing, capped at the budget) of a `flock`
counting semaphore with `daemon.budget` slots under the runtime dir, spawns
`sh -c '<cmd> "$@"' sh <extra…>` in the worktree root with the sanitised
environment, captures stdout and stderr interleaved into
`<state>/logs/<job-id>.log`, and enforces the timeout by killing the
process tree found through `/proc` (SIGTERM, 5 s grace, SIGKILL). The gate
shares the client's process group so Ctrl-C reaches it. No coalescing in
Phase 1. See [[sluice/changes/phase-1-cache-only.md]].

## 10. Security and safety

- Socket directory `0700`, owned by the user. No network listener in v1.
- The daemon runs gates with the requester's cwd and the requester's
  environment snapshot, minus git hook variables and `CLAUDE*`, plus the
  gate's declared `env` (Q11).
- `SLUICE_DISABLE=1` bypasses everything (runs the gate directly, no cache,
  no queue). `SLUICE_CACHE=0` disables only the cache.

## 11. Refinements taken from the reference projects

From [[sluice/research/references/agent-task-queue.md]]:

- Decouple request from job: a job outlives a cancelled tool call or a
  disconnected hook client as long as another subscriber exists; only an
  explicit `cancel` kills it. Last-subscriber behaviour is configurable (5.3).
- Output: full interleaved log on disk, bounded tail in the result (taken
  from the interleaved stream, not stderr-or-stdout), inline output capped
  with an "omitted bytes" marker, text plus `structured_content` in every
  MCP result.
- Process control: `setsid` process group, SIGTERM, 5 s grace, SIGKILL to the
  group even if the shell already exited; drain stdout and stderr
  concurrently.
- Store epoch timestamps, never mixed local/UTC strings; never auto-delete
  the database on errors; never clear job rows without killing children.
- CLI and MCP paths share identical semantics (their `tq run` bypasses
  output capture and results; Sluice must not).
- Their answer to agent timeouts is a bounded wait (≤ 30 s) returning a
  handle plus a `task_status` poll tool. Sluice's MCP design decision between
  that and a blocking `verify` with progress notifications is recorded in
  [[sluice/design/adr/0005-mcp-verify-blocking-or-handle.md]] (pending the
  Claude Code timeout facts).

From [[sluice/research/references/mergetrain.md]]:

- Daemon single instance = `flock` on the runtime dir **plus** a lease row
  in SQLite (owner `user:pid`, heartbeat, TTL, random token); every write
  that matters is fenced with `WHERE token = ?`, so a stale daemon cannot
  clobber a newer one's state.
- PID liveness triage dead / alive / unknown (`EPERM` means alive); never
  steal an unexpired lease; reclaim an expired one only when no job is in
  progress.
- Translate SQLite busy into a typed retryable error, never into a gate
  failure (contention must never cache a false negative).
- Read-only probes (`sluice status`) open the DB `mode=ro` and refuse other
  schema versions.

From [[sluice/research/references/rmcp.md]]:

- rmcp 3.6 (MSRV 1.88, edition 2024) imposes no server-side timeout on tool
  handlers; cancellation arrives as a `CancellationToken`, progress goes out
  with `notify_progress` when the client sent a `progressToken`.
- MCP Tasks (SEP-2663) are implemented in rmcp but require the client to
  declare the extension and force hand-written `call_tool`; deferred to v2
  unless Claude Code is confirmed to support them.
- Logging strictly to stderr, `with_ansi(false)`; never `println!` in the
  MCP process.

From [[sluice/research/references/gate-cache.md]]: everything in section 4
and [[sluice/design/cache-key.md]] (private index + private object dir,
unknown-tree policy, miss records, `--no-optional-locks`).

## 12. Open design points (tracked as wanted concepts)

[[sluice/design/scheduler.md]], [[sluice/design/protocol.md]],
[[sluice/design/mcp-tools.md]], [[sluice/design/psi-admission.md]],
[[sluice/design/executor.md]].
