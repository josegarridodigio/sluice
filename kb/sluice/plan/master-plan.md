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
| 1 | Cache only, no daemon | **done** (2026-10-22) | [[sluice/changes/phase-1-cache-only.md]], [[sluice/changes/phase-1-acceptance.md]], [[sluice/changes/hit-path-latency.md]]. |
| 2 | Daemon, scheduler, coalescing | **done** (2026-10-22) | [[sluice/changes/phase-2-daemon.md]], [[sluice/changes/phase-2-acceptance.md]]; design in [[sluice/design/protocol.md]], [[sluice/design/scheduler.md]], [[sluice/design/executor.md]], ADR-0007. |
| 3 | MCP front-end and agent policy | **done** (2026-10-22) | [[sluice/changes/phase-3-mcp.md]], [[sluice/changes/phase-3-acceptance.md]] (end to end with real sessions via `scripts/phase3-e2e.sh`). |
| 4 | Load-aware admission and observability | **done** (2026-10-22) | [[sluice/changes/phase-4-psi.md]], [[sluice/changes/phase-4-acceptance.md]]; design [[sluice/design/psi-admission.md]]. |
| 5 | Packaging and real-world validation | **done** (2026-10-22) | [[sluice/changes/packaging.md]] (release `v0.1.0` published, installer verified against it), [[sluice/changes/phase-5-validation.md]] (benchmark on this repository), README demo. |
| 6 | Local merge queue (mergetrain-inspired) | **in progress** (design approved 2026-10-22) | [[sluice/design/merge-queue.md]], ADR-0009 accepted; user answers in section 14 of the design. |

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

Status: done 2026-10-22. All acceptance criteria proven by
`crates/sluice-gate/tests/run.rs` and the latency measurement; evidence in
[[sluice/changes/phase-1-acceptance.md]]. Decision added:
[[sluice/design/adr/0006-hook-flag.md]].

## Phase 2 — Daemon, scheduler, coalescing

Tasks:

- [x] Design docs: protocol, scheduler (+ ADR-0007), executor and lifecycle, committed-tree lookup decision.
- [x] `sluice-gate-proto`: typed messages with round-trip tests.
- [x] `sluice-gate-daemon::store`: SQLite schema, fenced lease, jobs, events, totals, log pruning.
- [x] `sluice-gate-daemon::scheduler`: budget, weights, backfill + head reservation, repo caps, coalescing by key.
- [x] `sluice-gate-daemon::executor`: process groups, renice/ionice, log capture, timeout, cancel, kill-tree, pid-reuse guard.
- [x] `sluice-gate-daemon::server`: socket, handshake, subscribers, scheduling loop, marker double-check, orphan reconciliation, stop.
- [x] CLI: `sluice daemon [--foreground] | stop`, client on misses with autostart and fallback, `status [--json]`, `logs` via daemon, `hit` events.
- [x] Acceptance tests and evidence document ([[sluice/changes/phase-2-acceptance.md]]); README/docs.
- [x] Reconciliation pass ([[sluice/changes/phase-2-reconciliation.md]]): 56 findings, code items fixed, rest documented.
- [x] Blocking client moved to `sluice-gate-proto` ([[sluice/design/adr/0008-client-in-proto-crate.md]]).

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

Tasks:

- [x] Design [[sluice/design/mcp-tools.md]]; facts [[sluice/research/claude-code-facts.md]].
- [x] `sluice-gate-core::lookup` shared hit check; `sluice-gate-mcp` with rmcp 3.6 (`verify`, `status`, `gate_list`, `logs`), progress, cancellation; `sluice mcp`.
- [x] Fail-fast message points to `verify`; `[mcp] progress_interval`.
- [x] `docs/agents.md`; `docs/manual-test-phase-3.md`.
- [x] Automated MCP client tests (`crates/sluice-gate/tests/mcp.rs`).
- [x] Reconciliation pass ([[sluice/changes/phase-3-reconciliation.md]]): 42 findings; `extra_args` now in the key, per-gate errors kept, coverage added; rest documented.
- [x] End-to-end run with two real Claude Code sessions in this repository (`scripts/phase3-e2e.sh`), recorded in [[sluice/changes/phase-3-acceptance.md]]; progress tokens confirmed.

Deliverables: `sluice mcp` built with rmcp ([[sluice/design/mcp-tools.md]]),
tools `verify`, `status`, `gate_list`, `logs`; agent detection and
`agent_hook_policy`; `docs/agents.md` with a ready-to-paste CLAUDE.md section.

Acceptance: documented manual end-to-end test with two Claude Code agents in
two worktrees: one runs, the other coalesces; later `git commit` hooks hit the
cache; on a miss the hook fails fast with the actionable message.

## Phase 4 — Load-aware admission and observability

Tasks:

- [x] Design [[sluice/design/psi-admission.md]].
- [x] `psi.rs` sampler and admission state machine; server integration; `psi_max_wait`; status fields; `pressure:` line.
- [x] Frozen `status --json` schema test with fixture.
- [x] Integration tests with fake PSI files; `scripts/psi-pressure-demo.sh` measurements.
- [x] Reconciliation pass ([[sluice/changes/phase-4-reconciliation.md]]): 36 findings; thresholds validated, docs corrected, metrics export defined as `status --json`.

Deliverables: PSI-based admission, stable `sluice status --json` schema;
that JSON (runs, hits, coalesced count, wait and run times, time saved,
PSI) **is** the metrics export the user's HTML dashboard consumes (no
separate exporter).

Acceptance: under synthetic CPU pressure new jobs wait; thresholds documented
with measurements.

## Phase 5 — Packaging and real-world validation

Tasks:

- [x] Release workflow (`cross`, musl x86_64/aarch64, checksums) and `scripts/install.sh` ([[sluice/changes/packaging.md]]).
- [x] Sluice verifies itself: `.sluice.toml`, `.githooks/`, CONTRIBUTING.
- [x] Reproducible multi-worktree benchmark `scripts/validation-bench.sh` and evidence ([[sluice/changes/phase-5-validation.md]]).
- [x] README demo GIF (`scripts/demo/`).
- [x] Tag `v0.1.0` and watch the first release workflow run (approved by the user; green on the first run).
- [x] README install line with the release URL and the installer.

Deliverables: release builds (Linux x86_64/arm64), install script,
`cargo install sluice-gate`, man page / README. Validation on this
repository (Sluice runs its own gates) with before/after numbers from a
reproducible multi-worktree benchmark; private repositories are never
referenced here.

## Phase 6 — Local merge queue

- [x] Design and ADR-0009 ([[sluice/design/merge-queue.md]]); user answers recorded 2026-10-22.
- [ ] Daemon extension API (`Extension`, `DaemonHandle`), `welcome.features`, queue messages in `sluice-gate-proto`.
- [ ] Crate `sluice-gate-queue`: store, runner, push protocol, reconcile.
- [ ] CLI `sluice queue enqueue|status|approve|retry|cancel|reconcile` and MCP `queue_enqueue|queue_status|queue_approve`.
- [x] Acceptance tests 1–8 of the design (local bare remote, kill during push): [[sluice/changes/phase-6-acceptance.md]] (2026-10-22).
- [ ] Docs (`docs/agents.md`, README), change note, acceptance note, reconciliation report.

## Debugging and observability (after Phase 6, before the final report)

- [x] `[debug] level` in the user config and `sluice daemon restart [--debug]`; `SLUICE_LOG` keeps precedence.
- [x] Persistent logs for `sluice mcp` (`<state>/logs/mcp-<pid>.log`) and for the CLI (`<state>/logs/client.log`, one line per invocation: gate, tree, decision, exit code, duration), under the same retention as job logs.
- [x] Size-based rotation of `<state>/daemon.log`.
- [x] `sluice doctor` (git version, PSI, socket, lease, daemon vs installed binary version, config validity) and `sluice doctor --bundle` (a `.tar.gz` with version, configs, `status --json`, `queue status --json`, recent daemon/client/MCP logs and the last failed job logs, with secrets and home paths redacted) for bug reports; CONTRIBUTING points to it.

Why: today the log level is fixed at daemon start, the MCP server and the hook client leave no persistent trace, `daemon.log` grows without bound and a bug report needs manual collection.

## Agent integration package (after Phase 6, before the final report)

- [x] Claude Code plugin in this repository, the repository doubling as its marketplace: registers the `sluice mcp` server, ships a `sluice` skill (verify → commit → `queue_enqueue` → stop; exit code 3; reading `status`/`logs`; queue conflicts), and an optional `PreToolUse` hook that redirects raw test commands run through Bash to `verify` and denies `sluice queue approve` to agents.
- [x] `docs/agents.md` points to the plugin; the per-repository `CLAUDE.md` block shrinks to one line or disappears.
- [x] Proven with real non-interactive sessions on the cheapest model (`haiku`): the skill loads, the hook redirects, the agent verifies and enqueues ([[sluice/changes/agent-plugin.md]]).

Why: one install per user instead of a block pasted into every repository's `CLAUDE.md`; the skill only enters the context when relevant; the hook is the only piece that enforces the queue.

## Working agreements

- Everything in English (code, docs, commits, KB).
- Every increment: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` green.
- Full test suite runs once at the end of each task, not after every edit.
- Every change gets a KB document under `sluice/changes/` or `sluice/fixes/`;
  decisions get an ADR under `sluice/design/adr/`.
- No pushes or publishing without the user's approval.
- Commit messages carry no AI attribution trailers.

## Next steps

1. Phase 6 implementation per the checklist above.
2. Final reconciliation over the whole KB and the final report.

Earlier plan for Phase 3: `sluice-gate-mcp` with rmcp per [[sluice/design/mcp-tools.md]]
   (`verify`, `status`, `gate_list`, `logs`), fail-fast message pointing to
   `verify`, `docs/agents.md`, automated MCP client test, documented manual
   end-to-end run with two Claude Code agents.
