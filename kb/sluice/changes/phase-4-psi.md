---
title: "Change: Phase 4, PSI-based admission and status schema"
tags: [sluice, change, phase-4]
---

# Change: Phase 4, PSI-based admission and status schema (2026-10-22)

Implements [[sluice/plan/master-plan.md]] Phase 4 from
[[sluice/design/psi-admission.md]].

## What changed

| Where | What |
|---|---|
| `sluice-gate-daemon::psi` (new) | `PsiSample`, `read_sample(root)` for the `/proc/pressure` format, `Admission` (threshold rule, 80 % / two-sample hysteresis, starvation guard `starvation_override`, `info()`, `blocked_ms_total()`); PSI switches off for the daemon's life after a failed read. 6 unit tests. |
| `scheduler.rs` | `head_queued_at()`, `admit_head(now)` (starts the head only if budget and repo cap allow). |
| `server.rs` | `DaemonConfig.admission` and `psi_root`; a sampling task every `psi_poll_interval` wakes the scheduling loop on state changes; `admit(now, psi_blocking)` plus the starvation guard (one job per `psi_max_wait`, info log with the job id); `status` fills `budget.psi` and `totals.psi_blocked_ms_total`. |
| proto | `Totals.psi_blocked_ms_total` (default 0); `tests/status_schema.rs` pins the `status` JSON (`fixtures/status-schema-1.json`, `STATUS_SCHEMA_VERSION == 1`). |
| core `config.rs` | `admission.psi_max_wait` (default 5 min, must be > 0). |
| CLI | `daemon_cmd.rs` passes the admission config and `/proc/pressure`; `sluice status` prints `pressure: cpu X% mem Y% (ok|blocking admission|unavailable)`. |
| `scripts/psi-pressure-demo.sh` | starts N busy loops and prints `/proc/pressure/cpu` every second; exits 2 without PSI. |
| tests | `tests/daemon.rs`: blocks then resumes with fake PSI files, starvation guard with a log line, missing root → disabled. The shared test config keeps PSI off so host load cannot stall older tests. |

## Verification

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace` green (agent run, 2026-10-22). Acceptance evidence
and the measured pressure numbers: [[sluice/changes/phase-4-acceptance.md]].
