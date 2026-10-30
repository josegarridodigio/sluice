# Configuration

Sluice reads two TOML files:

- **`.sluice.toml`** at the root of the repository (committed): the gates and
  the merge-queue defaults.
- **`~/.config/sluice/config.toml`** (per user, optional): the daemon budget,
  PSI admission, logging and agent detection. A missing file means defaults.
  `SLUICE_CONFIG` points elsewhere.

Both files start with `version = 1`. This is the **config format version**,
not the Sluice release: it is frozen, and only additive, optional fields are
added to it. Unknown fields are errors at every level, so a typo cannot
silently change behaviour.

For ready-to-adapt files per stack see [gates-cookbook.md](gates-cookbook.md).

## Repository config: `.sluice.toml`

```toml
version = 1

[defaults]
agent_hook_policy = "fail-fast"   # fail-fast | wait | run-unqueued

[gates.lint]
cmd = "npm run lint"
weight = 1
hooks = ["pre-commit"]

[gates.test]
cmd = "npm test -- --maxWorkers=2"
weight = 4
timeout = "20m"
hooks = ["pre-commit", "pre-push"]
key_inputs = ["node --version"]
key_files  = [".env.test"]
env        = { NODE_ENV = "test" }
paths        = ["src/**", "package.json", "package-lock.json"]
paths_ignore = ["src/**/*.md"]
```

The file is read from the current worktree's root; there is no parent-directory
search. Each worktree may carry its own revision of it: the gate definition is
part of the cache key. A file with only `version = 1` is valid (every
`sluice run` then fails with `unknown gate`).

### Top level and `[defaults]`

| Field | Type | Default | Notes |
|---|---|---|---|
| `version` | int | required | Config format version; must be `1`. |
| `defaults.agent_hook_policy` | `fail-fast` \| `wait` \| `run-unqueued` | `fail-fast` | What a hook invoked by an agent does on a cache miss. |
| `defaults.human_hook_policy` | `wait` \| `run-unqueued` | `wait` | Same, for a human. `fail-fast` is not allowed here. |
| `defaults.tail_lines` | int | unset | Log lines shown on failure for this repository; overrides the user config's `cache.tail_lines` (40). |

### `[gates.<name>]`

Gate names match `[a-z0-9][a-z0-9_-]*` and are at most 64 characters.

| Field | Type | Default | Notes |
|---|---|---|---|
| `cmd` | string | required | Run with `sh -c` in the worktree root. Must not be empty. |
| `weight` | int >= 1 or `"auto"` | `1` | Budget units consumed while running, at most: the number is a ceiling that learned weights only lower. Roughly the number of CPU threads the gate uses. `"auto"` (0.4.0) uses `ceil(budget / 2)` of the daemon that admits the job and learns from there, so you need not guess; an omitted `weight` stays `1`. |
| `timeout` | duration | `30m` | Execution time only; queue wait does not count. Formats such as `90s`, `5m`, `1h`. |
| `stall_timeout` | duration or `"off"` | `max(5m, 3 x expected duration)` | How long the running job may show no progress (no CPU time, block I/O, page faults, output or process change of its own) before `status` and the clients report it as stalled. Report only: Sluice never ends a stalled job (ADR-0024), `timeout` stays the only limit. A value below `1m` is raised to `1m`; `"off"` for gates that legitimately idle (watch and serve gates). The default scales with the gate's learned duration, so a slow suite is not reported early. Needs the release after 0.7.0: older binaries reject the key. |
| `hooks` | list | `[]` | Hook names this gate is meant for. Free-form; `pre-commit` and `pre-push` are recognised by `sluice hook print`. `verify` without an explicit gate list runs the gates whose `hooks` contain `pre-commit`. |
| `key_inputs` | list of commands | `[]` | The stdout of each command (`sh -c` in the worktree root, with exactly the gate's environment and nothing inherited from the process that derives the key) is folded into the cache key. They run on every cache consultation, hits included, so keep them fast. A failing command makes the run uncached, never a gate failure. |
| `key_files` | list of paths | `[]` | Relative to the repository root, no `..`. The content hash is part of the key. For files git ignores; a missing file counts as "absent". |
| `env` | table | `{}` | Added to the gate's environment and to the key. |
| `cache` | bool | `true` | `false` makes the gate always run (still queued). |
| `mode` | `"gate"` \| `"queue-only"` | `"gate"` | `queue-only`: always runs under the budget, never cached, identical simultaneous requests share one run, and the `fail-fast` policy does not apply (the hook waits). `cache = true` is an error with it. `sluice hook print` and `sluice doctor` warn when a queue-only gate has `hooks`. Needs Sluice 0.3.0 or later. |
| `paths` | list of patterns | unset | When set, the cache key covers only the tree entries matching one of these patterns, plus `.sluice.toml`. Must not be empty. Needs 0.3.0 or later. |
| `paths_ignore` | list of patterns | `[]` | Entries matching any of these are removed from the scope. Alone, it means "everything except". Needs 0.3.0 or later. |
| `depends_on` | list of gate names | `[]` | Validated (the gates must exist, no self-reference) but reserved: not scheduled. |

Binaries older than 0.3.0 reject a `.sluice.toml` that uses `mode`, `paths` or
`paths_ignore` (`unknown field`), so every clone and worktree running Sluice on
that file needs 0.3.0 or later.

### Path scoping

`paths` and `paths_ignore` let a change outside a gate's scope keep its cached
pass. It is opt-in: a gate that reads files outside its scope can pass stale.

- Patterns are git glob pathspecs, relative to the repository root whatever the
  current directory, byte-wise and case-sensitive. Leading `/`, `./`, `..`,
  `:` (magic pathspecs), `!` (negation), backslashes and control characters are
  rejected; at most 64 patterns per list.
- A pattern without `*`, `?` or `[` is literal: it matches the path itself and,
  as a directory, everything under it (`docs` matches `docs/a/b.md`; `doc`
  does not match `docs/x`).
- A pattern with wildcards must match the full path: `*` and `?` never cross
  `/`, `**/` matches zero or more leading directories, a trailing `/**`
  matches everything inside. A wildcard pattern does not recurse into a
  directory it matches: `src/1*` selects nothing under `src/12/`; write
  `src/1*/**`.
- Selected means (`paths` unset or any `paths` pattern matches) and no
  `paths_ignore` pattern matches; `.sluice.toml` is always added.
- Every entry of the working-tree snapshot counts, with mode and blob id (so
  `chmod +x` and a symlink retarget invalidate). Ignored files never do: use
  `key_files` for those.
- With `cache = false` they are accepted and have no effect.

See "Choosing paths" in [gates-cookbook.md](gates-cookbook.md).

### `[queue]` (merge queue defaults)

All optional. The daemon reads them from the base commit
(`git show <base>:.sluice.toml`), never from a queued branch or the working
tree; a missing or invalid file at the base yields the defaults. See
[merge-queue.md](merge-queue.md).

| Field | Type | Default | Notes |
|---|---|---|---|
| `remote` | string | `origin` | Remote of the default target. Must not be empty, start with `-` or contain whitespace; at enqueue it must be a configured remote. |
| `target` | string | the remote's HEAD branch | Default target branch for `sluice queue enqueue`. |
| `gates` | list | gates with a `pre-push` hook, else every gate | Gates a train must pass; each must exist. The gates whose `hooks` contain `pre-push` are always added to the list (the repository's `pre-push` hook runs them during the train's push and is answered from the train). |
| `max_train` | int >= 1 | `4` | Jobs combined into one train; `1` disables batching. |
| `audit_ref` | bool | `true` | Push `refs/sluice/landed/<sha>` atomically with the target. |
| `push_timeout` | duration | `10m` | Limit for the push, including the repository's `pre-push` hook. |
| `network_timeout` | duration > 0 | `60s` | Limit of every other network call of the queue (fetch of the base, `ls-remote` checks). |
| `prepare` | string | unset | Command run once per train in the merge worktree before the gates (install the dependencies a fresh worktree lacks, for example `pnpm install --frozen-lockfile --prefer-offline`). Must not be empty. See "Preparing the merge worktree" in [merge-queue.md](merge-queue.md). Needs 0.6.0 or later; older binaries reject the file. |
| `prepare_timeout` | duration > 0 | `10m` | Limit of `prepare` (and of the probe that attributes its failure). Needs 0.6.0 or later. |
| `prepare_weight` | int >= 1 or `"auto"` | `"auto"` | Budget units `prepare` consumes, like a gate's `weight`. Needs 0.6.0 or later. |
| `land` | `"push"` or `"pull-request"` | `push` | How a verified train reaches the target: one push of the verified commit to it, or a push to `sluice/train/<id>` and a pull request that the forge and a human merge (see "Landing through a pull request" in [merge-queue.md](merge-queue.md)). Read from the target commit's file, else from the user config's `[queue] land`; a queued branch's own file never sets it. One-way: needs the release after 0.7.0, older binaries reject the file. |

The per-user queue keys below are rejected here, and the keys above are
rejected in the user file.

## User config: `~/.config/sluice/config.toml`

Every section and field is optional.

```toml
version = 1

[daemon]
budget = 8                     # default: max(2, available CPUs)
nice = 10
ionice_class = "best-effort"   # none | best-effort | idle
ionice_level = 7
log_retention_days = 7
idle_exit = "0s"
last_subscriber_gone = "cancel"   # cancel | finish

[admission]
psi_enabled = true
cpu_some_avg10_max = 60.0
memory_some_avg10_max = 20.0
psi_poll_interval = "1s"
psi_max_wait = "5m"
overcommit = 1.5               # 1.0 disables over-admission
cpu_some_avg10_low = 25.0
memory_some_avg10_low = 5.0
overcommit_interval = "5s"
memory_aware = true            # admit by learned memory, not only by CPU
memory_reserve = "auto"        # "auto" (10 % of the machine's memory) or a size such as "3G"
memory_guard = false           # opt-in: cancel Sluice's own largest gate job under real memory exhaustion

[scheduler]
head_reserve_after = "30s"
backfill = true                # jobs that cannot delay the reserved head start before it
learned_weights = true
fairness = "lane"              # lane | off
human_first = true
order = "fifo"                 # fifo | sjf

[cache]
ttl_days = 30
tail_lines = 40

[mcp]
progress_interval = "15s"
verify_parallel = true

[queue]
enabled = true
allow_agent_approval = false
tick = "5s"
retention = "30d"
# tool_env = "login-shell"        # login-shell | daemon
# tool_path = "/opt/node/bin:/usr/bin:/bin"
# land = "pull-request"           # push | pull-request: default of the repositories' `[queue] land`

[debug]
level = "info"
daemon_log_max_mib = 20

[agent_detection]
env_any_set = ["CLAUDECODE"]
env_any_equals = {}

[agent]
# heavy_commands = ["make", "cargo build"]   # replaces the built-in list
heavy_commands_extra = []                    # appended to it
exec_weight = 0                              # 0 = ceil(budget / 2)

[containment]
enabled = true                 # false: no cgroups (0.4.1 behaviour)
slice_cpu_weight = 50
gates_cpu_weight = 100
agents_cpu_weight = 30
ui_cpu_weight = 400
tools_cpu_weight = 25
sandbox_nice = 19
cpu_ceiling = ""               # e.g. "80%" of all CPUs; empty = none
pids_max = 4096
memory_policy = "oomd"         # oomd | none
memory_pressure_limit = "auto" # or "30%"
memory_pressure_duration = "auto"   # or "15s"
memory_high = ""               # opt-in per session, e.g. "8G"
memory_max = ""                # opt-in per session
gates_oom_score_adj = "auto"   # "auto" (800) or 0 to 1000
gates_memory_max = ""          # opt-in ceiling of all gate jobs together, e.g. "20G"
gates_memory_swap_max = ""     # opt-in swap ceiling of the same

[repos."/path/to/repo"]
budget_share_max = 4
```

| Field | Default | Notes |
|---|---|---|
| `daemon.budget` | `max(2, cpus)` | Total weight of gates running at once; `cpus` honours the daemon's CPU affinity (`taskset`) and cgroup CPU limit. Must be >= 1. (0.2.0 and older: `max(2, cpus - 1)`.) |
| `daemon.nice` | `10` | 0 to 19; applied to gate processes. |
| `daemon.ionice_class` | `best-effort` | `none`, `best-effort` or `idle`. |
| `daemon.ionice_level` | `7` | 0 to 7. |
| `daemon.log_retention_days` | `7` | Job logs older than this are pruned daily by the daemon. |
| `daemon.idle_exit` | `0s` | Parsed but has no effect: the daemon never exits when idle. |
| `daemon.last_subscriber_gone` | `cancel` | `cancel` or `finish`: what happens to a job when everyone waiting for it is gone. |
| `admission.psi_enabled` | `true` | Hold new work while the machine is under pressure (Linux PSI). |
| `admission.cpu_some_avg10_max` | `60.0` | Percent, 0 to 100: do not admit new jobs above this. |
| `admission.memory_some_avg10_max` | `20.0` | Percent, 0 to 100. |
| `admission.psi_poll_interval` | `1s` | Must be > 0. |
| `admission.psi_max_wait` | `5m` | While PSI blocks, admit the queue head once it has waited this long. Must be > 0. |
| `admission.overcommit` | `1.5` | Over-admission: a job that does not fit the budget may start, up to `floor(overcommit × budget)` units, while the machine is calm (both PSI values below the `*_low` marks, not right after a block) and at least `ceil(weight / 3)` CPUs are idle; at most one such start per `overcommit_interval`. `1.0` turns it off. 1.0 to 4.0. Needs PSI. |
| `admission.cpu_some_avg10_low` | `25.0` | Percent: over-admission only below this CPU pressure. |
| `admission.memory_some_avg10_low` | `5.0` | Percent: over-admission only below this memory pressure. |
| `admission.overcommit_interval` | `5s` | At most one over-admission per interval, and none sooner than this after PSI stopped blocking. Must be > 0. |
| `admission.memory_aware` | `true` | Memory-aware admission: a job starts only when the memory it is expected to use fits `MemAvailable` (capped by any cgroup memory limit above the daemon, as in a container or a unit with `MemoryMax=`, and by `[containment] gates_memory_max`) minus the reserve and minus what the running jobs are still expected to grow by (jobs started in the same pass count in full), or when nothing else runs (and nothing started earlier in the same pass), so an idle machine always starts the first job. The expectation is the largest working set (`anon` + swap of the job's cgroup, or the resident set without one) of the gate's last 5 finished runs plus 20 %; a run killed for lack of memory counts 25 % more; a gate with no passing run is estimated instead: `1.2 x` the largest peak of its recent failed or timed-out runs when it has any, else the 75th percentile of the learned peaks of the gates of the same or lighter weight, clamped between 1 GiB and its weight share (`weight / budget`) of the usable memory (the share alone when nothing is learned); such a first run may start as a memory probe when a quarter of its estimate is free, one per gate and one per machine at a time, and neither a guess nor a lower bound ever allows over-admission (the next release; 0.6.1 lowered the guess to the largest learned peak of lighter gates). Swap never counts as available. A job that waits only for memory shows `wait_detail: memory` in `sluice status`. `false` restores CPU-only admission. Needs 0.6.0: older binaries reject the key. |
| `admission.memory_guard` | `false` | Opt-in Sluice-side memory guard, for machines where `systemd-oomd` is absent or older than systemd 257. On every sampler tick the daemon looks at `MemAvailable`, the memory pressure (`full avg10` of the gates' scope, or the system's) and the working sets of the running gate and `exec` jobs. When `MemAvailable` is below `memory_reserve`, the jobs together hold more than the reserve, and the machine stalls (`full avg10` of 10 % or more, or `MemAvailable` below half the reserve) for at least 3 seconds (wall time, whatever the sampling period), it cancels the job with the largest working set (`killed`, `killed_by = "memory"`, an `interventions` row with cause `sluice_memory_guard`) and rests 15 seconds. It never touches anything but Sluice's own jobs, and never a single job that holds less than the reserve. Needs 0.6.0: older binaries reject the key. |
| `admission.memory_reserve` | `"auto"` | Memory this admission leaves for the desktop, the agents and the page cache: `"auto"` is a tenth of the machine's memory (3 GiB on 30 GiB), or a size such as `"3G"` or `"512M"`. Needs 0.6.0. |
| `scheduler.head_reserve_after` | `30s` | Backfill behind the queue head freely only this long; after it the head holds a reservation (see `backfill`); also the ageing bound: a job waiting longer goes ahead of everything else, in arrival order. |
| `scheduler.backfill` | `true` | A reserved head (overdue, blocked by budget or memory) has a predicted start, its shadow time, from the running jobs' learned durations. A job behind it still starts when it fits now and either the head would still fit at its shadow time beside it and every job already started behind the head (*extra resources*, no duration needed), or it has a learned duration and `max(1.5 x duration, duration + 10 s)` ends before the shadow time (*ends in time*). A job with no learned duration uses only the first rule; a memory probe is never a backfill; nothing starts while pressure blocks admission, and nothing is killed for overrunning. `false`: the strict reservation, nothing behind the head starts. Needs the release after 0.7.0: older binaries reject the key. |
| `scheduler.learned_weights` | `true` | After 3 runs, a gate holds the 75th percentile of its measured CPU demand (CPU plus run-queue wait over wall time, last 10 runs), never more than its `weight` (for `"auto"`, half the budget) and never less than a quarter of it. `sluice exec` jobs learn the same way, per repository and `--label`, starting from their `--weight` or `agent.exec_weight`. |
| `scheduler.fairness` | `"lane"` | `lane`: at equal priority, worktrees take turns (a worktree with jobs running or several queued goes behind the others). `off`: arrival order. |
| `scheduler.human_first` | `true` | Jobs a person waits for (`sluice run` from a terminal, human hooks) go before agent jobs. |
| `scheduler.order` | `"fifo"` | `fifo` or `sjf` (shortest expected job first with ageing: highest `(waited + expected) / expected` next, expected = median of the gate's last runs, 60 s when unknown). |
| `cache.ttl_days` | `30` | Age after which a marker is ignored. |
| `cache.tail_lines` | `40` | Log lines shown on failure (`defaults.tail_lines` overrides it per repository). |
| `mcp.progress_interval` | `15s` | Progress notifications while `verify` runs (floored at 1s). |
| `mcp.verify_parallel` | `true` | `verify` starts all its gates at once and, with its `fail_fast` parameter (default true), returns at the first failure, cancelling the gates nobody else waits for. `false`: one gate after another, every gate runs (0.2.0 behaviour). |
| `queue.enabled` | `true` | Kill switch for the merge queue runner. |
| `queue.allow_agent_approval` | `false` | Let agents approve trains: the MCP `queue_approve` tool, and `sluice queue approve` run from an agent's environment. |
| `queue.tick` | `5s` | Runner housekeeping period (floored at 50 ms). |
| `queue.retention` | `30d` | Finished queue rows older than this are pruned at daemon start and hourly. |
| `queue.tool_env` | `"login-shell"` | Where merge trains get the environment of their tools (gates, `prepare`, `key_inputs`): `login-shell` resolves your login shell's environment once when the daemon starts (a restart picks up a new toolchain) and falls back to the daemon's own environment, with the reason in `sluice status` and the daemon log, when it cannot; `daemon` uses the daemon's own environment, whoever started it. Never the environment of whoever enqueued. Not understood by 0.6.0 and older. |
| `queue.tool_path` | unset | An explicit `PATH` for merge trains' tools; wins over `tool_env` for `PATH`. Must not be empty. Not understood by 0.6.0 and older. |
| `queue.land` | unset (`push`) | Default of `[queue] land` for repositories whose target commit does not set it: `push` or `pull-request` (see [merge-queue.md](merge-queue.md)). The repository's own value wins. Not understood by 0.7.0 and older. |
| `debug.level` | `info` | Log filter (`RUST_LOG` syntax, for example `sluice_gate_daemon=debug,info`) of the daemon, `sluice mcp` and `sluice run`. `SLUICE_LOG` wins. |
| `debug.daemon_log_max_mib` | `20` | Rotate `daemon.log` above this size (0 = never); keeps `daemon.log.1` to `.3`. |
| `agent_detection.env_any_set` | `["CLAUDECODE"]` | Any of these variables set and non-empty means the caller is an agent (`SLUICE_AGENT` overrides). |
| `agent_detection.env_any_equals` | `{}` | Variable/value pairs that mean agent, for example `{ CLAUDE_CODE_ENTRYPOINT = "cli" }`. |
| `agent.heavy_commands` | built-in list | Replaces the list of commands that the plugin guard rewrites to `sluice exec` and that the shell shims route through the queue. Entries are command words (`cargo build`, `docker compose build`), compared after package-manager equivalences (`pnpm build`, `npm run build` and `yarn build` are all `run build`). The built-in list is in [Heavy commands](#heavy-commands-with-no-config). Entries must not be empty, contain control characters or have more than 8 words; at most 256 entries with `heavy_commands_extra`. |
| `agent.heavy_commands_extra` | `[]` | Appended to the built-in (or replaced) list. |
| `agent.exec_weight` | `0` | Default weight (ceiling) of `sluice exec` when `--weight` is absent; `0` means `ceil(budget / 2)`. Since 0.4.0 the daemon lowers it after 3 runs of the same repository and label, from the measured demand (never below a quarter of it). |
| `containment.enabled` | `true` | Agent resource containment in cgroup v2 through the systemd user manager (ADR-0017). `false` keeps the 0.4.1 behaviour. Where cgroup v2, a reachable user manager or the delegated controllers are missing, containment degrades by itself and `sluice doctor` says why. |
| `containment.slice_cpu_weight` | `50` | `CPUWeight` of `sluice.slice` against the rest of the user's session. 1 to 10000. |
| `containment.gates_cpu_weight` | `100` | `CPUWeight` of the scope holding the daemon and the gate jobs. 1 to 10000. |
| `containment.agents_cpu_weight` | `30` | `CPUWeight` of the scope holding the agent sessions. 1 to 10000. |
| `containment.ui_cpu_weight` | `400` | `cpu.weight` of a session's `ui` leaf (the agent and its MCP servers). 1 to 10000. |
| `containment.tools_cpu_weight` | `25` | `cpu.weight` of a session's `tools` leaf (the commands the agent runs). 1 to 10000. |
| `containment.sandbox_nice` | `19` | `nice` for shells that cannot be moved to `tools` (cgroupfs read-only in a sandbox). 0 to 19. |
| `containment.cpu_ceiling` | `""` | Optional hard CPU ceiling of the agents scope, a percentage of all CPUs (`"80%"`, 1% to 100%). Empty means none: weights already give the CPU back when it is idle. |
| `containment.pids_max` | `4096` | `pids.max` of a session. Must be >= 1. |
| `containment.memory_policy` | `"oomd"` | `oomd`: where `systemd-oomd` is active and systemd is 257 or newer, the agents scope gets its own memory pressure policy, so `systemd-oomd` acts inside it first and kills only the offending leaf. The gates' scope (`sluice-daemon.scope`) then gets its own policy with the same limit and half the duration (rounded up, at least 1s), so oomd tries the gates before the agents, and `daemon/` is marked `user.oomd_omit` so the daemon itself is never chosen. `none`: memory is only accounted. Never a `MemoryHigh` or `MemoryMax` by default. |
| `containment.memory_pressure_limit` | `"auto"` | `ManagedOOMMemoryPressureLimit` of the agents scope (and of the gates' scope, see `memory_policy`): `"auto"` or `"1%"` to `"99%"`. `auto` reads the policy that watches your user manager (`systemctl show user@UID.service`, then `oomctl dump`, then `oomd.conf`, then oomd's built-in 60%) and uses `min(30%, 0.6 x its limit)`, so it always stays below it. |
| `containment.memory_pressure_duration` | `"auto"` | `ManagedOOMMemoryPressureDurationUSec` of the agents scope: `"auto"` or a duration above zero. `auto` is `min(15s, 3/4 of the user manager's duration)` in whole seconds, never below 1s (0.8.0 and older: `min(5s, /4)`). The gates' scope gets half of it, rounded up to whole seconds and never below 1s (see `memory_policy`). |
| `containment.memory_high` | `""` | Opt-in `memory.high` per session, a size (`"8G"`, `"512M"`). Empty means none. Throttling without swap can raise pressure that `systemd-oomd` attributes to the whole user manager: advised only together with swap. |
| `containment.memory_max` | `""` | Opt-in `memory.max` per session (the kernel kills inside the session when it is hit). Empty means none. |
| `containment.gates_oom_score_adj` | `"auto"` | Floor of the `oom_score_adj` of gate and `exec` jobs: `"auto"` (800) or an integer from 0 to 1000. The kernel's OOM killer then prefers a gate's processes to the agents' (their browsers run at 300). A job never gets less than the daemon itself inherited, because an unprivileged process can only raise the value. Each job also gets `memory.oom.group = 1` on its own cgroup, so a killed job dies whole and is reported as `killed_by = "memory"`. Applied also where cgroups are unavailable. Needs 0.6.0: older binaries reject the key. |
| `containment.gates_memory_max` | `""` (none) | Optional `memory.max` of the `jobs/` cgroup, the parent of every gate and `exec` job (a size such as `"20G"`). Written on `jobs/` only, never on a job's own cgroup, the daemon scope or above, so the daemon and the agents are never inside the limit. Past it the kernel's OOM killer picks a task in `jobs/`, and because each job cgroup has `memory.oom.group = 1` the whole job dies and is reported as `KILLED for lack of memory`. Empty writes `max`, so removing the key undoes an earlier value. Gate and `exec` jobs only; no `memory.high` counterpart exists on purpose (it throttles instead of killing and builds up memory pressure). Needs 0.6.0: older binaries reject the key. |
| `containment.gates_memory_swap_max` | `""` (none) | Optional `memory.swap.max` of `jobs/`, same rules. Without it a job that reaches `gates_memory_max` swaps first and is killed later. Needs 0.6.0. |
| `repos."<path>".budget_share_max` | unset | Optional cap on the budget one repository may use at once. Must be >= 1. |

### Heavy commands with no config

With no `[agent]` section at all, the guard and the shims already know these
(0.4.0 added everything from `ng build` on):

| Group | Entries |
|---|---|
| Containers and generic builders | `docker build`, `docker buildx build`, `docker compose build`, `make`, `gradle`, `mvn`, `ninja`, `cmake --build`, `bazel build`, `bazel test` |
| Rust, Go, .NET, Python | `cargo build`, `cargo test`, `cargo clippy`, `cargo check`, `cargo doc`, `cargo nextest`, `go build`, `go test`, `dotnet build`, `dotnet test`, `pytest` |
| JavaScript tools | `run build` (so `npm run build`, `pnpm build`, `yarn build`), `tsc`, `vite build`, `webpack`, `next build`, `jest`, `vitest`, `react-scripts build\|test`, `astro build`, `nuxt build` |
| Angular, Nx, Turborepo | `ng build\|test\|lint\|e2e`, `nx run\|run-many\|affected\|build\|test\|lint\|e2e`, `turbo run\|build\|test\|lint` |
| Browser test runners | `playwright test`, `cypress run` |

**Launchers.** `npx X`, `bunx X`, `pnpm exec X`, `pnpm dlx X`, `yarn exec X`,
`yarn dlx X`, `npm exec X` and `npm x X` are classified as `X` (so `npm exec
playwright test --config=x` and `pnpm dlx vitest run` are heavy). `-y`,
`--yes` and a `--` before `X` are skipped; any other option before `X` (for
example `npm exec -p pkg X`, `npm exec --help`) stops the unwrap and the
command is not heavy. At most 4 launchers are unwrapped.

**Never heavy.** Dev servers and watch modes never end, so queueing them would
hold a slot forever: `ng serve`, `nx serve`, `next dev`, `astro dev|preview`,
`nuxt dev|preview`, `react-scripts start`, `vite` alone or with only options
(and `vite dev|serve|preview`), and any command with `--watch`,
`--watch=true`, `--watchAll` (`--watch=false` is not a watch; `-w` counts only
for `tsc`, `jest`, `vitest`, `webpack` and `vite`). This rule wins over the
list and over `heavy_commands_extra`.

**Package scripts.** `npm run <script>`, `npm test`, `pnpm <script>` and
`yarn <script>` are classified by what the script runs, not by its name. The
nearest `package.json` from the command's directory upwards (up to the git
root, at most 1 MiB) supplies `scripts.<name>`; its body is split on `&&`,
`||`, `;`, `|` and `&`, leading `NAME=value`, `env` and `cross-env` words are
dropped, and each piece is classified with the same rules, following nested
`npm run` calls (depth 4). The script is heavy when any piece is: `npm run
axe` that runs `playwright test` is queued, `npm run start` that runs `ng
serve` is not. An invalid or missing `package.json` means not heavy.
`npm run build` is heavy by name. `-C`, `--dir`, `--prefix` and `--cwd` are not
followed.

The daemon reads `[daemon]`, `[scheduler]`, `[admission]`, `[repos]`, `[queue]`,
`[debug]` and `cache.*` once at start: run `sluice daemon restart` to apply
changes (`sluice daemon restart --debug` forces `debug` logging for that
instance only). The client and the MCP server re-read the file on every call.

Environment variables that override or complement these files are listed in
[cli.md](cli.md#environment-variables). Durations are strings such as `90s`,
`5m`, `1h`.
