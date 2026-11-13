---
title: "Change: the documentation matches 0.12.1; Sluice's own gates use key_env"
tags: [sluice, changes, docs, key-env, containment]
---

# Change: the documentation matches 0.12.1

Why: the 0.12.1 launch post links to the repository, and a review of README and docs
against ADR-0026 to ADR-0030 found statements that 0.9.0 to 0.12.1 had made stale.

## What changed

- `README.md`: the containment limitation no longer says Sluice cannot hold a process
  that moves itself out (ADR-0028 moves it back) and says what it still cannot bound;
  the validation bullet says which releases ran on which machines (no host names) and
  what is not validated (other desktops, thresholds still to tune); a "Memory
  containment of every gate job" feature; the commands that update the Claude Code
  plugin after `sluice update`.
- `docs/configuration.md`: every "Needs the release after X" names the release
  (0.8.0, 0.11.0, 0.12.0); the `admission.memory_aware` row describes ADR-0026
  (family estimate, only a guess probes, `1.25 x` once) and the desktop guard.
- `docs/compatibility.md`: "the release after X" and "the next release" name the
  release; the `containment_schema` history up to 5.
- `docs/how-it-works.md`: the daemon's allowlisted environment (ADR-0030) replaces the
  denylist description; a "Containment of what a job starts" item (swap ceiling, moved
  back processes, desktop guard, telemetry); the idle exemption under the guard.
- `docs/debugging.md`: the swap ceiling under "The kernel ceiling killed my job"; new
  sections "A process of my job left its cgroup" (with the 0.12.1 fix and the
  `reclaim_escaped = false` workaround) and "Jobs wait while the desktop is under
  memory pressure".
- `crates/sluice-gate-core/src/config.rs`: a doc comment said "the release after 0.7.0".
- `.sluice.toml`: `key_env = ["RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_BUILD_TARGET"]` on
  `fmt`, `clippy` and `test` (ADR-0029). Every gate key changes once.
- `CHANGELOG.md`: Unreleased entries.

## How it was verified

- Each version named was checked against `CHANGELOG.md` sections and the ADRs.
- Log and status strings quoted in `debugging.md` were copied from
  `crates/sluice-gate-daemon/src/server.rs`, `telemetry.rs` and
  `crates/sluice-gate-proto/src/lib.rs`.
- `sluice doctor` in this repository: no `key-env` warning.
- The `fmt`, `clippy` and `test` gates through Sluice `verify`.
