---
title: Problem statement
tags: [sluice, research]
---

# Problem statement

## Situation

The user runs several Claude Code agents in parallel on the same project,
usually one per git worktree. The project has expensive verifications (test
suites, linters, type checks, CodeQL) wired into `pre-commit` and `pre-push`
hooks. Every agent triggers them independently. The result is CPU, RAM and
disk saturation: every agent slows down and the machine becomes unresponsive.

Most of the load comes from the local toolchain the agents drive (test
runners, bundlers, linters), not from the model.

## Three distinct causes

### 1. Uncontrolled concurrency

N agents produce N simultaneous copies of the test suite. Git hooks live in
the shared `.git/hooks` (or `core.hooksPath`), so every worktree runs the
full hook set with nothing coordinating between them. There is no global
notion of "how much verification work is this machine already doing".

### 2. Redundancy

The same tree is verified many times:

- by the agent itself before committing,
- by `pre-commit`,
- by `pre-push` (often re-running everything `pre-commit` already ran),
- after a no-op rebase that changes no content,
- by a coordinator's merge push.

Real-world reports measured more than half of suite runs being on trees that
were already proven green, and pushes taking 13–16 minutes under load mostly
because `pre-push` redid `pre-commit`'s work.

### 3. Excessive scope

Full suites where a changed-files subset would do. This is the project's
responsibility (what its gate commands run), not this tool's. Sluice must
make narrow gates easy: named gates, per-gate weights, per-gate cache keys.

## Hard constraint: agent shell timeouts

Claude Code's Bash tool has a timeout (default about 2 minutes; configurable
with `BASH_DEFAULT_TIMEOUT_MS` / `BASH_MAX_TIMEOUT_MS` in the `env` section of
`~/.claude/settings.json`). There are reports of these limits being ignored
and processes killed with exit code 144 / SIGURG. A git hook that blocks in a
queue runs inside the agent's `git commit` Bash call and can be killed
mid-wait, leaving the agent confused and the work undone.

Block's agent-task-queue solved this by moving the blocking wait into an MCP
tool call, which has no shell timeout. See
[[sluice/research/references/agent-task-queue.md]].

## What a solution must provide

- Verify each tree once (cache keyed by exact tree content + gate definition).
- Bounded, global, load-aware concurrency across all repos and worktrees of a user.
- Coalescing: identical in-flight requests share one run.
- Two front-ends (CLI for hooks, MCP for agents) over one brain.
- Hooks that never hang inside an agent: fail fast with an actionable message.
- Token-friendly output: one line on success, log tail on failure.
- Stack-agnostic: only named shell commands, any hook manager.

## What it must not become

- Not CI and not a substitute for it. A cache hit is a local optimisation;
  CI stays authoritative. Caching is off when `CI` is set unless overridden.
- Not a build system or artifact cache.
- No merge queue in v1.
- No Windows in v1.

## Non-functional requirements

- Single binary, Rust, only `git` required at runtime.
- Cache-hit path < 30 ms end to end, without needing the daemon.
- Robust to agent crashes: orphans killed, stale locks cleared.
- Safe by default: never modifies the user's index or working tree.
- Observable: `sluice status [--json]`.

See [[sluice/design/architecture.md]] for the design and
[[sluice/plan/master-plan.md]] for phases.
