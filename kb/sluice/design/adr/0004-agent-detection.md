---
title: "ADR-0004: Detecting that a hook runs inside a coding agent"
tags: [sluice, adr, agents]
---

# ADR-0004: Detecting that a hook runs inside a coding agent

Status: accepted (2026-10-22). Implemented in Phase 1 (`sluice-gate-core::agent`); the evidence below was shown to the user with the Phase-0 answers.

## Context

Hooks must fail fast on a cache miss when invoked from an agent's Bash tool
(shell timeouts, G7) and may wait when invoked by a human. Sluice needs a
cheap, reliable signal.

## Evidence

Environment of a Bash tool subprocess in Claude Code 2.1.308 on the user's
machine (captured 2026-10-22 from inside this session):

```
CLAUDECODE=1
CLAUDE_CODE_ENTRYPOINT=cli
CLAUDE_CODE_VERSION=2.1.308 (Claude Code)
CLAUDE_CODE_SESSION_ID=<uuid>
CLAUDE_CODE_EXECPATH=/home/jose/.local/share/claude/versions/2.1.308
CLAUDE_PID=<pid>
CLAUDE_CODE_CHILD_SESSION=1
CLAUDE_CODE_SESSION_ATTENDED=1
CLAUDE_EFFORT=high
AI_AGENT=claude-code_2-1-308_agent     # set by the Warp terminal integration, not by Claude Code itself; do not rely on it
```

`CLAUDECODE=1` is documented (env-vars.md) as set in every subprocess
Claude Code spawns: Bash, hooks, status line **and stdio MCP servers**.
`CLAUDE_CODE_CHILD_SESSION=1` is documented for Bash, hooks and the status
line but not for MCP servers. `CLAUDE_CODE_ENTRYPOINT` is observed but not
documented. Git hooks inherit the environment of the `git` process, which
inherits the Bash tool's environment, so the markers are visible inside
hooks. Details in [[sluice/research/claude-code-facts.md]].

Because `sluice mcp` (and a daemon it autostarts) also carries
`CLAUDECODE=1`, detection is evaluated only in the `sluice run` client at
hook/terminal time, never inside the daemon, and every variable whose
name starts with `CLAUDE` is removed from the environment built for gate
processes.

## Decision

A hook invocation is treated as **agent-invoked** when any of these holds,
evaluated in order:

1. `SLUICE_AGENT` is set: `1` forces agent, `0` forces human (override).
2. Any variable in `agent_detection.env_any_set` (default `["CLAUDECODE"]`)
   is set and non-empty. (`CLAUDE_CODE_CHILD_SESSION` would be more precise
   for "inside a Bash call" but is newer; it can be added to the list once
   the user confirms the minimum Claude Code version to support.)
3. Any `key = value` pair in `agent_detection.env_any_equals` matches
   (default empty; users can add other agents' markers, e.g. for Codex or
   Gemini CLI, once verified).

Detection lives in `sluice-gate-core` (`agent.rs`), is pure (takes an env
map), and is unit-tested. `sluice run --explain` prints which rule fired;
the fail-fast message names it too. `SLUICE_AGENT` accepts the truthy and
falsy words of the config format; any other value is ignored.

## Consequences

- Other agents are not detected by default; `docs/agents.md` (Phase 3)
  will explain how to add their markers or export `SLUICE_AGENT=1`.
- Users who run Claude Code with their own terminal wrapper that strips the
  environment lose detection; the `fail-fast` message still explains what to
  do.

## Alternatives considered

- Detecting a non-TTY stdin: humans also run hooks from editors and scripts
  without a TTY; too many false positives.
- Process-tree inspection (walk parents looking for `claude`): slower,
  fragile across sandboxes, and unnecessary given the env marker.
