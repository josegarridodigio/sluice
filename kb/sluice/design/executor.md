---
title: Executor, daemon lifecycle and persistence
tags: [sluice, design, executor, daemon, phase-2]
---

# Executor, daemon lifecycle and persistence (Phase 2)

Status: implemented in Phase 2 (`sluice-gate-daemon`, CLI `daemon_cmd`, proto `client`). Related: [[sluice/design/protocol.md]],
[[sluice/design/scheduler.md]], [[sluice/design/architecture.md]] sections
4b and 5, [[sluice/research/references/agent-task-queue.md]],
[[sluice/research/references/mergetrain.md]].

## Daemon lifecycle

- Binary mode `sluice daemon [--foreground]`. Without `--foreground` the
  process detaches itself (`nix::unistd::daemon(true, false)`: fork,
  `setsid`, stdio to `<state>/daemon.log`). No `unsafe` is needed.
- **Single instance:** the daemon takes an exclusive `flock` on
  `<runtime>/daemon.lock` for its whole life and writes a lease row in
  SQLite (`owner = "<uid>:<pid>"`, random `token`, `heartbeat_at`,
  `expires_at = now + 90 s`, heartbeat every 15 s). Every write to `jobs`
  that matters (state transitions) is fenced with `WHERE daemon_instance = ?`.
  A lost lease (heartbeat rejected) does not stop the daemon: it keeps
  serving its running jobs and refuses new `run` requests with
  `error{internal}`. The daily log pruning runs from the heartbeat task
  (and once at start).
- **Autostart from clients:** a client that cannot connect takes a
  blocking `flock` on `<runtime>/start.lock`, re-checks the socket, spawns
  its own executable as `sluice daemon` (stdin null, stdout and stderr
  appended to `<state>/daemon.log`, own process group, environment minus
  every `GIT_*` variable), waits up to 5 s for the socket with 20 ms polls
  (an early non-zero exit is reported at once; while an old daemon is still
  draining after `stop`, the client retries for up to 5 s), releases the
  lock. Failure text: `the daemon could not be started (<why>); see
  <daemon.log>`. The client then runs the gate itself (Phase 1 path) so a
  broken daemon never blocks work. Start-up errors of `sluice daemon`
  itself: `a daemon is already running (lock <path>)` exits 75
  (`EX_TEMPFAIL`, so the client can tell it apart and retry during a
  drain); `unusable socket path <p>`, config errors and runtime creation
  failure exit 1.
- **Stop:** `sluice daemon stop [--force]` sends `stop`; the daemon removes
  the socket file first, drops queued jobs (`done{killed}` with their job
  id), finishes or (with `--force`, SIGTERM or SIGINT) kills running
  jobs, releases the lease and the lock, exits 0. The command prints
  `sluice: daemon stopping (N running jobs)` (or `no daemon running`, exit
  0) and waits for the old daemon to release `daemon.lock` (up to 60 s,
  printing `waiting for N running jobs to finish`, then a note and exit 0
  if it gives up; `--force` polls the pid for up to 10 s) so the next
  client gets a fresh daemon.
- **Restart:** `sluice daemon restart [--debug] [--force]` (`daemon_cmd::restart`)
  refuses with exit 1 while gates are running unless `--force`, stops the
  daemon as `stop` does, then autostarts a detached one; with `--debug` the
  new instance gets `SLUICE_LOG=debug` (that instance only). It prints
  `no daemon running; starting one` when there was none and
  `daemon started` (`(log level debug)` with `--debug`) on success.
- **Idle:** never exits on its own in v1 (`daemon.idle_exit = 0s`, Q10).
- **Restart reconciliation:** on start, rows in `jobs` with
  `state = 'running'` from another `daemon_instance` are orphans (queued
  rows of other instances are closed as `orphaned` too). For each,
  if `/proc/<pgid>` exists and its `starttime` (field 22 of
  `/proc/<pid>/stat`) equals the recorded `proc_start_ticks`, the process
  group is killed (SIGTERM, 5 s, SIGKILL); the row is marked
  `outcome = 'orphaned'`. Mismatching start time means the pid was reused:
  only the row is updated.

## Executor

- Spawn: `sh -c '<cmd> "$@"' sh <extra…>` with `current_dir = request.cwd`,
  `env_clear().envs(child_env)`, `stdin = null`, stdout and stderr piped,
  `process_group(0)` (new group; the daemon owns the gate, no terminal).
- Record `pgid`, `proc_start_ticks` and `log_path` in SQLite right after
  the spawn (the output pumps are already running; a queued row has a null
  `log_path`).
- Priority: the `nix` crate exposes no `setpriority`, and calling libc
  directly needs `unsafe`, so the CPU priority is applied with the
  `renice` binary right after spawn (`renice -n <nice> -p <pid>`, skipped
  when `nice = 0` or the binary is absent); the I/O class through the
  `ionice` binary when present (`ionice -c <2|3> -n <level> -p <pid>`),
  skipped silently otherwise. Results are ignored (debug log).
- Output: both pipes drained concurrently into one append-only log
  `<state>/logs/<job-id>.log` (interleaved, 8 KiB chunks) with a header
  line (`# sluice job …`) and footer (`# exit: …`). `log_bytes` is reported
  in `progress`.
- Timeout: `gate.timeout` from the moment the process starts (queue wait
  excluded; `0` disables it). On expiry: `killpg(SIGTERM)`, 5 s grace,
  `killpg(SIGKILL)`, outcome `timeout`. If the leader exits but a
  grandchild keeps the pipes open, the same deadline applies to the pumps
  and the group is SIGKILLed, so `wait` never hangs.
- Cancel (last subscriber gone with `cancel`, `stop --force`): same kill
  sequence, outcome `killed`; the log footer says `(killed)`. Under
  `finish`, a job whose last subscriber left keeps running (or stays
  queued and still runs later).
- After exit 0 with a key: re-snapshot the tree with `probe_env` from the
  request (`tree::snapshot`, scratch dir from the daemon's own
  `XDG_RUNTIME_DIR`); unchanged → `MarkerStore::write` (`cmd` normalised,
  `committed_tree` taken from the request, `sluice_version` the daemon's),
  `prune`; changed → miss record `the gate modified the tree`,
  `not_cached_reason`. Non-zero → miss record `gate failed` with the exit
  code; timeouts `timed out`; cancellations `killed`. The gate environment
  is the request's `child_env` with `gate.env` laid over it.
- The `tail` in `done` is the last `cache.tail_lines` lines of the log.
- Log retention: on start and daily (from the heartbeat task, so it stops
  if the lease is lost), delete job logs older than
  `daemon.log_retention_days` whose job row is terminal or that belong to
  no row at all (in-process fallback runs). The same pass deletes
  `mcp-<pid>.log` files (the `sluice mcp` logs) older than the retention by
  mtime; `client.log` is never deleted by it (it rotates by size, below).
- Other files in `<state>/logs/`: `mcp-<pid>.log` (tracing output of one
  `sluice mcp` process, in addition to its stderr), `client.log` (one line
  per `sluice run`, see `docs/debugging.md`; one `O_APPEND` write per line;
  above 5 MiB it is renamed to `client.log.1`, one generation kept).
  Job-log lookups (`sluice logs`, MCP `logs`) only consider names that start
  with a digit, i.e. job ids.
- `daemon.log` rotation: when larger than `[debug] daemon_log_max_mib`
  (default 20) it is renamed `daemon.log.1`, shifting `.1` to `.2` and `.2`
  to `.3` (three generations). It runs in `sluice daemon` before detaching
  (nothing writes the file then) and from the daily pruning task. At runtime
  the task renames the file, reopens `daemon.log` for appending and points
  stdout and stderr at it with the safe `nix::unistd::dup2_stdout` /
  `dup2_stderr` wrappers (the same helpers used at start-up), so tracing
  keeps writing to the new file. A foreground daemon logs to the terminal
  and never rotates. The runtime path is not covered by an integration test
  (the task runs every 24 h); the start-up path is.

## Persistence (SQLite, `rusqlite` bundled, WAL)

File `<state>/sluice.db`. `Store::open_read_only` serves diagnostics
(`sluice doctor` reads the lease row and the failed jobs through it); all
writes go through the daemon. The merge queue keeps its own `queue_*` tables
in the same file ([[sluice/design/merge-queue.md]] section 4) and the daemon's
`schema.version` stays 1.
The state dir, the logs dir and the socket directory are created `0700`
(`dirs::create_private_dir`, which also applies to missing parents such as
`~/.local/state`); files inside follow the process umask.

```sql
CREATE TABLE schema (version INTEGER NOT NULL);            -- 1
CREATE TABLE lease (
  name TEXT PRIMARY KEY, owner TEXT NOT NULL, token TEXT NOT NULL,
  heartbeat_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE TABLE jobs (
  id TEXT PRIMARY KEY, key TEXT, tree TEXT, repo TEXT NOT NULL, gate TEXT NOT NULL,
  cmd TEXT NOT NULL, weight INTEGER NOT NULL, caller TEXT NOT NULL, session TEXT,
  state TEXT NOT NULL,            -- queued | running | done
  outcome TEXT,                   -- pass | fail | timeout | killed | cached | error | orphaned
  exit_code INTEGER,
  queued_at INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER,   -- unix ms
  log_path TEXT, pgid INTEGER, proc_start_ticks INTEGER,
  daemon_instance TEXT NOT NULL, subscribers_max INTEGER NOT NULL DEFAULT 1,
  coalesced INTEGER NOT NULL DEFAULT 0, marker_written INTEGER NOT NULL DEFAULT 0);
CREATE INDEX jobs_state ON jobs(state);
CREATE INDEX jobs_queued_at ON jobs(queued_at);
CREATE TABLE events (                 -- client-side hits
  at INTEGER NOT NULL, repo TEXT NOT NULL, gate TEXT NOT NULL, saved_ms INTEGER NOT NULL);
```

Timestamps are unix milliseconds (integers), never strings. Busy timeouts
map to a typed retryable error, never to a gate failure. The database is
never deleted automatically; a schema mismatch is a hard error with a
message naming the file. `totals(since)` filters jobs by `queued_at` and events by `at`;
`prune_logs` refuses job ids containing path separators or `..`.

## Client side (`sluice run` on a miss)

1. Phase 1 hit path unchanged (no daemon involved).
2. On a miss with policy `wait`: connect (autostart if needed), send `run`
   with everything precomputed (`cmd` trimmed but not whitespace-normalised,
   `cwd` = worktree root), print `queued`/`started` lines to stderr for
   humans (`sluice: test queued (position 2, 5 of 8 units busy[, sharing
   job <id>])` — the `U of B` part comes from one extra `status` call and
   is omitted if it fails — then `sluice: test running (job <id>)`),
   stream nothing else unless `-v` (then poll the log file every 500 ms),
   print the `done` summary exactly as Phase 1 does, exit with the mapped
   code. A connection lost after `queued` is final: `sluice: error: lost
   the connection to the daemon`, exit 1, no fallback (the job may keep
   running in the daemon). `killed` prints `KILLED after <s>s (job …)` and
   exits 143; a job dropped before it started prints `KILLED before it
   started (job <id>): the daemon stopped`.
3. If the daemon cannot be reached or started (or refuses before
   `queued`): `sluice: daemon unavailable (<why>); running in-process`,
   then the Phase 1 in-process run with the `flock` semaphore. Gates run
   this way, or under `run-unqueued` (which takes no slots at all), do not
   appear in `status`, do not consume the daemon's budget and get no
   `nice`/`ionice`. A zero `timeout` means no timeout on both paths.
4. `run-unqueued` policy never touches the daemon.
5. On a hit, send the best-effort `hit` event (20 ms budget) when the
   socket file exists.

## Properties to test (Phase 2 acceptance)

- Kill the client mid-run with `cancel` policy → gate process group gone within 6 s; with `finish` → marker written.
- Kill the daemon (SIGKILL) mid-run → next daemon start kills the orphaned group and marks the row `orphaned`; no stale `daemon.lock`/socket blocks the restart.
- Timeout → whole group dead, outcome `timeout`, exit 124 at the client.
- Two daemons cannot run at once (second start exits with a clear message).
- Client fallback when the socket directory is unwritable.
