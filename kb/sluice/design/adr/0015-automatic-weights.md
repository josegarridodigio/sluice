---
title: "ADR-0015: Automatic weights and learned exec weights"
tags: [sluice, adr, zero-config, scheduler, config]
---

# ADR-0015: Automatic weights and learned exec weights

Status: accepted (2026-10-23). Plan: [[sluice/plan/zero-config-plan.md]]
task T4. Builds on [[sluice/design/adr/0011-scheduler-throughput.md]]
(learned weights, M2) and amends
[[sluice/design/adr/0013-compatibility-promise.md]].

## Context

A gate's `weight` is a guess the user must make. On a trial on a real Angular
project a `test` gate declared with weight 8 started 20 workers, and every
`sluice exec` job (builds, linters routed by the guard) reserved half the
budget whatever it really used, because learned weights only apply to gates.
The user wants weights that need no guessing, but also that a declared
number keeps its meaning.

## Decision

- A declared integer `weight` stays a ceiling: learned weights only lower it,
  never below `max(1, ceil(weight / 4))` (unchanged from ADR-0011).
- An omitted `weight` keeps meaning 1. Changing that default would break the
  compatibility promise (every 0.1.0 or 0.2.0 file loads with the same
  effective values).
- New value `weight = "auto"`: the ceiling is `ceil(budget / 2)` of the
  daemon that admits the job (not the client's view of the budget); learning
  lowers it like any ceiling. `sluice init` writes `"auto"` for test runners
  and multi-threaded builds.
- `sluice exec` jobs learn too: their statistics key is the repository root
  (or the canonical cwd outside git) plus `exec:<label>`. The ceiling is
  `--weight` when given, else `[agent] exec_weight` or `ceil(budget / 2)`.
  Same window and warm-up as gates.
- `sluice status` and its JSON show the configured and the effective weight
  (optional fields `weight_effective`, `weight_auto` in schema 1).
- `sluice doctor` warns when a gate's unbounded learned weight exceeds its
  declared ceiling by more than half, suggesting a higher number or `"auto"`.

## Consequences

- A `.sluice.toml` with `"auto"` is rejected by binaries older than 0.4.0
  (they know only integers). This is the one-way compatibility ADR-0013
  already allows for new keys, extended to a new value of an existing key;
  `docs/compatibility.md` and the CHANGELOG say so.
- A job that truly needs more than half the budget under `"auto"` is admitted
  at half and relies on over-admission and PSI; users who know better declare
  a number.
- Exec statistics grow the `gate_stats` data by one key per label and
  repository; pruned like gate statistics.
