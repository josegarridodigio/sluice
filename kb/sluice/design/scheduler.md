---
title: Scheduler and coalescing
tags: [sluice, design, scheduler, phase-2, alpha]
---

# Scheduler and coalescing

Status: implemented in Phase 2 (`sluice-gate-daemon::scheduler`), extended
by alpha tasks B1 and L1 (ADR-0011). Decisions:
[[sluice/design/adr/0007-scheduler-fairness.md]] (backfill and
reservation) and [[sluice/design/adr/0011-scheduler-throughput.md]]
(throughput policies). Design and deviations of B1/L1:
[[sluice/design/scheduler-throughput.md]]. Related:
[[sluice/design/protocol.md]], [[sluice/design/executor.md]],
[[sluice/design/psi-admission.md]].

## Model

- **Budget** `B = daemon.budget` units. Default `max(2, available CPUs)`:
  the daemon's affinity mask (`Cpus_allowed_list` of `/proc/self/status`)
  bounded by the cgroup CPU limit (v2 `cpu.max`, v1
  `cpu.cfs_quota_us`/`cpu.cfs_period_us`, tightest of the cgroup and its
  ancestors), `sluice_gate_core::cpus`. Until 0.2.0 it was `max(2, cpus - 1)`.
- A **job** is one execution of one gate on one tree: `(key, repo, lane,
  gate, weight, timeout, cmd, env, cwd)`. The configured weight is clamped
  to `1..=min(B, cap)` where `cap` is the repository's `budget_share_max`
  if any (logged once per clamped job); the result is `weight_cfg`. With
  learned weights (below) the **effective weight** `w` may be lower; `w`
  is what the job holds. `repo` is the repository identity (the main
  worktree path, from the git common dir), shared by every worktree of a
  project for the cap and `status`; `lane` is the worktree toplevel of the
  request (for `exec`, its cwd), used for fairness.
- A **request** is one client waiting for a job. A job has one or more
  subscribers; requests never run anything themselves. Each request is a
  `job_requests` row (who, how, when it attached, whether it left early).
- **Used** = Σ w of running jobs. A job is admissible nominally when
  `used + w ≤ B`, the repository cap allows it (counted over running jobs
  of that repo) and PSI is not blocking.

## Coalescing (G3)

In-flight lookup by key over the queued and running jobs. On a `run`
request:

1. `key == null` → new job, never coalesced, never cached.
2. `key` already queued or running → attach the request to that job
   (`queued{coalesced:true}`, plus an immediate `started` when the job is
   already running), regardless of worktree: markers are shared across
   worktrees, so the result is valid for both.
3. Otherwise → create the job and enqueue it.

**Double-check before start:** before spawning a keyed job the daemon calls
`MarkerStore::lookup` once more; a fresh marker completes it immediately
with `status:"cached"` for every subscriber.

## The decision: a pure plan over a snapshot

Every admission pass (job finished, request arrived, PSI blocking state
changed, 1 s timer) builds a `Snapshot` (now, policy, pressure, running and queued
jobs, time of the last over-admission) and calls `scheduler::plan`, a pure
function; `Scheduler` only owns the state and applies the plan. Unit tests
drive `plan` and `rank` directly.

### Priority order (`rank`)

```
rank(j) = ( overdue(j) ? 0 : 1,            // waited ≥ head_reserve_after
            overdue(j) ? j.queued_at : 0,   // overdue: strict FIFO
            human_first ? class(j) : 0,     // 0 human, 1 agent (or unknown)
            fairness ? lane_rank(j) : 0,
            order == sjf ? −hrrn(j) : 0,
            j.queued_at, j.id )
lane_rank(j) = running jobs of j's lane + index of j among its lane's queued jobs
hrrn(j)      = (waited_ms + expected_ms) / expected_ms   (expected unknown: 60 s)
```

`head_reserve_after` is the single ageing bound: an overdue job outranks
everything, overdue jobs are FIFO among themselves, so the ADR-0007 bound
(`head_reserve_after` + longest running job) holds under every switch
combination (property test `no_starvation_under_any_switch_combination`).
With every switch off the order is plain FIFO (ADR-0007).

### `plan`

1. **PSI blocking:** nothing starts, except the starvation guard
   (`psi_max_wait`, [[sluice/design/psi-admission.md]]) which may start the
   first ranked job within the nominal budget and its repository cap
   (`admission = guard`).
2. **Nominal pass** in rank order: a job that fits starts
   (`admission = nominal`). **Backfill:** while the first waiting job does
   not fit, lighter jobs behind it may start. **Reservation:** once the
   first waiting job is overdue and blocked by the budget, nothing behind
   it starts until it fits (a job blocked only by its repository cap never
   reserves).
3. **Over-admission** (ADR-0011 M3), only when nothing started nominally in
   this pass and `cap > B`: the reserved head, else the first ranked job
   with `used + w ≤ cap` and its repository cap free, starts above the
   budget (`admission = over`) when the pressure is calm (PSI enabled with
   both values, not blocking, resumed at least `overcommit_interval` ago,
   CPU `some avg10 < cpu_some_avg10_low`, memory `< memory_some_avg10_low`),
   the mean idle CPUs of the last two utilisation samples are
   `≥ max(1, ⌈w/3⌉)`, the newest utilisation sample was taken at least one
   sampler tick (`psi_poll_interval`) after the latest admission of any kind
   (`Scheduler::last_start_at`), and no over-admission happened in the last
   `overcommit_interval`. `cap = ⌊overcommit × B⌋` (`overcommit` is
   validated to `1.0..=4.0`; `1.0` disables over-admission). Over-admitted
   jobs are never preempted; while `used > B` nothing is admitted nominally.

Each queued job left waiting gets a cause: `budget`, `psi`, `repo-cap`,
`head-reserved`, `order` (it fitted the budget before this pass but
higher-ranked jobs took the room; detail `human-first`, `lane` or
`outranked`) or `memory` (the budget has room but the memory the job is
expected to use does not, next section). `status` keeps the four frozen wire
reasons (an `order` or `memory` wait reports `budget`) and adds `wait_detail`. Each pass adds the elapsed time
to every queued job's previous cause (the state before the sample decides);
the accumulators are the job's `wait_*_ms` columns and the largest one is
the `done.wait_reason`.

## Memory-aware admission (ADR-0019 piece 1)

The CPU budget and PSI do not see memory: a PSI reading lags a job that is
still ramping up (memory pressure was about 0 at each admission of three
`pnpm test` gates that then grew to 18 GiB each on a 30 GiB machine). `plan`
therefore takes an optional `Snapshot.memory: Option<MemoryFacts
{ available_bytes, total_bytes, reserve_bytes }>`; `None` is the rule off and
every plan is exactly the one CPU-only admission produces (differential test).

**Inputs.** `MemAvailable` and `MemTotal` of `<proc_root>/meminfo`, read at each
admission pass and capped by every cgroup with a numeric `memory.max` from the
daemon's own cgroup (`<proc_root>/self/cgroup`) up to the root, and by the gates'
`jobs/` ceiling (`[containment] gates_memory_max`): such a cgroup caps the total
at `memory.max` and the available memory at `memory.max - memory.current`
(`memory::cap_by_cgroups`; in a container or a unit with `MemoryMax=` the
machine's figures are far too large, found by the container tests of ADR-0019); `reserve = [admission] memory_reserve` (`auto` = 10 % of
`MemTotal`, a size like `3G` otherwise: what the desktop, the agents and the
page cache need to stay alive; deliberately not learned). Per running job its
current working set (`cur`) and expected peak `E`; per queued job `E`.

**Working set and expected peak.** The 1 s sampler (`sample_tick`) reads, per
running gate or `exec` job, `anon` of the job leaf's `memory.stat` plus
`memory.swap.current` (page cache is left out: `MemAvailable` already counts
it as available, counting it twice would refuse jobs that fit), or the summed
`VmRSS` of the job's process group when containment gives no leaf. `JobUsage`
keeps the last value and the running peak (and `memory.peak`, shown only). At
the end of a passing run, or of a run killed for lack of memory, one row goes
to `gate_memory` (table of the containment schema group, version 2; newest 20
per `(repo, gate, cmd_hash)`, pruned with `gate_stats`; `exec` jobs use
`exec:<label>`). `E = max(anon_peak of the last 5 rows) × 1.2`, a killed run
counting `× 1.25` first (it stopped early). One sample is enough (a maximum
needs no average). A failing run that is not a memory kill teaches nothing; the
weight's `learns_from` is unchanged (passing runs only). An unknown peak is
guessed as `w / B × (MemTotal − reserve)` with the job's effective weight `w`
(for `auto` on a budget of 12, half of the usable memory), marked `guessed` in
`status` until a run replaces it. *Amended in 0.6.1:* the guess is
`min(w / B × usable, prior(w))`, where `prior(w)` is the largest expected peak
(same formula as above) among the learned keys of this daemon whose weight is
`≤ w`; with no such key the proxy stands alone (the 0.6.0 behaviour). The
weight of a key is the effective weight its newest recorded run was admitted
with (`PeakCache` in memory; at start `gate_memory.job_id` joined with
`jobs.weight`, a key whose job row was pruned is no prior). The daemon hands
the scheduler a table `weight → largest peak` (`Snapshot.priors`,
`prior_peak`), so `plan` stays pure. The figure is still `guessed`:
over-admission needs the gate's own learned peak.

**Rule.**

```
headroom_r  = max(0, E_r − cur_r)                  # a running job still to grow
H           = Σ headroom of running jobs + Σ E of jobs started earlier in this pass
free        = MemAvailable − reserve − H           # swap never counts
alone       = nothing runs and nothing was started earlier in this pass
fits_mem(j) = alone  ||  E_j ≤ free
```

`fits_mem` is an extra condition of the nominal pass (the weight and
repository-cap tests are unchanged), of the starvation guard, and, stricter, of
over-admission, which also needs a **learned** `E` and takes no `alone`
exemption (never over-admit blind). Jobs started earlier in the pass add their
whole `E` to `H`, so two big jobs cannot both start off one stale
`MemAvailable`. A job blocked only by memory reports `wait_detail = memory`
(wire reason `budget`), its wait is added to the budget accumulator, and PSI
blocking still comes first (a blocked pass reports `psi`).

Memory waits are observable only in the daemon log: each queued job keeps an
in-memory `memory_wait_ms` (time under `WaitCause::Memory`, added where the other
accumulators are, besides the budget one) and the first time a pass left it
memory-blocked. `admit_with` queues a `MemoryNote` for the first block and for the
start of a job with `memory_wait_ms > 0`; the daemon drains them
(`take_memory_notes`) and logs one `info` line each (`waits for memory: ...`,
`started after waiting ...s for memory ...`). Never per tick, no schema or wire
change, `plan` stays pure.

**Invariants.**

- Never `Σ E > free` unless that job started with nothing else running and
  nothing started earlier in the pass (`alone`): an idle machine always starts
  the first ranked job, so progress is always possible. Any running job, however
  light, withdraws the exemption (a big job never joins others on faith) (if the agents, not Sluice, use the memory, that
  job is the preferred victim of the kernel and of oomd, W6a/W6c).
- No hold-and-wait: a job holds nothing while it waits, admission is
  all-or-nothing, so memory cannot create a cycle.
- The overdue-head reservation covers memory: a head that fits the budget but
  not memory reserves once overdue (a head blocked only by its repository cap
  still never does), so nothing behind it starts; running jobs end (timeouts
  bound them), the machine drains, `alone` becomes true and it starts (also when
  its `E` exceeds what is free even then). The ADR-0007 bound
  (`head_reserve_after` + the longest running job) holds under every switch
  combination (`no_starvation_under_any_switch_combination` has the memory
  switch; `an_overdue_head_blocked_only_by_memory_starts_within_the_bound`).
- Weights are untouched (ADR-0015): memory adds a second constraint, it does
  not change the CPU weight or its learning; CPU-driven over-admission stays
  CPU-driven, now also needing a learned peak that fits.
- A memory-blocked job that is not overdue still lets smaller jobs backfill.
- `plan` stays a pure function of the snapshot; `jobs` and `gate_stats` are
  unchanged and `sched_schema` stays 1.
- A prior never raises an estimate (`min` with the proxy) and never makes a job
  "learned": the `alone` exemption and the over-admission rule are as before.

## Learned weights (ADR-0011 M2)

The daemon's 1 s sampler reads `/proc/[0-9]*/stat` for the processes of
each running gate's process group and their tasks'
`/proc/<pid>/task/<tid>/schedstat` (CPU and run-queue wait, deltas keyed by
`(tid, start time)`), plus the leader's `utime+stime+cutime+cstime`. At the
end of a run: `cpu_ms = max(sampled, leader)`,
`demand = (cpu_ms + run_delay_ms) / wall_ms`. Without `schedstat` the
fallback `cpu / wall` is kept only when CPU `some avg10 < 10` at both ends.
A run never sampled (shorter than a tick) records nothing. Only passing
runs are samples: a failing run often stops early and would teach a weight
below what the suite needs (the job's own `jobs` columns record its demand
regardless). Samples go to `gate_stats`, keyed by `(repo identity, gate, cmd_hash)` (`cmd_hash` = 16
hex of SHA-256 over the normalised command, NUL, and the extra arguments
joined by NUL); the newest 20 per key are kept, keys unseen for 90 days are
pruned daily, and the daemon caches the last 10 per key in memory.

Resolved at every admission pass, not at enqueue, with `learned_weights =
true` and at least 3 samples:
`w = clamp(round_half_up(P75(demand)), max(1, ⌈weight_cfg/4⌉), weight_cfg)`
(nearest-rank P75 of the last 10). Never above the configured weight. A
queued job takes the current learned weight as soon as one exists; a running
job keeps the weight it was admitted with, and `jobs.weight` records it.
`exec` jobs keep their declared weight. The same history gives
`expected_ms` (median wall time of the last 10 passing runs) for SJF and
`status`.

## Utilisation sampler and `idle_while_waiting_ms` (L1)

Every `psi_poll_interval` (1 s; the task runs with PSI off too) the daemon
reads the `cpuN` lines of `/proc/stat` for the CPUs of its affinity mask
(`idle = idle + iowait`, `total` = user..steal). `idle_frac = ΣΔidle /
ΣΔtotal`, `idle_cpus = idle_frac × N`. Under a cgroup CPU limit `L < N`
(read under the parent of the proc root), `idle_cpus = max(0, L − (N −
idle_frac × N))` and `idle_frac = idle_cpus / L`. If at least one job was queued at
the previous sample, `waiting_wall_ms += Δt` and `idle_while_waiting_ms +=
Δt × idle_frac`. Both are per daemon instance, in memory, reported in
`status.totals`. The proc root is a `DaemonConfig` field (tests use a fake
tree).

## Per-repository cap

`repos."<main worktree path>".budget_share_max = N` limits the units
running for one repository (all its worktrees together). Keys are exact
paths. A job blocked only by the cap reports `reason:"repo-cap"`. The
daemon reads the user config once at start: cap, budget, scheduler and
admission changes need `sluice daemon stop`; the client re-reads the file
on every run.

## Subscribers and cancellation

- `last_subscriber_gone = cancel` (default): when the last subscriber
  disconnects, the job is killed if running or dropped if queued; outcome
  `killed` with `killed_by = last-subscriber` (or `early-verdict` when the
  last subscriber's `cancel` carried that reason, see the MCP `verify`).
- `finish`: the job continues, the marker is written on a pass, nobody is
  notified (exec jobs are always killed).
- Explicit `cancel` from the client is the same as disconnecting; its
  optional `reason` is recorded. `sluice daemon stop` drops queued jobs and,
  with `--force`, kills running ones (`killed_by = stop`).

## Metrics (SQLite, [[sluice/design/executor.md]] for the schema)

Per job: requested_at (client send time), queued_at, admitted_at,
started_at, finished_at, outcome, exit_code, weight (effective),
weight_cfg, admission, expected_ms, the five wait accumulators, cpu_ms,
run_delay_ms, demand_milli, killed_by, subscribers_max, coalesced count,
caller, session; one `job_requests` row per request. `sluice logs
--timeline [<job>] [--since 10m]` prints one line per job. Derived for
`status`: hits, coalesced, time saved, and the per-instance
`psi_blocked_ms_total`, `idle_while_waiting_ms`, `waiting_wall_ms`,
`over_admissions`.

## Merge queue jobs

The merge queue ([[sluice/design/merge-queue.md]]) submits its gate runs
through `DaemonHandle::submit_run`, the same registration path as a client
`run` (session `queue:<train-id>`, `via = queue`, caller human), so they
obey every rule here; the scheduler has no queue specific code.

## Properties tested

- Six concurrent requests for the same key → one process run.
- Different keys with weight 4 under `B = 8` → never more than two at once;
  weight 8 under `B = 20` → 2 suites, learned 6 → 3.
- A weight-B agent job behind an endless stream of weight-1 human jobs in
  other lanes starts within `head_reserve_after + longest running job`
  under all 16 combinations of fairness, human_first, SJF and
  over-admission.
- Over-admission: only when calm, idle ≥ ⌈w/3⌉ and the interval passed;
  never over the cap, never while blocking or without PSI; the guard never
  over-admits; under reservation only the head; budget 6 / cap 9 never runs
  two weight-6 jobs.
- Memory (ADR-0019): two jobs expected to peak at 18 GiB with 22 GiB available
  and a 3 GiB reserve → one starts, the other waits with `wait_detail =
  memory`; jobs started in the same pass are counted; never `Σ E > free` unless the
  job started with nothing else running (property test); rule off or plenty of memory → CPU-only plans under all
  16 switch combinations; an overdue memory-blocked head starts within
  `head_reserve_after` + the longest running job.
- Marker appearing while queued → `cached`, no process run.
- Last subscriber gone → process killed under `cancel`; an early-verdict
  cancel kills only a job nobody else waits for.
