---
title: "Change: W6e, the opt-in Sluice-side memory guard"
tags: [sluice, change, queue-trust, memory, guard, admission, w6]
---

# Change: W6e, Sluice-side memory guard (ADR-0019 piece 7)

Plan: W6e of [[sluice/plan/queue-trust-plan.md]]. Decision:
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]] (approved 2026-10-26). Analysis:
[[sluice/research/memory-gates-w6-analysis.md]] (B6, B7). Written without running tests (the
`cargo test` hook): `cargo fmt` and `cargo check --workspace --all-targets -j 4` only; the full
verification is the plan's final step.

## What changed

1. **Config** `[admission] memory_guard = false` (`AdmissionConfig.memory_guard`; a bool, so
   serde validates it). One-way for older binaries (`deny_unknown_fields`): fixture
   `tests/fixtures/unreleased/admission-memory-guard.toml`, `docs/compatibility.md`.
2. **Pure decision** `crates/sluice-gate-daemon/src/guard.rs` (new): `Guard::observe(now_ms,
   Sample, &[GuardJob]) -> Option<Victim>`, `in_trouble`, `cancel_message`,
   `parse_full_avg10`/`read_full_avg10`. Constants: `TROUBLE_MS = 3000`, `MIN_TROUBLED_SAMPLES = 2` (review correction), `COOLDOWN_MS =
   15_000`, `PRESSURE_FULL_AVG10 = 10.0`. Trouble = `MemAvailable < reserve` AND the jobs' working
   sets together `> reserve` AND (`full avg10 >= 10` OR `MemAvailable < reserve / 2`). Three
   seconds of trouble (see "Review correction") choose the job with the largest working set (smallest id on a
   tie); then a 15 s rest during which nothing is counted. A disabled guard never chooses.
3. **Daemon wiring** (`server.rs`): `Shared.guard`; `sample_tick` ends with `guard_tick` (also
   when no job runs, so the count resets). `guard_tick` reads `<proc_root>/meminfo`, resolves the
   reserve (`memory_reserve_bytes`), takes the working sets already sampled by W6b
   (`JobUsage::memory_now`) of gate and `exec` jobs that are running and not already cancelled,
   and the pressure (`guard_pressure`: the daemon scope's `memory.pressure` `full avg10`, else the
   system's `<psi_root>/memory`). `cancel_for_memory` goes through the normal cancel path
   (`RunningHandle::cancel`, process group kill), sets `entry.cancelled`, `entry.killed_by =
   "memory"` (`memkill::KILLED_BY_MEMORY`) and the new `entry.guard_message`; `close_job` replaces
   the generic "was killed" with that message (`guard_message`). W6a's classification does not
   overwrite it: `merged_killed_by` already lets Sluice's own cancel reason win, and the
   classifier never calls a cancel a memory kill. Clients print it through the existing
   `killed_for_memory` paths.
4. **Intervention**: cause `sluice_memory_guard` (`interventions::CAUSE_SLUICE_MEMORY_GUARD`),
   leaf `jobs/<id>`, detail = the message, recorded through `record_interventions` (which logs at
   warn and appends the explain log); an extra `warn` line carries the figures.
5. **Doctor** (`memory-guard`): `GuardFacts`, `memory_guard`, `oomd_gap`. States whether the
   guard is on; when it is off and oomd cannot protect the gates' scope (containment off, no
   memory controller, oomd inactive, systemd < 257, `memory_policy = "none"`) it suggests
   `[admission] memory_guard = true`. Always an `ok` line (advice, not a defect).

## Decisions

- **Pressure source**: the daemon scope's own `memory.pressure` (what oomd would watch), with the
  system's `/proc/pressure/memory` as the fallback so the guard also works without containment
  (cgroup v1, no user manager) as B7 requires. It is a share of time, not an absolute size.
- **Thresholds are constants**, not keys: three keys would be three things to tune and to keep
  compatible; the values mirror oomd's own behaviour (limit order of the agents' policy, 15 s
  post-action delay).
- **Only jobs not already cancelled count** as candidates and as "held": a cancelled job whose
  memory has not come back yet would be chosen twice; the cooldown covers the interval.
- **Never an agent's process**: the candidate list is `State.jobs` entries with a process and a
  stats key, i.e. Sluice's own gate and `exec` jobs.
- Single job: acts only when it alone holds more than the reserve (then it is the culprit, and
  nothing else could be cancelled to help).

## Tests (none creates memory pressure, none allocates)

- `guard::tests`: third consecutive sample chooses the largest; one calm sample resets; each
  condition is needed (available, held above reserve, pressure threshold, half-reserve fallback
  without a pressure reading); a single job below (or exactly at) the reserve is never touched;
  a single job above it is; cooldown of 15 s with nothing counted, then a fresh run of 3; ties by
  id; disabled never acts; no jobs no trouble; message figures; pressure file parsing.
- `config`: `the_memory_guard_is_off_unless_asked_for`, compat fixture
  `admission_memory_guard_fixture_parses`.
- `tests/daemon.rs` (fake `proc_root` with `meminfo` and per-pid `stat`/`status`, fake pressure
  directory, real tiny `sleep` gates): `the_memory_guard_cancels_the_largest_gate_under_exhaustion`
  (healthy: nothing; exhausted: the 6 GiB gate ends `killed`, `killed_by = memory`, the guard's
  message with the figures, process group dead, one `sluice_memory_guard` row on `jobs/<id>`, the
  3 GiB gate keeps running through the cooldown),
  `the_memory_guard_does_nothing_when_off_or_when_nothing_is_to_blame` (off: both keep running
  under the same exhaustion; on with a lone gate below the reserve: untouched).
- Doctor: `the_guard_line_says_whether_it_is_on_and_suggests_it_where_oomd_cannot_act`,
  `the_oomd_gap_names_the_first_missing_condition`.

## Gaps

- The scope's `memory.pressure` branch of `guard_pressure` is not exercised by a daemon test
  (containment needs a user manager; the daemon tests run without it); the test drives the
  fallback file. The file read is the same `read_full_avg10`, covered by the unit test.
- Real exhaustion is not provoked anywhere (rule of the plan); the thresholds are therefore
  untested against a real stall and may need adjusting after the first field reports.

## Docs

`kb/sluice/design/containment.md` (§4, §7), `docs/configuration.md`, `docs/debugging.md`,
`docs/compatibility.md`, ADR-0019 (piece 7 note).

## Review correction

The guard counted `TROUBLE_SAMPLES = 3` samples assuming one-second ticks, but the sampler runs
every `psi_poll_interval` (50 ms in the daemon tests, configurable), so the real wait was
anything from 150 ms up. It now waits for **wall time**: trouble must persist for
`TROUBLE_MS = 3000` ms from the first troubled sample of the run, with at least
`MIN_TROUBLED_SAMPLES = 2` troubled samples (a lone sample never acts), whatever the period.
A calm sample or the cooldown resets the run. `Guard` keeps `since_ms`; `troubled()` still
counts the samples. Unit tests updated for the 3 s rule (acts at 3000 ms) and
`the_wait_is_wall_time_whatever_the_sampling_period` (200 ms tick, 5 s tick, a lone sample);
the daemon test now takes about 3 s. Docs: ADR-0019 piece 7, `containment.md`, `docs/configuration.md`.

A second review correction: `detach_with` set `killed_by = "last-subscriber"` unconditionally, so a
client that dropped its connection right after a guard cancel overwrote `memory`. Both `detach_with`
and the forced stop now go through `note_killed_by`, which keeps the first cause (unit test
`the_first_cancel_cause_is_kept`). No other place overwrites a cause: the queued-job drop of
`stop` runs on jobs that are not running, and `cancel_for_memory` only acts on a job that is not
already cancelled.
