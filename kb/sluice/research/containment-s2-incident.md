---
title: "Incident: a containment spike's memory test ended the desktop session"
tags: [sluice, research, containment, incident, systemd-oomd, memory]
---

# Incident: a containment spike's memory test ended the desktop session

Date: 2026-10-24, 10:31–10:32 (local). Plan: [[sluice/plan/containment-plan.md]]
spike S2. Decision affected: [[sluice/design/adr/0017-agent-resource-containment.md]]
point 4 (memory by `MemoryHigh`).

## What happened

| Time | Event (from the system journal) |
|---|---|
| 10:31:47 | S2 starts a memory test: a process asking for 600 MB in a transient scope `app.slice/sluice-s2-mem.scope` with `MemoryHigh=200M` and `MemorySwapMax=0` |
| 10:31:47–10:32:18 | The kernel keeps throttling and reclaiming that cgroup (it sat at ~230 MB, above `memory.high`, with no swap). The stall counts in the memory pressure of every ancestor, including `user@1000.service` |
| 10:32:18 | `systemd-oomd`: "Killed …/user@1000.service/session.slice/dbus.service due to memory pressure for …/user@1000.service being 77.76% > 50.00% for > 20s with reclaim activity". The killed cgroup was the largest one (10.3 GB): the session bus, plus the terminal and the agent sessions that had been started through D-Bus activation |
| 10:32:18–19 | GNOME Shell shut down ("Shutting down GNOME Shell"), the browser aborted (`FATAL dbus/bus.cc … D-Bus connection was disconnected`, reported as SIGTRAP), logind removed session 2 |

Ubuntu ships `user@.service.d/10-oomd-user-service-defaults.conf` with
`ManagedOOMMemoryPressure=kill` and `ManagedOOMMemoryPressureLimit=50%`.
`systemd-oomd` watches the pressure of the whole user manager subtree and,
when it is exceeded, kills the descendant cgroup it ranks first (by usage and
reclaim), which need not be the cgroup that causes the pressure.

## Findings

1. **`MemoryHigh` is not a safe limit on its own.** Throttling a cgroup above
   `memory.high` without swap creates sustained memory pressure, and that
   pressure is attributed to every ancestor. With `systemd-oomd` watching the
   user manager, the result is a kill of an unrelated, larger cgroup — here
   the user's whole desktop session. ADR-0017 point 4 ("memory by
   `MemoryHigh`, never kill the agent") must be redesigned around
   `systemd-oomd`: the policy has to make the agent session that causes the
   pressure the one that is acted on, never the desktop.
2. **Where the agent lives matters.** Agents started from a terminal that was
   activated through D-Bus share `session.slice/dbus.service` with the session
   bus. Moving agent sessions into their own cgroup (ADR-0017) also removes
   them from the cgroup `systemd-oomd` picked here, which is a point in favour
   of containment, provided the memory policy is right.
3. **Spikes must not create memory pressure on a user's desktop.** Memory
   behaviour is studied from documentation and in a disposable VM or
   container with its own systemd, never on the developer's machine.

## Cleanup and state

The leftover test scope (`sluice-s2-mem.scope`) and `sluice.slice` were
stopped by the lead after the user reported the crash; no Sluice unit
remains. The S2 spike's partial results were lost with the session (its
worktree had no commits). The process S4 had moved out of the login scope
ended with that session, so that side effect is gone too.

## Follow-up

- S2 is redone for CPU only (weights, latency, gate priority); the CPU rounds
  that ran at 10:20–10:22 completed without effect on the desktop.
- New spike S5 (documentation and a disposable VM only): the memory policy
  with `systemd-oomd` — `ManagedOOMMemoryPressure=` and
  `ManagedOOMMemoryPressureLimit=` on Sluice's own units so that the agents
  scope is acted on before the user manager's limit is reached,
  `ManagedOOMPreference=`, `MemoryHigh` with and without swap, and whether to
  set any memory limit by default at all.
