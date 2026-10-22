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
sluice run <gate> [--hook <hook>] [-v] [--explain] [-- extra args]   # hooks, terminal, scripts (Phase 1)
sluice cache status [--all] | clear [--gate <gate>]                  # markers of the current repo (Phase 1)
sluice init [--force]                                                # starter .sluice.toml + plain hook snippet (Phase 1)
sluice hook print husky|lefthook|pre-commit|plain                    # integration snippets (Phase 1)
sluice logs [<job-id>] [-n <lines>]                                  # job logs (Phase 1)
sluice daemon [--foreground] | stop [--force]                        # the brain: one per user, Unix socket (Phase 2)
sluice status [--json]                                               # queue, in-flight, cache stats, time saved (Phase 2)
sluice mcp                                                           # MCP stdio server spawned by the agent (Phase 3)
```

`sluice mcp` exits 70 with `sluice: 'mcp' is not implemented yet (it arrives
with the daemon and MCP phases)` until Phase 3.

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
   `run-unqueued` escape hatch and the in-process fallback when the daemon
   is unavailable), so budget, coalescing and PSI apply globally.
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
  `GIT_PREFIX`; Sluice honours them for its own probes and strips them from
  the gate's environment. The tree is always the working-tree content.
- Key = SHA-256 over (`sluice-cache-v1` prefix, tree, gate name,
  normalised command, sorted env, `key_inputs` outputs, `key_files`
  hashes). The prefix tracks the cache-format version; markers also carry
  a `format` field that is checked on read.
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

Job ids are `<unix-ms>-<pid>-<6 hex digits of the sub-second nanoseconds>`;
they are unique per machine and sort chronologically. `$XDG_STATE_HOME`
is ignored when empty; with no `HOME` the state dir falls back to
`/.local/state/sluice`.

## 5. Daemon

### 5.1 Lifecycle

- Socket: `$XDG_RUNTIME_DIR/sluice/daemon.sock`; fallback
  `/tmp/sluice-<uid>/daemon.sock` with mode `0700` on the directory.
- Autostart: a client that cannot connect takes an `flock` on
  `<runtime-dir>/start.lock`, re-checks the socket, spawns its own
  executable as `sluice daemon` (which detaches itself with
  `nix::unistd::daemon` before creating its tokio runtime and sends stdout
  and stderr to `<state>/daemon.log`; `SLUICE_LOG` sets its log filter),
  polls the socket every 20 ms for up to 5 s, releases the lock. The
  spawned daemon's environment has every `GIT_*` variable removed so a
  hook's variables never stick to it. Double start is impossible because
  the daemon holds `<runtime-dir>/daemon.lock` for its whole life and a
  fenced lease row in SQLite. The socket file is `0600`.
- If the daemon cannot be reached or started, `sluice run` says
  `daemon unavailable (<why>); running in-process` and runs the gate
  itself under the Phase 1 `flock` semaphore, so a broken daemon never
  blocks work.
- Idle exit: none in v1 (Q10); `sluice daemon stop [--force]` stops it
  (`no daemon running` when there is none).
- `systemd --user` socket activation: later, not v1.

### 5.2 Request handling

For each `Run` request (repo, gate, key, cwd, env snapshot, policy, client kind):

1. Look for a queued or running job with the same key. If found,
   subscribe to its event channel and stream the outcome when it arrives
   (coalescing, G3).
2. Otherwise enqueue a **job** with the gate's weight.
3. The **scheduler** admits jobs when `used + weight <= budget` and PSI is
   below thresholds. Policy in [[sluice/design/scheduler.md]] (FIFO with
   backfill and head-of-line reservation, [[sluice/design/adr/0007-scheduler-fairness.md]]).
4. The **executor** runs the command in the worktree root, in a new
   process group, with `nice`/`ionice` applied through the `renice` and
   `ionice` binaries, stdout+stderr streamed to `<state>/logs/<job-id>.log`,
   enforcing the gate timeout and killing the whole group on timeout or
   cancel.
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

SQLite (`rusqlite`, bundled) at `<state>/sluice.db`: jobs table (id, key,
repo, gate, state, timestamps from which wait and run times are derived,
exit code, log path, coalesced count, daemon instance id), plus aggregate
metrics for `sluice status` (hit count, coalesced count, estimated time
saved = sum of marker durations for hits and run times for coalesced
subscribers of passed jobs).

Markers stay as files inside `.git`: the hit path must not depend on the DB.

## 6. IPC protocol

Newline-delimited JSON over the Unix socket, with a `protocol` field in
the hello/welcome handshake. Client messages: `hello`, `run`, `status`,
`logs`, `event`, `stop`, `cancel`; server messages: `welcome`, `queued`,
`started`, `progress`, `done`, `status`, `log`, `stopping`, `error`.
Authoritative in [[sluice/design/protocol.md]]; types in
`sluice-gate-proto`.

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
| agent | `fail-fast` | exit 3 with a three-line message naming the gate, the detection rule and the fix (`run 'sluice run <gate>' from your shell first`; Phase 3 adds the `verify` MCP tool). Applies only when the cache was consulted and missed; with caching off or an unknown tree the gate runs instead. |
| agent | `wait` | wait for budget (risky under Bash timeouts) |
| agent | `run-unqueued` | run immediately outside the queue |
| human | `wait` | wait in the daemon queue; `queued (position N, U of B units busy)` and `running (job …)` notices (the in-process fallback prints `waiting for N of M budget units` instead) |
| human | `run-unqueued` | run immediately outside the queue |
| anyone, not in a hook | `wait` | the policies only apply inside hooks (`--hook` given, or `GIT_DIR`/`GIT_INDEX_FILE` present) |

`sluice run --explain <gate>` prints every decision (repository, git
version, cache mode, caller and rule, hook, policies, budget, tree, key,
lookup result) and exits 0 without running anything.

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
  signal (`FAIL exit=signal`), by `daemon stop --force`, by cancellation
  or dropped from the queue by `daemon stop` (`KILLED`); 1 Sluice error (`sluice: error: <message>`
  on stderr: no repository, bad config, unknown gate, spawn or log
  failure); 2 usage error; 70 command not implemented yet.
- Stderr notices on the way to an uncached run: `sluice: cache disabled
  (<reason>)`, `sluice: tree unknown (<reason>); running uncached`,
  `sluice: cannot snapshot the tree / derive the cache key / read the cache
  (<error>); running uncached`, `sluice: SLUICE_DISABLE is set; running
  '<gate>' directly`, `sluice: waiting for N of M budget units for '<gate>'`.
- The failure tail is introduced by `--- last N lines of <log> ---` and
  skips Sluice's own framing lines of the log.
- `-v/--verbose` streams the gate output to stderr while it runs; the log
  file always has everything.
- `--json` variants (Phase 2, `status`); the MCP tool returns the same text block.

## 9b. Running a gate: daemon path and in-process fallback

On a miss with policy `wait`, `sluice run` sends a `run` request to the
daemon ([[sluice/design/protocol.md]]) and prints
`sluice: <gate> queued (position N, U of B units busy[, sharing job <id>])`
on `queued` and `sluice: <gate> running (job <id>)` on `started`; `-v`
tails the log file meanwhile. The daemon runs the gate as described in
[[sluice/design/executor.md]] and the client maps `done` to the Phase 1
output lines and exit codes (`cached` prints the hit line; `killed` prints
`KILLED` and exits 143). A cache hit additionally sends a best-effort
`hit` event (about 1 ms) so `status` can count hits and time saved.

When the daemon is unavailable, `sluice run` runs the gate itself: it
takes `weight` slots (all or nothing, capped at the budget) of a `flock`
counting semaphore with `daemon.budget` slots under the runtime dir
(`run-unqueued` runs the same way but takes no slots), spawns `sh -c '<cmd>'` (with `"$@"` and the extra
arguments appended only when there are any, because a compound command
followed by `"$@"` is a syntax error) in the worktree root with the
sanitised environment, captures stdout and stderr interleaved into
`<state>/logs/<job-id>.log` (header `# sluice job <id>: gate '<gate>' in
<root>` and `# command: <cmd>`, footer `# exit: <code> after <s>s`; stdout
and stderr pumped by two threads, so ordering is by arrival; the gate's
stdin is `/dev/null`), and enforces the timeout by killing the process
tree found through `/proc` (SIGTERM, 5 s grace, SIGKILL). In this path
the gate shares the client's process group so Ctrl-C reaches it. The
weight is clamped to `[1, budget]`, slots are
`<runtime>/semaphore/slot-<n>.lock`, waiting polls with 25–500 ms backoff
and has no timeout, and there is no coalescing. `sluice logs` asks the
daemon for the newest (or the given) job's tail and otherwise prints the
newest log file in `<state>/logs/` (by file name); the path goes to stderr
and the content to stdout. See [[sluice/changes/phase-1-cache-only.md]]
and [[sluice/changes/phase-2-daemon.md]].

## 10. Security and safety

- Socket directory `0700`, owned by the user. No network listener in v1.
- The daemon runs gates in the worktree root with the requester's
  environment snapshot, minus git hook variables and `CLAUDE*`, plus the
  gate's declared `env` (Q11). The runtime directory is `0700`, the socket
  `0600`, the state and logs directories `0700`; files inside follow the
  umask.
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
