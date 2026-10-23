---
title: "Change: alpha reconciliation of documentation and code (task R3)"
tags: [sluice, change, alpha, reconciliation, docs]
---

# Change: alpha reconciliation of documentation and code (R3, 2026-10-23)

Task R3 of [[sluice/plan/alpha-plan.md]], before v0.3.0. Every statement
about behaviour in the design documents, the ADRs, the user documentation,
the plugin skill and the CLI help was checked against the code at HEAD
(663edaf). Discrepancies were fixed in the documentation; no product code
changed (the `--help` strings were all correct, so `docs/cli.md` was only
regenerated). Recent fixes that changed behaviour after some documents were
written: [[sluice/changes/alpha-review-fixes.md]],
[[sluice/changes/ci-flakes.md]], [[sluice/changes/alpha-readiness.md]],
[[sluice/changes/scheduler-throughput.md]].

## 1. User-facing documents and the CLI

`docs/cli.md` now matches `sluice <cmd> --help` byte for byte for the
global help and all 28 command blocks (checked by a script that extracts
each block and diffs it with the real output).

| Statement / location | Code location | Finding | Resolution |
|---|---|---|---|
| `docs/cli.md` global help block | `crates/sluice-gate/src/main.rs` (`about`/`long_about`) | Block still had the old one-line exit-code summary | Regenerated from `sluice --help` |
| `docs/cli.md` commands | `main.rs`: `guard` subcommand | `sluice guard` had no section and no table row | Section and row added with its real help |
| `docs/cli.md` exit codes, `sluice exec` row | `exec.rs` `EXIT_PIPE` (141) | 141 missing from the per-command table | Added |
| `docs/configuration.md` | `config.rs` `AgentConfig`, `heavy.rs` `DEFAULT_HEAVY_COMMANDS` | The user config `[agent]` table (`heavy_commands`, `heavy_commands_extra`, `exec_weight`, default list, limits) was undocumented | Example block and three field rows added |
| `docs/how-it-works.md` Scheduling | `config.rs` `DaemonConfig`, `scheduler.rs` | Default budget stated as `max(2, cpus - 1)`; queue described as FIFO only | Now `max(2, cpus)`, learned weights, human first, lanes, order, over-admission, `logs --timeline` |
| `docs/gates-cookbook.md` rules of thumb | `cpus.rs` `default_budget` | Same stale budget default | Fixed |
| `docs/gates-cookbook.md` exec weight | `AgentConfig::exec_weight_or` | Default weight did not mention `[agent] exec_weight` | Added |
| `docs/debugging.md` | `logs.rs` `timeline`, `timeline_line` | `sluice logs --timeline` and its line format undocumented | New section |
| `docs/debugging.md` doctor | `doctor.rs` `check_scoped_gates`, `queue-only-hooks` | The `scoped-gates` and `queue-only-hooks` checks were not listed | Added |
| `docs/agents.md` verify parameters | `sluice-gate-mcp` `VerifyParams`, `QueueEnqueueParams`, `LogsParams` | `fail_fast`, parallel start, `branch`, `base`, `job_id`, `tail` undocumented | Added |
| `docs/compatibility.md` features | `proto/lib.rs` `FEATURE_*`, `server.rs` | `welcome.features` listed as `["queue"]`; now `["exec","queue-only","queue"]` | Fixed |
| `docs/compatibility.md` database | `daemon/migrate.rs` `SCHED_TABLE` | `sched_schema` missing | Table row and paragraph added |
| `docs/compatibility.md` plugin | `guard/mod.rs` | Said the guard only denies | Now describes the rewrite and the old-binary fallback |
| `docs/compatibility.md` "`cache prune`" | `marker.rs` `prune`, `run.rs` | No such command; it is the TTL pruning | Reworded |
| `docs/validation.md` | `.sluice.toml` | Gates are path-scoped | Note added |
| `plugin/skills/sluice/SKILL.md`, `reference.md` | `VerifyParams::fail_fast` | `fail_fast` missing; "all selected gates run even after one fails" is wrong by default | Fixed; `logs` and `queue_enqueue` parameters added |
| `scripts/alpha/README.md` s5 | `s5_merge_queue.py` `setup_remote` | Said ssh/http arrive with H2; H2 shipped as Rust tests and the harness options still exit "not implemented" | Reworded |
| `README.md`, `plugin/README.md`, `CONTRIBUTING.md`, `docs/merge-queue.md`, `docs/manual-test-phase-3.md` | various | Flags, defaults, exit codes, env vars, config keys, git 2.31 minimum, link targets checked | No discrepancy |

Checked and correct: every `[daemon]`, `[admission]`, `[scheduler]`,
`[cache]`, `[mcp]`, `[queue]`, `[debug]`, `[agent_detection]` and `[repos]`
default and range in `docs/configuration.md` against `config.rs`
(budget = CPUs, overcommit 1.5, overcommit interval 5 s, learned weights on,
`verify_parallel` on, `fail_fast` default true, retention 30 d, tick 5 s).

## 2. Design documents and ADRs

Each group was checked statement by statement by a separate pass; all
findings were fixed in the text. Notable rows (full diff in the worktree):

| Statement / location | Code location | Finding | Resolution |
|---|---|---|---|
| command-control.md 0.4, 2.1.3, 8.6, ADR-0010 | `AgentConfig::exec_weight_or`, `server.rs` `register_exec` | Exec weight "ceil(budget/2) until learned"; exec never learns and `exec_weight` exists | Rewritten, question 6 decided |
| command-control.md 0.7, 4.7, ADR-0010 | `guard/matcher.rs` `Edit::Gate` | A gate rewrite replaces the typed words; it is not a pure prefix insertion | Corrected |
| command-control.md 2.1 | `exec.rs` `exec()` | Order of SLUICE_JOB pass-through and command resolution | Swapped |
| command-control.md 4.1, 4.2, 4.3, 4.5, 4.6 | `guard/mod.rs`, `lex.rs`, `matcher.rs`, `rules.rs` | `SLUICE_GUARD` values, tokenizer refusals, skip words, `cd` rules, error handling, permission table all incomplete or stale | Rewritten from the code |
| command-control.md 5 | `shim.rs`, `which.rs` | Shim script sample, directory (`XDG_DATA_HOME`), messages, "doctor reports stale shims" (not implemented) | Corrected |
| executor.md | `executor.rs`, `migrate.rs`, `reaper.rs`, `run.rs` | `"$@"` only with arguments, backup procedure, reaper module name, exec jobs, passing-runs-only stats, client messages | Corrected |
| architecture.md | `main.rs`, `config.rs`, `dirs.rs` | CLI list lacked exec, guard, shim, timeline; user-config tables, socket fallback, scheduler, doctor checks outdated | Updated |
| scheduler.md, scheduler-throughput.md, ADR-0011, ADR-0007 | `scheduler.rs`, `server.rs`, `sampler.rs`, `status.rs`, `logs.rs` | Learned weights resolved per pass, passing runs only, over-admission conditions, cgroup idle CPUs, status/verify/timeline text, type sketch, predictions and open questions, "order by arrival" superseded | Corrected; measured L3 results recorded |
| psi-admission.md | `psi.rs` `calm_for_overcommit` | "Weighted admission out of scope"; overcommit keys missing | Updated |
| config-format.md | `config.rs`, `shim.rs`, `which.rs` | Env vars, version error text, 64-pattern error, `[agent]` errors, additive-key list | Added |
| path-scoped-keys.md, ADR-0012 | `config.rs`, `scope.rs`, `lookup.rs`, `doctor.rs` | Pattern validation, function signatures, `scope` only in MCP status, doctor warnings, differential-test oracle, measured results | Corrected |
| cache-key.md, committed-tree-lookup.md | `tree.rs`, `key.rs`, `marker.rs`, `cache.rs`, `git.rs` | Scratch dir fallback, auto-maintenance flags, extra args in key, marker fields, committed-tree lookup implemented | Corrected |
| compatibility.md, ADR-0013 | `proto/lib.rs`, `migrate.rs`, `config.rs`, fixtures | Gap table stale (G1..G14 closed), migrations are additive with backups, features list, exit codes, fixture plan vs as built | Rewritten as as-built |
| protocol.md | `proto/lib.rs`, `extension.rs`, `queue.rs` | `queue-approve` env now carries agent-detection variables, `train_hash` conditions, unknown enum values parse as `unknown`, status schema fixture | Updated |
| mcp-tools.md | `sluice-gate-mcp/src/lib.rs` | MCP log file, instructions text, optional `working_directory`, `requested_at`, progress prefix, early-verdict lines | Corrected |
| merge-queue.md, ADR-0009 | `queue/src/git.rs`, `extension.rs`, `sluice-gate/src/queue.rs`, `server.rs` | git maintenance flags and env, features list, CLI output blocks, 60 s reply timeout, agent approval refusal | Corrected |

## 3. CHANGELOG (`Unreleased`)

Cross-checked with `git log v0.2.0..HEAD` (37 commits).

| Entry | Finding | Resolution |
|---|---|---|
| Timeline in "Changed" | New feature | Moved to "Added" and merged with `logs --timeline` and `idle_while_waiting_ms` |
| "Learned gate weights are resolved at every admission pass" | Refinement of an unreleased feature, listed as a change from 0.2.0 | Folded into the scheduler-throughput entry |
| Budget default | 0.2.0 value not stated | Added "0.2.0: CPUs minus one" |
| Fixed: guard `cd`/`CDPATH`/`PATH=`/approval, scheduler over-admission/learned weights/cgroup, database backups, doctor unsafe socket | Defects of features that never shipped in 0.2.0 | Removed (not user-visible relative to 0.2.0) |
| Fixed: non-UTF-8 environment, queued job dropped by last subscriber, process-group reuse | Apply to 0.2.0 code | Kept, reworded |
| "cache `prune`" | No such command | Reworded to TTL pruning |
| "Known limitations" README section | Section is named "Status and limitations" | Fixed |

## 4. Plans

- `master-plan.md`: Alpha readiness checkboxes ticked (the badge change is
  recorded as part of R4), status table row `alpha` added, "Next steps"
  rewritten.
- `alpha-plan.md`: new section 4a with the progress of every task (C, F, G,
  H1-H5, A0-A2, B0, B1, D0, D1, E1-E4, L1-L3, R1, R2, X1 done; E5 and R3 in
  progress; R4 pending) and corrected harness inventory states.

## 5. Knowledge base graph

`kb_related_documents` without arguments reported no wanted concept before
this note was written; this note links only to existing documents.

## Open discrepancies: none

Two maintainer reminders, not discrepancies: the release bump must change
`plugin/.claude-plugin/plugin.json`, `Cargo.toml` and the plugin test
together (all still say 0.2.0), and no `compat/v0.3.0` fixture directory
exists until the release.
