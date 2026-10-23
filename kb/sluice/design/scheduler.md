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
`head-reserved`, or `order` (it fitted the budget before this pass but
higher-ranked jobs took the room; detail `human-first`, `lane` or
`outranked`). `status` keeps the four frozen wire reasons (an `order` wait
reports `budget`) and adds `wait_detail`. Each pass adds the elapsed time
to every queued job's previous cause (the state before the sample decides);
the accumulators are the job's `wait_*_ms` columns and the largest one is
the `done.wait_reason`.

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
- Marker appearing while queued → `cached`, no process run.
- Last subscriber gone → process killed under `cancel`; an early-verdict
  cancel kills only a job nobody else waits for.
