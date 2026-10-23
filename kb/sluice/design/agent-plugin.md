---
title: "Design: the agent plugin and its session hook"
tags: [sluice, design, plugin, session-start, shims, zero-config]
---

# Design: the agent plugin and its session hook

Decision: [[sluice/design/adr/0014-zero-config-agent-integration.md]]. Evidence:
[[sluice/research/plugin-path-spike.md]]. Plan task T5:
[[sluice/plan/zero-config-plan.md]]. Shims: [[sluice/design/shell-shims.md]],
guard: [[sluice/design/command-control.md]].

## Pieces

| Piece | File | Role |
|---|---|---|
| MCP server | `plugin/.mcp.json` | `sluice mcp`: `verify`, queue tools |
| Skill | `plugin/skills/sluice/` | how to verify, the init proposal flow, the missing-binary flow |
| `SessionStart` hook | `plugin/hooks/session-start.sh` | finds `sluice`, compares versions, hands over |
| `sluice agent session-start` | `crates/sluice-gate/src/session_start.rs` | shims, `PATH` line, context, log line |
| `PreToolUse` hook | `plugin/hooks/guard.sh` | rewrites simple heavy commands (unchanged) |

## The wrapper (`session-start.sh`)

POSIX `sh`, always exits 0, matcher `startup|resume|clear|compact`, timeout 10 s.

1. Finds `sluice`: `PATH`, then `~/.local/bin/sluice`, then `~/.cargo/bin/sluice`.
2. Missing: prints `additionalContext` asking the agent to tell the user and, only
   with the user's consent, run the installer one-liner. A hook cannot prompt and
   never installs anything.
3. `sluice --version` older than the plugin version (read from
   `../.claude-plugin/plugin.json`): the same with `sluice update`.
4. Otherwise runs `sluice agent session-start --plugin-version <v>` with the hook JSON
   on stdin and prints its answer only when it starts with `{"hookSpecificOutput"`
   (a crashing or garbage-printing binary prints nothing).

## The subcommand

Reads the hook JSON (`cwd`, `source`, `session_id`) and runs under a 300 ms watchdog
(a thread that exits 0 unless the answer is being written, as the guard does):

1. `shim::install_into` in the stable shim directory (`$SLUICE_SHIM_DIR`, else
   `$XDG_DATA_HOME/sluice/shims`, else `~/.local/share/sluice/shims`): the same code
   as `sluice shim install`; rewrites only stale or missing shims, prunes old ones,
   never touches files Sluice did not write. A failure aborts silently.
2. If `CLAUDE_ENV_FILE` is set, appends
   `case ":$PATH:" in *":<dir>:"*) ;; *) export PATH='<dir>':"$PATH" ;; esac`
   (absolute, single-quoted, `'` escaped; `"`, `$`, backtick and `\` escaped inside the
   pattern) unless the file already has that exact line. `SessionStart` fires again on
   resume and the env file is reused, so the line is idempotent. `CLAUDECODE` already
   marks the agent, so no `SLUICE_SHIMS=1` is written.
3. Prints the context (below). With no env file the context is still printed.
4. Logs `caller=agent hook=session-start gate=- decision=<rules|init-offer|non-git>`
   to `logs/client.log`.

Any failure (invalid JSON, unwritable shim dir or env file, unreadable user config,
panic, timeout) prints nothing and exits 0.

## Context variants (at most 1 500 characters, tested)

- **Repository with `.sluice.toml`** (found at or above `cwd`): the rules and the gate
  names (up to 20): verify before every commit with the `verify` tool; builds and test
  runners may be queued automatically, never retry one without Sluice, do not prefix
  commands; do not wrap builds or tests in subshells, `$(...)` or scripts to capture
  output, redirect instead; no several dev servers or watchers while gates run; never
  approve merge trains and never push to the target branch.
- **Git repository without `.sluice.toml`** (a `.git` at or above `cwd`): propose Sluice
  once; run `sluice init --dry-run`, show the output to the user, run `sluice init` only
  after the user agrees; do not insist after a refusal.
- **Elsewhere**: one line saying there is nothing to set up.

## Limits

Commands run by absolute path, under `env -i` or from login shells that reset `PATH`
are not caught (ADR-0014). `sluice doctor` reports whether the shims are first in
`PATH`. The plugin does not use `bin/` (appended last to `PATH`, cannot shadow `npm`).

## Tests

`crates/sluice-gate/tests/session_start.rs` (shims, env line once across two runs, a
real `sh -c '(npx ng build)'` reaching the daemon after sourcing the env file twice,
the three variants, broken environment, foreign files, warm run under 300 ms) and
`plugin/hooks/test-session-start.sh` (missing, older, equal and newer binary, fallback
location, broken binary, real binary), run by the Rust test of the same name.
