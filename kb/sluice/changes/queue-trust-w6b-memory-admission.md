---
title: "Change: W6b, gate jobs are admitted by learned memory"
tags: [sluice, change, queue-trust, memory, scheduler, admission, w6]
---

# Change: W6b, memory-aware admission (ADR-0019 piece 1)

Plan: W6b of [[sluice/plan/queue-trust-plan.md]]. Decision:
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]]. Analysis:
[[sluice/research/memory-gates-w6-analysis.md]] (A6, A7, A8, B3, C; tests T-W6-1..5).
Written without running tests (the `cargo test` hook): `cargo fmt` and
`cargo check --workspace --all-targets -j 4` only; the full verification is the plan's
final step.

## What changed

1. **Working set sampling** (`crates/sluice-gate-daemon/src/memory.rs`, new; `sampler.rs`;
   `server.rs` `sample_tick`/`read_job_samples`). Every tick, per running gate or `exec` job:
   `anon` of the job leaf's `memory.stat` plus `memory.swap.current` (`read_cgroup`), also
   `memory.peak`; without a readable leaf the summed `VmRSS` of the job's process group
   (`read_rss`, under `<proc_root>`). `JobUsage` keeps the last value and the running peak
   (`observe_memory`, `memory_now`, `memory_peak`, `cgroup_peak`).
2. **Learning** (`gate_memory` table, `store.rs` `GateMemory`, `insert_gate_memory`,
   `gate_memory_recent`, `prune_gate_memory`; `migrate.rs` `CONTAINMENT_2`,
   `CONTAINMENT_SCHEMA_VERSION` 2). Additive migration of the containment group: no backup,
   older binaries ignore the table and are not refused by the newer version number
   (`migrate::apply` leaves a higher version alone). **`gate_stats` and `sched_schema` are
   unchanged.** Columns: repo, gate, cmd_hash, job_id, finished_at, outcome (`pass` or
   `killed`), `anon_peak_bytes`, `peak_bytes` (`memory.peak`, display only), `oom_killed`.
   Newest 20 per key, pruned with `gate_stats` (90 days unseen). `server.rs`: `learned_memory`
   (passing runs and memory kills; a failure that is not a memory kill teaches nothing; needs a
   sampled peak above zero), `Closed.memory`, `announce_closed` (updates `Shared.peaks`, a
   `PeakCache` of the last 5 runs per key, and `Scheduler::set_learned_peak`), `load_peaks` and
   `seed_peaks` at start. `memory::expected_peak`: `E = max(anon_peak of the last 5) × 1.2`, a
   killed sample `× 1.25` first. One sample is enough. `learns_from` (the weight) unchanged.
3. **Admission rule** (`scheduler.rs`, pure `plan`). New `MemoryFacts`, `Expected`,
   `Snapshot.memory: Option<MemoryFacts>` (`None` = rule off), `QueuedSnap.expected_peak`,
   `RunningSnap.current`/`expected_peak`, `WaitCause::Memory` (accumulator index 0, wire reason
   `budget`, detail `memory`), `Plan.expected`, `running_headroom`. `plan` was split into
   `Pass` (`nominal`, `blocked`, `over_admit`, `MemPass`) and lost its `too_many_lines`
   allow. The exact rule is in [[sluice/design/scheduler.md]] "Memory-aware admission".
   `Scheduler::set_memory`, `set_learned_peak`, `memory_facts`, `memory_headroom`.
4. **Daemon wiring** (`server.rs` `admit`): reads `<proc_root>/meminfo` per pass
   (`memory::read_meminfo`, `memory::facts`) and the running jobs' latest working set, then
   `Scheduler::set_memory`. Unreadable `meminfo` or `memory_aware = false`: the rule is off for
   the pass.
5. **Config** (`sluice-gate-core/src/config.rs`): `[admission] memory_aware = true`,
   `memory_reserve = "auto"` (10 % of `MemTotal`) or a size (`AdmissionConfig::memory_reserve_bytes`),
   validated. One-way for older binaries (`deny_unknown_fields`): fixture
   `tests/fixtures/unreleased/admission-memory.toml`, `docs/compatibility.md`.
6. **Status/JSON** (additive, schema 1, `sluice-gate-proto`): running jobs `memory_bytes`,
   `memory_expected_bytes`, `memory_expected_learned`; queued jobs `memory_expected_bytes`,
   `memory_expected_learned` and `wait_detail = "memory"`; `budget.memory {available_bytes,
   reserve_bytes, headroom_bytes}`. All optional and skipped when absent, so the frozen fixture
   `status-schema-1.json` did not change. `sluice status` text: `memory:` line, `mem 3.1 GiB/18.0 GiB`
   per running job (`~` marks a guess), `mem 18.0 GiB` per queued job and `(budget: memory)`.

## Decisions

- **Peak sampling, not `memory.peak`**, for admission (page cache inflates it and `MemAvailable`
  already counts cache as available); `memory.peak` is stored and shown only.
- **Proxy is computed in `plan`**, not by the caller: it needs the effective weight of the pass.
  `QueuedSnap.expected_peak` is the learned figure only; `Plan.expected` says learned or guessed.
- **`WaitCause::Memory` shares accumulator 0** (frozen `jobs.wait_*_ms` columns), so
  `done.wait_reason` of a job that waited only for memory reads `budget`; `wait_detail` and
  `logs --timeline` do not distinguish it either. Distinguishing it needs a column (not additive).
- **Reservation**: `(!fits_budget || (fits_cap && !fits_mem)) && overdue(head)`.
- **Learned peak applies to running jobs too** (headroom uses the key's current expectation).
- Doctor lines for the learned peaks are left to W6c (with the gates policy line).

## Tests (none creates memory pressure)

- `scheduler/tests.rs` (T-W6-1..3): two 18 GiB gates never start together; headroom counts what a
  running job still has to grow; jobs started in the same pass count in full; a gate alone (nothing running) runs
  with tiny memory and a running job, however light, withdraws the exemption; an overdue head blocked only by memory reserves; `Memory` reports `budget` +
  `memory` and lands in the budget accumulator; PSI first, the guard respects memory;
  over-admission needs a learned peak that fits; proxy `w/B × usable`, learned replaces it;
  property `started_jobs_never_exceed_free_memory_unless_alone` (4800 random snapshots);
  differential `memory_off_or_plentiful_plans_like_cpu_only_admission` (16 switch combos);
  `no_starvation_under_any_switch_combination` now 32 combos (memory switch);
  `an_overdue_head_blocked_only_by_memory_starts_within_the_bound`.
- `memory::tests`: `meminfo`, switch and reserve, `anon` + swap of a fake leaf, `VmRSS` sum,
  expected peak (maximum, window of 5, killed `× 1.25`, margin 20 %), `PeakCache`.
- `server::tests::memory_is_learned_from_passes_and_memory_kills_only`; `sampler::tests`.
- `store::tests` (keep 20, prune, ignored without the table); `tests/compat.rs` (T-W6-4):
  every old release's database gets `gate_memory` with `sched_schema` and `gate_stats` unchanged,
  a v1 containment database migrates in place without a backup, a 0.5.0-style migration step
  opens a v2 file.
- `tests/daemon.rs` (T-W6-5): fake `proc_root/meminfo`, second gate waits with `wait_detail =
  memory` and starts when `meminfo` recovers (wait charged to the budget accumulator); an `alone`
  gate runs with 1 GiB available and a second one then waits; `memory_aware = false` ignores `meminfo`; a real tiny gate
  (`sleep 1`, real `/proc`) writes its `gate_memory` row and the next run's expectation is learned.
  There are no fake job leaves in the daemon test: containment needs a user manager, so the daemon
  test exercises the process-group fallback; the leaf reader is covered by `memory::tests`.
- Config, proto round trip, `sluice status` rendering, compat fixture.

## Verification

`cargo fmt` and `cargo check --workspace --all-targets -j 4` clean. Clippy, tests: at the end of
the plan.

## Verification (lead)

The first `verify` found clippy items (`too_many_lines` in `UserConfig::parse`, split into
`validate_admission`, and in status `render`, split into `render_jobs`;
`similar_names`, `iter_kv_map`, `match_same_arms` (`Budget | Memory`),
`range_plus_one`, `manual_is_multiple_of`) and one host-dependent test:
`a_train_kills_its_other_gates_under_the_finish_policy`. The queue harness runs the
daemon with a budget of 2, so a first run's estimate is half the host's usable memory
and the two gates of that test (one waits for the other) could no longer run together:
the harness now sets `memory_aware = false` (memory admission is covered by its own
deterministic tests with fake `meminfo`). In production the budget is the CPU count and
that estimate is what keeps two unknown heavy suites apart. Then fmt, clippy and the
whole suite passed through Sluice (2026-10-26).

## Review correction

The `alone` exemption was too broad: it held whenever no *heavy* job (5 % of `MemTotal` or
more) ran, so a big job could start next to any number of light ones with no regard for
memory. The rule is now: a job fits when `E_j <= free` (`free = MemAvailable - reserve - H`),
or when nothing at all runs and nothing started earlier in the pass (the true progress
exemption). `MemoryFacts::heavy`, `HEAVY_PERCENT` and the `heavy` flag of `MemPass` are gone
(`MemPass.busy` replaces it; `running_memory` became `running_headroom_of`). Over-admission
still needs a learned `E` that fits, with no exemption, and the memory-blocked overdue-head
reservation is unchanged (it is what lets a big job drain the machine and start). Tests:
`a_gate_alone_runs_even_when_it_does_not_fit` (a light running job withdraws the exemption),
the property test oracle (`busy`), `an_overdue_head_blocked_only_by_memory_starts_within_the_bound`
now also with a head whose `E` never fits (40 GiB on 32), `a_gate_alone_runs_with_tiny_memory_...`
(daemon: a second gate waits while the first runs). The differential test is unchanged: with
the rule off or plenty of memory `fits` is true whatever `busy` says. Docs: design
`scheduler.md`, ADR-0019 piece 1, `docs/configuration.md`.
