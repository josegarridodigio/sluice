---
title: "Merge trains run their tools in the user's login environment"
tags: [sluice, changes, merge-queue, environment]
---

# Merge trains run their tools in the user's login environment

Decision: [[sluice/design/adr/0020-tool-environment-of-trains.md]]. Plan:
[[sluice/plan/queue-env-plan.md]] (0.6.1, branch `fix/train-tool-env`).

## Why

A daemon restarted from a non-interactive SSH shell had a `PATH` without the user's Node
toolchain; every train failed with `key_input_failed: node --version: exit status 127`
while agents' gates passed (a client's run carries the client's environment, a train runs
in the daemon's).

## What changed

- `sluice_gate_core::env`: `login_env(base, shell, deadline)` runs
  `<shell> -l -i -c "printf '%s' <MARKER>; env -0"` with `env_clear()` plus `HOME`, `USER`,
  `LOGNAME`, `LANG` (from the base), `SHELL` (the shell used) and `TERM=dumb`; stdin null,
  stderr discarded, own process group, SIGTERM then SIGKILL of the group at the deadline
  (`git::signal_group`, `DEADLINE_*` reused, now `pub(crate)`). `parse_login_env` takes the
  bytes after the last `LOGIN_ENV_MARKER`, NUL-split `KEY=VALUE`, ignoring malformed
  entries. `login_shell` (`$SHELL`, else field 7 of `/etc/passwd` for the uid through
  `rustix`, else `/bin/sh`), `login_tool_env` (shell + `login_env` + `daemon_env`),
  `LoginEnvError` (spawn, timeout, failed, no marker, empty), `LOGIN_ENV_DEADLINE` (10 s).
  Stdout is collected progressively so a daemon an rc file leaves holding the pipe cannot
  lose the output.
- `config`: `UserQueueConfig.tool_env: ToolEnv { LoginShell (default), Daemon }` and
  `tool_path: Option<String>` (rejected when blank). Fixture
  `tests/fixtures/unreleased/queue-tool-env.toml`.
- `sluice-gate-queue`: new `toolenv` module (`resolve`, `ToolEnvSource { LoginShell,
  Daemon { reason }, ToolPath }`, `view`). `Core` gets `tool_env` and `tool_env_source`,
  resolved once in `Core::with_push_program` (called from `on_start` inside
  `spawn_blocking`, before the runner). Used by the train gates' `RunRequest.child_env`
  (`runner.rs`), the train's `derive_key` env, and the prepare job and base probe
  (`prepare.rs`). Not changed: `Core.git` (git env), the push env, agents' and hooks' runs,
  the vouch. Rule: `tool_path` wins; `tool_env = "daemon"` uses the daemon's sanitised env;
  `login-shell` overlays the login env on it (login values win), falling back with the
  reason. **Tests**: when `QueueOptions.base_env` is given the login shell is not resolved
  (the machine's shell would decide) unless `QueueOptions.login_shell` injects a fake one.
- Observability: daemon log (`info` with shell/vars/PATH entries, `warn` with the reason on
  fallback); `QueueSummary.tool_env: Option<QueueToolEnv { source, reason, path }>` in the
  status JSON (only the `PATH`, never the whole environment); `sluice status` line
  `tool env: login shell | daemon (<reason>) | tool_path`; `sluice doctor` check
  `train-tools` (first word of each cached gate's `key_inputs` of the current repository,
  looked up in the daemon's reported train `PATH`; silent without a daemon).
- `QueueSummary` is no longer `Copy` (it owns the optional view).
- Docs: `docs/merge-queue.md`, `docs/how-it-works.md`, `docs/configuration.md`,
  `docs/compatibility.md`, `kb/sluice/design/merge-queue.md` section 5,
  `plugin/skills/sluice/reference.md` (exit 127 note), `CHANGELOG.md`.

## How it was verified

New tests: `env.rs` (parser on noise, repeated marker, malformed entries, no marker;
passwd lookup; fake `sh` login shells: noise then works, timeout kills the group, exit 1,
no marker, empty result, spawn failure, `daemon_env` applied), `toolenv.rs` (overlay,
fallback with reason, daemon/tool_path start no shell, test base env), config tests and the
compat fixture, the status JSON round-trip, `status.rs` text, `doctor.rs` lookup, and four
queue integration tests (`trains_find_the_tools_of_the_login_shell`,
`tool_env_daemon_does_not_use_the_login_shell`, `tool_path_gives_the_trains_their_tools`,
`a_failing_login_shell_falls_back_to_the_daemon_env`). `cargo check --workspace
--all-targets` is clean; the full suite, clippy and fmt run through Sluice at the end of
the task.
