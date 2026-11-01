---
title: "A kernel ceiling on every gate job (ADR-0027)"
tags: [sluice, changes, memory, containment, cgroup, daemon]
---

# A kernel ceiling on every gate job (ADR-0027)

User decisions (2026-11-01): a dynamic ceiling on every running job (not only on jobs
started beyond their estimate), the floor is half the reserve, swap left as it is.
Incident and evidence: [[sluice/research/memory-freeze-2026-11-01.md]]. Decision:
[[sluice/design/adr/0027-a-kernel-ceiling-on-every-gate-job.md]].

## What changed

- `crates/sluice-gate-daemon/src/ceiling.rs` (new): `ceilings(available, floor, jobs)`,
  the pure decision (`anon + H / n` with `H = MemAvailable − floor`; below the floor the
  largest job gets `anon − deficit`, the others `anon + SLACK`; `MIN_CEILING`, page
  aligned), and `Writer`, a dedicated thread that writes `memory.max` with the newest
  value per leaf, skips repeats and forgets ended leaves (`retain`), because lowering
  `memory.max` below the usage blocks until the kernel reclaimed or killed.
- `crates/sluice-gate-daemon/src/server.rs`: `Shared::ceiling` and `ceiling_cut`;
  `ceiling_tick`, called by `sample_tick` after `guard_tick`, reads each running job's
  leaf `anon`, the capped `MemAvailable` (`gates_memory`) and the floor, queues the
  ceilings and logs a warning once per job when its ceiling falls below its usage;
  `ceiling_of` fills `RunningJob::memory_ceiling_bytes` in `status_report`.
- `crates/sluice-gate-daemon/src/memory.rs`: `read_anon(leaf)`.
- `crates/sluice-gate-core/src/config.rs`: `AdmissionConfig::memory_ceiling` (default
  `true`), `memory_floor` (default `"auto"`), `memory_floor_bytes(total)` (half of
  `memory_reserve_bytes`), validation; `struct_excessive_bools` allowed on the table.
- `crates/sluice-gate-proto/src/lib.rs`: `RunningJob::memory_ceiling_bytes` (additive,
  schema 1) and `ceiling_text` (`  ceiling 14.5 GiB` in the text status).
- Fixture `crates/sluice-gate-core/tests/fixtures/unreleased/admission-memory-ceiling.toml`
  and `admission_memory_ceiling_fixture_parses` in `tests/compat.rs`.
- Docs: `docs/configuration.md` (keys and table rows), `docs/compatibility.md`,
  `docs/how-it-works.md`, `docs/debugging.md` ("The kernel ceiling killed my job"),
  `kb/sluice/design/config-format.md`, `CHANGELOG.md` (Unreleased).

## Why

Six global OOMs in two days on a second machine, each after 22 s to 2.5 min of a frozen
machine, all from one test suite started through the idle-machine exemption. Victim
ordering worked; the time before the kill did not, because only the kernel acts once the
machine thrashes and it acts late. A limit written before the shortage is the only thing
that holds during it.

## Verification

- Unit tests: `ceiling::tests` (lone job, shared headroom never past the floor, below the
  floor the largest is cut and the others held, minimum and page alignment, empty, writer
  newest value / `max` / forgotten leaf), the config test
  `the_memory_ceiling_is_on_and_its_floor_is_half_the_reserve`, the proto test
  `a_running_job_shows_its_kernel_ceiling`, the compat fixture test.
- `fmt`, `clippy -D warnings` and `cargo test --workspace` through `verify`.
- The daemon tests run without containment, so `ceiling_tick` itself is exercised only on
  a real machine: after installing, a light job's leaf shows a finite `memory.max` and
  `status` shows its `ceiling`. No memory-pressure experiment was run (spike safety).
