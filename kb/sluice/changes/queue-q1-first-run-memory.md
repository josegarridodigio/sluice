---
title: "Queue Q1: first runs measure their memory (ADR-0022)"
tags: [sluice, changes, queue, memory, admission]
---

# Queue Q1: first runs measure their memory

Wave Q1 of [[sluice/plan/queue-scheduling-plan.md]] (S4). Decision:
[[sluice/design/adr/0022-first-run-memory-estimate.md]]; evidence:
[[sluice/research/queue-scheduling-analysis.md]] sections 1.3, 1.6, 2.3, 4.3, 4.4.
Status: implemented; verification through Sluice pending (the lead runs `fmt`,
`clippy` and `test`).

## What changed

1. **Lower bounds.** A run that ends `fail` or `timeout` stores its peak as a lower
   bound. A pass and a memory kill keep their meaning (`gate_memory`, samples); a cancel,
   an early-verdict kill, a cached or errored run store nothing. Weights and durations
   still learn from passes only (`learns_from` is unchanged).
2. **Estimate order** per key: learned > `1.2 x` the largest of the last 5 lower bounds
   (`lower-bound`) > capped prior (`guessed`): `clamp(P75 of the learned peaks of the keys
   of lighter or equal weight, 1 GiB, weight share of usable memory)`; the weight share
   alone when nothing is learned. When the share is below 1 GiB the share wins.
3. **First-run probe.** A key without a learned peak that does not fit its estimate may
   start when `0.25 x` the estimate is free, no other job of the key runs or started in
   the pass, and no probe runs or started on the machine. While running, a probe holds
   `0.25 x` its estimate as headroom, not the full figure. Over-admission still needs a
   learned peak (a guess and a lower bound never over-admit). A probe is a nominal
   admission on the wire (`admission` stays `nominal`: the column is persisted); it is
   flagged inside the scheduler and in the log.
4. **Visibility.** `memory_expected_basis` (`learned | lower-bound | guessed`) on running
   and queued jobs of `status --json` (additive); the text status spells it
   (`~0.4 GiB (lower bound)`, `~2.2 GiB (guessed)`); the "waits for memory" line ends the
   figure with the basis; a probe start logs
   `... starts as a memory probe: expected 14.0 GiB guessed, needs a quarter (3.5 GiB),
   free at start ..., reserve ..., waited ...s for memory`.

## Migration

`containment_schema` 3 (`CONTAINMENT_3` in `migrate.rs`): table `gate_memory_bound`, the
columns of `gate_memory`, one index. Additive, applied on open without a backup, like
`gate_memory` in 0.6.0; older binaries ignore the table and a newer version is not an error
for them.

## Deviation from the ADR

The ADR says "additive column". A column of `gate_memory` cannot work: released binaries
select every row of `gate_memory` with no filter and load each as a finished run's working
set, so a bound stored there would be taken for a measurement and lower their estimates.
A table of its own makes "older binaries ignore it" true. Two smaller interpretations: the
running headroom of a probe is a quarter of its estimate (otherwise the probe would hold
the whole guess and block everything the probe was meant to let through), and a probe note
is logged for every probe start, not only after a wait.

## Files and symbols

- `crates/sluice-gate-daemon/src/scheduler.rs`: `Basis`, `Expected { bytes, basis }`,
  `PeakPriors` (now `Vec<(weight, peak)>`), `prior_peak` (P75), `PRIOR_FLOOR_BYTES`,
  `MemoryFacts::estimate_primed` (lower bound parameter), `estimate_job`, `MemPass`
  (`probing`, `keys`, `probe_ok`, `start`), `Pass::probe_mem`, `Pass::nominal`,
  `Plan::probes`, `QueuedSnap::lower_bound`, `RunningSnap::{lower_bound, learn_key,
  probe}`, `Scheduler::set_lower_bound`, `Job::probe`, `MemoryNote::probe`,
  `memory_note_line`, `memory_probe_line`.
- `crates/sluice-gate-daemon/src/memory.rs`: `PeakCache::{push_bound, lower_bound,
  lower_bounds}`, `priors` (one entry per learned key).
- `crates/sluice-gate-daemon/src/store.rs`: `GateMemory::is_lower_bound`,
  `Store::{insert_gate_bound, gate_bounds_recent}`, `prune_gate_memory` (both tables).
- `crates/sluice-gate-daemon/src/migrate.rs`: `CONTAINMENT_3`, schema version 3.
- `crates/sluice-gate-daemon/src/server.rs`: `learned_memory`, `load_peaks`,
  `seed_peaks`, `close_job`, `announce_closed`, status views, log selection.
- `crates/sluice-gate-proto/src/lib.rs`: `memory_expected_basis`, `expected_text`.
- Docs: `docs/how-it-works.md`, `docs/cli.md`, `docs/debugging.md`,
  `docs/configuration.md`, `docs/compatibility.md`, `CHANGELOG.md`.

## Tests

Scheduler (pure): estimate ordering and cap clamping, P75 prior, probe admission (the
evidence case, one per key, one per machine, share, learned never probes, quarter held),
lower bound never over-admits, probe log format, scheduler-level flag and serialisation;
the memory-admission property test now accounts for probes. `server`: lower bound on
fail/timeout, none on pass, memory kill, cancel, early verdict. `memory`: bound window.
`store`: bound rows, 20 per key, prune, file without the table. `compat`: every old
database reaches version 3, v1 and v2 migrate in place without a backup, older releases
open a v3 file. `proto`: basis text and additive JSON field.

Note on the evidence replay: with the 14 GiB guess and 20 GiB available the job simply
fits; the test replays the case with a running suite whose headroom leaves 5 GiB free, so
the guess does not fit and a quarter (3.5 GiB) does.

Measure afterwards (ADR-0022): guessed-wait minutes per day, probes killed for memory
(target 0), probe peak against estimate, runs until a key is learned.

Lead review: accepted both deviations (separate `gate_memory_bound` table so released binaries
never read a bound as a measurement; a running probe holds a quarter of its estimate as headroom,
corrected every pass from its real working set). Fixed four clippy findings
(`redundant_closure_for_method_calls` in `server.rs` and `scheduler/tests.rs`, `type_complexity`
in `server.rs::seed_peaks`, now `KeyedPeaks`). Full fmt, clippy and test through Sluice: pass.
