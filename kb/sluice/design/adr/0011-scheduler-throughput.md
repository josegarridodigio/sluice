---
title: "ADR-0011: Scheduler throughput and latency under overload"
tags: [sluice, adr, alpha, scheduler]
---

# ADR-0011: Scheduler throughput and latency under overload

Status: accepted (2026-10-22). Design: [[sluice/design/scheduler-throughput.md]].
Plan: [[sluice/plan/alpha-plan.md]] tasks B0, B1, L1–L3. Supersedes parts
of [[sluice/design/adr/0007-scheduler-fairness.md]] as stated below.

## Context

In the alpha overload run (12 agents, weight-8 suites, budget 19 on 20
CPUs) Sluice did 48 % less work than running everything directly but
finished later (162.5 s vs 147.3 s) with twice the p95 `verify` latency
(79.6 s vs 37.5 s): only two suites fitted in the budget while a suite's
real average demand was about 6 CPUs, so about 45 % of the CPUs were idle
while jobs waited. PSI never blocked.

## Decision

Adopted, each behind its own config switch so the tuning loop (L3) can
measure them one at a time:

1. Default budget = online CPUs (`max(2, n)`, honouring the affinity mask
   and cgroup CPU limits), instead of CPUs − 1.
2. Learned weights for gates: demand = (CPU time + run-queue wait) / wall
   from the job's tasks (`/proc/*/task/*/schedstat`), P75 of the last 10
   passing runs, used after 3 runs, bounded to `[max(1, ceil(w_cfg / 4)),
   w_cfg]`; resolved at every admission pass (a queued job takes the learned
   weight as soon as it exists); not for `exec` jobs.
3. PSI-driven over-admission up to 1.5 × budget when CPU `some avg10` < 25,
   memory < 5, at least `max(1, ceil(w / 3))` CPUs idle (mean of the last two
   samples, the newest taken after the latest admission), nothing started
   nominally in the same pass, and no over-admission in the last 5 s
   (`admission.overcommit_interval`, also the wait after a PSI resume);
   `admission.overcommit` is validated to 1.0 (off) to 4.0; the existing
   blocking hysteresis and `psi_max_wait` stay.
4. Parallel `verify` with an early verdict: the gates of one `verify`
   start together; the first failure ends the call and cancels sibling
   jobs nobody else waits for.
5. Fairness by worktree path (round-robin at equal priority) and humans
   first, with ageing: any job waiting longer than `head_reserve_after`
   goes ahead in arrival order (keeps ADR-0007's no-starvation guarantee).
6. Shortest-expected-job-first is implemented but off by default;
   supersede of queued older trees is deferred (no stale queued work under
   the defaults).
7. Instrumentation (L1): a per-job timeline and requesters, and
   `idle_while_waiting_ms` (integral of the idle CPU fraction while the queue
   is not empty) in `status.totals`, and per job `queue_wait_ms` and
   `wait_reason` in `done` and `verify`, all optional fields.
8. Storage: `schema.version` stays 1 so 0.2.0 binaries still open the
   database; the additions (nullable timeline columns in `jobs`, tables
   `job_requests` and `gate_stats`) are versioned by their own
   `sched_schema` table and applied by the migration framework of
   ADR-0013 (transactional, `.bak` copy first, tested from a real 0.2.0
   database).

Targets (plan, workstream L): wall ≤ baseline, p95 ≤ 1.2 × baseline, suite
executions = distinct trees, idle CPU while waiting < 10 % of wall, peak
load ≤ baseline, in the normal, overload and low-core variants.

## Consequences

- Predicted overload result: wall about 95 s, p95 about 43 s, idle while
  waiting about 4 %. On a 6-CPU machine p95 was predicted at 1.2–1.25 ×
  baseline, at or just over the target.
- Measured (L3 round 3, shipped defaults 1.5 × / 5 s, learned weights
  resolved at admission): every target met in every variant. Overload: wall
  0.58 × the baseline, p95 1.10 ×, idle while waiting 7.8 %; low-core: p95
  0.94 ×, idle 5.5 %; normal: p95 1.14 ×, idle 5.0 %
  ([[sluice/changes/alpha-readiness.md]]).
- Admission decisions become pure functions over a snapshot (unit-tested);
  the scheduler state grows by the learned statistics.

## Alternatives considered

- Simply raising the default budget above the CPU count: brings back the
  overload it exists to prevent on small machines.
- Learning from utime+stime only: under contention it under-reports demand
  and admits more exactly when the machine is full.
