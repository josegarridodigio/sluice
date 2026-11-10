---
title: "ADR-0030: The daemon runs in an allowlisted environment"
tags: [sluice, adr, daemon, environment, security, agents]
---

# ADR-0030: The daemon runs in an allowlisted environment

Status: accepted (2026-11-10). Supersedes the denylist of
[[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]] section 1 for the daemon's own
environment. Related: [[sluice/design/adr/0020-tool-environment-of-trains.md]],
[[sluice/design/adr/0004-agent-detection.md]]. Change:
[[sluice/changes/daemon-allowlisted-environment.md]].

## Context

A field report from a second machine running 0.11.0: the first client to need a daemon
was a coding agent inside a wrapper around Claude Code. The autostarted daemon kept that
session's environment for its lifetime, including the session id, the API URL and a
bearer token minted for that one chat (`CLAUDIO_CHAT_ID`, `CLAUDIO_API_URL`,
`CLAUDIO_API_TOKEN`), plus an `AI_AGENT` marker. `env::daemon_env` was a denylist
(`GIT_*`, `CLAUDE*`, per-invocation `SLUICE_*`, the `[agent_detection]` names), so any
marker under another name passed.

What actually reached whom, measured with an isolated daemon:

- **Gate jobs of `sluice run`, `verify` and hooks** run in the *requesting client's*
  environment (`child_env` + the gate's `env`, `env_clear()` in the executor). A second
  caller's job never saw the first caller's variables.
- **Every gate job descends from the daemon process.** `/proc/<daemon>/environ` held the
  marker, so tools that classify a process by its ancestry (the wrapper's terminal guard
  refuses a terminal to any process with an ancestor carrying the session id) treated jobs
  a human started as the agent's. The browser e2e suite failed five specs locally and
  passed in CI.
- **Train gates, `prepare` and `key_inputs`** run in the daemon's sanitised environment
  (with the login shell's on top, ADR-0020): there the token *was* in the job environment.

## Decision

1. **Allowlist.** `env::daemon_env(env, detection, pass)` keeps only `PATH`, `HOME`, `USER`,
   `LOGNAME`, `SHELL`, `LANG`, `LANGUAGE`, `TZ`, `TMPDIR`, `DBUS_SESSION_BUS_ADDRESS`
   (systemd scopes go through the session bus), `CI` (a property of the machine), every
   `LC_*`, `XDG_*` and `SLUICE_*` variable, and the names of the new user key
   `[daemon] pass_env`. The old denylist (now `env::strip_invocation_env`) still runs after
   it and wins: agent-detection and per-invocation variables are never kept, even when
   `pass_env` names them. `SLUICE_HOOKS_DELEGATE` joins the per-invocation list.
2. **Applied everywhere the daemon's environment comes from:** an autostarting client
   (`proto::client::spawn_daemon`), the daemon's `sanitised_env` (train tool environment,
   key inputs, git), and `sluice daemon` itself: before anything else, while
   single-threaded, it compares its environment with `daemon_env` of it and, when they
   differ, re-executes itself (same pid, same argv) with only the allowlisted variables
   plus `SLUICE_DAEMON_REEXEC`. That covers a daemon started by hand from an agent's shell,
   by a service unit or by an older client. The marker stops a second re-execution and is
   itself stripped from what jobs get.
3. **Warning by name only.** The re-executed daemon logs the names (never the values) of
   the dropped variables that look like credentials (a `_`-separated word `TOKEN`,
   `SECRET`, `PASSWORD`, `PASSWD`, `KEY`, `APIKEY` or `CREDENTIALS`). When re-execution
   fails it warns that the process keeps them.
4. **The login shell keeps the denylist.** What a login shell prints describes the user
   (their rc files), not a session; trains still get `JAVA_HOME`, `CARGO_HOME`, `NVM_DIR`
   from it. With `[queue] tool_env = "daemon"`, a variable a train needs must be named in
   `[daemon] pass_env`.
5. **`env_remove` per gate.** `[gates.<name>] env_remove = [...]` removes variables from the
   gate's environment before `env` is applied (`Gate::apply_env`), in every path: client
   `child_env`, hook units, and the train runner (which removes from the tool environment
   before handing the request to the daemon). `env` cannot unset (an empty value is still
   set, and a `NAME=` prefix check still matches). A name in both is a configuration
   error. Not part of the cache key, like the rest of the caller's environment; `key_env`
   reads the result.

## Consequences

- No job descends from a process that carries another session's id or credentials; train
  gates no longer receive them.
- What remains caller-dependent is deliberate: a job requested by an agent runs in that
  agent's environment. A gate whose result depends on such a marker (the e2e case above
  when an agent calls `verify`) names it in `env_remove`, or in `key_env` when both
  results are legitimate.
- The user's own API keys exported by a profile are dropped from the daemon too; trains
  get them back only through the login shell or `pass_env`.
- `.sluice.toml` with `env_remove` and a user config with `pass_env` are rejected by 0.11.0
  and older (one-way, like earlier keys).

## Rejected

- **Extending the denylist with the wrapper's names**: the next wrapper picks other names.
- **Requiring a systemd user unit**: the autostart is the zero-config path (ADR-0014);
  a unit stays possible and is now re-executed clean anyway.
- **Building the daemon's environment from the login shell**: an rc file that waits or
  fails would delay or break every autostart; ADR-0020 keeps the login shell for trains.
