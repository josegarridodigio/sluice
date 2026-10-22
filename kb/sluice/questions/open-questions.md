---
title: Open questions
tags: [sluice, questions]
---

# Open questions

Questions asked to the user, with answers once received. Newest at the end.

## Phase 0 (asked 2026-10-22)

| # | Question | Proposal | Answer |
|---|---|---|---|
| Q1 | Repository location and remote URL | Local: `/home/jose/Escritorio/Proyectos/sluice` (current dir). Remote: to be created by the user (GitHub?). | Answered 2026-10-22: this directory; remote on GitHub: https://github.com/josegarridodigio/sluice (given 2026-10-22). |
| Q2 | crates.io crate names | `sluicegate` (CLI, binary `sluice`), `sluicegate-core`, `sluicegate-proto`, `sluicegate-daemon`, `sluicegate-mcp`. All verified free on crates.io on 2026-10-22 via the crates.io API (HTTP 404). `sluice`, `sluice-rs`, `sluice-cli` are taken (HTTP 200). | Answered 2026-10-22: **`sluice-gate-*`** (hyphenated): `sluice-gate` (CLI, binary `sluice`), `sluice-gate-core`, `sluice-gate-proto`, `sluice-gate-daemon`, `sluice-gate-mcp`. Re-verified free on crates.io the same day. |
| Q3 | License | MIT OR Apache-2.0 dual license (Rust ecosystem convention, compatible with gate-cache MIT and agent-task-queue Apache-2.0). | Answered 2026-10-22: **MIT** only. |
| Q4 | Target platforms v1 | Linux first; keep macOS compiling (no PSI, no ionice) but untested until Phase 5. | Answered 2026-10-22: **Linux only** for v1. macOS is not a target; no effort spent keeping it compiling. |
| Q5 | Pando project id / user id | Pando is per-directory here, so the project is the directory itself (code project id derived from the path). KB documents use the `sluice/` prefix. `user_id`: `jose`. | Answered implicitly: project = this directory (`home_jose_Escritorio_Proyectos_sluice`), KB prefix `sluice/`, memories in scope `project/`. |
| Q6 | Real-world test beds | `the-external-repository` (heavy pre-push with CodeQL, Husky?) plus its worktrees. Confirm and name any other. | Answered 2026-10-22: `the-external-repository` and its worktrees. |
| Q7 | Minimum supported Rust version | Latest stable at project start, edition 2024. Rust is not installed on this machine: approve installing via rustup (`~/.cargo`, `~/.rustup`, no sudo). | Answered 2026-10-22: install rustup stable in the home dir (done by the agent). Edition 2024, MSRV = stable at project start. |
| Q8 | Pando MCP for this directory | Registered and working (reconnected with `/mcp` on 2026-10-22). The KB lives in `kb/` (auto-imported by Pando's watcher). Should `kb/` be committed to the repo? Proposal: yes. | Answered 2026-10-22: version all of `kb/`, including `kb/memory/`. |
| Q9 | Indexing reference repos | Answered 2026-10-22: the refs live in `refs/` inside the project, gitignored and excluded from the Sluice code index (`WatchExclude`); indexed as `ref-*` projects via `code_index_project`. | resolved |
| Q10 | Daemon idle exit | Keep the daemon alive forever (simplest) vs exit after N minutes idle (cleaner). Proposal: never exit in v1, add `idle_exit` later. | Answered 2026-10-22: the daemon never exits on idle in v1; `sluice daemon stop` stops it. |
| Q11 | Environment for gate processes | The daemon runs gates; which environment do they get? Proposal: the client's full environment snapshot minus `GIT_*` hook vars, plus the gate's `env`. Alternative: daemon env + allow-list. | Answered 2026-10-22: the client's full environment, sanitised (`GIT_*` hook vars and `CLAUDE*` removed), plus the gate's `env`. |
| Q12 | Pre-commit key content | Hash the working tree (what tests see) or the index being committed (what gets committed)? Proposal, aligned with gate-cache: copy the hook's index and `add -A` on top, i.e. working tree content; record the committed tree in the marker so a later `pre-push` on a clean checkout hits. | Answered 2026-10-22: working-tree content, with the committed tree recorded in the marker. |
| Q13 | MCP `verify` shape | (a) blocking call with progress notifications and cancellation (simplest for the model, one turn), or (b) bounded wait ≤ 30 s returning a job handle plus a `verify_status` poll tool (agent-task-queue's proven pattern). Facts found: Claude Code's MCP idle window is 30 min and progress notifications reset it; calls over 2 min are backgrounded, not killed. Proposal: (a), recorded in [[sluice/design/adr/0005-mcp-verify-blocking-or-handle.md]]. | Answered 2026-10-22: blocking `verify` with progress notifications and cancellation (ADR-0005 accepted). |
## Decisions taken without asking (reversible, reported)

- Cloned reference repos into `refs/` (first in a sibling dir, moved inside on user request).
- Initialized Pando in the project directory and registered the MCP server
  for this directory only (local scope in `~/.claude.json`).
- Set Pando's `KBPath` to `./kb`, the code embedding model to the Jina model
  already present in Ollama, and excluded `target/` from the watcher.

Related: [[sluice/plan/master-plan.md]].

| Q14 | Config hooks declaration | Asked 2026-10-22 together with the above. Per-gate `hooks = [...]` list vs a separate `[hooks]` table. | Answered 2026-10-22: per-gate `hooks = [...]` list. |
