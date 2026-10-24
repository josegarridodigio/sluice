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
    ├── sluice-daemon.scope             Delegate=yes, CPUWeight=100 (daemon + gate jobs)
    │   ├── daemon/                     the daemon and its reaper
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
| daemon | job start | gate and exec jobs start through `sluice cg-exec <leaf> -- <argv>`, a helper that writes its own PID to `<leaf>/cgroup.procs` and then `exec`s (no race, like the `nice`/`ionice` wrapper); combined with that wrapper in one argv chain |
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
(daemon, C2); otherwise it creates it with a `sleep 86400` holder (C1
`ensure_agents_scope`), which the daemon later adopts.

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
```

All optional with these defaults; unknown keys rejected as today.

## 8. Status and JSON

`sluice status` gains `containment: full|partial|off (<reason>)` and, per
agent session, `cpu <s> mem <MB> procs <n>`, plus `outside jobs <x>% CPU`
(agents' CPU not spent in Sluice jobs). JSON: optional `containment`,
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
