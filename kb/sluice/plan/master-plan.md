---
title: Sluice master plan
tags: [sluice, plan]
---

# Sluice master plan

Sluice is a stack-agnostic verification queue and cache for parallel coding
agents. One per-user daemon runs named gates (shell commands defined per
repository) with a bounded global budget, caches results per git tree, and
coalesces duplicate requests. A CLI (`sluice run`, for git hooks and humans)
and an MCP server (`sluice mcp`, for agents) are thin clients of that daemon.

Related: [[sluice/research/problem-statement.md]],
[[sluice/design/architecture.md]], [[sluice/questions/open-questions.md]].

## Status overview

| Phase | Name | Status | Notes |
|---|---|---|---|
| 0 | Setup and research | **done** (2026-10-22) | Questions answered, decisions recorded in ADR-0001..0005, workspace scaffolded and green. See [[sluice/changes/phase-0-scaffold.md]]. |
| 1 | Cache only, no daemon | **next** | |
| 2 | Daemon, scheduler, coalescing | not started | |
| 3 | MCP front-end and agent policy | not started | |
| 4 | Load-aware admission and observability | not started | |
| 5 | Packaging and real-world validation | not started | |
| later | Local merge queue (mergetrain-inspired) | not planned | Ask before starting. |

## Phase 0 — Setup and research

Goal: no product code; decisions recorded, workspace scaffolded, plan approved.

Tasks:

- [x] Discover Pando tooling. Finding: Pando is configured per project
  directory (`.pando.toml`, `KBPath = './kb'`, `.pando/data`). Pando was
  initialized in the project directory and registered as an MCP server for
  it (`claude mcp add -s local pando -- pando mcp-server --no-http`), then
  reconnected with `/mcp`. The KB watcher auto-imports every `.md` under
  `kb/` with its relative path. The indexing tool is `code_index_project`
  (absolute path, any location), so reference repos stay outside the project.
  Pitfall fixed: a `WatchExclude` line placed inside the `[Providers]`
  section made the server exit at start-up; it must sit at the top level.
- [x] Clone reference repositories into `refs/` inside the project (gitignored, user decision 2026-10-22)
  (agent-task-queue, gate-cache, rust-sdk, mergetrain).
- [x] Write research notes: [[sluice/research/references/agent-task-queue.md]],
  [[sluice/research/references/gate-cache.md]],
  [[sluice/research/references/rmcp.md]],
  [[sluice/research/references/mergetrain.md]].
- [x] Write [[sluice/research/problem-statement.md]].
- [x] Draft [[sluice/design/architecture.md]], [[sluice/design/config-format.md]],
  [[sluice/design/cache-key.md]].
- [x] Draft first ADRs: [[sluice/design/adr/0001-rust-edition-and-workspace.md]],
  [[sluice/design/adr/0002-shell-out-to-git.md]],
  [[sluice/design/adr/0003-crate-naming.md]],
  [[sluice/design/adr/0004-agent-detection.md]],
  [[sluice/design/adr/0005-mcp-verify-blocking-or-handle.md]].
- [x] Collect Claude Code facts (MCP timeouts, Bash timeouts, env markers,
  permissions): [[sluice/research/claude-code-facts.md]].
- [x] Verify crate name availability on crates.io (`sluicegate*` free).
- [x] Ask Phase-0 questions and record answers in [[sluice/questions/open-questions.md]] (Q1–Q14 answered 2026-10-22).
- [x] Install the Rust toolchain (rustup stable, rustc 1.99.1).
- [x] Index the reference repositories in Pando: project ids
  `ref-agent-task-queue`, `ref-gate-cache`, `ref-rmcp`, `ref-mergetrain`
  (stored with `remember` under `refs.<name>.project_id`). Sluice itself is
  `home_jose_Escritorio_Proyectos_sluice`.
- [x] Scaffold the Cargo workspace, CI (fmt, clippy, test), license, README skeleton, `.gitignore` ([[sluice/changes/phase-0-scaffold.md]]).
- [x] `git init` and first commit.
- [x] GitHub remote: https://github.com/josegarridodigio/sluice (origin), first push 2026-10-22.

Exit criteria met: plan, config format and crate names approved on 2026-10-22.

## Phase 1 — Cache only, no daemon

Deliverables: `sluice run <gate>`, `.sluice.toml` loading, tree computation,
cache key, markers with TTL, `sluice cache status|clear`, `sluice init`,
`sluice hook print <manager>`, a `flock`-based counting semaphore as a first
global concurrency guard, latency measurement of the hit path.

Acceptance:

- A gate that passed is skipped on the same tree from another worktree.
- Any change to a tracked or untracked non-ignored file invalidates the marker.
- The real index is never modified (tested by comparing index bytes before/after).
- Hit latency measured and documented (target < 30 ms end to end).
- Works from inside `pre-commit` and `pre-push` hooks with git's hook env vars set.

Design inputs: [[sluice/design/cache-key.md]], [[sluice/design/config-format.md]],
[[sluice/research/references/gate-cache.md]].

## Phase 2 — Daemon, scheduler, coalescing

Deliverables: versioned IPC protocol over a Unix socket
([[sluice/design/protocol.md]]), daemon autostart with a start lock, budget
scheduler with weights ([[sluice/design/scheduler.md]]), in-flight coalescing,
executor with process groups, timeouts, nice/ionice, per-job logs, SQLite job
history and metrics, `sluice status`, `sluice logs`.

Acceptance:

- 6 concurrent `sluice run test` on the same tree run the command once; all 6 get the result.
- With different trees, never more than the budget runs at once.
- Killing a client or the daemon leaves no orphan processes and no stale locks.

## Phase 3 — MCP front-end and agent policy

Deliverables: `sluice mcp` built with rmcp ([[sluice/design/mcp-tools.md]]),
tools `verify`, `status`, `gate_list`, `logs`; agent detection and
`agent_hook_policy`; `docs/agents.md` with a ready-to-paste CLAUDE.md section.

Acceptance: documented manual end-to-end test with two Claude Code agents in
two worktrees: one runs, the other coalesces; later `git commit` hooks hit the
cache; on a miss the hook fails fast with the actionable message.

## Phase 4 — Load-aware admission and observability

Deliverables: PSI-based admission, stable `sluice status --json` schema,
metrics export (time saved, hit rate, wait times) for the user's existing HTML
dashboard.

Acceptance: under synthetic CPU pressure new jobs wait; thresholds documented
with measurements.

## Phase 5 — Packaging and real-world validation

Deliverables: release builds (Linux x86_64/arm64), install script,
`cargo install sluice-gate`, man page / README. Validation on the user's real
project(s) with before/after numbers.

## Working agreements

- Everything in English (code, docs, commits, KB).
- Every increment: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` green.
- Full test suite runs once at the end of each task, not after every edit.
- Every change gets a KB document under `sluice/changes/` or `sluice/fixes/`;
  decisions get an ADR under `sluice/design/adr/`.
- No pushes or publishing without the user's approval.
- Commit messages carry no AI attribution trailers.

## Next steps

1. Phase 1, in `sluice-gate-core`: config parsing (`.sluice.toml`, user
   config), git wrapper module, tree hash with the private index/object dir,
   cache key, markers and miss records, agent detection.
2. Phase 1, in `sluice-gate`: `sluice run` (hit path, flock semaphore, run,
   marker write), `cache status|clear`, `init`, `hook print`.
3. Measure hit-path latency and record it in [[sluice/changes/hit-path-latency.md]].
