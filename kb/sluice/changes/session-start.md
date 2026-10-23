---
title: "Change: session start hook of the agent plugin (zero-config task T5)"
tags: [sluice, change, plugin, session-start, shims, zero-config]
---

# Change: session start and plugin (2026-10-23)

Task T5 of [[sluice/plan/zero-config-plan.md]]. Decision:
[[sluice/design/adr/0014-zero-config-agent-integration.md]]. Design:
[[sluice/design/agent-plugin.md]].

## What changed

- New hidden subcommand `sluice agent session-start --plugin-version <X.Y.Z>`
  (`crates/sluice-gate/src/session_start.rs`; `main.rs` only wires `Command::Agent` and
  `AgentCommand::SessionStart`). Symbols: `respond`, `run_hook`, `locate`, `context`,
  `env_line`, `ensure_env_line`, `Place`.
- `crates/sluice-gate/src/shim.rs`: new `install_into` (+ `Installed`), `default_dir`,
  `quote`, `current_binary`; `install` now calls `install_into` (output and exit codes
  unchanged).
- `plugin/hooks/session-start.sh` (new) and a `SessionStart` entry in `plugin/hooks/hooks.json`
  (matcher `startup|resume|clear|compact`, timeout 10); the `PreToolUse` entry is untouched.
- Skill (`SKILL.md`, `reference.md`): automatic shims, the rules, the init proposal flow, the
  missing or older binary flow. Plugin and marketplace descriptions mention the session hook.
  Versions are not bumped (the release script does it).
- `plugin/hooks/test-session-start.sh` (new), `crates/sluice-gate/tests/session_start.rs` (new).

## Why

Installing the plugin and the binary must be enough: shims were opt-in and the rules lived in
the user's own CLAUDE.md. The spike measured that `CLAUDE_ENV_FILE` reaches every Bash call,
subshell, script, background command and subagent, also after resume.

## How it was verified

`verify` through Sluice with gates fmt, clippy and test (including the new Rust tests and
`test-session-start.sh`). Warm run of the hook: see the test `a_warm_run_takes_under_300_ms`.
