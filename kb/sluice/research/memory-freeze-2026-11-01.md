---
title: "Desktop freezes under 0.9.0: the idle-machine exemption and global thrashing (2026-11-01)"
tags: [sluice, research, memory, admission, oom, thrashing, systemd-oomd, cgroup]
---

# Desktop freezes under 0.9.0: the idle-machine exemption and global thrashing (2026-11-01)

Incident analysis behind [[sluice/design/adr/0027-a-kernel-ceiling-on-every-gate-job.md]].
Follows [[sluice/research/memory-review-2026-10-30.md]] and
[[sluice/design/adr/0026-memory-admission-keeps-what-it-learned.md]].

## Machine and workload

A second machine (12 threads, 30 GiB RAM, 8 GiB disk swap, no zram, Ubuntu 26.04, kernel
7.0, systemd 259) running Sluice 0.9.0, about ten agent sessions, a desktop session, a dev
server, containers and local embedding engines. The heavy gate is a node test suite
(`pnpm -r exec node --test --test-concurrency=6`) run from many git worktrees of one project.

All data was read without creating memory pressure: the daemon log, a read-only backup of
the daemon database, the journal, `oomctl`, and a 1 s sampler of `/proc/meminfo` and the job
leaf's `memory.current` attached to two running jobs (with the user's approval; it would have
killed the job leaf at `MemAvailable` < 2 GiB, which never happened).

## Findings

1. **For that gate, the exemption is the only way in.** All 61 runs of the suite since
   0.9.0 started (2026-10-30 19:15 to 2026-11-01 15:28) were logged as "starts alone beyond
   its estimate". The estimate was 15.4-16.4 GiB (1.2 x the largest passing peak) and free
   memory at admission was 8.1-15.7 GiB, below estimate plus reserve (3.1 GiB) every time.
   The observed reserve of ADR-0026 decision 5 never applied: the exemption skips it.
   Outcomes: 46 passed, 9 failed (tests), 6 killed for memory.
2. **The job does not outgrow its history; the machine is short.** Passing peaks (anon +
   swap) were 12.3-13.3 GiB, and the two kills of 2026-11-01 died at 12.6 and 13.1 GiB,
   inside that range. The suite passes by pushing other processes into swap. The memory
   the job could have (its usage plus `MemAvailable`) ranged 13.5-17.2 GiB over one run;
   at its 13.0 GiB peak, `MemAvailable` was 3.1 GiB and swap was full. The 2026-11-01 13:28
   kill coincided with 8.8 GiB of growth outside gates during the run.
3. **Every kill was a global OOM after a freeze.** The 6 memory kills match one to one the
   6 global OOMs in the kernel log (`constraint=CONSTRAINT_NONE`, `global_oom`). The journal
   is silent for 22, 30, 37, 67, 72 and about 150 s before each one. During the longest one
   systemd-journald was killed by its watchdog (journal files corrupted, `kmsg` buffer
   overrun), and the job's longest quiet window was 2 min 24 s.
4. **Victim ordering works.** All 6 victims were Sluice job leaves (`oom_score_adj` 800,
   `memory.oom.group`); no agent or desktop process was killed by the kernel. What failed is
   the time before the kill, not the choice of victim.
5. **systemd-oomd acted after the kernel, not before.** The gates' scope policy (30 % for
   8 s) logged its kill at the same second as the kernel's, with pressure already at 76 %.
6. **The kill does not stop the loop.** An agent requested the gate again 33 s after the
   15:16 kill (after the ADR-0026 settling), and the exemption admitted it with swap still
   full. There is no command to cancel a running gate job; the job was stopped through its
   leaf's `cgroup.kill`, and the daemon recorded that run as `fail`.

## Why userspace cannot act in time

Sources gathered in English, Chinese, Russian and Japanese. The non-English searches led back
to the same primary sources; no independent finding appeared in only one language.

- The kernel's OOM detection is based on reclaim failure. With swap full only file pages can
  be reclaimed; reclaim keeps "succeeding" while every process refaults its own text, so the
  machine is livelocked long before a global OOM (Hocko,
  <https://lkml.iu.edu/hypermail/linux/kernel/1709.2/00488.html>; Weiner,
  <https://lkml.iu.edu/1908.0/07071.html>).
- systemd-oomd does not `mlock` itself (no `mlock` in `src/oom/`), decides from `avg10`
  held for the configured duration (`oomd-util.c`), and its man page warns that without swap
  headroom the system livelocks before it can react
  (<https://man7.org/linux/man-pages/man8/systemd-oomd.service.8.html>). Field reports of
  freezes with a silent oomd: systemd issue 36434, Red Hat bug 2248071.
- PSI can under-report during this kind of thrashing (LKML 2019,
  <https://lkml.iu.edu/1908.1/02149.html>).
- `memory.max` is enforced by the kernel inside the cgroup: "if a cgroup's memory usage
  reaches this limit and can't be reduced, the OOM killer is invoked in the cgroup"
  (<https://docs.kernel.org/admin-guide/cgroup-v2.html>). Lowering it below the current
  usage first stops new charges, then reclaims and OOM-kills until the limit is met; the
  write is synchronous (commit b6e6edcf). `memory.high` never kills and can be exceeded
  when swap is full (the S2 incident mechanism, still rejected).

Consequence: the daemon's guard (ADR-0019 piece 7) and oomd are both userspace and stall in
exactly the state they are meant to handle. Only a limit written **before** the shortage
holds during it.

## What admission alone cannot fix

Removing the exemption would make the suite wait for 16 + 3.1 GiB free, which the recorded
data shows almost never happens on this machine, and it would not protect a job admitted
normally when the rest of the machine grows during its run. The exemption is not the damage;
running a job with no kernel bound while the machine is near full is.
