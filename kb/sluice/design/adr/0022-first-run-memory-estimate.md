---
title: "ADR-0022: A gate's first runs measure their memory instead of waiting for a guess"
tags: [sluice, adr, daemon, memory, admission]
---

# ADR-0022: A gate's first runs measure their memory instead of waiting for a guess

Status: accepted (2026-10-28, user decision: queue items first, this one first). Research:
[[sluice/research/queue-scheduling-analysis.md]] (sections 1.3, 1.6, 2.3, 4.3). Plan:
[[sluice/plan/queue-scheduling-plan.md]] (S4). Amends the 0.6.1 note of
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]] (cold-start prior).

## Context

Memory admission (ADR-0019) admits a job when its expected working set fits. A gate with no
passing run has a *guessed* expectation: `min(weight proxy, largest learned peak of a gate of
lighter or equal weight)`. Measured on two machines (416 learned runs): the proxy
over-estimates by more than 4x in about 70 % of runs and under-estimates in 2 of 416. On a
second machine (12 threads, 30 GiB) the 10 guessed jobs of one day waited 125 minutes for
memory and then used 0.0-1.0 GiB against guesses of 13.8 GiB. Failing runs teach nothing
(`learns_from` takes passing runs only, rightly for weights and durations), so a gate that keeps
failing keeps waiting on the same guess.

## Decision

1. **Failed runs record a lower bound.** The peak of a run that ends `fail` or `timeout` is
   stored in `gate_memory` marked as a lower bound (additive column, schema group
   `containment`; older binaries ignore it). Memory-killed runs keep their current meaning.
   Durations and weights still learn from passing runs only.
2. **Estimate order for a key:** learned expectation (unchanged) > `1.2 x` the largest recent
   lower bound > capped prior, where the cap is `clamp(P75 of learned peaks of gates of lighter
   or equal weight on this machine, 1 GiB, weight share of usable memory)`; with no learned data
   on the machine, the current proxy. `status`/JSON show the basis
   (`learned | lower-bound | guessed`).
3. **A first run is a probe.** While a key has no passing run, one job of that key may start
   when free memory covers `0.25 x` its estimate and no other job of the key runs; at most one
   such probe runs on the machine at a time. Over-admission still needs a learned peak.
4. The downside of a wrong probe stays bounded by what exists: gate jobs are the preferred
   OOM victims (`oom_score_adj`, `memory.oom.group`), the optional guard, and a kill that
   explains itself (`KILLED for lack of memory`), which then teaches the key.

## Consequences

- New gates and failing gates start promptly; the estimate converges after one run.
- A probe that needs far more than a quarter of its estimate can be memory-killed once; that
  run's peak is learned and the next run waits correctly.
- Measured afterwards: guessed-wait minutes per day (log), probes memory-killed (target 0),
  probe peak vs estimate, runs until a key is learned.

## Alternatives considered

- Data-driven prior alone (median of learned peaks): too low for the one heavy first run, and
  still never measures; used here only as the cap.
- Learning failed runs as full samples: a run that stops at the first error under-states the
  suite; it is kept as a lower bound only.
