---
title: Executor, daemon lifecycle and persistence
tags: [sluice, design, executor, daemon, phase-2]
---

# Executor, daemon lifecycle and persistence (Phase 2)

Status: design for Phase 2. Related: [[sluice/design/protocol.md]],
[[sluice/design/scheduler.md]], [[sluice/design/architecture.md]] sections
4b and 5, [[sluice/research/references/agent-task-queue.md]],
[[sluice/research/references/mergetrain.md]].

## Daemon lifecycle

- Binary mode `sluice daemon [--foreground]`. Without `--foreground` the
  process detaches itself (`nix::unistd::daemon(true, false)`: fork,
  `setsid`, stdio to `<state>/daemon.log`). No `unsafe` is needed.
- **Single instance:** the daemon takes an exclusive `flock` on
  `<runtime>/daemon.lock` for its whole life and writes a lease row in
  SQLite (`owner = "<user>:<pid>"`, random `token`, `heartbeat_at`,
  `expires_at = now + 90 s`, heartbeat every 15 s). Every write to `jobs`
  that matters (state transitions) is fenced with `WHERE daemon_instance = ?`.
- **Autostart from clients:** a client that cannot connect takes a short
  `flock` on `<runtime>/start.lock`, re-checks the socket, spawns
  `sluice daemon` (detached), waits up to 5 s for the socket with 20 ms
  polls, releases the lock. Failure to start is reported; the client then
  runs the gate itself (Phase 1 path) so a broken daemon never blocks work.
- **Stop:** `sluice daemon stop [--force]` sends `stop`; the daemon removes
  the socket file first (so new clients start a fresh one), finishes or
  kills jobs, releases the lease, exits 0.
- **Idle:** never exits on its own in v1 (`daemon.idle_exit = 0s`, Q10).
- **Restart reconciliation:** on start, rows in `jobs` with
  `state = 'running'` from another `daemon_instance` are orphans. For each,
  if `/proc/<pgid>` exists and its `starttime` (field 22 of
  `/proc/<pid>/stat`) equals the recorded `proc_start_ticks`, the process
  group is killed (SIGTERM, 5 s, SIGKILL); the row is marked
  `outcome = 'orphaned'`. Mismatching start time means the pid was reused:
  only the row is updated.

## Executor

- Spawn: `sh -c '<cmd> "$@"' sh <extra…>` with `current_dir = request.cwd`,
  `env_clear().envs(child_env)`, `stdin = null`, stdout and stderr piped,
  `process_group(0)` (new group; the daemon owns the gate, no terminal).
- Record `pgid`, `proc_start_ticks` and `log_path` in SQLite before the
  first byte of output is read.
- Priority: `setpriority(PRIO_PROCESS, child_pid, daemon.nice)` right after
  spawn; I/O class through the `ionice` binary when present
  (`ionice -c <class> -n <level> -p <pid>`), skipped silently otherwise.
- Output: both pipes drained concurrently into one append-only log
  `<state>/logs/<job-id>.log` (interleaved, 8 KiB chunks) with a header
  line (`# sluice job …`) and footer (`# exit: …`). `log_bytes` is reported
  in `progress`.
- Timeout: `gate.timeout` from the moment the process starts (queue wait
  excluded). On expiry: `killpg(SIGTERM)`, 5 s grace, `killpg(SIGKILL)`,
  outcome `timeout`.
- Cancel (last subscriber gone with `cancel`, `stop --force`): same kill
  sequence, outcome `killed`.
- After exit 0 with a key: re-snapshot the tree with `probe_env` from the
  request (`tree::snapshot`); unchanged → `MarkerStore::write`, `prune`;
  changed → miss record `the gate modified the tree`, `not_cached_reason`.
  Non-zero → miss record with exit code.
- The `tail` in `done` is the last `cache.tail_lines` lines of the log.
- Log retention: on start and daily, delete logs older than
  `daemon.log_retention_days` whose job row is terminal.

## Persistence (SQLite, `rusqlite` bundled, WAL)

File `<state>/sluice.db`. Clients open it `mode=ro` only for diagnostics;
all writes go through the daemon.

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
message naming the file.

## Client side (`sluice run` on a miss)

1. Phase 1 hit path unchanged (no daemon involved).
2. On a miss with policy `wait`: connect (autostart if needed), send `run`
   with everything precomputed, print `queued`/`started` lines to stderr
   for humans (`sluice: test queued (position 2, 5 of 8 units busy)`),
   stream nothing else unless `-v` (then poll the log file), print the
   `done` summary exactly as Phase 1 does, exit with the mapped code.
3. If the daemon cannot be reached or started: fall back to the Phase 1
   in-process run with the `flock` semaphore and say so once on stderr.
4. `run-unqueued` policy never touches the daemon.
5. On a hit, send the best-effort `hit` event (20 ms budget).

## Properties to test (Phase 2 acceptance)

- Kill the client mid-run with `cancel` policy → gate process group gone within 6 s; with `finish` → marker written.
- Kill the daemon (SIGKILL) mid-run → next daemon start kills the orphaned group and marks the row `orphaned`; no stale `daemon.lock`/socket blocks the restart.
- Timeout → whole group dead, outcome `timeout`, exit 124 at the client.
- Two daemons cannot run at once (second start exits with a clear message).
- Client fallback when the socket directory is unwritable.
