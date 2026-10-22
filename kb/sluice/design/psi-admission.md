---
title: PSI-based admission
tags: [sluice, design, psi, phase-4]
---

# PSI-based admission (Phase 4)

Status: design for Phase 4. Related: [[sluice/design/scheduler.md]],
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
  with `psi=0`. If the files are missing or unreadable, admission falls
  back to budget-only and `status` reports `psi.enabled = false` with the
  reason in the daemon log, once.

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
  `admission.psi_max_wait` (default 5 min) while blocked only by PSI, one
  job is admitted anyway (pressure caused by other users' work must not
  stall Sluice forever). Logged at info.
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
memory_some_avg10, blocking }`; text mode prints
`pressure: cpu 12.3% mem 0.4% (ok)` or `(blocking admission)`.
Metrics export (Phase 4) adds `psi_blocked_ms_total`.

## Acceptance (Phase 4)

- Unit: parser for both files, including missing `full` lines and
  malformed input; the hysteresis and starvation-guard state machine with
  synthetic samples.
- Integration: with thresholds set to 0.0 (always blocking) a queued job
  waits and `status` says `psi`; with the thresholds raised it starts.
  Synthetic CPU pressure (`stress`-like busy loops in the test, one per
  CPU) must raise `cpu.some.avg10` above a low threshold within the
  10-second window on CI and the queued job must wait, then resume when
  the load stops. Numbers recorded in
  [[sluice/changes/phase-4-acceptance.md]].
