---
title: "ADR-0007: FIFO with backfill and head-of-line reservation"
tags: [sluice, adr, scheduler]
---

# ADR-0007: FIFO with backfill and head-of-line reservation

Status: accepted (2026-10-22), designed for Phase 2; amended by [[sluice/design/adr/0011-scheduler-throughput.md]] (default budget, learned weights, over-admission, lane fairness and humans first; the no-starvation guarantee through `head_reserve_after` is kept).

## Context

The daemon admits weighted jobs under a global budget. Pure FIFO wastes
capacity: a weight-6 suite at the head of the queue blocks weight-1
linters while 5 units sit idle. Pure backfill starves heavy jobs: a steady
stream of light jobs can keep the budget from ever draining enough for the
heavy one. Agents issue both kinds constantly.

## Decision

- Order by arrival (FIFO).
- **Backfill:** jobs behind the head may start whenever they fit, as long
  as the head has waited less than `scheduler.head_reserve_after`
  (default 30 s).
- **Reservation:** after that, the head reserves the budget: no job behind
  it starts until the head fits. Running jobs finish normally.
- No priorities, no preemption, no dynamic weight resizing in v1 (weights
  are only clamped at enqueue to the budget and the repository cap).
- The reservation applies only when the head is blocked by the budget; a
  head blocked by its repository cap never stops other repositories.

## Consequences

- Worst-case extra wait for a heavy job is bounded by
  `head_reserve_after` plus the longest job already running.
- Utilisation stays high in the common case (short bursts of light jobs).
- One knob to document and tune; `sluice status` shows `reason` per queued
  job so the behaviour is observable.

## Alternatives considered

- Strict FIFO: simplest, poor utilisation with mixed weights.
- Weighted fair queuing per repository: fairer across projects, but more
  state and harder to explain; the per-repo cap covers the practical case.
- Priorities (humans before agents): tempting, but humans mostly hit the
  cache and agents are the bulk; revisit if real usage shows starvation.
