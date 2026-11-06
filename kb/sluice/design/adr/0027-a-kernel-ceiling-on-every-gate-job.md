---
title: "ADR-0027: Every gate job runs under a kernel ceiling that keeps a floor of memory free"
tags: [sluice, adr, daemon, memory, containment, cgroup, oom, thrashing]
---

# ADR-0027: Every gate job runs under a kernel ceiling that keeps a floor of memory free

Amended by [[sluice/design/adr/0028-gate-jobs-do-not-degrade-the-desktop.md]] (2026-11-06):
swap is now bounded per job (decision 6) and the exemption (decision 8) is held while the
desktop guard holds.

Status: accepted (2026-11-01, user decisions: a dynamic ceiling on every job, not only on
jobs started beyond their estimate; the floor is half the reserve; swap left as it is).
Research: [[sluice/research/memory-freeze-2026-11-01.md]]. Amends
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]] (decision 5, the optional ceiling,
and the S5 constraint "no absolute ceiling on by default") and
[[sluice/design/adr/0026-memory-admission-keeps-what-it-learned.md]] (alternative "a per-job
`memory.max`", rejected there because nobody configures the limit).

## Context

On 2026-11-01 a second machine (30 GiB, 8 GiB swap) froze for 22 s to 2.5 min before each
of six global OOMs under 0.9.0. Each time, the victim was a Sluice gate job, as ADR-0019
intends, but only after the whole machine had livelocked: with swap full the kernel keeps
reclaiming file pages, every process refaults its own text, and the global OOM killer waits
for reclaim to fail. systemd-oomd and the daemon's own guard are userspace and stalled with
everything else; systemd-journald was killed by its watchdog during the longest freeze.

The gate (a 12.3-13.3 GiB test suite) entered every time through the idle-machine exemption
of ADR-0026 decision 6: free memory never covered its estimate plus the reserve. It passes
most of the time by pushing other processes into swap, and fails the machine when the rest
of it grows during the run.

ADR-0019 and ADR-0026 order the victims and admit by learned memory, but no limit stops a
running job from taking the last free page. A limit is the only mechanism the kernel
enforces inside the cgroup, independent of userspace, and the daemon can compute it: it
already samples `MemAvailable` and every job's working set each second.

## Decision

1. **A dynamic ceiling on every running gate and `exec` job.** Each sampler tick (1 s), the
   daemon computes the headroom `H = MemAvailable − floor` (`MemAvailable` capped by the
   limited cgroups above the daemon, as for admission) and writes each job leaf's
   `memory.max`:
   - `H ≥ 0`: `anon(job) + H / n`, `n` the number of running jobs. The ceilings add up to
     no more than the RAM that can be given away while keeping the floor free, so jobs
     cannot together push the machine below the floor even if the daemon stalls for a
     while: the limits already written hold.
   - `H < 0`: the job with the largest `anon` gets `anon − |H|` (below its usage: the kernel
     reclaims its page cache and, if that is not enough, OOM-kills the whole job,
     `memory.oom.group`); every other job gets `anon + slack` (no room to grow until the
     machine recovers). `slack` is 64 MiB, the sampler's error margin between the reading
     and the write.
   - `anon` is the leaf's `memory.stat` `anon` (RAM only). `memory.max` counts page cache
     too, which the kernel reclaims before killing; `MemAvailable` already counts it as
     available, so using `memory.current` would count it twice.
   - A ceiling never goes below 64 MiB.
2. **The floor is half the reserve.** `[admission] memory_floor = "auto"` (default) is half
   of `memory_reserve` resolved, 5 % of `MemTotal` with the default reserve (1.5 GiB on
   30 GiB): the level the guard already treats as "about to thrash". A size such as
   `"2G"` overrides it.
3. **On by default, with containment.** `[admission] memory_ceiling = true` (default). It
   needs job leaves (containment `full`); with partial or no containment nothing is written
   and the guard (opt-in) stays the only Sluice-side protection. `false` writes `max` again.
4. **Writes never block the sampler.** Lowering `memory.max` below the usage is synchronous
   (the kernel reclaims or kills before the write returns). A dedicated thread performs the
   writes, keeping only the newest value per job; a write still in progress delays the next
   one for that job and nothing else.
5. **A ceiling kill reads as a memory kill.** The kernel's in-cgroup OOM raises the leaf's
   `oom_kill` and `oom_group_kill`, which ADR-0019 piece 4 already reads: the job ends
   `killed_by = "memory"` with the existing message, and admission settles (ADR-0026). The
   daemon logs one line per job the first time its ceiling falls below its usage, with the
   floor and `MemAvailable`.
6. **Swap is unchanged.** `memory.swap.max` stays as configured (`gates_memory_swap_max`,
   `max` by default). The ceiling bounds RAM; the recorded passing runs relied on other
   processes being swapped out, which stays possible.
7. **The static ceiling stays.** `gates_memory_max` / `gates_memory_swap_max` on `jobs/`
   still bound the jobs together; the dynamic ceiling acts per leaf below it.
8. **The exemption stays.** ADR-0026 decision 6 is unchanged: a job alone on the machine
   still starts beyond its estimate. Under the ceiling it can no longer take the floor:
   it passes when the machine has room and dies alone, quickly, when it has not.

## Consequences

- A job that would have frozen the machine is killed inside its own cgroup within about a
  second of the machine reaching the floor, instead of a global OOM after a minute or two of
  livelock. On the recorded run (13.0 GiB peak with 3.1 GiB still available) the ceiling
  never binds: the floor is 1.5 GiB.
- The kill can come while the machine still has memory above the floor in swap or page
  cache: some runs that passed by thrashing the rest of the machine now fail with a memory
  kill. That is the intended trade: the machine stays usable and the message tells the agent
  to retry once memory is free, or the human that the gate does not fit.
- A job at its ceiling with swap still free first swaps out its own pages (the ceiling
  bounds RAM, decision 6): it slows down alone instead of pushing the rest of the machine
  into swap, and dies when swap runs out. With swap full, as in the incident, it dies at
  once.
- With several jobs, each gets an equal share of the headroom per second. A job growing
  faster than `H / n` per second meets its ceiling, reclaims its own page cache and, if the
  machine is really short, dies; with headroom left, the next tick raises its ceiling.
- Growth outside gates still cannot be bounded by Sluice. When it alone takes the floor, the
  ceilings shrink to the jobs' usage, the largest job is killed by its own ceiling, and the
  rest is the kernel's (where gates remain the preferred victims, ADR-0019).
- The daemon writes one file per running job per second; skipped when the value is
  unchanged at page granularity.
- New config keys `[admission] memory_ceiling` and `memory_floor` are one-way (older
  binaries reject them): `docs/compatibility.md`.
- `status` shows each running job's ceiling, and the JSON gains `memory_ceiling` per running
  job (additive, schema 1).

## Alternatives considered

- **Remove the exemption.** The recorded data says the suite would wait for hours, and it
  does not protect a job admitted normally when the machine grows during its run.
- **A ceiling only on jobs started beyond their estimate.** Leaves the normally admitted
  job unprotected against growth outside gates, which caused one of the six kills.
- **Turn the guard on by default.** It is userspace: it stalls with the machine.
- **`memory.high`.** Throttles without killing, can be exceeded when swap is full, and its
  pressure made oomd kill the desktop in the S2 incident. Still rejected.
- **`memory.swap.max = 0` on jobs.** Would stop a job from filling the shared swap, but
  kills runs that pass today with no data showing it is needed. Left for a later measure.
- **Host-wide knobs (MGLRU `min_ttl_ms`, `memory.min` on the session).** Effective against
  livelock, but they change the whole machine; Sluice never changes the machine outside its
  own cgroups. Documented as advice for the human.

## Open items

- A command to cancel a running gate job (none exists; the incident needed `cgroup.kill`).
- A job killed from outside through `cgroup.kill` is recorded as `fail`, not `killed`.
- Measure after a week: ceiling kills per day, global OOMs (target 0), freezes (journal gaps
  before kills), passing runs lost to the ceiling.
