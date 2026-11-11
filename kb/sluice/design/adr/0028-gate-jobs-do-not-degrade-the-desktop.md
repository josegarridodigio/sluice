---
title: "ADR-0028: Gate jobs keep their processes, bound their swap and yield to a degraded desktop"
tags: [sluice, adr, daemon, memory, containment, cgroup, swap, psi, telemetry, chromium]
---

# ADR-0028: Gate jobs keep their processes, bound their swap and yield to a degraded desktop

Status: accepted (2026-11-06). Evidence: [[sluice/research/desktop-hang-2026-11-05.md]].
Research: [[sluice/research/chromium-scope-escape.md]],
[[sluice/research/cgroup-escape-and-swap.md]]. Amends
[[sluice/design/adr/0027-a-kernel-ceiling-on-every-gate-job.md]] (decision 6, "swap is
unchanged", and decision 8, the exemption) and
[[sluice/design/adr/0026-memory-admission-keeps-what-it-learned.md]] (decision 6). Plan:
[[sluice/plan/memory-containment-goal.md]].

## Context

ADR-0027 made sure no gate job can take the machine below a memory floor: since 0.10.0
no OOM and no memory kill happened on three machines. On 2026-11-05 the desktop of one of
them hung anyway after a memory peak (66 % of RAM used, 1.6 GiB more swap, input lag in the
compositor), while the ceiling never bound. The analysis found three gaps and one blind
spot:

1. **Processes leave their job.** Chromium moves its browser process into a systemd scope
   of its own through the session bus (`StartTransientUnit` with its pid), with no switch
   to turn it off; a 2026-11-05 Chromium change moves every descendant too. Its memory then
   escapes the job's working set, learned peak, `memory.max` and kill. Playwright starts
   browsers in a process group of their own, so the job's group kill misses them as well.
   Any program can do the same (`systemd-run --user --scope`, GIO launches).
2. **Swap is unbounded.** A job held at its `memory.max` swaps its own pages out; with no
   `io` controller delegated to the user (user@.service delegates `pids memory cpu`), that
   I/O competes with the desktop's own swap-ins. All six freezes of 2026-11-01 happened
   with swap full.
3. **The desktop has no voice.** Admission and the ceiling look at `MemAvailable` and the
   jobs; nothing looks at whether the user's session is stalling. A job alone on the
   machine still starts beyond its estimate whatever the desktop's state.
4. **No data.** Sluice kept the working set of each job but not its memory pressure, its
   swap, its escaped processes or the session's pressure, so the hang could not be
   attributed.

## Decision

1. **Telemetry (part A).** Every sampler tick (1 s) the daemon reads, for each running job
   with a leaf: `memory.pressure` (`some`/`full` `avg10`), `memory.swap.current`, `anon`
   and `file` of `memory.stat`, and the processes descended from the leaf's processes that
   live in another cgroup (one `/proc` pass per tick, shared with the CPU sampler; only
   descendants outside the leaf's `cgroup.procs` have their `cgroup` file read). Once per
   tick it reads the `memory.pressure` of the user's session: `session.slice` of the user
   manager the daemon runs under (where GNOME and KDE put the compositor and the shell),
   or the manager itself without one; read only. Logged: each process that leaves a job
   (once), a job's first `full avg10 >= 10 %`, the session crossing `some avg10 >= 10 %`
   and falling back under 5 %, and a summary line at the end of any job that swapped,
   stalled or had escapees. Stored per finished job in `job_memory_pressure`
   (`containment_schema` 5, newest 5,000 rows). `status` (CLI and MCP) shows per running
   job its swap, its pressure and its escaped processes, and the session's pressure on the
   `memory:` line.
2. **Escaped processes are moved back, and die with the job.** A process found outside its
   job's leaf is written back into the leaf's `cgroup.procs` in the same tick. The kernel
   only asks for write access to `cgroup.procs` of the common ancestor
   (`user@UID.service`, owned by the user), and `Delegate=` does not protect the scope
   it moved to. When the job ends, whatever is left of it is killed: the leaf with
   `cgroup.kill`, and every escapee seen during the run that is still alive through a
   pidfd, after checking its start time, so a reused pid is never signalled.
   `[containment] reclaim_escaped = true` by default; `false` only reports.
3. **A swap ceiling on every job.** Each tick the daemon writes every leaf's
   `memory.swap.max = swap(job) + (SwapFree − swap_floor) / n`; below the swap floor every
   job keeps what it holds and swaps out no more (lowering `memory.swap.max` swaps nothing
   back in), so a job at its RAM ceiling dies in its own cgroup instead of thrashing.
   `[admission] swap_ceiling = true` by default, `swap_floor = "auto"`: a quarter of
   `SwapTotal`. Written by a second writer thread, like the RAM ceiling. No swap, nothing
   written.
4. **The desktop guard.** While the session's `some avg10` is at or above 10 % (until it
   falls under 5 %): no job starts, not even alone on the machine (the PSI starvation
   guard still applies, so the queue cannot starve forever), and the RAM ceilings keep the
   whole `memory_reserve` free instead of the floor (half of it). `[admission]
   desktop_guard = true` by default.
5. **The exemption stays, under the guard.** A job alone on the machine may still start
   beyond its estimate (ADR-0026 decision 6, ADR-0027 decision 8), but not while the desktop
   guard holds; under it, the RAM ceiling and now the swap ceiling bound it.

## Why these thresholds

- `some avg10 >= 10 %` of the session: some task of the session waited on memory one
  second in ten. No authoritative "interactive" threshold exists; systemd-oomd uses
  `full` at 50-60 % for 20-30 s to kill, Android's lmkd reacts to 70 ms of partial stall in
  one second. 10 % `some` is far earlier than any kill policy, early enough to stop adding
  load, and the clear level (5 %) avoids flapping. To be revised with part A's data.
- Swap floor a quarter of `SwapTotal`: enough for the desktop's own swap traffic, little
  enough not to kill runs that pass with moderate swap today. The per-job swap peaks are
  now recorded to revise it.

## Consequences

- A Chromium browser started by a gate is back in its job within a second, so its memory
  counts in the job's working set from then on (the pages charged while outside stay with
  the scope it left, which systemd removes once empty) and it dies with the job even when
  Playwright leaves it behind.
- A test that deliberately starts something in a unit of its own (`systemd-run --user
  --scope`) sees it pulled back into the job; such a gate sets `reclaim_escaped = false`.
  A process the job daemonised into the leaf no longer survives the job.
- A job that would swap beyond its share dies in its own cgroup, reported `killed_by =
  "memory"` like any ceiling kill, and admission settles (ADR-0026).
- While the desktop is degraded the queue waits; `status` shows the session's pressure so
  a human can tell why.
- Two more files written per running job per second at most (the swap ceiling, skipped
  when unchanged), one `/proc` pass shared with the existing sampler, and one `cgroup`
  read per descendant outside its leaf.
- New config keys `swap_ceiling`, `swap_floor`, `desktop_guard` and `reclaim_escaped` are
  one-way (older binaries reject them): `docs/compatibility.md`. New status fields are
  additive (schema 1).

## Alternatives considered

- **Unset `DBUS_SESSION_BUS_ADDRESS` in gate jobs.** Keeps Chromium (which then sets
  `disabled:`) in place, but it is specific to one program, breaks tests that need the
  session bus, and other programs fall back to `$XDG_RUNTIME_DIR/bus`. Rejected as the
  mechanism; moving back works for any program.
- **The Flatpak/Snap environment trick (`FLATPAK_SANDBOX_DIR`).** Chromium dropped it on
  2026-10-14. Rejected.
- **A cgroup namespace for jobs.** Stops a process from moving itself, not systemd, which
  does the move in the root namespace. Rejected.
- **Count the escapee's scope instead of moving it back.** Accounts its memory, but leaves
  it outside the ceiling and the kill. Rejected.
- **`memory.swap.max = 0` on jobs.** Turns every swap into an OOM, killing runs that pass
  with a little swap; no data says that is needed. Rejected in favour of a share.
- **`memory.low` on the session.** Would protect the desktop from reclaim, but needs root:
  Sluice never changes the machine outside its own cgroups (ADR-0027). Documented as advice.
- **Freeze job growth while the desktop is degraded.** Would kill jobs for pressure Sluice
  may not cause; the guard stops adding load and keeps the reserve instead.

## Open items

- Review after a week of part A data: session pressure during heavy jobs, per-job swap
  peaks, escapes per gate; tune the 10 %/5 % levels and the swap floor.
- Escapes are seen within one tick (1 s): memory charged while outside stays outside.
- The GNOME Shell hang itself is not explained; capturing the shell's threads during the
  next hang (`top -H`, a stack) needs a second machine with SSH access to this one.
- Zero-config hook units use the same containment; nothing to change there.

## Amendment (2026-11-11): the detection never reaches beyond the job

A `0` in a dying job's `cgroup.procs` made every process a descendant of the job and the
daemon moved the user's desktop session into the leaf, where the job's OOM group kill
ended it. The walk now ignores pids 0 to 2, never reports or descends through the daemon,
its ancestors or a process outside `/user.slice/`, in `init.scope` or in `session.slice`,
and more than 64 escapees for one job in one tick is a detection error that is logged and
never acted on. See [[sluice/fixes/escape-detection-reclaimed-the-session.md]].
