---
title: "Design: agent resource containment"
tags: [sluice, design, containment, cgroups, systemd]
---

# Design: agent resource containment

Decision: [[sluice/design/adr/0017-agent-resource-containment.md]]. Plan:
[[sluice/plan/containment-plan.md]]. Values below come from the S-wave notes
(`kb/sluice/research/containment-s*.md`); every one is a config default.

## 1. Tree

```
user@UID.service/
└── sluice.slice                        CPUWeight=50
    ├── sluice-daemon.scope             Delegate=yes, CPUWeight=100 (daemon + gate jobs),
    │   │                               own oomd policy where the agents' exists (§4)
    │   ├── daemon/                     the daemon and its reaper; xattr user.oomd_omit=1
    │   └── jobs/<job-id>/              one leaf per gate or exec job
    └── sluice-agents.scope             Delegate=yes, CPUWeight=30, own oomd policy (§4)
        ├── holder/                     a tiny process keeping the scope alive
        └── s-<session-id>/             pids.max=4096; subtree_control +cpu +memory +pids
            ├── ui/     cpu.weight=400  the agent and its MCP servers; xattr user.oomd_avoid=1
            └── tools/  cpu.weight=25   everything the agent's shells run
```

Session ids are the agent's session id sanitised to `[A-Za-z0-9_-]{1,64}`
(`CLAUDE_CODE_SESSION_ID` / hook JSON `session_id`); agents without one use
`s-pid<pid>`. Empty session cgroups are removed by the daemon (every minute,
and at session end when visible).

All unit creation goes through the user manager by shelling out to `busctl
--user` (`StartTransientUnit` with `PIDs`, `Slice`, `Delegate`, `CPUWeight`,
`ManagedOOM*` properties) or `systemd-run --user`, like git is shelled out
(ADR-0002); no D-Bus library. Inside the delegated scopes Sluice owns the
subtree and writes cgroupfs files directly (single-writer rule of
systemd.io/CGROUP_DELEGATION: Sluice never writes to cgroups systemd created
for others).

## 2. Who does what

| Actor | When | Action |
|---|---|---|
| daemon | start | detect (§6); if available, adopt itself into `sluice-daemon.scope` (`StartTransientUnit` with its own PID, then move itself and the reaper into `daemon/`); create `sluice-agents.scope` adopting a holder child (`sluice daemon holder`, sleeps until the daemon stops) and move the holder into `holder/`; enable controllers |
| daemon | job start | gate and exec jobs start through `sluice cg-exec <leaf> --oom-score-adj <N> -- <argv>`, a helper that writes its own PID to `<leaf>/cgroup.procs`, raises its `oom_score_adj` (ADR-0019) and then `exec`s (no race, like the `nice`/`ionice` wrapper); combined with that wrapper in one argv chain |
| daemon | periodic | prune empty `s-*` and `jobs/*` cgroups; read `memory.events`/journal for kills (§5); sample per-session usage for `status` |
| session hook (`sluice agent session-start`) | every fire | ensure the daemon (and so the scopes) exist; create `s-<id>/{ui,tools}`; move the agent PID and its current descendants to `ui` (direct write; on EACCES one `AttachProcessesToUnit sluice-agents.scope /s-<id>/ui`); set `user.oomd_avoid` on `ui`; write the env-file line (§3); fail open within the existing 300 ms watchdog (S3: 5–13 ms for the move) |
| env-file line | before every shell command | move the shell to `tools`, else `renice -n 19` it (§3) |
| shims | before a heavy tool | same as the env-file line, with `s-pid<ppid-agent>` when no session is known |
| `doctor` | on demand | the S4 checks with their messages; current session's placement |

## 3. The env-file line

One guarded line, written once per session (idempotent like the PATH line):

```sh
export SLUICE_SESSION_LEAF='<agents>/s-<id>/tools'; if [ -w '<agents>/s-<id>/tools/cgroup.procs' ]; then echo $$ > '<agents>/s-<id>/tools/cgroup.procs'; else renice -n 19 $$ >/dev/null 2>&1; fi
```

`SLUICE_SESSION_LEAF` (added by C3/C4) is the `tools` leaf of the session; the
shims read it to place themselves before a heavy tool (never inside a Sluice
job, which has its own leaf, and only for paths under `/sys/fs/cgroup`). When
CPU cannot be weighted but the cgroups are usable (`partial` with `cpu`
missing) the line ends with `; renice -n 19 $$ >/dev/null 2>&1` so the shell is
niced after a successful move too. When containment is unavailable the hook
writes no cgroup line, only `renice -n 19 $$ >/dev/null 2>&1` where §6 says
nice applies (cgroup v1/hybrid, no systemd, no controller delegated, `cpu`
missing) and nothing for an unreachable user manager, a read-only cgroupfs or
`enabled = false`. Agents scope creation: the hook uses it when it exists
(daemon, C2); otherwise it creates it with a bounded `sluice daemon holder --lifetime 600`
(C1 `ensure_agents_scope`; it exits after its lifetime, when its scope stops
being active, or when no scope adopted it; the hook checks the scope is active
and holds it, else kills the holder and fails open), which the daemon later
retires.

No output in any branch (S1: `2>/dev/null` alone still printed under zsh and
`|| echo` adds noise). Inside the Bash sandbox the test is false (cgroupfs is
read-only) and the shell stays in `ui` at nice 19 (layout B, S2: agent UI p99
about 2 ms). Never `AttachProcessesToUnit` from the shell: in the sandbox
`$$` is not a host PID (S1).

## 4. Memory

- Default: no `MemoryHigh`/`MemoryMax`. Accounting from `memory.current` and
  `memory.peak` per session.
- Own policy on `sluice-agents.scope`, set at creation, only when
  `systemd-oomd` is active and systemd ≥ 257:
  `ManagedOOMMemoryPressure=kill` with a limit and duration DERIVED from the
  policy that actually watches the user manager on this machine (vendors
  differ: Ubuntu 50 %/20 s, Fedora raised it to 80 %; administrators can
  change it): read the monitored ancestor's limit and duration (`systemctl
  show user@UID.service -p ManagedOOMMemoryPressureLimit -p
  ManagedOOMMemoryPressureDurationUSec`, falling back to `oomctl dump` and to
  oomd's defaults 60 %/20 s from `oomd.conf`), then
  `limit = min(30 %, 0.6 × ancestor limit)` and
  `duration = min(5 s, ancestor duration / 4)`, never below 1 s. Config
  `[containment] memory_pressure_limit` / `memory_pressure_duration` default
  to `"auto"` (this rule) and accept an explicit value as an override. S5 live
  T3 showed the kill lands on the offending leaf inside the scope only.
  Principle: no absolute threshold is on by default; CPU uses weights
  (proportions), memory pressure is a share of time, `pids.max` is a generous
  safety cap. Users never have to tune values for their machine.
- Opt-in per-session `memory_high`/`memory_max` in config; documented with the
  incident and only advised together with swap.
- Gate jobs are the preferred victims of the kernel (ADR-0019 piece 2, amending
  ADR-0017 point 4). `sluice cg-exec --oom-score-adj N` raises the job's
  `oom_score_adj` to `max(N, inherited)` between the move into the leaf and the
  `exec` (an unprivileged process can only raise it; failure is a debug log, the
  job runs anyway). `N` is `[containment] gates_oom_score_adj`, `"auto"` = 800
  (a browser in an agent's leaf runs at 300, the kernel adds `adj x total / 1000`
  to the badness of a process). The adj is applied also when containment is
  unavailable (no cgroup v2, no user manager): the daemon then passes the leaf
  `-` and only the adj is written; with `[containment] enabled = false` nothing
  is wrapped (0.4.1 behaviour). Each job leaf gets `memory.oom.group = 1`, so a
  kill that picks a gate process (global or in-cgroup) takes the whole job
  instead of one worker; **never on `jobs/` or above**: the kernel walks to the
  highest ancestor with the flag (`mem_cgroup_get_oom_group`), so `doctor` reads
  `memory.oom.group` of every ancestor of `jobs/` and warns when one is `1`. S5's
  objection to `oom.group` concerned session cgroups that contain the agent; a
  job leaf never does. `systemd-oomd` does not read `oom_score_adj`.
- Optional ceiling on the gates (ADR-0019 piece 5): `[containment] gates_memory_max`
  and `gates_memory_swap_max` (sizes, empty by default) are written to `memory.max` /
  `memory.swap.max` of `jobs/` when the daemon scope is created or reused
  (`Tree::apply_gates_ceiling`, best effort), never on a job leaf and never on the
  scope or above; unset writes `max`, so removing the key undoes a previous value.
  A hit is an in-cgroup OOM kill only: the kernel picks a task under `jobs/` and the
  leaf's `oom.group` takes the whole job; the leaf's `oom_kill`/`oom_group_kill`
  counters rise, so piece 4 reports it like any memory kill. No `memory.high`
  key is offered (it throttles and creates pressure without killing, S5 §1). `doctor`
  shows a `gates-ceiling` line when either key is set.
- A gate killed for memory says so (ADR-0019 piece 4). In `launch()`, while the
  leaf still exists, the daemon reads its `memory.events` and `memory.peak`.
  The job is `killed` with `killed_by = "memory"` (and
  `Outcome.memory_peak_bytes`) when the leader ended with a signal or exit 137
  **and** `oom_kill`/`oom_group_kill` of the leaf is above zero, or the ending
  is `SIGKILL`-shaped (signal 9 or exit 137) with no counter and one bounded
  journal query finds a `systemd-oomd` line naming `jobs/<id>`. Never from the
  exit code alone; Sluice's own cancels and timeouts keep their status. The
  message reads `<gate> was killed for lack of memory after 41.2s (job peak 18.1
  GiB; 1.4 GiB available at the end)` plus a hint not to retry unchanged.
- Gate jobs are admitted by learned memory (ADR-0019 piece 1; design in
  [[sluice/design/scheduler.md]], "Memory-aware admission"). The 1 s sampler
  reads each job leaf's `anon` + `memory.swap.current` (page cache excluded,
  `MemAvailable` already counts it) and `memory.peak` (shown only); without a leaf
  it sums the `VmRSS` of the job's process group. At job end the working set goes
  to `gate_memory` (containment schema group version 2). This is the only
  prevention piece: pieces 2 and 4 react when memory runs out anyway.
- Gates are the first candidates of `systemd-oomd` (ADR-0019 piece 3, W6c).
  `sluice-daemon.scope` gets its own `ManagedOOMMemoryPressure=kill` exactly when
  the agents' policy is created (oomd active, systemd >= 257, `memory_policy =
  "oomd"`): `derive_gates_policy` gives it the **same limit** as the agents' and
  `duration = max(1 s, ceil(agents / 2))` in whole seconds. The same limit, not a
  laxer one: oomd orders monitored scopes by the shorter duration (measured on two
  machines, `research/memory-gates-w6d-oomd-spike.md`), so a gates' trigger that
  fires less often than the agents' would hand the kill back to the agents, which
  is the incident. The candidates under the scope are the `jobs/<id>` leaves and
  `daemon/`; the `daemon/` leaf is marked `user.oomd_omit=1` right after it is
  created and before the daemon moves in (omit excludes a leaf absolutely, avoid
  only reorders; a dead daemon abandons its jobs). A scope that outlived its
  daemon (jobs kept it alive) gets the properties again with `systemctl --user
  set-property --runtime`; the agents' scope is not touched on reuse. Where the
  policy is not created (no oomd, systemd < 257, `memory_policy = "none"`), the
  kernel preference (piece 2) and admission (piece 1) are what remain, and the
  opt-in guard (§4 below) can cover the rest. `status` shows the policy as
  `containment.gates_oom_policy`; `doctor` as `gates-memory-policy`.
- The Sluice-side guard (ADR-0019 piece 7, W6e, `[admission] memory_guard = true`,
  off by default). Once per sampler tick the daemon reads `MemAvailable`, the
  `full avg10` of the daemon scope's `memory.pressure` (the system's
  `/proc/pressure/memory` when the scope has none) and the working sets of the
  running gate and `exec` jobs. A sample is troubled when `MemAvailable` is below
  the reserve (`memory_reserve`), the jobs together hold more than the reserve
  (a shortage the agents cause is not the guard's business, and a single job
  below the reserve is never touched) and there is real stall: `full avg10` at
  least 10 %, or `MemAvailable` below half the reserve. Trouble that lasts
  3 s of wall time (from the first troubled sample, at least 2 samples, whatever
  the tick) cancels the job with the largest working set (the smallest id
  on a tie) through the normal cancel path (process group kill): `killed`,
  `killed_by = "memory"`, message "cancelled by Sluice for lack of memory: job
  working set X, MemAvailable Y below the reserve Z", an interventions row (cause
  `sluice_memory_guard`, leaf `jobs/<id>`), a warning in the daemon log. Then it
  rests 15 s (oomd's post-action delay) before it can act again. Pure decision:
  `sluice-gate-daemon/src/guard.rs`. It never touches anything but Sluice's own
  jobs. The thresholds are constants, not keys. `doctor` (`memory-guard`) says
  whether it is on and, where oomd cannot act, suggests it.
- Never touch units Sluice did not create; `ManagedOOMPreference`/`MemoryLow`
  for desktop units are documented as administrator drop-ins only.

## 5. Explaining interventions

The daemon watches, for cgroups under `sluice.slice`, `memory.events`
(`oom_kill`, `oom_group_kill`) and the user journal lines of `systemd-oomd`
naming a Sluice path (`journalctl --user -u systemd-oomd`-style read through
the `journalctl` binary, or the system journal when readable). Each event is
stored (time, session, leaf, cause, the leaf's last command line when known)
and shown in `sluice status` (`interventions:`), `status --json`
(`interventions`, optional), `doctor`, and appended once to the next MCP
`verify`/`status` result and the guard's context note for that session.

A job leaf is removed the moment its job ends and the watcher polls every 10 s,
so the poll alone would miss almost every gate kill. Two additions (ADR-0019):
the daemon settles each job leaf itself at the job's end (the row `jobs/<id>`,
cause `oom_kill`/`oom_group_kill` or `oomd_memory_pressure`/`oomd_swap`, deduplicated
with what the poll already recorded and with the later journal read by the unique
`(at_ms, leaf, cause)` index), and the watcher keeps a backstop on the counters
of `jobs/` and of the daemon scope, which count the kills of removed descendants.
Growth that no job record explains, still there at the next pass, becomes the row
"a gate job died of OOM (job unknown)" (leaf `jobs`; a kill in the daemon's own
cgroup is reported as leaf `daemon`).

## 6. Detection and fallbacks (S4)

Order: cgroup v2 by `statfs` magic (not `stat -fc`); user manager reachable;
own cgroup under `user@UID.service` (else attach-by-bus path); delegated
controllers by the presence of `cpu.weight`, `memory.current`, `pids.max` in
a test leaf; `systemd-oomd` active and systemd version for §4; cgroupfs
writable. Outcomes and `doctor` lines follow the S4 matrix: `full`,
`partial (memory, pids; cpu by nice)`, `off (<reason>)`. In every degraded
case the 0.4.1 behaviour stays and the shell still gets `nice` 19 when CPU
cannot be weighted.

## 7. Config (`~/.config/sluice/config.toml`)

```toml
[containment]
enabled = true                 # false: 0.4.1 behaviour
slice_cpu_weight = 50
gates_cpu_weight = 100
agents_cpu_weight = 30
ui_cpu_weight = 400
tools_cpu_weight = 25
sandbox_nice = 19              # layout B
cpu_ceiling = ""               # e.g. "80%" of all CPUs; empty = none
pids_max = 4096
memory_policy = "oomd"         # "oomd" | "none"
memory_pressure_limit = "auto"     # derived from the system's oomd policy (§4); or e.g. "30%"
memory_pressure_duration = "auto"  # derived (§4); or e.g. "5s"
memory_high = ""               # opt-in per session, e.g. "8G"
memory_max = ""                # opt-in per session
gates_oom_score_adj = "auto"   # "auto" (800) or 0..=1000; floor of the jobs' oom_score_adj (§4)
gates_memory_max = ""          # opt-in ceiling of `jobs/` (all gates together), e.g. "20G" (§4)
gates_memory_swap_max = ""     # opt-in swap ceiling of `jobs/`

# [admission] (not [containment]; the guard also works without containment)
# memory_guard = false          # opt-in Sluice-side guard (§4): cancels its own largest job
```
All optional with these defaults; unknown keys rejected as today.

## 8. Status and JSON

`sluice status` gains `containment: full|partial|off (<reason>)` and, per
agent session, `cpu <s> mem <MB> procs <n>`, plus `outside jobs <x>% CPU`
(agents' CPU not spent in Sluice jobs) and, when the gates' scope has its own
oomd policy, `gates memory policy: systemd-oomd at L% for Ds`. JSON: optional
`containment` (with the optional `gates_oom_policy {limit_percent, duration_secs}`),
`sessions[]`, `interventions[]` (schema 1, additive).

## 9. Tests

Unit: detection parsing, config, env line rendering, session id
sanitising. Integration (no AI, no memory pressure): a scope tree created in
a test slice `sluice-test-<pid>.slice`, cg-exec placement, self-move, renice
fallback when the leaf is not writable, pruning, fail-open paths (no user
manager: `XDG_RUNTIME_DIR` pointing to an empty dir; read-only cgroupfs:
unwritable leaf). Harness s9 (E1): escapes interception cannot see (script,
absolute path, Python `subprocess`) end up in `tools` with the measured
weights; gates keep priority; CPU only. Memory behaviour is never tested on a
developer machine except the bounded T3-style check run by the lead with the
user's consent (`kb/sluice/research/containment-s5-memory-oomd.md` §9).
