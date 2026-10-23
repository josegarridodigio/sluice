---
title: "ADR-0014: Zero-config agent integration through a session hook"
tags: [sluice, adr, zero-config, plugin, shims]
---

# ADR-0014: Zero-config agent integration through a session hook

Status: accepted (2026-10-23). Plan: [[sluice/plan/zero-config-plan.md]]
tasks S0, T5. Evidence: [[sluice/research/plugin-path-spike.md]]. Builds on
[[sluice/design/adr/0010-command-control.md]].

## Context

The 0.3.0 agent plugin registers the MCP server, a skill and a PreToolUse
guard. The guard rewrites only simple commands; heavy commands inside
subshells, `$(...)`, `sh -c` or scripts escape it. Shell shims catch those,
but users had to install them and edit `PATH`, and the rules that kept agents
on `verify` lived in the user's private CLAUDE.md. On a trial on a real
Angular project the machine saturated while Sluice ran a single lint gate.
The user's requirement: installing the plugin (and the binary) is enough.

## Decision

- The plugin adds a `SessionStart` hook (matchers `startup`, `resume`,
  `clear`, `compact`). A thin POSIX wrapper finds `sluice`, compares its
  version with the plugin's and hands over to `sluice agent session-start`.
- Shims are always on in agent sessions, in every repository (with or
  without `.sluice.toml`). `session-start` installs or refreshes them in the
  stable directory `$XDG_DATA_HOME/sluice/shims` and prepends that directory
  through `CLAUDE_ENV_FILE` with a guarded line, so the entry appears once even
  when the hook fires again on resume. Measured on Claude Code 2.1.309: the
  env file reaches the main session, background commands, subshells,
  `$(...)`, `sh -c`, scripts and subagents, also after `--resume`. The
  plugin's `bin/` directory is not used: it is appended last to `PATH` and
  cannot shadow `npm`. No guard fallback for subagents is needed.
- `CLAUDECODE` already marks the session as an agent, so shims queue without
  `SLUICE_SHIMS=1`; `SLUICE_SHIMS=0` remains the escape hatch.
- The rules for the agent travel as `additionalContext` (about 1 500
  characters at most): verify before commits, never retry a queued command
  without Sluice, no subshells or scripts around builds and tests, no
  parallel dev servers during gates, never approve trains. In a git
  repository without `.sluice.toml` the context asks the agent to propose
  Sluice once, show `sluice init --dry-run` and run `init` only after the user
  agrees.
- Missing or older binary: nothing is installed by the hook. The context
  tells the agent to explain the situation and run the install command or
  `sluice update` only with the user's consent. A hook cannot prompt.
- Fail open: any error prints nothing and exits 0, under a watchdog; a broken
  Sluice never blocks a session.

## Consequences

- Commands invoked by absolute path, under `env -i`, or from login shells
  that reset `PATH` are not caught; `npm run` scripts reach
  `node_modules/.bin` directly, so package scripts are classified at the
  `npm`/`pnpm`/`yarn` entry point (plan task T1).
- The behaviour depends on Claude Code's handling of `CLAUDE_ENV_FILE`; the
  spike is cheap to re-run and `sluice doctor` reports whether the shims are
  first in `PATH`.
- Writing the shim directory and the env file are user-level side effects of
  having the plugin enabled; they never touch a repository.
