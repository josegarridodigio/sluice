# Reference: block/agent-task-queue

Source studied: `/home/jose/Escritorio/Proyectos/sluice/refs/agent-task-queue` (shallow clone, single
commit `48d16da`, 2026-08-28, "Merge pull request #36 ... Add inline progress for long-running queued
tasks"; version 0.4.1 per `pyproject.toml:3`). All line numbers below refer to that checkout.

## 1. What it is

A local FIFO queue that serialises expensive commands (builds, test suites, docker) issued by several
AI agents on the same machine, so they stop thrashing CPU/RAM/disk (`README.md:7-16`). It ships as an
MCP server (`task_queue.py`, FastMCP) plus a `tq` CLI (`tq.py`), both sharing one SQLite database
through `queue_core.py` (`queue_core.py:1-7`). Written in Python >= 3.10 with a single runtime
dependency, `fastmcp>=2.14.4` (`pyproject.toml:6-14`). License: Apache-2.0 (`pyproject.toml:11`,
`LICENSE`). Authors: Matt McKenna / Block (`pyproject.toml:7-10`). macOS and Linux only
(`README.md:663-666`).

## 2. Why they moved from a CLI wrapper to MCP

The rationale lives only in the README (the shallow clone has no earlier history to mine):

> The first attempt at solving this problem was a file-based queue CLI that wrapped commands:
> `queue-cli ./gradlew build`. **The fatal flaw:** AI tools have built-in shell timeouts (30s-120s).
> If a job waited in queue longer than the timeout, the agent gave up—even though the job would
> eventually run. (`README.md:670-676`)

"Why MCP solves this" (`README.md:694-698`): the server owns the queued command independently of any
tool request; long calls return a handle within 30 s instead of relying on a long client timeout; the
agent gets incremental inline output and can be steered between status calls; an interrupted status
request does not cancel the command. The comparison table (`README.md:700-705`) contrasts "External
workarounds" vs "Solved by design" for timeouts, filesystem vs SQLite (WAL) storage, and "Wrap every
command" vs "Automatic tool selection".

Note the design went through a second iteration: PR #36 replaced a fully blocking `run_task` with a
bounded 30 s wait plus `task_status` polling (`task_queue.py:65`, `README.md:38-40,134`). So even an
MCP call was considered too risky to hold open for a whole build.

## 3. Architecture

### SQLite schema (`queue_core.py:109-168`)

- `queue` (`queue_core.py:110-125`): `id INTEGER PK AUTOINCREMENT`, `queue_name TEXT NOT NULL`,
  `status TEXT NOT NULL` (only `waiting` / `running`), `pid` (owning server/CLI process),
  `server_id TEXT` (8-char uuid per process instance, `task_queue.py:54`, `tq.py:47`), `child_pid`,
  `command`, `working_directory`, `worktree_root`, `repo_name`, `git_branch`, `agent_name`,
  `created_at` / `updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP`.
- `task_results` (`queue_core.py:129-133`): `task_id INTEGER PK`, `result_json TEXT NOT NULL`,
  `completed_at`. Holds the terminal result after the `queue` row is deleted, so status stays
  queryable without holding a slot (`README.md:523-525`).
- Index: `idx_queue_status ON queue(queue_name, status)` (`queue_core.py:166-168`). Nothing else.
- Migrations are `ALTER TABLE ADD COLUMN` statements that swallow "duplicate column name"
  (`queue_core.py:136-163,196-210`).
- Connection: `sqlite3.connect(timeout=60)`, `PRAGMA journal_mode=WAL`, `busy_timeout=60000`,
  commit on success / rollback on exception (`queue_core.py:172-186`).
- `ensure_db` runs `SELECT 1 FROM queue`; on any `OperationalError` it deletes `queue.db`, `-wal`,
  `-shm` and recreates the schema (`queue_core.py:507-521`).
- Data dir: `$TASK_QUEUE_DATA_DIR` or `/tmp/agent-task-queue` (`queue_core.py:22`), containing
  `queue.db`, `agent-task-queue-logs.json` (NDJSON metrics) and `output/` (`queue_core.py:39-46`).

### Queue semantics

- FIFO within an exact `queue_name`, ordered by `id`: `count_waiting_ahead` counts waiting rows with
  a smaller id (`queue_core.py:404-414`).
- Claim (`attempt_task_start`, `queue_core.py:454-504`): lower `busy_timeout` to 100 ms, `BEGIN
  IMMEDIATE`, compute `waiting_ahead`, call `can_acquire_task`, then
  `UPDATE queue SET status='running', updated_at=?, pid=? WHERE id=? AND status='waiting'`; the task
  starts iff `rowcount > 0`. Returns `(started, position)`; `busy_timeout` restored in `finally`.
- Capacities (`can_acquire_task`, `queue_core.py:417-451`): each exact queue has capacity 1 unless
  overridden. If the exact queue is itself a configured scope, running tasks are counted across the
  scope *and its descendants*; otherwise only in the exact queue. Then every configured ancestor
  scope clamps `available_slots = min(available, cap - running_in_scope)`. Start is refused when
  `available_slots <= 0` **or** `waiting_ahead >= available_slots`, which is what keeps FIFO inside a
  child queue even when a parent has spare slots (test `tests/test_queue.py:449-514`).
- "Hierarchical capacities": names are `/`-separated; `queue_scopes("gradle/emu-5557")` yields
  `["gradle", "gradle/emu-5557"]` (`queue_core.py:328-332`); scope membership uses
  `LIKE 'scope/%' ESCAPE '\'` with manual escaping (`queue_core.py:377-392`). Capacities come from
  repeatable `--queue-capacity SCOPE=N` flags (N >= 1, `queue_core.py:300-325`), are process-local
  and never persisted; every server/CLI sharing the DB must pass the same flags
  (`README.md:64-67,414-415`). Fairness across sibling queues is "best-effort"; FIFO is only
  guaranteed within an exact queue (`README.md:332-333`).
- Waiting is pure polling: every `POLL_INTERVAL_WAITING` (1 s, env `TASK_QUEUE_POLL_WAITING`,
  `queue_core.py:23`) the waiter runs cleanup then `attempt_task_start`; "database is locked" is
  swallowed and retried (`task_queue.py:379-419`, `tq.py:336-390`). No wake-up signal between
  processes.
- Completion deletes the `queue` row (`task_queue.py:422-431`, `queue_core.py:730-736`); the terminal
  dict is written with `INSERT OR IGNORE` so the first writer wins, which is how an explicit cancel
  beats a racing completion (`task_queue.py:444-456`).

### How running tasks are tracked

- DB side: `pid` + `server_id` identify the owner; `child_pid` is written right after spawn
  (`task_queue.py:732-733`, `tq.py:496-499`).
- In-process (server): `_active_task_ids` set guarded by a `threading.Lock` (`task_queue.py:58-59`)
  and `_background_tasks: dict[int, asyncio.Task]` (`task_queue.py:63`). The whole lifecycle
  (wait for turn, execute, store result, release slot, prune outputs) runs in one background asyncio
  task named `queued-task-<id>` (`task_queue.py:850-954`), deliberately detached from the MCP request
  (`task_queue.py:61-62`).
- Subprocess: `asyncio.create_subprocess_shell(command, cwd, env, stdout=PIPE, stderr=PIPE,
  start_new_session=True)` (`task_queue.py:724-731`). Env is `os.environ` plus `KEY=v,KEY2=v2`
  pairs (`task_queue.py:1045-1050`).
- Origin metadata: `git -C <wd> rev-parse --show-toplevel --git-common-dir HEAD
  --symbolic-full-name HEAD` with a 2 s timeout gives worktree root, repo name (worktree-aware via
  the common dir) and branch or short SHA (`queue_core.py:213-289`). `agent_name` falls back to the
  FastMCP `client_id` (`task_queue.py:1052`).

### Output files and the failure tail

- Two files per task: `output/task_<id>.log` (formatted) and `task_<id>.raw.log` (raw interleaved
  stdout+stderr) (`task_queue.py:719-720`, `README.md:568-570`). Formatted header: `COMMAND:`,
  `WORKING DIR:`, `STARTED:`, then the compatibility markers `--- STDOUT ---` / `--- STDERR ---`
  (`task_queue.py:736-741`); footer `--- SUMMARY ---`, `EXIT CODE: <n>` (or
  `TIMEOUT (killed after Ns)` / `CANCELLED`), `DURATION: <s>` (`task_queue.py:779-781,808-810,818-820`).
  The IntelliJ plugin detects failure by grepping that `EXIT CODE` line (`intellij-plugin/README.md:35`).
- Both pipes are drained by two concurrent asyncio tasks in 8192-byte chunks through an incremental
  UTF-8 decoder (`errors="replace"`), flushing both files per chunk and appending lines to a
  per-stream `deque(maxlen=TAIL_LINES_ON_FAILURE)` (`task_queue.py:713-714,744-769`). A 2 MB stderr
  regression test exists (`tests/test_queue.py:758-775`).
- Tail = last 50 lines (`--tail-lines`, `task_queue.py:94-99`) of **stderr if any, else stdout**
  (`task_queue.py:782-783,837-838`); `"(no output)"` placeholder; `tail` is `None` on success
  (`task_queue.py:846`).
- Inline incremental output (`_read_new_output`, `task_queue.py:459-480`): read `raw.log` from
  `output_offset`, cap at `MAX_INLINE_OUTPUT_BYTES = 16 KiB` (`task_queue.py:68`) with a
  `[... N earlier output bytes omitted ...]` prefix; an offset past EOF resets to 0; returns the new
  offset (= file size) and seconds since last mtime.
- Retention: keep the newest 50 formatted logs by mtime (`--max-output-files`), delete both files of
  older tasks, and prune `task_results` to the same count (`task_queue.py:258-301`). Output files for
  a reused id are unlinked at registration (`task_queue.py:351-360`).
- Metrics: NDJSON appended to `agent-task-queue-logs.json`, rotated once to `.json.1` above 5 MB
  (`queue_core.py:610-635`). Events: `task_queued`, `task_started` (wait seconds), `task_completed`
  (exit code, duration, stdout/stderr line counts, RSS before/after), `task_timeout`,
  `task_cancelled`, `task_error`, `zombie_cleared`, `orphan_cleared`.

### Zombie / orphan cleanup

Core pass per queue name (`cleanup_queue`, `queue_core.py:639-727`):
1. `running` rows whose `pid` is not a live task-queue process: `kill_process_tree(child_pid)`,
   delete row, metric `zombie_cleared reason=parent_died`.
2. `waiting` rows with a dead owner: delete, `orphan_cleared reason=waiting_parent_died`.
3. `running` rows whose `updated_at` is older than `max_lock_age_minutes` (default 120,
   `--lock-timeout`): kill child tree, delete, `zombie_cleared reason=timeout`
   (`queue_core.py:708-727`). This fires regardless of whether the child is alive.

Liveness heuristic `is_task_queue_process` (`queue_core.py:536-585`): `os.kill(pid, 0)`, then
`ps -p PID -o args=` (5 s timeout) and accept if either of the first two argv basenames is one of
`task_queue`, `task_queue.py`, `agent-task-queue`, `tq`, `tq.py`, `pytest`, or the cmdline contains
`task_queue`/`agent-task-queue`. Any exception means "assume alive" to avoid false kills.

Server-level extra pass (`task_queue.py:187-254`): rows with our pid but a different `server_id`
(PID reuse after restart) and rows with our pid+server_id that are not in `_active_task_ids`
(clients that disconnected without cleanup, e.g. cancelled sub-agents, `tests/test_queue.py:1301-1302`)
both get their child killed and the row deleted (`orphan_cleared reason=stale_server_instance` /
`not_in_active_set`). Cleanup targets include every queue under any configured ancestor scope so a
stale sibling cannot keep a parent scope full (`queue_core.py:350-374`, `tests/test_queue.py:1404-1462`).

Cleanup runs inline at registration and on every 1 s poll (`task_queue.py:341,389`, `tq.py:471,352`);
there is no reaper thread.

Signals: `kill_process_tree` sends **SIGTERM to the process group** (possible because of
`start_new_session=True`), falling back to `kill(pid)` (`queue_core.py:588-600`). For its own child
the server waits 5 s then SIGKILLs the group, and SIGKILLs the group anyway if the shell exited early
so signal-resistant descendants die too (`_terminate_process`, `task_queue.py:661-682`). For a task
owned by another server: SIGTERM, sleep 1 s, SIGKILL (`task_queue.py:685-699`). `tq` on
SIGINT/SIGTERM: SIGTERM group, 5 s, SIGKILL, release row, exit 130 (`tq.py:437-465`); on timeout it
SIGKILLs immediately and returns 124 (`tq.py:503-521`). SIGURG is never mentioned.

### Lock handling

There is no file lock; SQLite is the lock. Claims use `BEGIN IMMEDIATE` with a 100 ms busy timeout
(`queue_core.py:469-471`), everything else uses the 60 s busy timeout. The README's "Database is
locked" remedy is `rm -rf /tmp/agent-task-queue/` (`README.md:639-644`); `tq clear` just runs
`DELETE FROM queue` after a y/N prompt and does not kill children (`tq.py:180-186`).

### How the MCP tool blocks

`run_task` registers the row, spawns the background task, then `_wait_for_initial_result` polls the
snapshot every 0.25 s (`STATUS_POLL_INTERVAL_SECONDS`, `task_queue.py:66`) until the task is terminal
or `wait_seconds` (0-30, default 30, `task_queue.py:65,957-960`) elapses, emitting
`ctx.report_progress` every 5 s (`task_queue.py:67,606-634`). `task_status` returns immediately if
the task is terminal or there is new output, otherwise polls at 0.25 s until output, a state change,
completion or the deadline (heartbeat) (`task_queue.py:637-658`). Because the command runs in a
separate asyncio task, a cancelled request only aborts the wait (`tests/test_queue.py:639-716`).
`cancel_task` stores the `cancelled` result first, then cancels the background task (which triggers
`_terminate_process`), or, if another server owns it, kills the recorded `child_pid` group and
deletes the row (`task_queue.py:1108-1156`).

## 4. MCP tool surface (`task_queue.py:964-1171`)

| Tool | Parameters | Annotations |
|---|---|---|
| `run_task` | `command: str`, `working_directory: str` (absolute), `queue_name="global"`, `timeout_seconds=1200`, `env_vars=""` (`K=v,K2=v2`), `agent_name=""`, `wait_seconds=30` | destructive, not idempotent, closed world (`:964-971`) |
| `task_status` | `task_id: int`, `output_offset=0`, `wait_seconds=30` | read-only, idempotent (`:1068-1075`) |
| `cancel_task` | `task_id: int` | destructive, idempotent (`:1100-1107`) |
| `clear_task_logs` | none | none; returns `Deleted N log file(s) from <dir>` (`:1159-1171`) |

Validation failures return a bare string `ERROR: ...` (`task_queue.py:1031-1043,1089-1094`).
Otherwise the result is a `ToolResult` with a text block and `structured_content={"result": {...}}`
(`task_queue.py:600-603`). Text formats (`task_queue.py:543-598`):

- `SUCCESS task_id=8 exit=0 31.2s command=<cmd> output=<path>.log`
- `FAILED task_id=9 exit=1 12.5s command=<cmd> output=<path>.log` + newline + tail
- `TIMEOUT task_id=N killed after <t>s command=<cmd> output=<path>` + tail
- `CANCELLED task_id=N after <d>s command=<cmd> output=<path>`
- `ERROR task_id=N: <error> output=<path>` / `ERROR task_id=N: Task is not active and no terminal result is available`
- `QUEUED|RUNNING task_id=N queue=<q> elapsed=<s>s [position=<p>] [process_alive=true|false]
  [last_output=<s>s_ago]` then `--- NEW OUTPUT ---` or `(no new output)`, then
  `Task continues in the background. Do not rerun it. Call task_status(task_id=N, output_offset=M)
  for the next update, or cancel_task(task_id=N) to stop it.`

Structured snapshot keys: active rows carry `task_id, status (queued|running), queue_name,
queue_position, elapsed_seconds, command, working_directory, process_alive, output_file,
new_output, next_output_offset, last_output_seconds_ago` (`task_queue.py:507-524`); terminal rows
carry `task_id, status (success|failed|timeout|cancelled|error), exit_code (None on timeout),
duration_seconds, [timeout_seconds], command, output_file, tail, [error]` plus the three output
fields (`task_queue.py:532-540,794-803,839-847`). The `run_task` docstring is prompt engineering:
"tell the user the exact command", "Do not rerun the command", and a "YOU MUST USE THIS TOOL" list of
build/container/package/test commands (`task_queue.py:981-1030`).

Server flags: `--data-dir`, `--max-log-size` (5 MB), `--max-output-files` (50), `--tail-lines` (50),
`--lock-timeout` (120 min), `--queue-capacity SCOPE=N` repeatable (`task_queue.py:72-116`).

## 5. `tq` CLI surface (`tq.py:573-654`)

Global options: `--data-dir`, `--queue-capacity SCOPE=N` (repeatable). If the first positional is not
a known subcommand, `run` is inserted (`tq.py:615-640`), so `tq ./gradlew build` works.

- `tq run [-q/--queue global] [-t/--timeout 1200] [-C/--dir .] COMMAND...` (`tq.py:596-600`). The
  argv is re-joined with `shlex.join` and executed with `shell=True` in a new session and
  **passthrough stdio** (no pipes, no output files, no `task_results` row; `tq.py:483-499`). Prints
  `[tq] Task #N queued in 'q'`, `[tq] Position #P in queue. Waiting...` on change, `[tq] Lock
  acquired [after Xs wait]`, `[tq] Running: ...`, `[tq] Directory: ...`, a 60-dash rule, then
  `[tq] SUCCESS in Xs` or `[tq] FAILED exit=N in Xs`. Exit code is the child's (`tq.py:646`,
  `tests/test_tq_cli.py:230-241`), 124 on timeout, 130 on SIGINT.
- `tq list [--json]`: text groups rows by queue as `[q] (N task(s))` and `  🔄|⏳ #id STATUS
  (pid=, child=) @ HH:MM:SS` (`tq.py:129-149`); JSON is
  `{"tasks":[{id,queue_name,status,command,pid,child_pid,created_at,updated_at}],"summary":{total,running,waiting}}`
  (`tq.py:85-115`). Reads only `queue`, never `task_results`.
- `tq logs [-n 20] [--json]`: last N NDJSON entries formatted per event (`tq.py:223-255`) or
  `{"entries":[...]}`.
- `tq clear [--json]`: `Clear N task(s) from queue? [y/N]` then `Cleared N task(s) from queue`;
  `--json` skips the prompt and prints `{"cleared":N,"success":true}` (`tq.py:155-193`).

## 6. Ideas for Sluice

### Take

- Bounded tool wait (<= 30 s) that returns a handle, plus offset-based incremental output and a
  heartbeat: this is their answer to agent-side timeouts and it is proven in Claude Code
  (`task_queue.py:65-68,621-658`).
- Decouple the request from the job: the job outlives a cancelled tool call; only an explicit
  `cancel` kills it (`task_queue.py:61-63`, `README.md:136`).
- Full output to a file, only a bounded tail in the tool result, plus a 16 KiB cap on inline output
  with an "omitted bytes" marker: cheap tokens, nothing lost (`task_queue.py:68,459-480`).
- Text plus `structured_content` in every result: humans grep the text, agents read the JSON
  (`task_queue.py:600-603`).
- Process group per job (`setsid`), SIGTERM -> 5 s -> SIGKILL, and SIGKILL the group even after the
  shell exits (`task_queue.py:661-682`).
- Drain stdout and stderr concurrently and keep a raw interleaved log (`task_queue.py:744-769`).
- Claim in one `BEGIN IMMEDIATE` transaction with a tiny busy timeout and a `waiting_ahead >=
  available_slots` guard that preserves FIFO under shared caps (`queue_core.py:417-504`).
- Separate "active" rows from "terminal results" so status survives slot release and keep the
  newest N results (`queue_core.py:128-134`, `task_queue.py:292-301`).
- Record worktree root / repo / branch per job with a 2 s-bounded `git rev-parse`
  (`queue_core.py:213-289`); Sluice already keys on the tree, so expose it in status output.
- "Clean before claim" including sibling queues under a shared scope, so one dead runner never
  wedges a parent cap (`queue_core.py:350-374`).
- Tool docstring guidance ("do not rerun", "tell the user the command") and the documented
  Claude Code permission pitfalls (section 7).

### Change or reject

- No daemon: each MCP server/CLI process owns its jobs and coordinates through SQLite polling at 1 s
  with `ps`-based liveness heuristics (`queue_core.py:536-585`). Sluice's single per-user daemon
  removes PID-reuse checks, stale-lock timeouts and polling; use notify/condvar instead.
- Capacities are process-local CLI flags that every participant must repeat identically
  (`README.md:64-67`); Sluice should own them in the daemon (and repo config).
- Slot counts only: no weights, no budget, no load awareness. Sluice adds weights/budget and PSI.
- No coalescing and no cache: the README demo shows agent B re-running the same build after A
  (`README.md:92-110`); terminal results are keyed by `task_id` only. Sluice's per-tree cache and
  request coalescing are the whole point.
- Fairness is FIFO per exact queue and best-effort across siblings (`README.md:332-333`); no
  priority or aging. Sluice should at least coalesce before ordering.
- `ensure_db` deletes the database on any `OperationalError` (`queue_core.py:507-521`) and the docs
  recommend `rm -rf` (`README.md:643`); do not auto-destroy state.
- Stale-lock reaper kills any job older than 120 min even if alive (`queue_core.py:708-727`); use
  per-gate timeouts on daemon-owned children instead.
- Tail picks stderr-or-stdout, not the interleaved stream (`task_queue.py:837-838`); tools like
  gradle/pytest mix channels, so take the tail of `raw.log` instead.
- `tq run` bypasses output capture and results entirely (`tq.py:483-499`), so CLI and MCP paths
  behave differently; in Sluice both must go through the daemon with identical semantics.
- `tq clear` deletes rows without killing children (`tq.py:185`); never do that.
- Timestamps mix `CURRENT_TIMESTAMP` (UTC) with `datetime.now().isoformat()` (local) and compare them
  as strings (`queue_core.py:123-124,486-488,709`); store epoch integers.
- `/tmp/agent-task-queue` is shared by every user on the host (`queue_core.py:22`); use a per-user
  runtime dir and socket permissions.
- Free-form `shell=True` command strings (`task_queue.py:724`); Sluice runs named, configured gates,
  which is both safer and what makes caching keyable.

## 7. Pitfalls they document

- Agent shell timeouts of 30-120 s make a blocking CLI wrapper unusable (`README.md:676`).
- Claude Code / Cursor: any `Bash(gradle:*)`, `Bash(./gradlew:*)`, `Bash(pytest:*)`... allow rule
  makes the agent use Bash and "bypass the queue entirely"; check both `settings.json` and
  `settings.local.json`, project and global (`README.md:424-434`, `examples/claude-code/SETUP.md:27-32`,
  `examples/README.md:11`). Claude Code must be restarted after the change (`SETUP.md:58`).
- The agent needs a `Read(/tmp/agent-task-queue/*)` permission to open the log files
  (`examples/claude-code/SETUP.md:15-24`).
- Their integration test forces the tool with `--allowedTools mcp__agent-task-queue__run_task` and
  a system prompt "Do not use the regular Bash tool" (`integration_test.sh:18-19,85-88`).
- Interrupting `run_task`/`task_status` does not stop the command; only `cancel_task` does
  (`README.md:136,356-358`). Cancelled sub-agents left orphan rows, hence the "not in active set"
  cleanup (`task_queue.py:226-227`, `tests/test_queue.py:1298-1302`).
- Agents re-run commands they think failed; every running response says "Do not rerun it"
  (`task_queue.py:591-595,988`).
- MCP progress notifications are optional and must not break execution; results never depend on the
  client rendering them (`README.md:353-354`, `task_queue.py:614-618`).
- A full stderr pipe deadlocks the child unless both pipes are drained concurrently
  (`README.md:569`, `task_queue.py:739-740`).
- Exit codes: `tq` propagates the child's code, 124 on timeout, 130 on SIGINT (`tq.py:463,521,646`);
  the MCP result reports `exit_code` (`None` on timeout, `task_queue.py:797`). `test_exit_code_preserved`
  checks `EXIT CODE: 42` in the log (`tests/test_queue.py:833-851`).
- PID reuse after a restart and SQLite id reuse after a DB recreation both produced wrong matches;
  hence `server_id` and the unlink of stale output files (`task_queue.py:52-53,351-352`).
- "Database is locked": look for zombie `task_queue` processes or delete the data dir
  (`README.md:639-644`).
- SIGURG, the Claude Code Bash tool's own timeout value, and Go-style signal issues are **not**
  mentioned anywhere in the repo (grep over `*.md`, `*.py`, `*.sh`, `*.json`).
