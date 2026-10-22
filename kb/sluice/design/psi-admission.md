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
  with `psi=0`. If a read fails (files missing or unreadable), admission
  falls back to budget-only for the rest of the daemon's life and `status`
  reports `psi.enabled = false` (the reason is logged once). A missing or
  malformed value in an otherwise readable file counts as calm. The root
  path (`/proc/pressure`) is a daemon config field so tests can fake it.

## Rule

Sampled every `admission.psi_poll_interval` (default 1 s) by a daemon
task; the scheduling loop is notified when the blocking state changes.

```
blocking = psi_enabled
        && (cpu.some.avg10 > admission.cpu_some_avg10_max      # default 60.0
            || memory.some.avg10 > admission.memory_some_avg10_max)   # default 20.0
```

- While `blocking`, no new job starts; running jobs continue. Queued jobs
  report `reason: "psi"` in `status`.
- Hysteresis: once blocking, admission resumes only when both values are
  below 80 % of their thresholds for two consecutive samples, so a job
  burst does not flap.
- Starvation guard: if the queue head has waited more than
  `admission.psi_max_wait` (default 5 min, measured from its queue time)
  while PSI blocks, the head is admitted anyway if it fits the budget and
  its repository cap; at most one job per `psi_max_wait`, so a backlog does
  not drain at once (pressure caused by other users' work must not stall
  Sluice forever). Logged at info with the job id.
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
memory_some_avg10, blocking }` and `totals.psi_blocked_ms_total` (time the
current daemon spent blocking; kept in memory, so it resets on restart,
unlike the other totals); text mode prints `pressure: cpu 12.3% mem 0.4%
(ok)`, `(blocking admission)` or `(unavailable)` (also shown when PSI is
disabled in the config). The `status` JSON schema is pinned by
`crates/sluice-gate-proto/tests/status_schema.rs` against a fixture;
adding a field means updating the fixture deliberately.

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
