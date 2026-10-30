# CLI reference

Every `sluice` subcommand, with its complete `--help` output in a collapsible
block. The help text is generated from the binary itself (`sluice --version`
shows which one you have); when in doubt, `sluice <command> --help` is
authoritative.

| Command | Purpose |
|---|---|
| [`sluice run`](#sluice-run) | Run a gate: cache hit in milliseconds, otherwise queue it in the daemon under the budget. |
| [`sluice exec`](#sluice-exec) | Run any command under the budget, without cache. |
| [`sluice guard`](#sluice-guard) | `PreToolUse` hook of the agent plugin: read the hook JSON on stdin, print a decision. |
| [`sluice shim`](#sluice-shim) | Shell shims that route heavy commands of non-Claude agents through the queue. |
| [`sluice agent session-start`](#sluice-agent-session-start) | Hidden: the `SessionStart` hook of the agent plugin. |
| [`sluice mcp`](#sluice-mcp) | Start the MCP stdio server (spawned by the coding agent). |
| [`sluice daemon`](#sluice-daemon) | Start the per-user daemon, or stop or restart it. |
| [`sluice status`](#sluice-status) | Show the daemon, budget, system pressure, jobs and totals. |
| [`sluice gate list`](#sluice-gate) | List the gates of this repository: those of `.sluice.toml` or, without one, the check units of its git hooks. |
| [`sluice cache`](#sluice-cache) | Inspect or clear cache markers for the current repository. |
| [`sluice init`](#sluice-init) | Detect the stack and the existing hooks, write `.sluice.toml` and rewire the hooks. |
| [`sluice hook print`](#sluice-hook) | Print the snippet for a hook manager. |
| [`sluice hooks`](#sluice-hooks) | Make Sluice the git hooks entry point of this clone (`on`, `off`, `status`) without touching the repository. |
| [`sluice logs`](#sluice-logs) | Show the log of a job. |
| [`sluice queue`](#sluice-queue) | Local merge queue: enqueue, approve, retry, cancel, reconcile. |
| [`sluice doctor`](#sluice-doctor) | Check the installation; optionally write a redacted bug-report bundle. |
| [`sluice update`](#sluice-update) | Update Sluice to the latest (or a given) release. |

## Global

<details>
<summary>Full help</summary>

```text
Sluice runs named gates (tests, linters, type checks) under a global concurrency budget and caches results per git tree so each tree is verified once. Git hooks run gates through `sluice run`, or through `sluice hooks on`, which runs the repository's own hook commands through the same queue and cache; coding agents call the MCP server started by `sluice mcp` (tools verify, status, gate_list, logs and the merge-queue tools queue_enqueue, queue_status, queue_approve, queue_retry, queue_cancel).

On a cache miss `sluice run` hands the gate to the per-user daemon (`sluice daemon`), starting it on demand; when the daemon cannot be reached or started the gate runs in-process. `sluice status` shows the queue. `sluice exec -- <cmd>` runs any other heavy command under the same budget, without cache.

Exit codes:
  0    pass or cache hit
  N    the gate's own code when it fails (`sluice run`, `sluice exec`)
  1    Sluice error
  2    usage error (also `sluice queue` and `sluice update`)
  3    refused by agent_hook_policy = "fail-fast"
  4    `sluice queue` request refused by the state of the queue
  75   `sluice daemon` found a daemon already holding the lock
  124  gate timeout
  125  `sluice exec` failed after the daemon accepted the request
  141  `sluice exec` stdout is a closed pipe
  143  gate killed (signal or `sluice daemon stop --force`)

Environment: SLUICE_DISABLE=1 runs the gate directly (no cache, no queue); SLUICE_CACHE=0|1 forces the cache off or on (on overrides CI); CI disables the cache; SLUICE_AGENT=1|0 forces agent or human detection; SLUICE_CONFIG points at the user config; SLUICE_SOCKET overrides the daemon socket path; SLUICE_LOG sets the log filter of the daemon, `sluice mcp` and `sluice run` (then `[debug] level` of the user config, then info).

Usage: sluice <COMMAND>

Commands:
  run     Run a gate: cache hit in milliseconds, otherwise queue it in the daemon under the budget
  exec    Run any command under the budget, without cache (builds, test runs, docker builds)
  guard   `PreToolUse` hook of the agent plugin: read the hook JSON on stdin, print a decision
  shim    Shell shims: route heavy commands of agents that only have a shell through the queue
  mcp     Start the MCP stdio server (spawned by the coding agent)
  daemon  Start the per-user daemon (detached unless --foreground), or stop it
  doctor  Check the installation (git, PSI, daemon, lease, configs, hooks) and optionally collect a redacted diagnostics bundle for bug reports
  status  Show the daemon, budget, system pressure (PSI), running and queued jobs, and totals
  gate    List the gates of the current repository
  cache   Inspect or clear cache markers for the current repository
  init    Write a `.sluice.toml` that fits the repository and wire its git hooks
  hook    Print hook integration snippets
  hooks   Make Sluice the git hooks entry point of this clone, without touching the repository
  logs    Show the log of a job
  queue   Local merge queue: enqueue committed branches, approve trains, land them
  update  Update sluice to the latest (or a given) release
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

</details>

## Commands

### `sluice run`

Run a gate: cache hit in milliseconds, otherwise queue it in the daemon under the budget.

<details>
<summary>Full help</summary>

```text
Run a gate: cache hit in milliseconds, otherwise queue it in the daemon under the budget

Usage: sluice run [OPTIONS] <GATE> [-- <EXTRA>...]

Arguments:
  <GATE>      Gate name from `.sluice.toml`, or, without one, a check unit of a hook that Sluice runs (see `sluice gate list`)
  [EXTRA]...  Extra arguments appended to the gate command

Options:
      --hook <HOOK>  Name of the git hook invoking Sluice (pre-commit, pre-push, ...). Enables the hook policy on a cache miss and, for pre-commit, records the committed tree
  -v, --verbose      Stream the gate output to stderr while it runs (it is always written to the log)
      --explain      Explain what Sluice would do (caller detection, policy, tree, key, cache lookup) and exit
  -h, --help         Print help
```

</details>

In a repository without `.sluice.toml` whose hooks Sluice can plan, `<GATE>` also
accepts the name of a check unit of a hook (`husky:pre-commit#2`, see
[`sluice gate`](#sluice-gate)).

### `sluice exec`

Run any command under the budget, without cache (builds, test runs, docker builds).

<details>
<summary>Full help</summary>

```text
Run any command under the budget, without cache (builds, test runs, docker builds).

The command (an argv, no shell: use `sluice exec -- sh -c '...'` for a script) is queued in the daemon like a gate and runs in its own process group with the caller's environment and directory, stdin `/dev/null` and no terminal. Its stdout and stderr are replayed byte for byte to this process's stdout and stderr; SIGINT, SIGTERM and SIGHUP are forwarded to the whole process group.

Exit codes: the command's own code; 128+N when it was killed by signal N; 124 on --timeout; 143 when the daemon stopped and killed it; 127 command not found; 126 not executable; 125 Sluice error after the request was accepted.

Without a usable daemon (not running and not startable, or older than 0.3.0) the command runs in-process under the same budget semaphore with inherited stdio. Inside a Sluice job (`SLUICE_JOB` set) it runs directly, without queueing. `SLUICE_DISABLE=1` always runs it in-process (with a terminal, if any).

Usage: sluice exec [OPTIONS] <CMD>...

Arguments:
  <CMD>...
          The command and its arguments (after `--`)

Options:
      --weight <N>
          Budget units the command takes; default half the budget, rounded up

      --label <L>
          Label shown in `sluice status` and messages; default the first two words

      --coalesce-key <K>
          Share one run between simultaneous requests with this key and the same command and directory (environment differences are ignored)

      --timeout <DUR>
          Kill the command after this long (for example `90s`, `20m`); exit 124

  -q, --quiet
          Do not print Sluice's informational lines (errors are still printed)

  -h, --help
          Print help (see a summary with '-h')
```

</details>

### `sluice guard`

`PreToolUse` hook of the agent plugin: read the hook JSON on stdin, print a decision. Setup: [agents.md](agents.md). It is not meant to be typed by hand.

<details>
<summary>Full help</summary>

```text
`PreToolUse` hook of the agent plugin: read the hook JSON on stdin, print a decision.

Denies `sluice queue approve` (reserved to humans). Rewrites a gate command to `sluice run <gate> -v` and a heavy command (`[agent] heavy_commands`) to `sluice exec --label <L> -- <cmd>` by inserting that prefix into the unchanged original, answering `allow` or `ask` as the agent client's permission settings permit, or not at all. Otherwise denies raw gate commands as the shell guard does. Never fails closed: any error prints nothing and exits 0. Never contacts the daemon. `SLUICE_GUARD=0` disables it; `SLUICE_GUARD=deny` denies with the rewritten command as a suggestion.

Usage: sluice guard [OPTIONS]

Options:
      --hook
          Accepted for the plugin's hook script (the hook JSON is always read on stdin)

  -h, --help
          Print help (see a summary with '-h')
```

</details>

### `sluice shim`

Shell shims: route heavy commands of agents that only have a shell through the queue. Setup and examples: [agents.md, section 9](agents.md#9-agents-other-than-claude-code-shell-shims). The hidden `sluice shim exec <name> -- <args>` is what each wrapper runs; it is not meant to be typed by hand.

<details>
<summary>Full help</summary>

```text
Shell shims: route heavy commands of agents that only have a shell through the queue.

For agents other than Claude Code (Codex, Cursor, Aider, ...): small `sh` wrappers named after the heavy commands (`make`, `cargo`, `pnpm`, `docker`, ...) that, put first in an agent shell's PATH, run those commands as `sluice exec --label <words> -- <real binary> <args>` in agent sessions (agent detection or `SLUICE_SHIMS=1`) and exec the real binary unchanged otherwise (human shells, `SLUICE_SHIMS=0`, inside a Sluice job, subcommands that are not heavy such as `docker ps` or `npm install`).

Usage: sluice shim <COMMAND>

Commands:
  install    Write one wrapper per command of the heavy list into the shim directory
  uninstall  Remove the shims (only files Sluice wrote; the directory only when empty)
  status     Show the shims, whether their directory is in PATH before the real binaries, which real binary each one runs, and what this shell would do
  help       Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice shim install`

Write one wrapper per command of the heavy list into the shim directory.

<details>
<summary>Full help</summary>

```text
Write one wrapper per command of the heavy list into the shim directory.

Default directory `$XDG_DATA_HOME/sluice/shims` or `~/.local/share/sluice/shims` (created with mode 0700; wrappers 0755, written atomically). Idempotent: existing Sluice shims are refreshed and shims of commands no longer in the list are removed. A file Sluice did not write is never overwritten unless --force.

Usage: sluice shim install [OPTIONS]

Options:
      --dir <DIR>
          Shim directory to use instead of the default

      --force
          Replace files with a shim's name that Sluice did not write

  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice shim uninstall`

Remove the shims (only files Sluice wrote; the directory only when empty).

<details>
<summary>Full help</summary>

```text
Remove the shims (only files Sluice wrote; the directory only when empty)

Usage: sluice shim uninstall [OPTIONS]

Options:
      --dir <DIR>  Shim directory to use instead of the default
  -h, --help       Print help
```

</details>

#### `sluice shim status`

Show the shims, whether their directory is in PATH before the real binaries, which real binary each one runs, and what this shell would do. `--json` prints `schema`, `dir`, `exists`, `in_path`, `path_index`, `sluice`, `this_shell {queue, reason}`, `shims [{name, state, real, active}]` (`state`: `current`, `stale`, `missing`, `foreign`) and `unlisted`.

<details>
<summary>Full help</summary>

```text
Show the shims, whether their directory is in PATH before the real binaries, which real binary each one runs, and what this shell would do

Usage: sluice shim status [OPTIONS]

Options:
      --dir <DIR>  Shim directory to use instead of the default
      --json       Print machine-readable JSON (`schema: 1`)
  -h, --help       Print help
```

</details>

### `sluice agent session-start`

Hidden (it is absent from `sluice --help`) and not meant to be typed: the
`SessionStart` hook of the agent plugin runs it at `startup`, `resume`,
`clear` and `compact`. It is documented here for debugging.

```sh
echo '{"cwd":"/path/to/repo","source":"startup"}' \
  | sluice agent session-start --plugin-version 0.4.0
```

It reads the hook JSON on stdin (`cwd`, `source`, `session_id`) and, under a
300 ms watchdog, installs or refreshes the shims in the stable shim directory,
appends a guarded line that prepends that directory to `PATH` to the file in
`CLAUDE_ENV_FILE` (once; a repeated call changes nothing), and prints the hook
JSON with the `additionalContext` (the rules, or the offer to run `sluice
init` in a git repository without `.sluice.toml`). It always exits 0 and
prints nothing on any error. Each call writes a `hook=session-start` line to
`client.log`; see [debugging.md](debugging.md#what-the-session-hook-decided).

### `sluice mcp`

Start the MCP stdio server (spawned by the coding agent).

<details>
<summary>Full help</summary>

```text
Start the MCP stdio server (spawned by the coding agent).

Tools: `verify` (run gates through the cache and the daemon, blocking with progress notifications), `status`, `gate_list`, `logs`, and the merge-queue tools `queue_enqueue`, `queue_status`, `queue_approve`, `queue_retry`, `queue_cancel`. Logs go to stderr only. Register `sluice mcp` as a stdio MCP server in the agent's config.

Usage: sluice mcp

Options:
  -h, --help
          Print help (see a summary with '-h')
```

</details>

### `sluice daemon`

Start the per-user daemon (detached unless --foreground), or stop it.

<details>
<summary>Full help</summary>

```text
Start the per-user daemon (detached unless --foreground), or stop it.

Exits 75 when another daemon still holds the daemon lock.

Usage: sluice daemon [OPTIONS] [COMMAND]

Commands:
  stop     Ask the running daemon to stop (queued jobs are dropped, running jobs finish)
  restart  Stop the daemon and start a fresh detached one
  help     Print this message or the help of the given subcommand(s)

Options:
      --foreground
          Stay attached to the terminal instead of detaching

  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice daemon stop`

Ask the running daemon to stop (queued jobs are dropped, running jobs finish).

<details>
<summary>Full help</summary>

```text
Ask the running daemon to stop (queued jobs are dropped, running jobs finish).

Without --force, waits (up to 60 s) until the running jobs finished and the daemon released its lock, so the next command starts a fresh daemon.

Usage: sluice daemon stop [OPTIONS]

Options:
      --force
          Kill running jobs instead of letting them finish

  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice daemon restart`

Stop the daemon and start a fresh detached one.

<details>
<summary>Full help</summary>

```text
Stop the daemon and start a fresh detached one.

Refuses (exit 1) while gates are running unless --force. With --debug the new daemon logs at `debug` level (`SLUICE_LOG=debug`) until it exits; `sluice status` shows the active level and the log path.

Usage: sluice daemon restart [OPTIONS]

Options:
      --debug
          Run the new daemon with `SLUICE_LOG=debug` (only that instance)

      --force
          Kill running gates instead of refusing

  -h, --help
          Print help (see a summary with '-h')
```

</details>

### `sluice status`

Show the daemon, budget, system pressure (PSI), running and queued jobs, and totals.

With containment (ADR-0017) the output ends with the containment block:

```text
containment: full
  s-7f3a9c  cpu 61.2s  mem 900 MB  procs 31
  s-b21e04  cpu 3.4s  mem 412 MB  procs 18
outside jobs: 3.2% CPU
interventions:
  2026-10-24T10:02:11Z  s-7f3a9c  s-7f3a9c/tools  oomd_memory_pressure
```

`containment:` is `full`, `partial (memory, pids; cpu by nice)` or
`off (<reason>)`. Each live agent session has one line (CPU seconds, memory
and process count read from its cgroup). `outside jobs` is the CPU the agents'
sessions used over the last sampling window (10 s), as a share of all CPUs:
CPU not spent in Sluice jobs, which run in their own scope. `interventions`
lists the last kills by the kernel OOM killer or `systemd-oomd` on Sluice's
cgroups (time, session, leaf, cause). With `--json` the optional fields
`containment` (`level`, `detail`, `outside_jobs_cpu_percent`, `window_ms`),
`sessions[]` (`id`, `cpu_seconds`, `memory_bytes`, `memory_peak_bytes`,
`procs`) and `interventions[]` (`at`, `session`, `leaf`, `cause`, `detail`)
are present when the daemon reports them (schema 1, additive).

With memory-aware admission (0.6.0, `[admission] memory_aware`) the job lists
carry memory, and the containment block may add the gates' oomd policy:

```text
memory:   22.0 GiB available, 3.0 GiB reserved, 16.0 GiB still to grow in running jobs
running:  1
  test             app                  weight 6    41.2s  1 subscriber(s)  mem 3.1 GiB/18.0 GiB  job 1761...
queued:   1
  #1   e2e             app                  weight 4  waiting     12.0s  (budget: memory)  expected 3m  mem ~9.0 GiB
containment: full
gates memory policy: systemd-oomd at 30% for 8s
```

`memory:` is what admission sees (`MemAvailable`, the reserve, and what the
running jobs are still expected to grow by). A running job shows its working set
over its expected peak (`~` when the peak is not learned: a guess from the
weight, or a lower bound from failed runs, with `(guessed)` or `(lower bound)`
after it); a queued job shows `(budget: memory)` when it fits the CPU
budget but not the memory it is expected to use. `gates memory policy` appears
only where the gates' scope has its own `systemd-oomd` policy. In `--json`:
`budget.memory` (`available_bytes`, `reserve_bytes`, `headroom_bytes`),
`running[]` `memory_bytes`, `memory_expected_bytes`, `memory_expected_learned`,
`memory_expected_basis` (`"learned"`, `"lower-bound"` or `"guessed"`),
`queued[]` `wait_detail` (`"memory"`; `reason` stays `"budget"`),
`memory_expected_bytes`, `memory_expected_learned`, `memory_expected_basis`, and
`containment.gates_oom_policy` (`limit_percent`, `duration_secs`). All optional
and additive.

With backfilling (`[scheduler] backfill`) the reserved head of the queue shows
its predicted start, and the totals count what backfilling did:

```text
queued:   2
  #1   test            app                  weight 6  waiting  4m10s  (budget: memory)  expected 9m  mem 18.0 GiB  reserved, eta 4m05s
  #2   lint            app                  weight 1  waiting  1m02s  (budget)  expected 120ms
backfill: 7 started before a reserved head, heads started 3.2s after their eta in total
```

`reserved, eta 4m05s` is the head's shadow time: when enough of the running jobs
are predicted to have ended for it to fit, from their learned durations (a job
past its expectation counts for 10 % of it plus 2 s more). It is a prediction;
it moves as jobs finish early or late. The `backfill:` line appears once a job
has started before a reserved head since the daemon started. In `--json`:
`queued[].reserved_eta_ms` (only on the reserved head, milliseconds from the
report) and `totals.backfilled` and `totals.backfill_head_delay_ms` (the sum,
over the reserved heads that started, of how long after its first computed eta
each did; zero when earlier). All optional and additive.

A running job that shows no progress is reported `STALLED` with how long, and the
signals that were flat (`cpu`, `cpu-wait`, `io`, `faults`, `output`, `processes`; the
ones the machine gives); the memory and weight it holds are on the same line:

```text
running:  1
  test             app                  weight 6    41m12s  1 subscriber(s)  mem 7.1 GiB/14.0 GiB  STALLED 36m05s (flat: cpu cpu-wait io faults output processes)  job j9
```

Quiet means no progress beyond small thresholds: CPU above 1 % of one CPU, waiting
for a CPU above 10 % of the interval, block I/O of 16 KiB or 16 operations, 128 minor
or one major page fault, any log byte, any change in the number of processes, all per
5 s read. The window is `max(5 min, 3 x expected duration)` or the gate's
`stall_timeout`. The line goes away when the job shows activity. In `--json`:
`running[].stalled` (`for_ms`, `flat`), present only while stalled; additive. Sluice
never ends a stalled job.

<details>
<summary>Full help</summary>

```text
Show the daemon, budget, system pressure (PSI), running and queued jobs, and totals

Usage: sluice status [OPTIONS]

Options:
      --json  Print machine-readable JSON instead of text
  -h, --help  Print help
```

</details>

### `sluice gate`

List the gates of the current repository.

```sh
sluice gate list          # name, command, weight, timeout, hooks, cache, origin
sluice gate list --json   # {schema, repository, gates[], excluded[]}
```

<details>
<summary>Full help</summary>

```text
List the gates: those of `.sluice.toml` or, without one, the check units of the repository's git hooks (named `<manager>:<hook>#<n>`), and the hooks and units that are not gates, with the reason

Usage: sluice gate list [OPTIONS]

Options:
      --json  Print machine-readable JSON instead of text
  -h, --help  Print help
```

</details>

With a `.sluice.toml` they are its gates, unchanged. Without one, they are the
**check units of its git hooks**
([ADR-0021](../kb/sluice/design/adr/0021-sluice-runs-the-repository-hooks.md),
[Git hooks](hooks.md#verify-and-gates-without-sluicetoml)): those of `pre-commit`,
`pre-merge-commit` and `pre-push`, named `<manager>:<hook>#<n>`
(`husky:pre-commit#2`), whether or not `sluice hooks on` was run. A line per gate:

```text
husky:pre-commit#2  npm run lint  (weight 4, timeout 0s, hooks: pre-commit, cache on, from the husky hook)
not gates:
  husky:pre-commit#1 `lint-staged`: it may change files or the index (...): it only runs inside the hook
  hook commit-msg: delegated, not a gate: ...
```

The `not gates:` block lists the hooks and units that cannot be verified, each
with the reason (a hook Sluice delegates, a mutating unit, a unit that needs what a
commit stages). `--json` has, per gate, `name`, `cmd`, `weight`, `timeout_seconds`,
`hooks`, `cache` and `source` (`config` or `hook`; `manager` too for `hook`), and
`excluded[]` with `hook`, `unit`, `command` (both null for a whole hook) and
`reason`. `sluice run <gate>`, the MCP tools `verify` and `gate_list` use the same
gates. A repository with neither a `.sluice.toml` nor such a unit still gets the
"no .sluice.toml found" error (its message points to `sluice hooks status`); exit
code 1 on any error, 0 otherwise.

### `sluice cache`

Inspect or clear cache markers for the current repository.

<details>
<summary>Full help</summary>

```text
Inspect or clear cache markers for the current repository

Usage: sluice cache <COMMAND>

Commands:
  status  Show, per gate, whether the current tree has a cached pass
  clear   Remove markers and miss records for the current repository
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

</details>

#### `sluice cache status`

Show, per gate, whether the current tree has a cached pass.

<details>
<summary>Full help</summary>

```text
Show, per gate, whether the current tree has a cached pass

Usage: sluice cache status [OPTIONS]

Options:
      --all   Also list every marker stored for the repository
  -h, --help  Print help
```

</details>

#### `sluice cache clear`

Remove markers and miss records for the current repository.

<details>
<summary>Full help</summary>

```text
Remove markers and miss records for the current repository

Usage: sluice cache clear [OPTIONS]

Options:
      --gate <GATE>  Only clear entries of this gate
  -h, --help         Print help
```

</details>

### `sluice init`

Write a `.sluice.toml` that fits the repository and wire its git hooks, in one
command.

```sh
sluice init --dry-run    # show the gates, the files and the diff; write nothing
sluice init              # do it
sluice init --force      # redo it over an existing .sluice.toml (idempotent)
```

`init` is the one command that writes your working tree, and only because you
ran it ([ADR-0016](../kb/sluice/design/adr/0016-init-detection-and-hooks.md)). It
writes at the repository root (`git rev-parse --show-toplevel`, so a
subdirectory is fine; outside a repository it exits 1), and it never stages or
commits: `git diff` shows everything it changed.

**Gates come first from the hooks you already have.** `init` reads husky
(`.husky/pre-commit`, `.husky/pre-push`), lefthook (`lefthook.yml`,
`.lefthook.yml`), the pre-commit framework (`repo: local` hooks of
`.pre-commit-config.yaml`) and plain hooks (`core.hooksPath` or `.git/hooks`,
samples ignored). Each line that runs one heavy, non-mutating command (a test
runner, `ng lint`, `cargo clippy`, an npm script that runs one) becomes a gate
named from its words (`ng test` is `test`, `npm run typecheck` is `typecheck`,
repeated names get `-2`), tagged with the hook it was found in. Formatters and
fixers (`--fix`, `--write`, `pretty-quick`, `lint-staged`), commitlint and your
own scripts are left alone.

**Then the stack at the root fills what is missing:**

| Found | Gates |
|---|---|
| `angular.json` with `lint`/`test` targets | `npx ng lint`, `npx ng test --watch=false` (the package manager's launcher for pnpm/yarn/bun) |
| `package.json` | `lint`, `typecheck` (or `type-check`) and `test` scripts, run with the package manager of the lockfile (`pnpm-lock.yaml`, `yarn.lock`, `bun.lock[b]`, else npm); `test` gets `env = { CI = "1" }` |
| `Cargo.toml` | `cargo fmt --check`, `cargo clippy ...`, `cargo test --workspace`, with `CARGO_BUILD_JOBS = "4"` |
| `pyproject.toml`, `setup.cfg`, `requirements*.txt` | `ruff` and `mypy` when configured, `pytest -q`; `.venv/bin/` prefix when a `.venv` exists |
| `go.mod` | `go vet ./...`, `go test -p 4 ./...` |
| nothing | the commented starter template |

Projects below the root are not configured; they are listed in a comment.

**`[queue] prepare`.** The merge queue verifies each train in a fresh worktree
without the git-ignored dependencies, so `init` also writes a `[queue]` table
with the command that restores them, picked from the lockfile at the root:
`pnpm-lock.yaml` `pnpm install --frozen-lockfile --prefer-offline`,
`package-lock.json` `npm ci`, `yarn.lock` `yarn install --immutable` (with a
`.yarnrc.yml`) or `yarn install --frozen-lockfile`, `bun.lock[b]`
`bun install --frozen-lockfile`, `uv.lock` `uv sync --frozen`, `poetry.lock`
`poetry install --no-root`. Nothing is written for cargo, go, a Node project
without a lockfile or an unknown stack. It needs Sluice 0.6.0.

**Weights and timeouts.** Lint, vet and type checks get `weight = 2`, a
formatter 1, test runners and multi-threaded builds `weight = "auto"` (half the
daemon budget at first, lowered by learning; see
[configuration](configuration.md); it needs Sluice 0.4.0). Timeouts are 10m for
lint-like gates and 30m for tests. `key_inputs` carry the tool versions.

**Hooks are rewired, always.** Every line that became a gate is replaced in
place by a call to `sluice run --hook <hook> <gate>` that falls back to the
original command when Sluice is not installed, so a teammate without Sluice
can still commit:

```sh
if command -v sluice >/dev/null 2>&1; then sluice run --hook pre-commit lint; else ng lint; fi
```

The original text is kept exactly (leading `NAME=value` words included) and
the status of whichever branch ran is the line's status. In the pre-commit
framework (`entry:` with `language: system`, which is not a shell) the same
goes through `sh -c '...'`. A command that cannot be nested safely (a single
quote inside a pre-commit entry, a `: ` inside a YAML value) is left alone and
its snippet is printed. Comments, other lines, the file
mode, CRLF line endings and the trailing-newline state are kept. lefthook and
pre-commit YAML are edited line by line (no re-serialisation, comments
survive); a local pre-commit hook also gets `pass_filenames: false`. A line
that cannot be matched safely (a symlink that leaves the repository, a pre-commit
hook with `args` or a non-`system` language) is left alone and its snippet is
printed. With no hook at all, `init` creates executable `.githooks/pre-commit`
and `.githooks/pre-push` (they start with
`command -v sluice >/dev/null 2>&1 || exit 0`, so a clone without Sluice can
still commit) and sets `core.hooksPath` locally; if `.git/hooks`
already has hooks of its own, or `core.hooksPath` points elsewhere, it prints
the snippet and says why instead. Gates that no hook calls keep `hooks = []`.

It is safe to run twice: lines that already call `sluice run` are recognised
and `init --force` changes no hook. The one exception is a line of 0.4.0, a
bare `sluice run --hook <hook> <gate>`: `init --force` upgrades it to the
fallback form, using the gate's `cmd` (with its `env` words) from the existing
`.sluice.toml` as the fallback command; a second `--force` changes nothing.

**Writes are atomic.** Every file `init` writes (`.sluice.toml`, hooks) goes to
a temporary file in the same directory, is synced, gets the target's mode (new
hooks 0755, a new config 0644) and replaces the target with one `rename`, so a
hook that `sh` is executing never reads half a file. A hook that is a symlink
to a file inside the repository is replaced at the link's target; the link
stays. A failed write removes its temporary file.

**Output.** A table of the gates (name, command, weight, hooks, source: hook or
detected), the files written, and a unified diff of the hook edits. `--dry-run`
prints the same and writes nothing. Exit 0 on success, 1 on error (not a
repository, `.sluice.toml` exists without `--force`), 2 on a usage error.

<details>
<summary>Full help</summary>

```text
Write a `.sluice.toml` that fits the repository and wire its git hooks.

Detects the stack at the repository root and the commands the existing hooks (husky, lefthook, pre-commit, plain) already run, writes the gates and rewrites those hook lines to `sluice run --hook <hook> <gate>`. Nothing is staged or committed.

Usage: sluice init [OPTIONS]

Options:
      --force
          Overwrite an existing `.sluice.toml`

      --dry-run
          Print the gates, the files and the diff of the hook edits; write nothing

      --hooks <HOOKS>
          How existing hooks are handled: `rewire` edits the hook files (default); `entry` leaves them untouched and makes Sluice the hooks entry point of this clone (`sluice hooks on`)

          Possible values:
          - rewire: Rewrite the hook lines that became gates to `sluice run --hook <hook> <gate>` (edits the repository's hook files)
          - entry:  Leave the repository's hook files alone: make Sluice the hooks entry point of this clone (`sluice hooks on`), which runs their commands through the queue or, when it cannot, the hooks untouched
          
          [default: rewire]

  -h, --help
          Print help (see a summary with '-h')
```

</details>

`--hooks entry` is for repositories whose hook files you cannot or do not want
to edit (husky hooks that a teammate owns, generated hooks): `init` writes only
`.sluice.toml` and runs `sluice hooks on`; `--dry-run` shows it as `would run
sluice hooks on`. It needs existing hook files to delegate to: with none, `init`
says so and does its usual setup. With the default `--hooks rewire` and a hook
manager that owns the hooks, `init` adds a note offering `--hooks entry`. See
[Git hooks](hooks.md).

### `sluice hook`

Print hook integration snippets.

<details>
<summary>Full help</summary>

```text
Print hook integration snippets

Usage: sluice hook <COMMAND>

Commands:
  print  Print the snippet for a hook manager
  help   Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

</details>

#### `sluice hook print`

Print the snippet for a hook manager.

<details>
<summary>Full help</summary>

```text
Print the snippet for a hook manager

Usage: sluice hook print <MANAGER>

Arguments:
  <MANAGER>
          Hook manager to print the snippet for

          Possible values:
          - husky:      Husky: scripts in `.husky/`
          - lefthook:   Lefthook: a `lefthook.yml` fragment
          - pre-commit: The pre-commit / prek framework: `.pre-commit-config.yaml`
          - plain:      Plain POSIX scripts in `.githooks/`

Options:
  -h, --help
          Print help (see a summary with '-h')
```

</details>

### `sluice hooks`

Make Sluice the git hooks entry point of this clone, without touching the
repository ([ADR-0021](../kb/sluice/design/adr/0021-sluice-runs-the-repository-hooks.md),
[Git hooks](hooks.md)).

```sh
sluice hooks on             # activate (idempotent)
sluice hooks status         # on or off, directory, repository hooks, manager, effective?, each hook
sluice hooks status --json
sluice hooks off            # remove the include and the managed files
```

<details>
<summary>Full help</summary>

```text
Make Sluice the git hooks entry point of this clone, without touching the repository.

`on` gives the clone a hooks directory of its own (under its git directory) and points `core.hooksPath` at it through a file included by the last `[include]` of the clone's git config, so husky's own `core.hooksPath` writes do not displace it. Every hook runs `sluice hooks run`: when Sluice understands the repository's hook (husky, lefthook, pre-commit or plain) it runs the hook's commands one by one through the queue and the cache; otherwise it executes the hook untouched, with the same arguments, stdin, directory, environment and exit code. `status` shows which. `off` removes the include and the managed files.

Usage: sluice hooks <COMMAND>

Commands:
  on      Activate the entry point in this clone (idempotent)
  off     Remove the include and the managed files; the previous `core.hooksPath` applies again
  status  Show whether the entry point is on, what it delegates to and whether it is effective
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')
```

</details>

The help above summarises the entry point; what runs for each hook is described
next.

`on` creates `<git common dir>/sluice/hooks/` with one script for each hook the
repository has (and none for the others) and sets `core.hooksPath` to it through a
file included by the last `[include]` of the clone's git config (only `git config`
writes it). Nothing in the repository changes. Every hook then goes through
`sluice hooks run`, which does one of two things, decided at each run:

- **units**: a husky, plain, lefthook or pre-commit hook that Sluice understands
  completely runs its commands as jobs of the daemon (queue, budget, containment,
  logs; cache for checks; lefthook's and pre-commit's through their own CLIs). The
  `agent_hook_policy` refusal (exit code 3) is not applied to them;
- **delegation**: every other hook is replaced by the repository's own hook, with
  the same arguments, stdin, working directory, environment, exit code and signal.
  A hook that is not installed exits 0.

`SLUICE_HOOKS_DELEGATE=1` (or `SLUICE_DISABLE=1`) forces delegation; see
[hooks](hooks.md). `on` prints the number of scripts written, the hooks directory
and what it delegates to. `off` removes the include and the managed files (the
repository's own `core.hooksPath` applies again) and prints `was already off` when
there was nothing to remove. Exit codes of `on`, `off` and `status`: 0, or 1 on an
error (not inside a git repository, a git or file failure, `core.hooksPath` still
not Sluice's after the include).

`status` prints `hooks entry point: on|off`, the directory, the repository's
hooks directory (`repository hooks`), the manager (`husky`, `lefthook`,
`pre-commit`, `plain hooks` or `none`), whether the entry point is `effective`
(Sluice's directory is the value `git config core.hooksPath` returns) and, when it
is not, a `problem` and `fix: sluice hooks on`; then a line for every hook the
repository has: `N unit(s) run through the queue: <kind> <command>; ...` (with a
`(delegated at run time unless ...)` line for the condition of a lefthook or
pre-commit hook, and a `cache shared with gate ...` or `same command as gate ...,
cache not shared: ...` note when `.sluice.toml` has a gate with the unit's
command) or `delegated (<reason>)`. With `--json`:

```json
{
  "schema": 1,
  "active": true,
  "effective": true,
  "directory": "<git dir>/sluice/hooks",
  "previous": ".husky/_",
  "underlying": ".husky/_",
  "manager": "husky",
  "problem": null,
  "hooks": [
    {"hook": "pre-commit", "mode": "units",
     "units": [{"index": 1, "name": "husky:pre-commit#1", "kind": "mutating", "command": "lint-staged"},
               {"index": 2, "name": "husky:pre-commit#2", "kind": "check", "command": "npm run lint",
                "shares": "cache shared with gate `lint`"}],
     "requires": "something is staged and no file is partially staged"},
    {"hook": "commit-msg", "mode": "delegate", "reason": "..."}
  ]
}
```

`shares` appears only when a `.sluice.toml` gate has the unit's command; `requires`
only for lefthook and pre-commit units (the hook is delegated at run time when it
does not hold; it is not evaluated by `status`). When the include was removed, a
later include or a worktree config wins, or a hook of the repository has no script
(re-run `on` after a manager installs a new hook), `problem` says why and
`sluice hooks on` repairs it.

`sluice hooks run [--common-dir <dir>] <hook> -- <args>` is internal (hidden from
`--help`): the managed scripts call it with the hook's arguments, and it never
fails a hook for a Sluice problem.

### `sluice logs`

Show the log of a job, or with `--timeline` the scheduler timeline of recent jobs.

<details>
<summary>Full help</summary>

```text
Show the log of a job

Usage: sluice logs [OPTIONS] [JOB_ID]

Arguments:
  [JOB_ID]  Job id; defaults to the most recent job

Options:
  -n, --lines <LINES>     Only print the last N lines
      --timeline          Print the scheduler timeline instead of the log: one line per job (requested, queued, admitted, started, finished, wait reasons, demand, requesters)
      --since <DURATION>  With --timeline and no job id: the jobs queued within this period [default: 10m]
  -h, --help              Print help
```

</details>

### `sluice queue`

Local merge queue: enqueue committed branches, approve trains, land them.

<details>
<summary>Full help</summary>

```text
Local merge queue: enqueue committed branches, approve trains, land them.

Agents enqueue committed branches; a daemon task combines them on top of the target branch in a disposable worktree, runs the gates once on the combined tree and shows the plan. Nothing is pushed until a human runs `sluice queue approve`. Exit codes: 0 ok; 1 error; 2 usage; 4 refused by the state of the queue (conflict-state, not-allowed, stale-plan, duplicate-branch).

Usage: sluice queue <COMMAND>

Commands:
  enqueue    Queue a committed branch for the next train
  status     Show trains, their members, gates, conflicts and queued jobs
  approve    Approve a ready train: print the plan and land it
  retry      Re-queue a conflicted or failed job with the current head of its branch
  cancel     Cancel a job that has not been pushed
  reconcile  Resolve trains whose push outcome is unknown (`needs_reconcile`)
  help       Print this message or the help of the given subcommand(s)

Options:
  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice queue enqueue`

Queue a committed branch for the next train.

<details>
<summary>Full help</summary>

```text
Queue a committed branch for the next train.

Defaults to the current branch of this worktree. Refuses a detached HEAD, a worktree with staged or unstaged changes to tracked files (untracked files do not matter), a branch that is already queued and a branch equal to the target.

Warns (on stderr) when the target refuses direct pushes, as far as your `gh` login can tell or as an earlier refused push taught the queue; the branch is queued anyway.

Usage: sluice queue enqueue [OPTIONS] [BRANCH]

Arguments:
  [BRANCH]
          Branch to queue; defaults to the current branch

Options:
      --base <BASE>
          Where the train lands: `<remote>/<branch>` (pushed), `<branch>` (a local branch, updated in place) or a bare name tried first as a branch of the configured remote and then as a local branch. Defaults to `[queue] remote` and `target`. Tags, raw SHAs, HEAD and checked-out local branches are refused

      --trust-member-config
          Human only. Build this job's train from the branch's own `.sluice.toml` (gates and `[queue]`) instead of the target's, to fix a configuration that makes every train fail. The train is alone, shows the file's diff and can only be approved by a human at the CLI. Agents are refused

  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice queue status`

Show trains, their members, gates, conflicts and queued jobs.

<details>
<summary>Full help</summary>

```text
Show trains, their members, gates, conflicts and queued jobs.

Inside a repository only its own trains and jobs are listed. With --json the `queue-status` payload is printed verbatim (it carries `schema: 1`). For a train that waits for its pull request (`awaiting_merge`) it first reads the pull request with your own `gh` (read-only, a few seconds at most, cached briefly) and tells the daemon, so a merged or closed pull request shows up.

Usage: sluice queue status [OPTIONS]

Options:
      --json
          Print machine-readable JSON instead of text

  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice queue approve`

Approve a ready train: print the plan and land it.

<details>
<summary>Full help</summary>

```text
Approve a ready train: print the plan and land it.

On a terminal it asks `Land this train? [y/N]`; without one it requires --yes. The train hash that was displayed is sent along, so a train that changed in the meantime is refused (stale-plan, exit 4). Agents must not run this command.

A train that lands through a pull request (`[queue] land = "pull-request"`) is pushed to `sluice/train/<id>` and this command then opens its pull request with your own `gh` (finding it when it already exists). Running it again on such a train whose pull request could not be opened finishes the job. Sluice never merges: the forge and you do.

Usage: sluice queue approve [OPTIONS] <TRAIN_ID>

Arguments:
  <TRAIN_ID>
          Train id, as shown by `sluice queue status`

Options:
      --yes
          Do not ask for confirmation (required without a terminal)

      --auto-merge
          Pull-request landing only: also arm the forge's auto-merge on the pull request (`gh pr merge --auto --merge --match-head-commit <S>`). The forge merges when its own rules are met; admin rights are never used

  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice queue retry`

Re-queue a conflicted or failed job with the current head of its branch.

<details>
<summary>Full help</summary>

```text
Re-queue a conflicted or failed job with the current head of its branch.

Run it in the repository of the job after rebasing or fixing the branch; the branch head is re-read here.

Usage: sluice queue retry [OPTIONS] <JOB_ID>

Arguments:
  <JOB_ID>
          Job id, as shown by `sluice queue status`

Options:
      --trust-member-config
          Human only. Build the job's train from the branch's own `.sluice.toml` (see `enqueue --trust-member-config`). A plain retry goes back to the target's configuration

  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice queue cancel`

Cancel a job that has not been pushed.

<details>
<summary>Full help</summary>

```text
Cancel a job that has not been pushed.

A job that is pushing is only cancelled if `sluice queue reconcile` proves that nothing landed. A job whose train waits for its pull request (`awaiting_merge`) is withdrawn too: the train is dissolved, the pull request Sluice opened is closed with your own `gh` and the `sluice/train/<id>` branch it pushed is deleted (a pull request somebody else opened is left alone). The MCP `queue_cancel` is refused for such a job.

Usage: sluice queue cancel <JOB_ID>

Arguments:
  <JOB_ID>
          Job id, as shown by `sluice queue status`

Options:
  -h, --help
          Print help (see a summary with '-h')
```

</details>

#### `sluice queue reconcile`

Resolve trains whose push outcome is unknown (`needs_reconcile`).

<details>
<summary>Full help</summary>

```text
Resolve trains whose push outcome is unknown (`needs_reconcile`).

Reads the remote and prints one line per train: the decision (landed, ready, queued, cancelled or refused) and the reason. It never pushes. For trains that land through a pull request (`awaiting_merge`) it also reads each pull request with your own `gh` and tells the daemon (merged, closed, head changed, ...); a human running it opens a missing pull request too.

Usage: sluice queue reconcile

Options:
  -h, --help
          Print help (see a summary with '-h')
```

</details>

### `sluice doctor`

Check the installation (git, PSI, daemon, lease, configs, hooks) and optionally collect a redacted diagnostics bundle for bug reports.

Besides the basics, the zero-config checks are:

| Check | Result |
|---|---|
| `shims` | The shims are installed in the stable directory and current (`warn` when missing or stale). First in `PATH` for this shell: in an agent shell `warn` when it is not; in a human shell `ok`, with a note that the agent plugin puts them in `PATH` for agent sessions. |
| `plugin` | Version of the installed agent plugin (read from `installed_plugins.json` under `$CLAUDE_CONFIG_DIR` or `~/.claude`) against the binary: `warn` when they differ, `ok` when equal, missing or unreadable. Never fails. |
| `weights` | For each gate that declares a number, `warn` when the weight the runs actually need (learned, without the ceiling) exceeds the declared ceiling by more than 50 %: raise it or use `weight = "auto"`. |
| `hooks` | Where the clone's hooks stand. With the entry point on (`sluice hooks on`): `ok` when it is effective and complete, `fail` with the reason when something displaced it or a hook of the repository has no Sluice script (fix: `sluice hooks on`). Otherwise: `ok` when a hook in the hooks directory (`core.hooksPath` or `.git/hooks`) calls sluice (for a husky v9 runner, `.husky/_` with an `h` file, the scripts it executes, `.husky/<hook>`, are inspected) or when the lefthook or pre-commit configuration calls sluice; `fail` when a hook manager (husky, lefthook, pre-commit) owns hooks that do not call sluice, or `.sluice.toml` has gates with hooks and the hook files do not call sluice (suggests `sluice hooks on`); `warn` when no hook calls sluice and there are hook files or gates with hooks to judge; `ok` when there is nothing to route. |
| `repo-config` | Where `.sluice.toml` stands. Besides `ok` (parses) and `fail` (does not parse, or missing): in a clone with no `.sluice.toml` whose hooks go through Sluice it is `ok`, "gates are derived from the hooks (n unit(s) can be verified, m left out; see 'sluice gate list')". With the entry point off a missing `.sluice.toml` still fails. |
| `cache` | In a repository: `warn` when the index has skip-worktree or assume-unchanged entries (it names up to 5 paths): the tree cannot be hashed, so the cache is disabled for this worktree. The detail gives `git update-index --no-skip-worktree <path>` / `--no-assume-unchanged <path>`. `ok` otherwise. |
| `target-protection` | In a repository whose `[queue]` remote and target are known (the configured `target`, else the remote's HEAD as already fetched), shows the last answer the forge gave at a `queue enqueue` (kept 30 days): `ok` when the target accepts direct pushes, `warn` when it requires a pull request or is protected and `[queue] land` is `push` (the queue cannot land there; see [Targets protected against direct pushes](merge-queue.md#targets-protected-against-direct-pushes)); with `land = "pull-request"` the protection is expected and the check is `ok`. Doctor never asks the forge itself; with no recorded answer the check is not shown. |
| `queue-pr-branches` | From the running daemon's store (no network; not shown without a daemon or when nothing applies): `ok` with the number of trains waiting for their pull request (`awaiting_merge`), `warn` listing the `sluice/train/<id>` branches that dissolved pull-request trains left behind (a closed pull request, a cleanup that failed), with the `git push <remote> --delete <branch>` that removes them (see [Landing through a pull request](merge-queue.md#landing-through-a-pull-request)). |
| `init` | In a git repository without `.sluice.toml`: `ok`, with the hint `run 'sluice init --dry-run'`. |

The `containment` group (JSON `group: "containment"`; the S4 messages of
ADR-0017) explains what the machine allows: `containment` (the summary,
`full`, `partial (...)` or `off (...)`), `cg-v2`, `cg-writable`, `systemd`,
`user-manager`, `delegate-cpu`, `delegate-memory-pids`, `own-cgroup` (and
`system-bus` in a login scope), `controller-files` (`cpu.weight`,
`memory.current` and `pids.max` present in the agents scope or the user
manager's cgroup), `slice`, `memory-policy` (on or off, with the thresholds
derived from the system's `systemd-oomd` policy shown when on), `gates-oom` (the
`oom_score_adj` gate jobs run at; a warning when an ancestor of the jobs'
cgroup has `memory.oom.group = 1`), `gates-memory-policy` (the oomd policy of the
gates' scope and whether `daemon/` carries `user.oomd_omit`), `memory-guard`
(whether the opt-in `[admission] memory_guard` is on, and a suggestion to turn
it on where oomd cannot act) and, when the CPU cannot be weighted, `fallback`. Inside an agent shell `session` says where
this shell and the agent are (`in ui`, `in tools` or outside the session) and
why. None of them fails the run.

```text
[ok  ] shims          27 shim(s) current in /home/u/.local/share/sluice/shims; first in PATH
[ok  ] plugin         agent plugin and binary are 0.4.0
[warn] weights        'test' needs about 9 but declares weight = 4: raise the number or use weight = "auto" (...)
[ok  ] init           this repository has no .sluice.toml; run 'sluice init --dry-run' to see what Sluice would propose
[ok  ] containment    full
[ok  ] memory-policy  memory policy on: systemd-oomd acts on the agents scope at 30% memory pressure for 15s (derived from the system policy on user@UID.service, 50% / 20s; systemd 259, systemd-oomd active)
[ok  ] session        this shell is in tools (s-7f3a9c/tools); agent (pid 4242) is in ui
```

(`weights` and `init` never appear together: the first needs a
`.sluice.toml`, the second is for repositories without one.) `--json` carries
the same checks with the same names.

<details>
<summary>Full help</summary>

```text
Check the installation (git, PSI, daemon, lease, configs, hooks) and optionally collect a redacted diagnostics bundle for bug reports.

Exits 1 when a check fails, 0 otherwise. Never uses the network. `--bundle` writes a `.tar.gz` with the doctor report, versions, configs, `status --json`, `queue status --json`, recent daemon, client and MCP logs and the logs of the last failed jobs. Home paths, secret-looking values and URL credentials are redacted; review the file before attaching it to an issue.

Usage: sluice doctor [OPTIONS]

Options:
      --json
          Print machine-readable JSON instead of text

      --bundle [<PATH>]
          Write a diagnostics bundle (default `./sluice-doctor-<timestamp>.tar.gz`)

  -h, --help
          Print help (see a summary with '-h')
```

</details>

### `sluice update`

Update sluice to the latest (or a given) release.

<details>
<summary>Full help</summary>

```text
Update sluice to the latest (or a given) release.

Downloads the release archive for this architecture with `curl` (or `wget`), verifies it against `SHA256SUMS`, checks that the new binary runs and replaces the running executable atomically. Then, unless --no-restart, it stops an idle daemon so the next `sluice` call starts the new version; a daemon running gates is left alone. `sluice mcp` servers of agent sessions keep the old binary until the agent reconnects. On a terminal it asks `Update sluice X -> Y? [y/N]`; without one it requires --yes (exit 2). Exit codes: 0 updated, up to date or --check; 1 error (network, checksum, permissions); 2 usage. Environment: `SLUICE_UPDATE_BASE_URL` points at a directory or `file://` URL holding the archives, `SHA256SUMS` and a `LATEST` file (or pass --version).

Usage: sluice update [OPTIONS]

Options:
      --check
          Only report whether an update is available; change nothing

      --version <vX.Y.Z>
          Install this release tag (for example v0.2.0), also an older one

      --yes
          Do not ask for confirmation (required without a terminal)

      --no-restart
          Do not stop the daemon after updating

  -h, --help
          Print help (see a summary with '-h')
```

</details>

## Exit codes

| Command | Codes |
|---|---|
| `sluice run` | 0 pass or cache hit; the gate's own code on failure; 3 refused by `agent_hook_policy = "fail-fast"`; 124 timeout; 143 gate killed by a signal; 1 Sluice error; 2 usage error. |
| `sluice exec` | The command's own code; 128+N when killed by signal N; 124 on `--timeout`; 143 when the daemon stopped and killed it; 127 not found; 126 not executable; 125 Sluice error after the request was accepted; 141 when its own stdout is a closed pipe. |
| `sluice daemon` | 75 when another daemon already holds the lock. |
| `sluice queue` | 0 ok; 1 error; 2 usage; 4 refused by the state of the queue (`conflict-state`, `not-allowed`, `stale-plan`, `duplicate-branch`). |
| `sluice hooks on`, `off`, `status`; `sluice gate list` | 0; 1 on an error (not in a git repository, a git or file failure; `gate list`: no `.sluice.toml` and no unit). |
| `sluice hooks run` (internal) | The hook's own code (delegated or the failing unit's); never 3 from Sluice. 126 when the hook file cannot be executed. 0 when the hook is not installed or the repository cannot be found. |
| `sluice doctor` | 1 when a check fails, 0 otherwise. |
| `sluice update` | 0 updated, up to date or `--check`; 1 error; 2 usage. |

`sluice run --explain <gate>` shows every decision Sluice would take (caller
detection, policy, tree, key, cache lookup) without running anything. When the
cache mode (`SLUICE_CACHE=0`, `CI`, `SLUICE_DISABLE`) or the gate's `cache = false`
turns caching off, it prints `lookup: OFF (<reason>)` instead of a HIT or MISS.

## Environment variables

| Variable | Effect |
|---|---|
| `SLUICE_DISABLE=1` | `sluice run` runs the gate directly (no cache, no budget, no log). Through `sluice mcp` it only turns the cache off. `sluice exec` always runs in-process. |
| `SLUICE_CACHE=0\|1` | Force the cache off or on (on overrides `CI`). |
| `CI` | Any value other than `0`, `false`, `no` or `off` disables the cache. |
| `SLUICE_AGENT=1\|0` | Force agent or human detection. |
| `SLUICE_JOB` | Set by Sluice on every process it runs. A nested `sluice exec` or `sluice run` then runs directly instead of queueing behind its own parent. Not meant to be set by hand. |
| `SLUICE_SHIMS=1\|0` | Shell shims: `1` routes heavy commands through the queue even when no agent is detected; `0` makes every shim run the real binary directly. |
| `SLUICE_SHIM_DIR` | Set by each shim to its own directory, so `sluice shim exec` skips it when resolving the real binary. |
| `SLUICE_CONFIG` | Path of the user config (default `~/.config/sluice/config.toml`). |
| `SLUICE_SOCKET` | Override the daemon socket path. |
| `SLUICE_LOG` | Log filter of `sluice daemon` (written to `~/.local/state/sluice/daemon.log`), `sluice mcp` (stderr and a log file) and `sluice run`; then `[debug] level` of the user config; then `info`. |
| `SLUICE_HOOKS_DELEGATE=1` | With the hooks entry point on, every git hook is delegated untouched (no units). Empty or `0` is off. `SLUICE_DISABLE` has the same effect on hooks. |
| `SLUICE_UPDATE_BASE_URL` | `sluice update`: a directory or `file://` URL holding the release archives, `SHA256SUMS` and a `LATEST` file. |
| `SLUICE_VERSION`, `SLUICE_INSTALL_DIR` | `scripts/install.sh` only: release tag to install (default latest) and target directory (default `$HOME/.local/bin`). |

## Files

- Full output of every run: `~/.local/state/sluice/logs/` (`sluice logs`
  prints the newest one). Other logs live in the same tree; see
  [debugging.md](debugging.md).
- Cache markers: `$(git rev-parse --git-common-dir)/sluice/markers/`.

## MCP tools

`verify` (blocks until the gates finish, with progress notifications and
cancellation), `status`, `gate_list`, `logs`, and for the merge queue
`queue_enqueue`, `queue_status`, `queue_approve` (refused unless the user
config sets `[queue] allow_agent_approval = true`), `queue_retry` and
`queue_cancel`. See [agents.md](agents.md).
