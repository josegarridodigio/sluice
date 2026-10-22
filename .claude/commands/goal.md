---
description: Drive Sluice to completion through every phase (0–5 plus the merge-queue phase), orchestrating parallel sub-agents with per-task model selection and keeping the Pando knowledge base in exact sync with the code.
---

# Goal: ship Sluice, all phases, fully documented

You are the lead implementation agent for Sluice in this repository. Your
goal is finished only when **every phase below is implemented, its
acceptance criteria are proven with evidence, and the Pando knowledge base
describes exactly what the code does**. You own the architecture, the plan
and the quality bar. You delegate bounded work to sub-agents and you choose
the model for each one to spend tokens where they buy correctness.

Nothing in this file overrides `CLAUDE.md`, the user's global rules
(English everywhere, no AI attribution in git history, full test suite only
at the end of a task) or the ADRs in `kb/sluice/design/adr/`. Read those
first if they are not in context.

## 0. Every iteration starts the same way

1. Load the Pando tools (`ToolSearch("select:mcp__pando__kb_search_documents,mcp__pando__kb_get_document,mcp__pando__kb_add_document,mcp__pando__kb_related_documents,mcp__pando__code_hybrid_search,mcp__pando__code_find_symbol,mcp__pando__code_index_status,mcp__pando__remember,mcp__pando__recall")`).
   If the `pando` MCP server is down, tell the user to run `/mcp` and
   reconnect; do not continue without it.
2. `kb_get_document("sluice/plan/master-plan.md")`. It is the single source
   of truth for status. Pick the first unchecked task of the first
   unfinished phase. Never redo a checked task.
3. `kb_search_documents` for the task's topic (no tag filter) and
   `recall` for project memories, so you build on prior decisions.
4. If the task has a counterpart in a reference project, query it through
   the code index (`ref-gate-cache`, `ref-agent-task-queue`, `ref-rmcp`,
   `ref-mergetrain`) before designing. Write down what you take and what
   you change.

## 1. The phases and their exit criteria

Phase 0 is done. Work through the rest in order; a phase is closed only
when its acceptance block is proven by automated tests or by a documented
manual run with the exact commands and their output stored in
`kb/sluice/changes/phase-N-acceptance.md`.

### Phase 1 — Cache only, no daemon
Deliverables: `.sluice.toml` and user config loading (strict parsing,
`humantime` durations), `git` wrapper module, tree hash from a private
index and private object directory (see `kb/sluice/design/cache-key.md`),
cache key, markers and miss records with TTL, agent detection
(ADR-0004), `sluice run` (hit path, `flock` counting semaphore, run, marker
write, tree-unchanged check), `cache status|clear`, `init`,
`hook print husky|lefthook|pre-commit|plain`, hit-path latency measured.
Acceptance:
- A gate that passed is skipped on the same tree from another worktree.
- Any change to a tracked or untracked non-ignored file invalidates the marker; an ignored file does not.
- The real index, `index.lock` and `.git/objects` are byte-for-byte untouched by a cache check (test it).
- Works from inside `pre-commit` and `pre-push` hooks with git's hook environment present (tests set `GIT_INDEX_FILE`, `GIT_DIR`, `GIT_WORK_TREE`, `GIT_PREFIX`).
- Hit latency measured on a repo of at least 5 000 files and recorded in `kb/sluice/changes/hit-path-latency.md`; target < 30 ms, explain any miss.
- `CI` disables the cache unless `SLUICE_CACHE=1`; `SLUICE_DISABLE=1` bypasses everything.

### Phase 2 — Daemon, scheduler, coalescing
Deliverables: versioned newline-JSON protocol in `sluice-gate-proto`
(`kb/sluice/design/protocol.md`), daemon with socket in
`$XDG_RUNTIME_DIR/sluice/`, autostart with a start lock and a fenced lease
row, budget scheduler with weights and a no-starvation rule
(`kb/sluice/design/scheduler.md` + ADR), in-flight coalescing, executor
with process groups, SIGTERM→SIGKILL escalation, nice/ionice, per-job
interleaved logs, SQLite job history and metrics, `sluice status [--json]`,
`sluice logs`, `sluice daemon stop`, orphan reconciliation on restart.
Acceptance:
- 6 concurrent `sluice run test` on the same tree run the command once; all 6 receive the result (counter-file test).
- With different trees, the number of concurrently running gates never exceeds the budget (weights honoured).
- Killing a client or the daemon mid-run leaves no orphan process and no stale lock (kill tests with `pgrep` assertions).
- A gate that exceeds its timeout is killed with its whole process group.

### Phase 3 — MCP front-end and agent policy
Deliverables: `sluice mcp` with `rmcp` (stdio, `server`, `macros`,
`transport-io`, `schemars`), tools `verify` (blocking, progress
notification every 15 s, cancellation token, text + `structured_content`),
`status`, `gate_list`, `logs`; `agent_hook_policy` (`fail-fast` default for
agents, `wait` for humans, `run-unqueued` escape hatch) wired into
`sluice run`; `docs/agents.md` with a ready-to-paste `CLAUDE.md` section
and the permission-bypass warning; `kb/sluice/design/mcp-tools.md`.
Acceptance:
- Automated: an MCP client test drives `verify` end to end against a throwaway repo and the daemon.
- Manual, documented step by step with output: two Claude Code agents in two worktrees call `verify`; one runs, the other coalesces; later `git commit` hooks hit the cache instantly; on a miss an agent-invoked hook fails fast with the actionable message; a gate longer than 2 minutes survives Claude Code's auto-backgrounding and a lowered `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` thanks to progress notifications.

### Phase 4 — Load-aware admission and observability
Deliverables: PSI admission (`/proc/pressure/cpu`, `/proc/pressure/memory`,
`some avg10` thresholds, graceful fallback when PSI is absent), stable
`sluice status --json` schema documented and versioned, metrics export
(hit rate, coalesced count, wait and run times, estimated time saved)
consumable by the user's HTML dashboard, `kb/sluice/design/psi-admission.md`.
Acceptance:
- Under synthetic CPU pressure (`stress`-like load generated in the test) new jobs wait and resume when pressure drops; thresholds and measurements documented.
- The JSON schema has a test that fails on accidental changes.

### Phase 5 — Packaging and real-world validation
Deliverables: release workflow producing Linux x86_64 and aarch64
binaries, install script, `cargo install sluice-gate` path verified from
a clean container or temp `CARGO_HOME`, man page or `--help` export in
`docs/`, README updated with real install instructions and a demo (GIF or
asciinema) of six requests becoming one run.
Acceptance:
- Validated on `the-external-repository` and its worktrees: before/after numbers (wall time per commit and per push, suite runs per day) recorded in `kb/sluice/changes/phase-5-validation.md`. Ask the user to run the agents during the measurement window; prepare the hook snippets and the measurement script yourself.
- Crates are **not** published to crates.io without an explicit "publish" from the user; everything else in the phase proceeds autonomously.

### Phase 6 — Local merge queue (previously "later", now in scope)
The user has authorised this phase. Start with design, not code:
1. Query `ref-mergetrain` and write `kb/sluice/design/merge-queue.md` plus ADR(s): enqueue a committed branch from a worktree; one lease-fenced runner; build the combined tree in a disposable worktree; run the repository's gates **through Sluice** so the cache and the budget apply; require approval before any push (approval is given through the CLI or MCP, never inferred); write-ahead marker and pin ref before an atomic push; `reconcile` for interrupted pushes; batch failure isolation as a stretch goal.
2. Present the design to the user in one message and wait for approval before implementing (this is the only mandatory stop in the whole goal besides crates.io publishing).
3. Implement as `sluice queue enqueue|status|approve|retry|reconcile` plus MCP tools `queue_enqueue`, `queue_status`, `queue_approve`, in a new crate `sluice-gate-queue` if it keeps `sluice-gate-core` sync-only.
Acceptance:
- Two agents enqueue conflicting and non-conflicting branches; the runner lands the non-conflicting one, reports the conflict for the other, and never pushes without approval.
- Killing the runner during a push leaves a recoverable state that `reconcile` resolves deterministically (test with a local bare remote).

## 2. Working in parallel with sub-agents

Use the `Agent` tool. Run independent tasks concurrently in one message;
never hand two agents the same files. Rules:

- **Isolation.** When two implementation tasks touch the same crate, run
  them sequentially or give each `isolation: "worktree"` and merge
  yourself. Research, documentation and test-writing tasks can run in
  parallel with implementation.
- **Briefs.** A brief states the goal, the exact files and symbols, the KB
  documents to read, the Pando project ids to query, what is already
  decided, the tests to write, and the shape of the report (under 15
  lines). Never paste large files into a brief; point at them.
- **Reports.** Ask every agent to end with: files touched, tests added,
  commands run with results, open doubts. Verify at least one claim of
  every report yourself before building on it.
- **You keep** architecture decisions, ADRs, the master plan, merges,
  commits and anything that crosses crate boundaries.

### Model selection (decide per task, state the choice in the brief)

Use `model: "sonnet"` (Claude Sonnet 5.5) when the task is bounded and the
spec is already written. It costs a fraction of Opus and is accurate on
well-specified work:
- writing tests from an explicit list of cases,
- hook snippets, `init` templates, `--help` text, man page, README sections,
- KB documents and reconciliation reports from a checklist,
- clippy/fmt clean-ups, renames, boilerplate for new subcommands,
- research notes with a precise brief and file pointers,
- measurement scripts and running benchmarks.

Use `model: "opus"` (Claude Opus 5.5) when being wrong is expensive or the
design is not fully pinned down:
- tree hashing and the git wrapper (index/object-dir isolation, hook env),
- scheduler, coalescing, lease fencing, executor and process-group handling,
- crash-safety and kill tests, anything with races or signals,
- the IPC protocol and the MCP server,
- the merge queue runner and reconciliation,
- debugging a failing or flaky test that a Sonnet agent could not fix in one try,
- adversarial review of Opus-written code before a phase closes.

Default to Sonnet; escalate to Opus when the brief needs more than two
paragraphs of judgement calls, or when a Sonnet agent reports doubt. Do
not spawn an agent for a change under a handful of tool calls; do it
yourself. Keep your own context lean: delegate reading large files, keep
only conclusions.

## 3. Definition of Done for every task

1. Code compiles; `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets -- -D warnings` and
   `cargo test --workspace` are green. Run the full suite once when the
   task is complete, not after every edit.
2. New behaviour has tests; every bug fix has a regression test.
3. Repository docs updated if behaviour or CLI changed (`README.md`,
   `docs/`, `--help` text).
4. `kb_add_document` under `sluice/changes/<slug>.md` (or `sluice/fixes/`)
   with: what changed, files and symbols, why, verification commands and
   results, links (`[[...]]`) to the design docs it implements. Update the
   existing document when one covers the topic.
5. Design docs touched by the change updated in the same task
   (`architecture.md`, `config-format.md`, `cache-key.md`, `protocol.md`,
   `scheduler.md`, `mcp-tools.md`, …). A new or changed decision gets an
   ADR with status `accepted` and a date.
6. Master plan checkbox ticked, status table and "Next steps" updated.
7. Commit with a Conventional Commits message in English, no attribution
   trailers. Verify with
   `git log -1 --format=%B | grep -iE "claude|anthropic|🤖"` returning
   nothing (the word "Claude" is forbidden even as a file name). Push to
   `origin main` when CI is green on the previous push.

## 4. Documentation must match the code, always

Discrepancies between the KB and the implementation are defects. Guard
against them at three points:

- **Per task:** step 5 above. If implementing shows the design was wrong,
  change the design doc and write an ADR explaining why; never leave the
  doc describing a different behaviour.
- **Per phase close:** spawn a Sonnet reconciliation agent with this
  brief: "Read every document under `kb/sluice/design/` and the README.
  For each statement about behaviour, find the code that implements it
  (`code_find_symbol`, `code_hybrid_search` on project
  `home_jose_Escritorio_Proyectos_sluice`, or grep). Report every
  statement with no implementation, every implementation with no
  statement, and every mismatch, as a table with file:line on both
  sides." Fix everything it finds, then run `kb_related_documents` with no
  arguments and document every wanted concept that belongs to the closed
  phase. Store the final table in `kb/sluice/changes/phase-N-reconciliation.md`.
- **Per goal end:** one last reconciliation pass over the whole KB, plus a
  check that `README.md`, `docs/` and `--help` agree with each other.

Keep `kb/` committed with the code so the history of a design change and
its implementation share a commit.

## 5. When to stop and ask

Ask the user only for: publishing to crates.io, the Phase-6 design
approval, deleting or rewriting history on `origin`, and any change to the
frozen `version = 1` config format or the on-disk cache format. For
everything else decide, document the decision in an ADR or in
`kb/sluice/questions/open-questions.md` under "Decisions taken without
asking", and continue. If blocked by something only the user can do (run
agents on the real project, provide a machine), finish every other task
first and leave a precise request.

## 6. Finishing

The goal is complete when: all seven phases show `done` in the master plan
with an acceptance document each; the final reconciliation report lists no
discrepancies; CI is green on `origin main`; and you have written
`kb/sluice/changes/final-report.md` summarising per phase what was built,
the measured numbers, the open limitations, and the exact commands a new
user runs to install and configure Sluice. End with a short message to the
user pointing at that report.
