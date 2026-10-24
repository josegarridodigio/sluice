---
title: "ADR-0017: Contain agent sessions in cgroups without wrapping the agent"
tags: [sluice, adr, containment, cgroups, systemd]
---

# ADR-0017: Contain agent sessions in cgroups without wrapping the agent

Status: accepted (2026-10-24) after the S-wave of
[[sluice/plan/containment-plan.md]]. Design: [[sluice/design/containment.md]].
Builds on [[sluice/design/adr/0014-zero-config-agent-integration.md]].
Evidence: [[sluice/research/containment-s1-sandbox.md]],
[[sluice/research/containment-s2-weights.md]],
[[sluice/research/containment-s2-incident.md]],
[[sluice/research/containment-s3-agent-tree.md]],
[[sluice/research/containment-s4-fallbacks.md]],
[[sluice/research/containment-s5-memory-oomd.md]].

## Context

Interception (guard, shims) puts 99.8 % of 1 901 real heavy agent commands
through Sluice, but no command analysis reaches every process: scripts,
`subprocess`, tools by path. For a team rollout the requirement is that no
process an agent session starts can make the machine unusable, however it is
started, without wrapping the agent's launcher and without per-developer
setup. The S-wave measured the mechanism on Ubuntu 26.04 (systemd 259, kernel
7.0, 20 CPUs) and mapped other systems.

## Decision

1. **Two layers.** Containment (kernel) keeps the machine usable for every
   process of an agent session; the queue, cache and coalescing keep their
   role (no repeated work). An escaped command costs efficiency, not
   stability.
2. **Placement without a wrapper.** The plugin's session hook moves the agent
   (`CLAUDE_PID`, ancestor walk as fallback) and its current descendants into
   the session's `ui` leaf by writing to `cgroup.procs` (5–13 ms), and through
   the user manager's `AttachProcessesToUnit` when the direct write is refused
   (agent started from an SSH/TTY login scope). Later MCP servers and their
   children inherit `ui`. The line the hook writes to `CLAUDE_ENV_FILE` moves
   each shell into `tools` when it can, and otherwise (Bash sandbox: cgroupfs
   read-only) lowers the shell to `nice` 19, which inside one cgroup leaves the
   agent responsive (S2: p99 about 2 ms). Shims do the same before running a
   heavy tool (agents other than Claude Code). Idempotent on resume, `/clear`
   and compaction (the hook fires again; re-attaching is a no-op).
3. **Tree and CPU.** `sluice.slice` (CPU weight 50) holds the daemon with the
   gate jobs (weight 100) and the agents scope (weight 30); per session a
   sub-cgroup with `ui` 400 / `tools` 25 (S2: agent UI p99 0.13 ms at 2x
   oversubscription versus 10 ms in one leaf). The weight that protects the
   desktop is the slice's. Weights are work-conserving (an idle machine gives
   a session 95–99 % of the CPU); no CPU ceiling by default (it made the
   agent's own UI stall and did not help the desktop), available as an option.
   `pids.max` 4096 per session.
4. **Memory.** Never `MemoryHigh` or `MemoryMax` by default: throttling
   without page scans creates pressure that `systemd-oomd` attributes to the
   whole user manager and answers by killing the largest unrelated cgroup
   (incident: the desktop session). Instead, where `systemd-oomd` is active
   and systemd is ≥ 257 (per-unit duration), the agents scope carries its own
   policy, `ManagedOOMMemoryPressure=kill` with a lower limit and a shorter
   duration than the user manager's (default 30 % for 5 s, configurable), so
   `systemd-oomd` acts inside the agents scope first and kills only the
   offending leaf (S5 live T3: only the throttled `tools` leaf was killed; the
   user manager stayed at 14.6 % against its 50 %). `ui` leaves carry
   `user.oomd_avoid`. Elsewhere: accounting only. `MemoryHigh`/`MemoryMax` per
   session stay explicit opt-ins.
5. **Explain every intervention.** A kill raises a generic desktop
   notification ("an application was stopped"); Sluice records each kill in an
   agent session (session, command, reason) and reports it in
   `sluice status`, `doctor` and the agent's next tool results.
6. **Fail open, explained.** Without cgroup v2, a reachable user manager,
   delegated controllers, or a writable cgroupfs, the session runs as in
   0.4.1 (with `nice` for CPU where possible) and `doctor` names the reason
   from the S4 matrix (detection by the presence of `cpu.weight`, not by
   distribution version).
7. **Observability.** Per-session CPU and memory and CPU used outside Sluice
   jobs appear in `sluice status` and its JSON (optional fields, schema 1).

## Consequences

- Not a security boundary: a process of the same user can leave the cgroup.
  It protects against accidental overload.
- Systemd < 252 (Ubuntu 22.04, Debian 11): memory and pids only, CPU by
  `nice`, unless an administrator delegates `cpu` with a drop-in. Systemd
  < 257 (Ubuntu 24.04, Debian 12): no own memory policy (both policies would
  share the 20 s duration), accounting only. Containers and cgroup v1: off.
- An agent's command can be killed under extreme memory pressure; that is the
  intended trade against losing the desktop, and it is always explained.
- Values were measured on a 20-CPU machine; E-wave and user checks repeat them
  on a smaller machine before release.
- Linux only, like the rest of Sluice.
