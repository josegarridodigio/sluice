---
title: "Plan: queue scheduling (backfilling, stalled jobs) and protected targets"
tags: [sluice, plan, daemon, scheduling]
---

# Plan: queue scheduling (backfilling, stalled jobs) and protected targets

Approved by the user on 2026-10-28; started after release 0.7.0. Research:
[[sluice/research/queue-scheduling-analysis.md]] (S1, S2, S4) and
[[sluice/research/protected-targets-analysis.md]] (S3). Decisions:
[[sluice/design/adr/0022-first-run-memory-estimate.md]] (S4),
[[sluice/design/adr/0023-backfilling-behind-a-reserved-head.md]] (S1),
[[sluice/design/adr/0024-stalled-jobs-are-reported-never-killed.md]] (S2); S3 gets its ADR
when its wave starts. Sub-agents run on Sonnet; each wave ends with the full verification
through Sluice, Matrix on the branch before a release (watched job by job).

## User decisions (2026-10-28)

| # | Question | Answer |
|---|---|---|
| 1 | Order | Queue first (S4, S1, S2), then S3 |
| 2 | Stalled jobs | Report only, never kill (no kill setting) |
| 3 | Enqueue on a target that requires pull requests | Warn only (never refuse) |
| 4 | Pull-request mode, target moves before the forge merges | Wait for the forge; never call a different merged tree verified |

## Waves

| Wave | Content | Status |
|---|---|---|
| Q1 | S4: lower bounds from failed runs, capped prior, first-run probe (ADR-0022) | done, verified through Sluice ([[sluice/changes/queue-q1-first-run-memory.md]]) |
| Q2 | S1: shadow time, extra-resources and duration rules, head-delay counter, switch (ADR-0023) | done, verified through Sluice ([[sluice/changes/queue-q2-backfilling.md]]) |
| Q3 | S2: stall detection and reporting, longest quiet window per job (ADR-0024) | done, verified through Sluice ([[sluice/changes/queue-q3-stalled-jobs.md]]) |
| Q4 | S3: protected targets: detection (warn), then pull-request landing (ADR to write) | pending |

## Evidence (a second machine, 12 threads, 30 GiB, 2026-10-28, daemon 0.6.3)

- `sluice status`: 3h02m with jobs queued that day, 1h12m of it with the CPUs idle.
- Small jobs (`typecheck` expected 28 s, 1.3 GiB; `mypy` 2.6 GiB; `ruff` 105 ms) waited
  15–23 min as `head-reserved` behind a head job waiting for memory (13.8–14.6 GiB learned),
  with 15 GiB available.
- Two test gates hung (no output, no CPU, 7.6 GiB resident) until their 30 min timeout,
  holding the memory budget and the queue both times. The hangs were the repositories' own
  test bugs; Sluice behaved as configured.

## S1 — Backfilling behind a reserved head

The head reservation keeps a big job from starving. Let a later job start when it fits the
free budget **and** is expected to finish before the head could start (EASY backfilling):
the head's earliest start is when enough running jobs end (their expected remaining time
from the learned durations); a candidate's expected duration (learned, with a margin) must
end before it. Unknown durations never backfill. Measure: queue wait and CPU-idle waiting
before/after on the same workload.

## S2 — Stalled-job detection

A job whose own counters (CPU, block I/O, log bytes, page faults, process count) stay flat
for a window is reported as `stalled` in `status`/`verify`/MCP, with the memory and weight
it holds. Never killed (user decision; ADR-0024).

## S3 — Targets protected on the forge (issue #5)

Done in the hook-runner release: `docs/merge-queue.md` says the queue cannot land on a target
protected against direct pushes. Next, one ADR for both:

- **Early detection** (`target_protected`): at enqueue and in `sluice doctor`, tell before any
  verification that the target refuses direct pushes. Forge-agnostic where possible
  (GitHub/GitLab APIs through their CLIs when present and authenticated); an inconclusive
  check never blocks, it only warns.
- **Pull-request landing** (`[queue] land = "push" | "pull-request"`): after approval, push the
  verified commit to a branch and open or update the pull request through the forge's CLI;
  the train is `landed` when the forge merges it (reconcile watches the PR). Decide forge
  detection, credentials (the approver's env, as for pushes), what "landed" means when the
  forge squashes or rebases, and how a PR closed without merge dissolves the train.

## S4 — First-run memory estimate that never converges for a failing gate

Seen the same day: a Python repository's `pytest` gate had no passing run, so its memory stayed "guessed" at
13.8 GiB (the cold-start prior: the largest learned peak of a gate of lighter or equal weight,
here another repository's 12 GiB test suite). It waited 26 min for memory, ran for 1m40s,
failed, and the failure taught nothing (`learns_from` takes passing runs only, rightly for
weights: a failing run often stops early), so the next run waited again behind the same guess.
A failed run's peak is a lower bound, not an estimate; options to evaluate: let a guessed
job start when the free memory covers a bounded share of the guess while no job of the same
gate runs; decay the prior towards the machine's median learned peak; or record failed-run
peaks as lower bounds and use max(lower bound, a capped prior). Measure on the recorded logs.
