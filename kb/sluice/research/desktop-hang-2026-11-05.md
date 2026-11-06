---
title: "Desktop hang of 2026-11-05 under Sluice 0.10.0 (evidence for ADR-0028)"
tags: [sluice, research, incident, memory, swap, containment, chromium, psi]
---

# Desktop hang of 2026-11-05 under Sluice 0.10.0

Read-only analysis made on 2026-11-06 on the affected workstation (20 threads, 30 GiB RAM,
8 GiB swap, GNOME Shell 50 on Wayland, NVIDIA open kernel module, kernel 7.0), plus the
daemon logs of two other team machines on 0.10.x. Sources: the systemd journal, sysstat
(`sar`, 10-minute samples), the daemon's `daemon.log` and `sluice.db` (opened read-only),
and the timestamps of the user's prompts in the agent sessions. Decision taken from it:
[[sluice/design/adr/0028-gate-jobs-do-not-degrade-the-desktop.md]].

## What the user saw

Only the mouse pointer moved; no window, key or key combination answered, except the
magic SysRq keys, which the user used to reboot (Emergency Sync, Remount R/O, reboot) at
20:37 CET. That is the signature of a hung compositor main thread, not of a frozen
kernel: the cursor is a hardware plane the kernel moves, mutter reads input on a thread of
its own, and SysRq is handled by the kernel.

## Timeline (CET)

| Time | Evidence |
|---|---|
| 18:40-19:20 | Memory peak: up to 21 GiB used (66 %), swap from 3.6 to 5.2 GiB, swap-out up to 1,759 pages/s, load average 32. GNOME Shell logs "Key repeat discarded, Wayland compositor doesn't seem to be processing events fast enough", input lag of 61-80 ms ("your system is too slow") and `value "inf" ... for property 'scale-x'` warnings. |
| 19:03 | The user's last typed prompt in any agent session. |
| 19:18 | GNOME Shell's last ordinary log line. |
| 19:30-20:30 | Memory back to 15-30 % used, swap steady, nobody at the keyboard, yet 5-6 % user CPU throughout (one of 20 CPUs busy; the idle reference on the same machine is 0.6-2 %). |
| 20:04 | The last Sluice job ends; the daemon stays idle. |
| 20:37 | SysRq reboot. No kernel OOM, no systemd-oomd kill, no Sluice memory kill in the whole day. |

There are no gaps longer than 45 s in the journal: the kernel and journald kept running.

## What ran during the peak

- A frontend `test` gate (Angular/Karma, about 14.5-15 GiB of `anon` + swap) started three
  times "alone beyond its estimate" (learned 17.7-21.2 GiB, 12.9-16.4 GiB free; ADR-0026
  decision 6, kept by ADR-0027 decision 8).
- Playwright `exec` jobs of 7.7-9.4 GiB (working set of the job leaf) in parallel.
- An agent's security audit building the same frontend (`ng build --configuration
  production`) in `bwrap` sandboxes started with `systemd-run --user`, outside Sluice.
- `outside_growth` rows of up to 3 GiB per heavy job.

The ADR-0027 ceiling never bound (MemAvailable never went under about 8.5 GiB, the floor is
1.5 GiB), so 0.10.0 did what it promises: no OOM and no thrash-to-death. It does not
promise an interactive desktop, and the desktop was not.

## Finding: Chromium leaves the job's cgroup

On 2026-11-05 the user manager created 385 `app-org.chromium.Chromium-<pid>.scope` units,
in bursts that match the Playwright jobs (UTC + 1). A bounded probe (a headless shell for
6-8 s inside a transient scope of its own, about 200 MB) confirmed it: the browser process
moved itself into `app.slice/app-org.chromium.Chromium-<pid>.scope`; renderers forked
earlier stayed behind. The mechanism and its conditions are in
[[sluice/research/chromium-scope-escape.md]]: `StartTransientUnit` over the session bus
with the browser's own pid, no switch or feature to turn it off, and a Chromium change of
2026-11-05 that also moves every descendant (`AttachProcessesToUnit`, `Delegate=true`).

Consequences for 0.10.0: the browser's memory is not in the job's working set (so not in
its learned peak, the estimate admission relies on), not under the job's `memory.max`, and
not reached by any kill of the job. Playwright starts browsers `detached`, so the job's
process-group kill misses them too.

## What is not proven

- That the memory peak caused the hang. It is the most likely trigger (the shell lagged
  during the peak and its last ordinary line came at the end of it), but the shell stayed
  hung after memory recovered, which points to a GNOME Shell, extension or driver bug that
  the lag exposed. The research found one mutter report of a Wayland freeze under memory
  stress through swap-in paths (mutter issue 2431) and no report of a hang with free RAM.
- Whose pages were swapped. sysstat is machine-wide; Sluice kept no per-job swap or
  pressure. The hang could not be attributed to a process for lack of per-process data.

Hence part A of ADR-0028: the daemon now records, per job, its memory pressure, swap, page
cache and escaped processes, and the session's pressure, so the next incident can be
judged on data.

## The other machines

- A 16-thread, 22 GiB machine on 0.10.1: since 2026-11-03 memory under 10 %, swap under
  6 %, clean shutdowns only.
- The 12-thread, 30 GiB machine of the 2026-11-01 freezes, on 0.10.0 since 2026-11-02:
  387 jobs, no OOM, no memory kill, swap at most 36 %.
