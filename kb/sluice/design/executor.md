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
  At start, a lease left by a dead daemon of the same user (its `uid:pid` owner no longer exists or
  is a zombie, `server::reclaim_dead_lease`) is released first, so a restart after SIGKILL is not
  refused for the 90 s TTL; the lease of a live owner is never touched. A lost lease (heartbeat rejected) does not stop the daemon: it keeps
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
  `state = 'running'` from another `daemon_instance` are orphans. For each,
  `executor::group_is_ours(pgid, proc_start_ticks)` decides whether the
  group still holds processes of that run: the leader `/proc/<pgid>`
  exists and its `starttime` (field 22 of `/proc/<pid>/stat`) equals the
  recorded `proc_start_ticks`, or the leader is gone but members of the
  group remain (Linux never hands out a pid number while it still names a
  live process group, so those can only be the run's descendants; the
  first version only looked at the leader and missed this case). A live
  leader with another start time means the pid was reused: nothing is
  signalled. Surviving groups are terminated together (SIGTERM, 5 s,
  SIGKILL); then running rows are closed as `outcome = 'killed'` (they
  count in `totals.runs`) and queued rows of other instances as `orphaned`
  (`Store::close_orphans`). Reconciliation is the safety net; the reaper
  below normally killed the groups seconds after the daemon died.

## Daemon death (the gate reaper)

Found by `s4_chaos.py` item 1 (task X1): a SIGKILLed daemon left all its
gate process groups running, because gates run in their own process
groups (so `killpg` reaches the whole tree) and nothing tied them to the
daemon's life; only the next daemon start reconciled them, and the next
start may be hours away.

- The daemon starts one companion process at start-up through a
  short-lived launcher, `sluice daemon reaper --detach` (hidden
  subcommand; `sluice_gate_daemon::reaper`, entry `daemon_cmd::reaper`, executed through `/proc/self/exe` so it
  works after `sluice update` replaced the file; own process group, empty
  environment, cwd `/`, stdin a pipe, stderr the daemon log). The launcher
  starts `sluice daemon reaper --daemon <daemon pid>` with the same stdin,
  prints its pid and exits; the daemon waits for the launcher only. The
  reaper is therefore reparented to init (or the session's subreaper) and
  is not a descendant of the daemon: it supervises the gates and must
  outlive the daemon, so it does not belong to the daemon's process tree
  (the s4 harness counts that tree as "processes the daemon is
  responsible for", and a reaper inside it survived a cancelled job by
  design). `DaemonConfig::reaper = None` (tests) disables it.
- For every job it spawns the daemon writes `+ <pgid> <start ticks>` to
  the pipe, and `- <pgid>` when the job ended. The write end exists only
  in the daemon (close-on-exec, never inherited by gates), so the kernel
  closes it when the daemon exits for any reason, SIGKILL and OOM
  included.
- The `+` line is written right after `executor::spawn` returns, with no
  await in between. Priorities no longer need helper processes after the
  spawn (see Priority below); only the fallback `renice`/`ionice -p` for a
  missing wrapper tool runs in `Running::wait`, after the registration and
  the store's `mark_started`. Until 2026-10-23 they ran inside `spawn`,
  before the registration, and a daemon killed while those helpers ran
  (tens of milliseconds, seconds on a loaded runner) left an unregistered
  gate behind (CI flake, see [[sluice/changes/ci-flakes.md]]). The window
  that remains is the few instructions between the fork and the pipe
  write.
- On end of file the reaper SIGTERMs every registered group that
  `group_is_ours`, waits up to `KILL_GRACE` (5 s), SIGKILLs the rest and
  exits. While it waits (`executor::terminate_groups_blocking`, also used
  by the daemon's own shutdown), a group is dropped the first time it is
  seen empty and never signalled again: its pgid may already name an
  unrelated new group (alpha review finding 8). After a clean stop the daemon closes the pipe, nothing is
  registered and the reaper exits at once.
- Clients waiting on a daemon that died get `sluice: error: the daemon died
  while running <gate>; it was killed` and exit 143 (`while <gate> was
  queued; it did not run`, exit 1, when it had not started); `sluice exec`
  prints `sluice: exec: the daemon died while running exec '<label>'; it was
  killed` and exits 125. Nothing is
  cached: markers are only written by the daemon after a verdict.
- If the reaper cannot be started (or dies) the daemon logs a warning and
  keeps working; reconciliation at the next start still applies.

Choice (recorded for X1). Options considered:

1. `PR_SET_PDEATHSIG` on each gate: it must be set in the child between
   `fork` and `exec`, i.e. in `CommandExt::pre_exec`, which is `unsafe`
   (`unsafe_code = "forbid"`). `nix 0.31` (already in the tree) wraps
   `prctl(PR_SET_PDEATHSIG)` safely, but only for the calling process.
   It also fires when the *thread* that forked the child exits, not the
   process: gates are spawned from tokio worker threads, so a retiring
   thread would kill healthy gates. It covers only the leader, not its
   descendants, and is cleared by `fork`. Rejected.
2. A per-gate supervisor (`sluice` wrapper that sets PDEATHSIG on itself
   and execs or forks the gate): one more process per gate, exit status
   and signal forwarding to re-implement, and the thread caveat remains.
   Rejected.
3. A per-daemon reaper fed through a pipe (chosen): one process per
   daemon, no `unsafe`, end of file on a pipe is a reliable process-death
   signal, the whole group (descendants included) is killed, and pid
   reuse is guarded exactly like reconciliation.
4. Reconciliation only: gates of a dead daemon would run until the next
   daemon start. Kept as the fallback, not enough on its own.

Limit: a job spawned in the instant before the daemon dies may be missed
by the reaper (registered right after the spawn); the next start
reconciles it. If both the daemon and the reaper are SIGKILLed together
(`pkill -9 sluice`), only reconciliation remains.

**Known limitation (accepted, alpha review finding 13): the
fork-to-registration window.** Between the fork in `executor::spawn` and
the `+ <pgid>` line on the reaper pipe there are a few instructions (no
await, no helper process) during which a SIGKILLed daemon leaves a gate
the reaper never heard of. Closing it would need the registration before
the fork (an unknown pgid) or a `pre_exec` hook (`unsafe`, forbidden).
The gate's row is already `queued`/`started` in the store with the
daemon's instance id, so the next daemon start reconciles it (kills the
group when its start ticks match, marks the row `killed`). Until then the
gate may run to completion unsupervised; it can never write a marker
(markers are written by the daemon after a verdict).

**A job finished while it is being spawned.** `detach` (last subscriber
gone) and `drain` (stop) drop a queued job under the same state lock as
the "still queued" check (`close_job`), so the scheduling loop cannot
admit it in between. Should a job's entry nevertheless be gone when
`launch` registers its process, the daemon cancels it (SIGTERM to the
group, SIGKILL after the grace), awaits it and untracks it from the
reaper instead of leaving it running unsupervised (alpha review
finding 7).

## Executor

- Spawn: `sh -c '<cmd>'`, or `sh -c '<cmd> "$@"' sh <extra…>` only when extra
  arguments exist (a compound command followed by `"$@"` is a syntax error),
  with `current_dir = request.cwd`,
  `env_clear().envs(child_env)`, `stdin = null`, stdout and stderr piped,
  `process_group(0)` (new group; the daemon owns the gate, no terminal).
  `ExecSpec` says what and where: `launch: Launch` (`Shell { cmd, extra }`
  for gates, `Argv { program, args }` for `sluice exec`) and `sink: Sink`
  (`Log` for gates, `Spool` for exec). Every job's environment gets
  `SLUICE_JOB=<job id>` (gates too), which turns a nested `sluice exec` or
  `sluice run` into a direct, unqueued run.
- `sluice exec` jobs ([[sluice/design/command-control.md]] section 2): the
  argv runs directly (no shell) with the caller's environment (plus
  `SLUICE_JOB`) and cwd, same group, priorities, timeout (`timeout_ms`, 0 =
  none) and kill sequence as gates. The `jobs` row has `gate = "exec:<label>"`,
  `key` NULL and `cmd` the shell-quoted argv (`via = exec` in `job_requests`);
  the declared weight is kept (no learned weight, ADR-0011 M2) and the caller's
  repository identity (else its cwd) feeds the per-repository cap. Output goes to `<logs>/<job>.exec` in
  the spool format of `executor::spool` (magic `SLUICEX1\n`, records
  `[stream u8][len u32 BE][bytes]`, streams kept apart, no header or
  footer); `done` is sent after both pumps finished. The leader's death by
  a signal is `ExecStatus::Signaled(n)` and reaches the client as
  `outcome.signal`. Exec jobs never write markers or miss records and
  ignore `last_subscriber_gone = "finish"` (the last subscriber leaving
  kills them). A forwarded `signal` is sent to the group with `killpg`;
  once the leader is gone the rest of the group is SIGKILLed, and after
  `KILL_GRACE` + 5 s with the leader alive the cancel sequence runs.
- Record `pgid`, `proc_start_ticks` and `log_path` in SQLite right after
  the spawn (the output pumps are already running; a queued row has a null
  `log_path`).
- Priority: the `nix` crate exposes no `setpriority`, and calling libc
  directly needs `unsafe` (no `pre_exec`), so the job is started through
  the standard tools, which set their own priority and `exec` the next
  program (same PID and process group; the reaper and `proc_start_ticks`
  are unaffected): argv is `[nice -n <N>] [ionice -c <2|3> [-n <level>]]
  <sh -c cmd | program args>`, with absolute paths resolved once per
  process (`find_in_path`). Every descendant inherits the priority from its
  first instruction (issue #1: the former asynchronous `renice -p <leader>`
  left children forked before it at nice 0). `nice = 0` and no ionice add
  no wrapper; the idle class takes no `-n`. When a tool is missing, that
  half falls back to `renice`/`ionice -p <pid>` from `Running::wait`
  (debug log). The command in the log header and the cache key do not
  include the wrapper.
- Containment placement (ADR-0017): with containment active, `launch` puts
  `sluice cg-exec <daemon-scope>/jobs/<job-id> --` in front of that chain
  (`ExecSpec::wrapper`), so the argv is `sluice cg-exec <leaf> -- [nice ...]
  [ionice ...] <sh -c cmd | program args>`. `cg-exec` writes its own pid to
  the leaf's `cgroup.procs` and `exec`s the rest, so, exactly like the
  `nice`/`ionice` wrappers, the PID, the process group, the reaper
  registration and `proc_start_ticks` are those of the job and every
  descendant starts inside the leaf. The leaf is created before the spawn and
  removed when the job ends (a leaf still holding a stray background process
  stays for the minute pruning). Without containment (off, disabled, or any
  setup error) the wrapper is empty and the chain is the 0.4.1 one. Since 0.6.0
  (ADR-0019) the wrapper is `sluice cg-exec <leaf> --oom-score-adj <N> -- ...`:
  `cg-exec` also raises its own `oom_score_adj` to `max(N, inherited)` (`N` is
  `[containment] gates_oom_score_adj`, 800 by default) before the `exec`, and a
  leaf of `-` means no move into a cgroup (only the adj is applied, where cgroup
  v2 or the user manager is missing). Both steps fail open. With
  `[containment] enabled = false` nothing is wrapped.
- Output: both pipes drained concurrently into one append-only log
  `<state>/logs/<job-id>.log` (interleaved, 8 KiB chunks) with a header
  line (`# sluice job …`) and footer (`# exit: …`). `log_bytes` is reported
  in `progress`.
- Unwritable log (read-only `logs/` or state directory; task X1): the gate
  runs anyway. The output goes to `logfile::JobLog::Memory`, a buffer
  keeping the last 256 KiB (`MEMORY_LOG_CAP`), which only feeds the `tail`
  of `done`; `Finished::log_error` carries the error, the daemon logs it
  and sends it once in `outcome.log_error` (optional field, omitted
  otherwise), and the client prints it once as `sluice: warning: cannot
  create …: Permission denied …; the output was kept in memory for the
  tail only`. The verdict, the marker and the miss record are unaffected
  (a pass marker is still only written after exit 0). The in-process path
  does the same with the same type. `sluice exec` spools are still
  required (the client replays the spool file), so an unwritable spool is
  a start error.
- Timeout: `gate.timeout` from the moment the process starts (queue wait
  excluded; `0` disables it). On expiry: `killpg(SIGTERM)`, 5 s grace,
  `killpg(SIGKILL)`, outcome `timeout`. If the leader exits but a
  grandchild keeps the pipes open, the same deadline applies to the pumps
  and the group is SIGKILLed, so `wait` never hangs.
- Cancel (last subscriber gone with `cancel`, `stop --force`): same kill
  sequence, outcome `killed`; the log footer says `(killed)`. Under
  `finish`, a job whose last subscriber left keeps running (or stays
  queued and still runs later).
- A `run` with `no_marker` (queue-only gate) skips the pre-start marker
  double-check and the whole bookkeeping below: no marker, no miss record.
- After exit 0 with a key: re-snapshot the tree with `probe_env` from the
  request (`tree::snapshot`, scratch dir from the daemon's own
  `XDG_RUNTIME_DIR`); unchanged → `MarkerStore::write` (`cmd` normalised,
  `committed_tree` taken from the request, `sluice_version` the daemon's),
  `prune`; changed → miss record `the gate modified the tree`,
  `not_cached_reason`. Non-zero → miss record `gate failed` with the exit
  code; timeouts `timed out`; cancellations `killed`. The gate environment
  is the request's `child_env` with `gate.env` laid over it.
- The `tail` in `done` is the last `cache.tail_lines` lines of the log (a
  spool is decoded first, streams merged; `logs` does the same).
- Log retention: on start and daily (from the heartbeat task, so it stops
  if the lease is lost), delete job logs (`<id>.log`, and `<id>.exec` for
  exec jobs) older than
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

**Migrations (0.3.0, ADR-0013 G9, ADR-0011 section 7.4).** `schema.version`
stays 1 so 0.1.0/0.2.0 binaries keep opening the file. Additions are
versioned by their own one-row table and applied on open by
`sluice_gate_daemon::migrate`: numbered additive migrations (new tables,
nullable columns) in one `IMMEDIATE` transaction with the version update;
before migrating an existing file a consistent copy is written next to it as
`sluice.db.bak-<version before>` (`VACUUM INTO` a temporary file, fsync,
rename, fsync of the directory; an existing backup is kept only when it opens
read-only and passes `PRAGMA quick_check`, otherwise it is replaced); a newer version refuses a read-write open (`NewerSchema`) while a
read-only open still reads the known columns. `sched_schema` 1 adds:

```sql
CREATE TABLE sched_schema (version INTEGER NOT NULL);       -- 1
-- jobs, all nullable: requested_at, admitted_at, weight_cfg, expected_ms,
--   wait_budget_ms, wait_psi_ms, wait_repo_cap_ms, wait_reserved_ms,
--   wait_order_ms, cpu_ms, run_delay_ms, demand_milli (INTEGER);
--   admission, killed_by (TEXT). `weight` is the effective weight.
CREATE TABLE job_requests (
  id INTEGER PRIMARY KEY, job_id TEXT NOT NULL, caller TEXT NOT NULL,
  session TEXT, client_pid INTEGER, via TEXT,       -- mcp | cli | hook | exec | queue
  requested_at INTEGER, attached_at INTEGER NOT NULL, finished_at INTEGER,
  coalesced INTEGER NOT NULL, detached INTEGER NOT NULL DEFAULT 0);
CREATE INDEX job_requests_job ON job_requests(job_id);
CREATE TABLE gate_stats (                              -- newest 20 per key, passing runs only
  repo TEXT NOT NULL, gate TEXT NOT NULL, cmd_hash TEXT NOT NULL,
  job_id TEXT NOT NULL, finished_at INTEGER NOT NULL, outcome TEXT NOT NULL,
  wall_ms INTEGER NOT NULL, cpu_ms INTEGER NOT NULL, run_delay_ms INTEGER,
  demand_milli INTEGER NOT NULL);
CREATE INDEX gate_stats_key ON gate_stats(repo, gate, cmd_hash, finished_at);
```

Rows written by a downgraded 0.2.0 daemon (its `INSERT` names its columns)
have NULL timeline fields, read as unknown (`sluice logs --timeline`).
Keys of `gate_stats` unseen for 90 days are pruned with the daily log prune.
Only passing runs teach a demand: failed runs write no `gate_stats` row, and
rows of failed runs written by earlier releases are ignored on load
(`WHERE outcome = 'pass'`; alpha review finding 5).

Timestamps are unix milliseconds (integers), never strings. Busy timeouts
map to a typed retryable error, never to a gate failure. The database is
never deleted automatically; a `schema.version` mismatch is a hard error
with a message naming the file. `totals(since)` filters jobs by `queued_at` and events by `at`;
`prune_logs` refuses job ids containing path separators or `..`.

## Client side (`sluice run` on a miss)

1. Phase 1 hit path unchanged (no daemon involved).
2. On a miss with policy `wait`: connect (autostart if needed), send `run`
   with everything precomputed (`cmd` trimmed but not whitespace-normalised,
   `cwd` = worktree root), print `queued`/`started` lines to stderr for
   humans (`sluice: test queued (position 2, 5 of 8 units busy)[, sharing
   job <id>]` — the `U of B` part comes from one extra `status` call and
   is omitted if it fails — then `sluice: test running (job <id>)`),
   stream nothing else unless `-v` (then poll the log file every 500 ms),
   print the `done` summary exactly as Phase 1 does, exit with the mapped
   code. A connection lost after `queued` is final, no fallback: when the
   connection broke (end of file, I/O error) the daemon died and its reaper
   kills the gate, so the client prints `sluice: error: the daemon died
   while running <gate>; it was killed` (exit 143; before `started`:
   `… while <gate> was queued; it did not run`, exit 1); other errors print
   `lost the connection to the daemon: <why>`, exit 1. `killed` prints `KILLED after <s>s (job …)` (`KILLED for lack of
   memory after <s>s (job …)` then the daemon's message when the daemon says it was
   killed for memory, as MCP `verify` does) and exits 143; a job dropped before it started prints `KILLED before it
   started (job <id>): the daemon stopped`.
3. If the daemon cannot be reached or started (or refuses before
   `queued`): `sluice: daemon unavailable (<why>); running in-process`,
   then the Phase 1 in-process run with the `flock` semaphore. Gates run
   this way, or under `run-unqueued` (which takes no slots at all), do not
   appear in `status`, do not consume the daemon's budget and get no
   `nice`/`ionice`. A zero `timeout` means no timeout on both paths. A socket path that can never work
   (an over-long `SLUICE_SOCKET`, an unsafe fallback directory) is a configuration
   error (exit 1), not a reason to fall back.
4. `run-unqueued` policy never touches the daemon.
5. On a hit, send the best-effort `hit` event (20 ms budget) when the
   socket file exists.
6. Inside a Sluice job (`SLUICE_JOB` set) a miss runs in-process right
   away, without the semaphore and without the daemon (client log decision
   `nested`): the parent job already holds the budget, so queueing could
   wait for it forever. The in-process gate processes also carry
   `SLUICE_JOB` (`<job id>`, or `inproc-<pid>` under `SLUICE_DISABLE`).
7. Queue-only gates skip the marker lookup and the agent `fail-fast`
   policy (the hook waits) and always run; to a daemon announcing
   `queue-only` the request carries `no_marker` and the namespaced key
   `sha256("queue-only\0" K)` so simultaneous identical requests coalesce.
   The in-process budget code is shared with `sluice exec`
   (`crates/sluice-gate/src/budget.rs`).

## Properties to test (Phase 2 acceptance)

- Kill the client mid-run with `cancel` policy → gate process group gone within 6 s; with `finish` → marker written.
- Kill the daemon (SIGKILL) mid-run → the reaper kills every gate group (descendants included) within seconds; the client reports the death as a kill; with the reaper gone too, the next daemon start kills the orphaned group and marks the row `killed`; no stale `daemon.lock`/socket blocks the restart.
- Timeout → whole group dead, outcome `timeout`, exit 124 at the client.
- Two daemons cannot run at once (second start exits with a clear message).
- Client fallback when the socket directory is unwritable.
