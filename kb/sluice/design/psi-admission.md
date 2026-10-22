---
title: PSI-based admission
tags: [sluice, design, psi, phase-4]
---

# PSI-based admission (Phase 4)

Status: implemented in Phase 4 (`sluice-gate-daemon::psi`), see [[sluice/changes/phase-4-psi.md]]. Related: [[sluice/design/scheduler.md]],
[[sluice/design/protocol.md]] (`budget.psi` in `status`),
[[sluice/design/config-format.md]] (`[admission]`).

## Why

The budget bounds how many gate units Sluice itself runs, but the machine
also hosts the agents, editors, browsers and whatever else the user runs.
Linux Pressure Stall Information reports how much time tasks spend stalled
on CPU or memory, which is exactly "is this machine already saturated".

## Source

- `/proc/pressure/cpu` and `/proc/pressure/memory`, lines
  `some avg10=<f> avg60=<f> avg300=<f> total=<µs>` (and `full …` for
  memory). Sluice reads `some avg10` for both.
- Availability: kernel ≥ 4.20 with `CONFIG_PSI=y`; some distributions boot
  with `psi=0`. If **both** files are unreadable at a sample (even once),
  admission falls back to budget-only for the rest of the daemon's life,
  `status` reports `psi.enabled = false` with null values and one `warn`
  is logged. With one file missing or a malformed value, PSI stays
  enabled and that metric is `null` (`n/a`), counted as calm for blocking
  and resuming; the parser accepts `some … avg10=<f>` anywhere on the
  line, rejects negative or non-finite numbers and ignores `full` lines.
  With `psi_enabled = false` no sampler runs and nothing is logged; the
  two cases are indistinguishable in `status` (`enabled: false`). The
  root path (`/proc/pressure`) is a daemon config field so tests can fake
  it. `[admission]` is read once at daemon start.

## Rule

Sampled every `admission.psi_poll_interval` (default 1 s) by a daemon
task (first sample at start, fixed period); the scheduling loop is woken
only when the blocking verdict changes. Block and resume transitions are
logged at `info` with the sampled values.

```
blocking = psi_enabled
        && (cpu.some.avg10 > admission.cpu_some_avg10_max      # default 60.0
            || memory.some.avg10 > admission.memory_some_avg10_max)   # default 20.0
```

- While `blocking`, no new job starts (the head-of-line reservation is
  not evaluated either); running jobs continue. Every queued job reports
  `reason: "psi"` in `status`, whatever else would block it.
- Hysteresis: `avg10 > max` blocks (strict); once blocking, admission
  resumes only when both values are `< 0.8 × max` for two consecutive
  samples (a sample in the 80–100 % band or above resets the count), so a
  job burst does not flap. Thresholds are validated as percentages in
  `[0, 100]`; `0` blocks on any pressure.
- Starvation guard: evaluated on every scheduling pass (wake-ups and the
  1 s tick); if the queue head has waited strictly more than
  `admission.psi_max_wait` (default 5 min, measured from its `queued_at`,
  so time spent waiting on the budget counts) while PSI blocks, the head
  alone is admitted if it fits the budget and its repository cap (no
  other job is tried); at most one admission per `psi_max_wait`, so a
  backlog does not drain at once (pressure caused by other users' work
  must not stall Sluice forever). Logged at `info` with the job id.
- Weighted admission under moderate pressure is out of scope for v1
  (binary gate is enough to stop the "machine unresponsive" failure mode).

## Configuration additions

```toml
[admission]
psi_enabled = true
cpu_some_avg10_max = 60.0
memory_some_avg10_max = 20.0
psi_poll_interval = "1s"
psi_max_wait = "5m"        # Phase 4 addition
```

## Observability

`sluice status --json` → `budget.psi = { enabled, cpu_some_avg10,
memory_some_avg10, blocking }` (values `null` when unknown) and
`totals.psi_blocked_ms_total`: blocked time accumulated per sample (each
sample adds the previous interval when the state before it was blocking,
so the onset interval is not counted and the resume interval is), kept in
memory for the current daemon only (resets on restart, keeps what accrued
if PSI later becomes unavailable), JSON only. Text mode prints, between
`budget:` and `running:`, `pressure: cpu 12.3% mem 0.4% (ok)`,
`(blocking admission)` or `(unavailable)` with `n/a` for null values
(also shown when PSI is disabled in the config). The `status` JSON schema is pinned by
`crates/sluice-gate-proto/tests/status_schema.rs` against a fixture of one
fully populated report; adding a field means updating the fixture
deliberately. **This JSON is the metrics export**: the user's HTML
dashboard reads `sluice status --json` (runs, passes, failures, hits,
coalesced, wait, run and saved time, PSI); there is no separate exporter.

## Acceptance (Phase 4)

- Unit: parser for both files, including missing `full` lines and
  malformed input; the hysteresis and starvation-guard state machine with
  synthetic samples.
- Integration (`tests/daemon.rs`): with the PSI root pointed at fake
  files reporting `some avg10=99`, a queued job waits with `reason: psi`
  and `budget.psi.blocking = true`, then starts after two calm samples;
  with `psi_max_wait = 300 ms` the head starts while still blocking and
  the log says so; with a missing root `psi.enabled == false` and jobs
  start normally. Real pressure is measured with
  `scripts/psi-pressure-demo.sh [loops] [seconds]` rather than in CI (busy
  loops equal to `nproc` barely move `some avg10`; see the numbers in
  [[sluice/changes/phase-4-acceptance.md]]).
