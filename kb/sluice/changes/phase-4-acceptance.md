---
title: "Phase 4 acceptance evidence"
tags: [sluice, acceptance, phase-4]
---

# Phase 4 acceptance evidence (2026-10-22)

| Criterion | Evidence |
|---|---|
| Under CPU pressure new jobs wait and resume when pressure drops | `tests/daemon.rs`: with the PSI root pointed at fake files (`cpu`: `some avg10=99.00`), a queued job reports `reason: psi` and `budget.psi.blocking = true`; rewriting the file to `avg10=0.00` (two samples at a 50 ms poll) starts it. Starvation guard: with `psi_max_wait = 300 ms` the head starts while blocking and the daemon log names the job. Missing root: `psi.enabled == false`, jobs start normally. |
| Thresholds documented with measurements | Defaults `cpu_some_avg10_max = 60.0`, `memory_some_avg10_max = 20.0`, hysteresis at 80 % for two samples, `psi_max_wait = 5m` ([[sluice/design/psi-admission.md]]). CPU measured with `scripts/psi-pressure-demo.sh 20 15` and `scripts/psi-pressure-demo.sh 40 15` on the development machine (12th Gen Intel i7-1280P, 20 logical CPUs, Linux 7.0, PSI available), load already around 10 from other work: 20 busy loops (= `nproc`) raised `cpu some avg10` to about 4.8 % after 15 s; 40 loops raised it to 17.3 % at 4 s, 35.2 % at 9 s and 47.1 % at 15 s, still rising when the run ended. The 60 % default was not reached in 15 s; `some` pressure grows with the run-queue length beyond the CPU count, so (extrapolating) it corresponds to a sustained oversubscription of about 2× on this machine, sooner on fewer cores. Memory pressure was not measured (a safe synthetic memory stall is not practical on a working machine); the 20 % default follows the design. |
| The JSON schema has a test that fails on accidental changes | `crates/sluice-gate-proto/tests/status_schema.rs` against `fixtures/status-schema-1.json`; `STATUS_SCHEMA_VERSION == 1`. |

Command and result: `cargo test --workspace` at the Phase 3/4 close:
229 tests passed, 0 failed (core, proto incl. the schema fixture, daemon
unit + integration incl. the three PSI tests, CLI incl. `tests/mcp.rs`);
the two-sample hysteresis is asserted by the `psi::tests::hysteresis` unit
test, the integration test only waits for the job to start.
