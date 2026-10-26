---
title: "Queue trust W1: neutral daemon environment, clean key inputs, observability"
tags: [sluice, changes, merge-queue, cache-key, environment]
---

# Queue trust W1: neutral daemon environment, clean key inputs, observability

Decision: [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]] sections 1, 2
and part of 7. Plan: [[sluice/plan/queue-trust-plan.md]] wave W1.

## What changed

- **Neutral daemon environment.** New `sluice_gate_core::env::daemon_env(env,
  &AgentDetectionConfig) -> EnvMap` removes `GIT_*`, `CLAUDE*`, `SLUICE_JOB`,
  `SLUICE_AGENT`, `SLUICE_CACHE`, `SLUICE_DISABLE`, `SLUICE_SESSION_LEAF`,
  `SLUICE_SHIM_DIR`, `SLUICE_SHIMS`, `SLUICE_GUARD` (review correction), every variable of `[agent_detection]` (`env_any_set` and the
  keys of `env_any_equals`) and the shim directory (`SLUICE_SHIM_DIR`, else the
  default one; matched lexically and by canonical path) from `PATH`, nothing else.
  Applied when a client spawns the daemon (`proto::client::spawn_daemon`, which
  loads the user config for the detection variables and falls back to the defaults
  when it is unreadable) and in the daemon's `sanitised_env`. Every start path goes
  through `ensure_daemon` (`sluice daemon restart`, `sluice run`, `exec`, `queue`,
  MCP), so there is no other spawn site.
- **Key inputs run in exactly the given env.** `key::collect_inputs` calls
  `env_clear()` before `envs(env)`. Callers (`lookup::derive_key_at` through
  `Resolved::child_env`, `runner::derive_key` through the daemon's sanitised env
  plus `gate.env`) already pass a full environment with `PATH`. Key format v1
  unchanged.
- **`client.log` carries `key=<12 hex>`** (`-` when none) after `tree=`; for a
  queue-only gate it is the namespaced coalescing key. `cwd` stays last.
- **Daemon `info` line for every `run` request that carries a hook**:
  `hook run request: gate=.. hook=.. tree=.. key=.. caller=.. decision=queued|coalesced`
  from `handle_run`, and `decision=hit by the double-check` from `start_gate`.
  No behaviour change.
- **`sluice run --explain`** prints `cache mode: enabled | disabled (<reason>) |
  bypass (...)` and, when the mode, a queue-only gate or `cache = false` disables
  caching, `lookup: OFF (<reason>)` instead of HIT/MISS.

## Files and symbols

- `crates/sluice-gate-core/src/env.rs` (new), `lib.rs` (module list)
- `crates/sluice-gate-core/src/key.rs`: `collect_inputs`, test
  `inputs_see_only_the_given_environment`
- `crates/sluice-gate-proto/src/client.rs`: `spawn_daemon`
- `crates/sluice-gate-daemon/src/server.rs`: `sanitised_env`, `hook_run_tag`,
  `log_hook_run`, `handle_run`, `start_gate`
- `crates/sluice-gate/src/clientlog.rs`: `Entry::key`, `format_line`
- `crates/sluice-gate/src/run.rs`: `Trace::key`, `run`, `run_traced`,
  `run_queued`, `explain`; `session_start.rs` (new field)
- `crates/sluice-gate/tests/debugging.rs`: key assertions in
  `client_log_records_miss_hit_fail_fast_and_disabled`
- Docs: `kb/sluice/design/cache-key.md`, `docs/debugging.md`, `docs/cli.md`,
  `docs/agents.md`, `docs/configuration.md`, `docs/how-it-works.md`

## Why

A daemon inherited whatever the client that autostarted it carried, and key inputs
inherited the deriving process's env, so a train and a pre-push hook could derive
different keys for the same tree (ADR-0018 context, faults 1 and 2). `key=` and the
hook log line make such a mismatch visible from the logs; `--explain` no longer
claims a MISS when the cache was never consulted.

## Verification

`cargo check --workspace --all-targets` clean; `cargo fmt` applied. Unit tests in
`env.rs` (fixed list, detection variables, shim removal, default shim dir, symlinked
shim dir, no PATH), `key.rs` and `clientlog.rs`. The full suite, clippy and fmt
check run once after the last wave through Sluice `verify`.

## Review correction

`SLUICE_SHIMS` (whether the client's shims act) and `SLUICE_GUARD` (the client's command-guard mode)
describe the invoking client's own session and leaked into the daemon like the others; both are now
stripped (`STRIPPED` in `env.rs`, unit test extended). Decisions over every `SLUICE_*` variable found in
the code: stripped, as per-invocation session state: `SLUICE_JOB`, `SLUICE_AGENT`, `SLUICE_CACHE`,
`SLUICE_DISABLE`, `SLUICE_SESSION_LEAF`, `SLUICE_SHIM_DIR`, `SLUICE_SHIMS`, `SLUICE_GUARD`. Kept, as
configuration: `SLUICE_CONFIG`, `SLUICE_LOG`, `SLUICE_SOCKET` (where the daemon binds and its clients
connect), `SLUICE_UPDATE_BASE_URL`, `SLUICE_INSTALL_DIR` (installer). Test-only `SLUICE_TEST_*` variables are
untouched.
