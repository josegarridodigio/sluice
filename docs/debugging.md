# Debugging Sluice

## Where everything is logged

All paths are under the state directory, `$XDG_STATE_HOME/sluice` or
`~/.local/state/sluice`.

| File | What it holds | Retention |
|---|---|---|
| `logs/<job-id>.log` | Output of one gate run (`sluice logs [job-id]`) | `[daemon] log_retention_days` (7) |
| `daemon.log` | The daemon's own tracing output (stdout and stderr of the detached daemon) | Rotated above `[debug] daemon_log_max_mib` (20 MiB): `daemon.log.1` to `.3` |
| `logs/mcp-<pid>.log` | Tracing output of one `sluice mcp` process (also on its stderr; stdout is the MCP channel and is never written) | `[daemon] log_retention_days`, pruned by the daemon |
| `logs/client.log` | One line per `sluice run`, hit or miss, hook or not | Rotated above 5 MiB to `client.log.1` (one generation) |

`client.log` lines look like this:

```
2026-10-22T10:00:00Z pid=4242 caller=agent hook=pre-commit gate=test tree=0123456789ab key=abcdef012345 decision=hit exit=0 ms=12 cwd=/home/u/repo
```

`caller` is `agent` or `human`; `hook`, `tree` and `key` (the first 12 hex digits
of the cache key the run looked up) are `-` when unknown. Two lines with the
same `key` asked the same question, so a hook that missed where an earlier
run hit shows a different `key`;
`decision` is `hit`, `miss-run` (queued in the daemon), `miss-fail-fast`
(refused by `agent_hook_policy`, exit 3), `coalesced` (shared a job another
caller started), `in-process` (ran without the daemon), `disabled`
(`SLUICE_DISABLE`) or `error`. Writing the line never fails the command.

The daemon logs the same facts for every `run` request that comes from a hook,
at `info`, in `daemon.log`: `hook run request: gate=test hook=pre-push
tree=0123456789ab key=abcdef012345 caller=agent decision=queued` (`decision` is
`queued`, `coalesced` or `hit by the double-check`). A hook run by the daemon's
own push of a merge train is answered from the train and logged as `push hook
vouched: train=<id> gate=<gate> verdict=verified` (or `verdict=refused: <why>`
for a gate the train did not verify). Comparing the `key` of a hook that missed
with the `key` of the run that passed tells whether the two asked the same
question.

When the cache was off for the run, `reason=<code>` follows `key=-`:

```
2026-10-22T10:00:00Z pid=4242 caller=agent hook=- gate=test tree=- key=- reason=skip-worktree decision=miss-run exit=0 ms=9100 cwd=/home/u/repo
```

The codes are `skip-worktree` (a skip-worktree or assume-unchanged index
entry), `unmerged`, `submodules`, `git-error` (git failed while hashing the
tree), `snapshot-failed`, `key-failed`, `cache-read-failed`, `disabled`
(`cache = false` in the user config, `CI` set, `SLUICE_DISABLE`) and
`queue-only`. The field is absent when the cache was used.

## The cache is off for this tree

A tree Sluice cannot hash is never cached: every run executes the gate again.
The usual cause is an index entry flagged `skip-worktree` or `assume-unchanged`
(sparse checkouts, or `git update-index` to silence a local change). `sluice
run` says so once on stderr, with or without `-v`, in hooks too:

```
sluice: cache off for this tree: skip-worktree or assume-unchanged entry: web/config.local.ts
```

The `verify` MCP tool puts the same reason on the gate's line (`PASS 12.3s (job
...) (not cached: tree unknown: skip-worktree or assume-unchanged entry: ...)`)
and in `not_cached_reason`. `sluice doctor` has a `cache` check that lists up
to 5 flagged paths (`warn`) and the way out:

```
[warn] cache          1 index entry flagged skip-worktree or assume-unchanged (web/config.local.ts): the cache is disabled for this worktree and every gate runs uncached; clear the flag with 'git update-index --no-skip-worktree <path>' or 'git update-index --no-assume-unchanged <path>'
```

`git ls-files -v` shows the flags: `S` is skip-worktree, a lowercase letter is
assume-unchanged.

## What the session hook decided

The agent plugin's `SessionStart` hook never shows anything to you, so to see
what it did:

- `logs/client.log`: each run of the hook that reaches the binary writes a line
  with `hook=session-start`, `caller=agent`, `gate=-`, the session's `cwd` and a
  `decision`: `rules` (the repository has a `.sluice.toml`; the rules were given
  as context), `init-offer` (a git repository without one; the agent was told to
  propose `sluice init`) or `non-git`, followed by `contained=full`, `partial`, `off` or `timeout` (cgroup containment of the session; `timeout` means it did not finish in the hook's budget). No such line after starting a session
  means the hook did not run (plugin not installed or disabled), the binary was
  not found or is older than the plugin (the wrapper script then only tells the
  agent, and writes nothing), or Sluice failed open.

  ```sh
  grep 'hook=session-start' ~/.local/state/sluice/logs/client.log | tail -3
  ```

- The env file: the hook appends one guarded line to the file Claude Code
  names in `CLAUDE_ENV_FILE` (one per session). From inside the session,
  `echo "$PATH" | tr : '\n' | head -3` must show the shim directory first.
- `sluice shim status`: the shim directory, whether it is in `PATH` and in
  which position, each shim `current`, `stale` or `missing`, the real binary
  it resolves to, and what this shell would do (queue or run directly).
- `sluice doctor`: the `shims` check (installed, current, first in `PATH`)
  and the `plugin` check (plugin and binary versions agree).

Cases: shims are not first in `PATH` (a shell that resets it: `env -i`, a
login shell; or a command by absolute path); `SLUICE_SHIMS=0` is set; the
plugin is older than the binary (update the plugin); or the binary is missing
or older than the plugin, in which case the agent was told to ask you before
installing it.

## Raising the log level

Precedence: the `SLUICE_LOG` environment variable, then `[debug] level` in
`~/.config/sluice/config.toml`, then `info`. The value uses `RUST_LOG`
syntax (`debug`, `sluice_gate_daemon=debug,info`). It applies to the daemon,
`sluice mcp` and the messages `sluice run` prints on stderr.

```toml
[debug]
level = "debug"
daemon_log_max_mib = 20
```

The daemon reads the level at start. To change it without editing anything:

```
sluice daemon restart --debug      # debug for that daemon instance only
sluice daemon restart              # back to the configured level
sluice daemon restart --force      # also kill running gates
```

Without `--force`, restart refuses while gates are running. `sluice status`
shows the daemon's active level and the log path (`log:  level debug, ...`).
An MCP server picks up its level when the agent starts it.

## Why did a job wait? `sluice logs --timeline`

```
sluice logs --timeline [JOB_ID] [--since 10m]
```

Prints one line per job from the daemon's database (it works without a
running daemon): `<job> <gate> <repo>: requested +0.0 queued +0.0 admitted
+31.2 (over) started +31.3 done +43.9 pass; waited budget 29.0s,
order 2.2s; demand 6.1/8; 3 request(s)`. Times are seconds after the job was
queued; `admitted` carries the kind of admission when it was not within the
budget (`over`: above it, by over-admission; `guard`: by the PSI starvation
guard); `waited` splits the queue time by cause (`budget`, `psi`, `repo-cap`,
`head-reserved`, `order`); `demand 6.1/8` is the measured CPU demand against
the configured weight. Without a job id it lists the jobs queued within
`--since` (default `10m`; exit 2 when the duration is invalid).

## `sluice doctor`

```
sluice doctor [--json] [--bundle [PATH]]
```

Checks, each `ok`, `warn` or `fail` with a one-line detail: git is found and
at least 2.31; the PSI files are readable; the state and runtime directories
exist (the runtime directory must be mode 0700); `socket-path`: the socket
path fits the 107-byte Unix socket limit, or the short fallback is in use
(see below), or it is too long; the daemon socket answers
and the daemon runs the installed version (otherwise: `run 'sluice daemon
restart'`); the lease row matches the live daemon pid; `database`: the
daemon's `sluice.db` opens (read-only, as `doctor` reads it) with the
expected schema, otherwise `fail` with the SQLite error (a daemon that
cannot open it does not start, and every gate then runs in-process; move
the file aside and the next daemon creates a fresh one, losing the job
history); the user config parses; inside a repository, `.sluice.toml`
parses, a hook in the hooks directory (`core.hooksPath` or `.git/hooks`)
calls sluice (with husky v9, where `core.hooksPath` is `.husky/_`, the scripts
that runner executes, `.husky/<hook>`, are the ones read), `cache`: a warning
naming up to 5 skip-worktree or assume-unchanged index entries, which disable
the cache for the worktree (see above), `queue-only-hooks`: a warning when a `mode = "queue-only"`
gate lists hooks, `scoped-gates`: one line per path-scoped gate, a warning
when `paths` matches no file or a well-known build input (`Cargo.toml`,
`package.json`, lockfiles, `Makefile`, `Dockerfile`, ...) or a symlink target
lies outside the scope, and `markers`: the marker directory size, `fail` when the
directory is not writable (passes are then never cached), `warn` with the
count and the first path when marker or miss files are damaged (truncated,
not valid JSON, or recorded under a key other than their file name; runs
ignore and replace them, `sluice cache clear` removes them); the `sluice`
found in `PATH` is the binary you are running; `shims`: the shims are installed in
the stable directory, current, and first in `PATH` for this shell (a warning
in an agent shell when they are not; in a human shell an `ok` that says the
agent plugin puts them in `PATH` for agent sessions); `plugin`: the version of
the installed agent plugin (from `installed_plugins.json`) against the binary,
a warning when they differ, never a failure; `weights`: a warning when a gate
declares a number well below (more than 50 % under) what its measured runs
need, suggesting a higher number or `weight = "auto"`; `init`: in a git
repository without `.sluice.toml`, an `ok` that suggests `sluice init
--dry-run`; on a containment host, `gates-memory-policy` (the oomd policy of the
gates' scope and whether `daemon/` carries `user.oomd_omit`) and `memory-guard`
(whether the opt-in guard is on, and a suggestion to turn it on where oomd cannot
act). It exits 1 when any check
fails and never uses the network (it does not look for new releases;
`sluice update --check` does).

## Damaged or read-only storage

- A read-only `logs/` (or state) directory never stops a gate: it runs, its
  output is kept in memory for the failure tail only, and the run prints
  once `sluice: warning: cannot create <log>: Permission denied (os error
  13); the output was kept in memory for the tail only`. `sluice logs`
  has nothing to show for that job.
- A read-only markers directory: the gate's verdict is returned, the pass
  says `(not cached: cannot write the marker: …)`, and `doctor` fails the
  `markers` check.
- If the daemon is killed (SIGKILL, OOM), its gate reaper (`sluice daemon
  reaper --daemon <pid>`, started with the daemon) terminates every gate it was running,
  descendants included, within about 5 s; waiting clients print `the daemon
  died while running <gate>; it was killed` and exit 143. The next client
  starts a new daemon, which also kills any gate group a dead daemon left
  behind and records those jobs as `killed`.

## Where the daemon lives (containment)

On a host with systemd 252 or later and a reachable user manager, the daemon
puts itself in its own transient unit and runs each gate or `exec` job in a
leaf of it (ADR-0017):

```
sluice.slice
|-- sluice-daemon.scope     daemon/ (the daemon and its reaper), jobs/<job-id>/
`-- sluice-agents.scope     holder/, s-<session-id>/{ui,tools}
```

See the units with `systemctl --user status sluice-daemon.scope
sluice-agents.scope`, or `systemd-cgls --user-unit sluice-daemon.scope`.
`sluice daemon holder` is the small process that keeps the agents scope alive
while the daemon runs. If anything fails (no user manager, cgroup files not
writable, a unit that cannot be created) the daemon logs one line in
`daemon.log` (`containment: off (<reason>)` or `cannot place the daemon`) and
runs exactly as before; `[containment] enabled = false` does the same.

Kills by the kernel OOM killer or `systemd-oomd` on those cgroups are
recorded by the daemon and shown in the `status` reply (`interventions`).

### Interventions

`sluice status` lists the last interventions (time, session, leaf, cause) and
`sluice doctor` explains why a session is or is not contained (`session`,
`memory-policy`, `slice`). Causes: `oom_kill` and `oom_group_kill` (the kernel
OOM killer acted inside that cgroup), `oomd_memory_pressure` and `oomd_swap`
(`systemd-oomd` killed it), `sluice_memory_guard` (Sluice's own opt-in memory
guard cancelled a gate job, see below). The daemon also appends each one to
`interventions.jsonl` in the state directory (kept to the newest 200 lines).
The agent is told about an intervention in its own session once: the next MCP
`verify` or `status` result and the next context note of the guard add the
line

```text
Sluice stopped s-abc/tools in your session at 2026-10-24 10:02:11 UTC: memory pressure on the machine (systemd-oomd, memory pressure); the machine was protected. Re-run the command; if it is heavy, run it through `verify`/the queue.
```

Which interventions were told is remembered per session in
`interventions-seen/<session>` under the state directory; delete the file to
hear them again. Nothing is told after six hours.

### A gate killed for lack of memory

When the kernel's OOM killer or `systemd-oomd` ends a gate job, the result is
not an ordinary failure and not "the daemon stopped": the job is `killed`,
`killed_by` is `memory`, and every client prints the daemon's message, for
example

```text
test was killed for lack of memory after 41.2s (job peak 18.1 GiB; 1.4 GiB available at the end; 14.2 GiB used outside gates). The machine did not have the memory this gate needs: run it again unchanged once memory is free (Sluice waits for it), or tell the human; lower its concurrency only if the gate's peak depends on it
```

`sluice run` and `sluice exec` print it after the headline `KILLED for lack of
memory after <secs>s (job <id>)`, MCP `verify` uses the same headline with the
message on the gate's line and in `message` (with `killed_by: "memory"`), and a
merge train fails with `gate <name> was killed for lack of memory` and
`killed_by` in the job's `detail`. The job peak is `memory.peak` of the job's
cgroup and includes page cache; the available memory is `MemAvailable` when the
job ended. The decision needs evidence outside the exit code: the job's own OOM
counters (`oom_kill`, `oom_group_kill` in `memory.events`) or a `systemd-oomd`
line in the journal naming `jobs/<id>`; a gate that merely exits 137 stays an
ordinary failure. The same event is an intervention (`sluice status`, leaf
`jobs/<id>`). Do not retry blindly: lower the suite's parallelism (for example
fewer test workers) or run it when the machine has more free memory.
`sluice doctor` (`gates-oom`) shows the `oom_score_adj` the gates run at and
warns when an ancestor of the jobs' cgroup has `memory.oom.group = 1`, which
would widen a kill to that whole cgroup.

Where `systemd-oomd` is active and systemd is 257 or newer, it looks at the gates
before the agents: `sluice-daemon.scope` has its own memory pressure policy (the
agents' limit, half the duration, rounded up to whole seconds), and `daemon/` has
`user.oomd_omit=1` so the daemon is never the victim. `sluice doctor`
(`gates-memory-policy`) prints the figures and warns when `daemon/` lacks the
attribute; `oomctl` lists both scopes, and `sluice status` shows
`gates memory policy: systemd-oomd at 30% for 8s`. Without oomd (Debian and Arch
by default) or below systemd 257 that policy does not exist: the kernel's
preference and memory admission remain, and `sluice doctor` (`memory-guard`)
suggests the opt-in guard.

#### The memory guard cancelled my job

With `[admission] memory_guard = true` (off by default) the daemon cancels its own
running job with the largest working set when, for 3 seconds in a row, `MemAvailable`
is below `memory_reserve`, the Sluice jobs together hold more than the reserve and
memory stalls (`full avg10` of 10 % or more, or `MemAvailable` below half the
reserve). The job ends `killed` with `killed_by = "memory"` and a message starting
`<gate> was cancelled by Sluice for lack of memory: job working set 18.0 GiB,
MemAvailable 1.1 GiB below the reserve 3.0 GiB`; `sluice status` lists an
intervention with cause `sluice_memory_guard` and the daemon log has a warning
`memory guard: cancelling the running job ...`. After an action the guard rests
15 seconds. It never acts when the shortage is not the gates' (they hold less than
the reserve) and never on anything but Sluice's own jobs. If it fires too often,
lower the suites' parallelism or lower `memory_reserve`; to turn it off, remove
the key.

### A gate that waits for memory

`sluice status` shows a job that fits the CPU budget but not the memory it is
expected to use as `(budget: memory)`, with the expected peak next to it
(`mem 18.0 GiB`; `~2.2 GiB (guessed)` when it is a guess because the gate has
not passed yet, `~0.4 GiB (lower bound)` when it comes from its failed runs), and
in `--json` as `reason: "budget"` with `wait_detail: "memory"`,
`memory_expected_bytes`, `memory_expected_learned` and `memory_expected_basis`
(`learned`, `lower-bound` or `guessed`).
Running jobs show `mem 3.1 GiB/18.0 GiB` (working set now over expected peak) and
a `memory:` line gives what admission sees: `MemAvailable`, the reserve and
what running jobs are still expected to grow by (`budget.memory` in JSON). The
job starts when a running job ends or when `MemAvailable` recovers; the wait is
counted as budget wait in `sluice logs --timeline`. When one job runs alone it
is never held back for memory, so a wait never lasts longer than the running
jobs. If a gate waits for memory it does not need, its learned peak may be
inflated by one huge run (it forgets after 5 runs); if it keeps being killed or
starved, lower its parallelism, raise `[admission] memory_reserve`, or turn the
rule off with `memory_aware = false`. The peak it learns is the working set
(`anon` + swap), not `memory.peak`, which includes page cache and is shown only
in a kill message.

A gate that has never passed is estimated, not learned. Its estimate is, in
order: `1.2 x` the largest peak of its last 5 runs that failed or timed out (a
lower bound: a run that stops at the first error shows at least that much, never
more), else the 75th percentile of the learned peaks of the gates of the same or
lighter weight, clamped between 1 GiB and the job's weight share of the usable
memory (the share alone when the daemon has learned nothing). While such a gate
has no passing run, one job of it may start as a *memory probe* when a quarter of
its estimate is free and no other job of the gate runs; at most one probe runs on
the machine at a time, and a probe never over-admits (that needs a learned peak).
A probe that needs far more than a quarter can be killed for lack of memory once;
that run's peak is learned and the next run waits correctly. Runs that end `fail`
or `timeout` store their peak in the `gate_memory_bound` table; a pass, a memory
kill or a cancel does not, and weights and durations still learn from passes only.

To see how long jobs actually waited for memory, grep the daemon log (see
[Where everything is logged](#where-everything-is-logged)). The columns and the
wire reason fold memory into the budget, so the log is the only record:

```text
job j7 (shop:test) waits for memory: expected 18.0 GiB learned, free 12.0 GiB, running headroom 4.0 GiB
job j7 (shop:test) started after waiting 12.3s for memory (expected 18.0 GiB learned, free at start 20.0 GiB, reserve 3.0 GiB)
job j9 (shop:e2e) starts as a memory probe: expected 14.0 GiB guessed, needs a quarter (3.5 GiB), free at start 20.0 GiB, reserve 3.0 GiB, waited 0.0s for memory
```

The expected figure ends with its basis: `learned`, `lower-bound` or `guessed`.
`grep "memory probe"` finds the first runs that started on a quarter of their
estimate. `grep "waits for memory"` finds every job that was held back (written once, when
it first is; a job that never starts, because its client went away, still leaves
this line) and `grep "after waiting"` gives the time of those that started.
`free` is `MemAvailable`; `running headroom` is what the running jobs are still
expected to grow by. Both lines are `info`.

A job that starts before the reserved head (`[scheduler] backfill`) is logged
with the rule that let it:

```text
job j12 (shop:lint) starts before the reserved head job j9 (shop:test) (extra resources): expected 0.1s, the head is predicted to start in 245.0s
job j13 (shop:typecheck) starts before the reserved head job j9 (shop:test) (ends in time): expected 3.4s, the head is predicted to start in 244.0s
```

`grep "starts before the reserved head"` finds them. If a heavy job starts much
later than the `eta` `sluice status` showed, compare `totals.backfill_head_delay_ms`
with the number of `backfilled` jobs; a running job that overran its learned
duration (or hangs) keeps the prediction late until it ends. Setting
`[scheduler] backfill = false` restores the strict reservation, under which a
job behind the head is `head-reserved` in `sluice logs --timeline` until the head
starts.

### A job reported as stalled

`sluice status` shows `STALLED 6m12s (flat: ...)` on a running job that has shown no
progress for its whole quiet window (`max(5 min, 3 x expected duration)`, or the gate's
`stall_timeout`), and `sluice run` and the MCP `verify` say so while they wait. The
daemon log records both ends of a stall and the longest quiet window of every
finished job:

```text
job j9 stalled: no progress for 5m0s while holding 6 unit(s) of budget; reported only, Sluice never ends it
job j9 active again after a stall of 7m12s
job j9 finished: longest quiet window 7m12s
```

`grep "stalled:"` finds the stalls, `grep "longest quiet window"` lists every job's
longest quiet stretch: a passing job whose figure is near the window is a gate to give
a longer `stall_timeout` (or `"off"`). A stalled job is usually a test waiting forever
(a lock, a socket, a promise that never settles): read `sluice logs <job>` and fix the
test. Network waits and `sleep`-based tests are quiet by design, so they can be reported
too. The gate is never ended by Sluice on this account; cancel the `verify` or `sluice
run` that waits for it, or let the gate `timeout` do it. `flat` lists the counters that
were read and did not move; a missing `io` means the machine did not give it.

### The stopgap holder

The session hook creates `sluice-agents.scope` itself when no daemon runs. It
needs a live process for that, `sluice daemon holder --lifetime 600`, which
exits by itself after ten minutes, when its scope is no longer `active`, or
when no Sluice scope adopted it within 30 seconds; the hook checks that the
scope is active and holds the process, and otherwise kills the holder and
leaves the session uncontained. The daemon retires it when it takes the scope
over.

Test-only environment variables (not for users): the integration tests start
a daemon with `SLUICE_TEST_CONTAINMENT_PREFIX=sluice-test-<pid>-<case>` so its
units are `<prefix>.slice`, `<prefix>-daemon.scope` and
`<prefix>-agents.scope` (the value must start with `sluice-test-`), and with
`SLUICE_TEST_MANAGER_RUNTIME_DIR` pointing at the real runtime directory while
the daemon's own lock and socket stay private.

## Socket path too long

A Unix socket path can be at most 107 bytes on Linux (108 with the
terminating NUL). When `$XDG_RUNTIME_DIR/sluice/daemon.sock` is longer (a
deeply nested `XDG_RUNTIME_DIR`, common in containers and test harnesses),
the daemon and every client use `/tmp/sluice-<uid>/daemon.sock` instead;
nothing to configure. You can see it in three places:

- `daemon.log`: `socket path fallback in use: <preferred> is N bytes, ...`;
- `sluice status`: `socket:   fallback in use (...)` under the `daemon:` line;
- `sluice doctor`: `[ok  ] socket-path    fallback in use: ...`.

Sluice creates `/tmp/sluice-<uid>` with mode 0700. It refuses to use it,
with an error naming the directory and the problem, when it is a symlink, is
not a directory, is owned by another user, or is group or world writable
(someone else may be able to replace the socket). Fix it with
`chmod 700 /tmp/sluice-<uid>`, or remove it if it is not yours, and retry.

`SLUICE_SOCKET` always wins. When it is longer than the limit, `sluice run`
exits 1 with `SLUICE_SOCKET is N bytes, longer than the 107-byte Unix socket
limit` instead of silently running the gate in-process; set a shorter path.

## `--bundle`

`sluice doctor --bundle` writes `./sluice-doctor-<timestamp>.tar.gz` (or the
given path) with: `doctor.json`, `version.txt`, `user-config.toml` and
`repo-sluice.toml` (if present), `status.json`, `queue-status.json` (when the
daemon runs the queue), and under `logs/`: the last 2000 lines of
`daemon.log` and `client.log`, the newest three `mcp-*.log` and, under
`logs/jobs/`, the logs of the last five failed jobs.

Every text file is redacted before it is archived:

- your home directory becomes `~`;
- the value of any `KEY=value`, `key: value` or JSON `"key": "value"` whose
  key contains `token`, `secret`, `password`, `passwd`, `auth`, `key` or
  `credential` (case-insensitive) becomes `<redacted>` (this also hides
  values such as `key_inputs` in a config, on purpose);
- the `user:password@` part of URLs is removed.

Redaction is pattern based. Logs of your gates can still contain project
names, commands and paths, so open the bundle and review it before attaching
it to an issue.
