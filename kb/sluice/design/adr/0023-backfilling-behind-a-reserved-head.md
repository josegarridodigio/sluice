---
title: "ADR-0023: Jobs that cannot delay a reserved head start before it (backfilling)"
tags: [sluice, adr, daemon, scheduler]
---

# ADR-0023: Jobs that cannot delay a reserved head start before it (backfilling)

Status: accepted (2026-10-28). Research: [[sluice/research/queue-scheduling-analysis.md]]
(sections 1.4, 2.1, 2.2, 2.6, 4.1, 4.4). Plan: [[sluice/plan/queue-scheduling-plan.md]] (S1).
Amends ADR-0007's reservation paragraph and ADR-0019 piece 1 ("an overdue head blocked by
memory reserves").

## Context

An overdue queue head that does not fit (budget or memory) sets a reservation, and no later job
starts until it does (`scheduler.rs`, `Pass::nominal`). It prevents heavy jobs from starving,
but never asks whether a later job would disturb the head. On a second machine (12 threads,
30 GiB), one day: 48 % of all queued time was `head-reserved`; 91 % of those jobs were small
(expected under 60 s); median wait 6 min, worst 27 min; a 105 ms linter waited 15-23 min.
Learned durations exist for 96 % of passing runs there; actual/expected has median ~1.0, p90 ~2,
and `1.5x + 5 s` is exceeded by about 7 % of runs. A replay of that day (open loop, a ranking
not a prediction) cuts summed waits from 631 to 283-359 min with no heavy job started later.

## Decision

1. **Shadow time.** Each pass computes the reserved head's earliest start: running jobs are
   released in order of predicted end (`started_at + expected`, pessimistic remainder for jobs
   past their expectation: `max(expected - elapsed, 0.1 x expected + 2 s)`), until the head fits
   budget and memory. Running jobs carry `started_at` and `expected_ms` for this.
2. **A later job may start** (EASY backfilling) when it fits now and either:
   a. **extra resources**: the jobs that would still run at the shadow time, plus this job
      (and every job already backfilled) for the whole interval, plus the head, fit the budget
      and the usable memory; this needs no duration and works for first runs; or
   b. **ends in time**: its learned duration, as `max(1.5 x expected, expected + 10 s)`, ends
      before the shadow time. Jobs without a learned duration only use rule (a).
3. **What does not change:** over-admission, PSI blocking and lane fairness keep their rules;
   a backfilled job keeps its rank for later passes; nothing is killed for overrunning.
4. **Measured and switchable:** the head's actual start minus its first shadow time is
   recorded (`backfill_head_delay_ms` in `status` totals), with the backfilled count; the
   shadow time is shown on the reserved head in `status`/JSON; `[scheduler] backfill = false`
   restores the strict reservation.

## Consequences

- The reservation's guarantee becomes "nothing starts that is not shown not to delay the
  head", which keeps ADR-0007's no-starvation property; an estimate error can delay a head by
  at most the overrun of a backfilled job, and every pass recomputes the shadow.
- A head with an inflated guessed memory shrinks backfill room (ADR-0022 reduces such guesses).
- A hung running job makes the shadow pessimistic until it ends (ADR-0024 reports it).

## Alternatives considered

- Duration rule only: no benefit for first runs; noisy for short jobs.
- Conservative backfilling (a reservation for every queued job): complexity without measured
  benefit at Sluice's queue lengths.
