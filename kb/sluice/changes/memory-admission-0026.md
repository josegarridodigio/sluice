---
title: "Memory admission keeps what it learned (ADR-0026)"
tags: [sluice, changes, memory, admission, scheduler, containment, systemd-oomd]
---

# Memory admission keeps what it learned

Decision: [[sluice/design/adr/0026-memory-admission-keeps-what-it-learned.md]]. Evidence:
[[sluice/research/memory-review-2026-10-30.md]]. Branch `feat/memory-admission-0026`.

## What changed

1. **Family estimate** (`scheduler.rs`): new `Basis::Family`; `same_family` compares
   `(repo, gate)`; `Scheduler::family_of` gives the largest learned peak of the other
   command hashes of a gate; `QueuedSnap`/`RunningSnap` carry `family_peak`;
   `MemoryFacts::estimate_primed` takes it after the key's own learned peak (the larger of
   the family and `1.2 x` the lower bound wins). `MemPass::keys` blocks a probe beside any
   running job of the same family.
2. **Only a guess probes**: `MemPass::probe_ok` requires `Expected::guessed()` (new), so a
   lower bound or a family peak is admitted on its whole estimate.
3. **Margins once** (`memory.rs`, `expected_peak`): `max(1.2 x passing, 1.25 x killed)`.
4. **Settling after a memory kill** (new `settle.rs`, `Settle`/`Verdict`, 10 s minimum):
   `server.rs` calls `note_memory_kill` from `record_interventions` (kernel and oomd kills
   and guard cancels of a `jobs/` leaf, `is_gate_memory_kill`) and from `memory_end`; `admit`
   blocks every start, the starvation guard included, while `settling` holds, reading the
   gates' scope `memory.pressure` (`gates_some_avg10`). Log lines: "admission settles after
   a memory kill ..." and "admission resumes: ...".
5. **Observed reserve** (`memory.rs`: `observed_reserve`, `outside_gates`, `OutsideTrack`,
   `HEAVY_JOB_BYTES`, `GROWTH_WINDOW`; `facts` takes the observed value, capped at
   `MemTotal / 4`; `MemoryFacts::reserve_observed`): `sample_tick` records the memory
   outside gates for every running job; `close_job` stores the growth of jobs whose peak
   reached 1 GiB in the new table `outside_growth` (`migrate.rs` containment schema 4,
   `store.rs` `insert_outside_growth`/`outside_growth_recent`) and in
   `Shared::growth`, loaded at start (`load_growth`). Proto `MemoryBudget::reserve_basis`
   (`configured | observed`), shown in the `memory:` status line.
6. **Visible exemption**: `Plan::beyond`, `MemPass::beyond`, `MemoryNote::beyond` and
   `memory_beyond_line` ("starts alone beyond its estimate: ..."); `RunningView` and proto
   `RunningJob::memory_beyond_estimate`, shown after the job's memory in `status`.
7. **Agents' oomd window** (`sluice-gate-core` `cgroup/detect.rs`):
   `DERIVED_DURATION_CAP` 15 s and `derive_pressure_policy` uses `3/4` of the ancestor's
   duration in whole seconds (Ubuntu: 15 s agents, 8 s gates). Docs: `configuration.md`,
   `cli.md`, `debugging.md`.
8. **Kill message** (`memkill.rs`): `MemoryEvidence` gains `outside_bytes` and
   `other_arguments`; the message names the memory outside gates, says when other
   arguments did not lower the peak (`did_not_lower`, `PeakCache::family_expected`) and
   advises to rerun unchanged once memory is free or tell the human. Same advice in
   `guard::cancel_message`, the session-start rule 6, the MCP instructions, `docs/agents.md`
   and the plugin skill.

## Verification

Through Sluice on 2026-10-30: `fmt`, `clippy` and `test` all pass on the final tree. New tests:
`settle.rs` (4), `memory.rs` (expected peak, observed reserve, outside memory), scheduler
(family estimate, a new hash of a running gate waits, lower bound never probes, beyond flag
and line), `memkill.rs` (message), `detect.rs` (15 s / 8 s), `compat.rs` (0.8.0 opens a
containment-4 file).
