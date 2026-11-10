---
title: "Change: the daemon runs in an allowlisted environment; gates can remove variables"
tags: [sluice, change, daemon, environment, security, config]
---

# Change: the daemon runs in an allowlisted environment; gates can remove variables

Decision: [[sluice/design/adr/0030-the-daemon-runs-in-an-allowlisted-environment.md]].

## Why

A field report (0.11.0, a second machine): the daemon autostarted by an agent session of a
Claude Code wrapper kept that session's id, API URL and bearer token in its environment for
days. Gate jobs of other callers did not receive them (they run in the requesting client's
environment), but every job descended from the daemon, so an ancestry-based agent guard
classified a human's e2e run as the agent's and five browser specs failed; train gates did
receive the token. The denylist only knew `CLAUDE*`.

## What changed

- `crates/sluice-gate-core/src/env.rs`: `daemon_env(env, detection, pass)` is an allowlist
  (`KEPT`, `KEPT_PREFIXES`, `pass`) followed by the former denylist, renamed
  `strip_invocation_env`. New `REEXEC_VAR` (`SLUICE_DAEMON_REEXEC`) and
  `looks_like_credential`. `SLUICE_HOOKS_DELEGATE` and `REEXEC_VAR` added to `STRIPPED`.
  `login_tool_env` uses `strip_invocation_env`.
- `crates/sluice-gate-core/src/config.rs`: `Gate::env_remove` (validated: a variable name,
  not also in `env`), `Gate::apply_env`; `DaemonConfig::pass_env` (validated); sample
  `.sluice.toml` mentions `env_remove`.
- `crates/sluice-gate-core/src/lookup.rs`, `crates/sluice-gate-queue/src/runner.rs`
  (`derive_key` closure and the train gate request's `child_env`): use `Gate::apply_env`.
- `crates/sluice-gate-proto/src/client.rs` (`spawn_daemon`) and
  `crates/sluice-gate-daemon/src/server.rs` (`sanitised_env`): pass `[daemon] pass_env`.
- `crates/sluice-gate-queue/src/toolenv.rs`: the injected test login shell output goes
  through `strip_invocation_env`, like `login_tool_env`.
- `crates/sluice-gate/src/daemon_cmd.rs`: `clean_environment` re-executes `sluice daemon`
  (same pid and argv) with the allowlisted environment unless it already has it;
  `report_environment` logs the dropped credential-looking names after tracing starts.
- `crates/sluice-gate/src/hookentry/gates.rs`: a hook unit does not share a gate's cache
  entry when the gate has `env_remove`; `hookentry/runner.rs` builds units with none.
- Docs: `docs/configuration.md`, `docs/compatibility.md`, `CHANGELOG.md`,
  [[sluice/design/config-format.md]].

## Verification

- Reproduced before the change with an isolated daemon (own `XDG_*` directories): a job
  of a second caller printed no `FOO_SESSION_SECRET`, the daemon's `/proc/<pid>/environ`
  had it.
- Unit tests in `env.rs`: allowlist, `pass_env` cannot re-admit detection variables,
  idempotence, credential names. Config tests for `pass_env`; compat fixture
  `crates/sluice-gate-core/tests/fixtures/unreleased/gate-env-remove.toml`.
- Integration tests in `crates/sluice-gate/tests/daemon_cli.rs`:
  `an_autostarted_daemon_keeps_only_the_allowlisted_environment`,
  `a_daemon_started_by_hand_restarts_in_a_clean_environment` (same pid, marker present,
  token name logged, value never), `env_remove_unsets_a_caller_variable`.
- `fmt`, `clippy` and `test` gates through Sluice `verify`.
- Operational: the affected machine's daemon was restarted from a minimal environment
  (no running or queued jobs at the time).
