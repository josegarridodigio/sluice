---
title: "Zero-config: review and reconciliation"
tags: [sluice, changes, zero-config, review]
---

# Zero-config: review and reconciliation

Plan: [[sluice/plan/zero-config-plan.md]] tasks R1–R3 (2026-10-23).

## What landed

All tasks of the plan are merged on the branch `zero-config-integration`:
T1 [[sluice/changes/heavy-classification.md]], T2
[[sluice/fixes/gate-priority-inheritance.md]], T3
[[sluice/changes/init-detection.md]], T4
[[sluice/changes/automatic-weights.md]], T5
[[sluice/changes/session-start.md]], T6
[[sluice/changes/zero-config-docs.md]], proofs
[[sluice/changes/zero-config-proof.md]] (s8 37/37, s7 64/64) and
[[sluice/changes/plugin-e2e-zero-config.md]] (10/10 cases on haiku, Claude
Code 2.1.309). Decisions: ADR-0014, ADR-0015, ADR-0016. Spike:
[[sluice/research/plugin-path-spike.md]].

Verification of the merged tree: `sluice run fmt`, `clippy` and `test`
pass (test 110.7 s).

## Review findings (R1) and fixes (R2)

| Finding | Severity | Resolution |
|---|---|---|
| `ionice` without `-t` exits 1 without running the gate when `ioprio_set` is refused | high | fixed: `-t` added (`executor.rs::command`) |
| `ten_concurrent_shimmed_makes_stay_within_the_budget` flaky: an idle `make` learns weight 1 after three runs (intended since 0.4.0) | test | learned weights off in that test; `twenty_concurrent_execs_respect_the_budget` is safe (one label per job) |
| s7 asserted a fixed exec concurrency | test | s7 sandbox sets `learned_weights = false` (E1) |
| A shim call equal to a gate's command becomes `sluice run <gate> -v`: stdout carries the verdict, the tool's output goes to stderr (as with the guard's gate rewrite since 0.3.0) | behaviour | documented here; unchanged |
| The session-start wrapper logs nothing when the binary is missing or older | behaviour | documented in `docs/debugging.md` |
| Git hooks run by husky put `node_modules/.bin` first, so shims cannot catch `ng` there | limit | `sluice init` rewires the hooks (ADR-0016); s8 covers a husky-style wrapper |
| Wrapper, env line quoting, fail-open paths, `SLUICE_JOB` export by gate and exec jobs | checked | no issue |
| Client names in fixtures, docs, KB, commits | checked | none |

## Open items (not blocking 0.4.0)

- Exec weight learning is tested at scheduler level and through s8, not by a
  daemon integration test through `register_exec`.
- Commands invoked by absolute path, under `env -i` or from login shells that
  reset `PATH` are not caught (ADR-0014).
- The plugin e2e and the spike depend on Claude Code's `CLAUDE_ENV_FILE`
  behaviour; re-run them when Claude Code changes (`scripts/plugin-e2e.sh`,
  cases f–j).

Open discrepancies between code, KB, docs and CHANGELOG: none found.
