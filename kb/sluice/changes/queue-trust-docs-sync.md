---
title: "Queue trust: documentation sync (agent surfaces and full audit)"
tags: [sluice, changes, docs, merge-queue, memory, agents]
---

# Queue trust: documentation sync

Decisions: [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]],
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]]. Plan:
[[sluice/plan/queue-trust-plan.md]]. Documentation only, plus two agent-facing
strings in Rust (the MCP `INSTRUCTIONS` and the session-start rules); no product
behaviour changed.

Method: the items were enumerated from the code (config structs and validation in
`config.rs` / `config/containment.rs`, `DEFAULT_HEAVY_COMMANDS`, the reasons the queue
writes, the clap definitions, the proto types, the MCP and CLI renderers, the doctor
checks) and checked against the docs; each mismatch below was fixed in the doc.

## Part 1: what agents must know

| File | Change |
|---|---|
| `plugin/skills/sluice/SKILL.md` | Memory kill is not a retry-unchanged failure; a gate waiting for memory is only queued; a short pointer for queue reasons; `--trust-member-config` is human-only; `cargo check`/`cargo doc` in the shim bullet. |
| `plugin/skills/sluice/reference.md` | `queue_retry` row (no trust flag); `KILLED for lack of memory` in "Reading a verify result" (and the shell form `KILLED after ...` plus `sluice: <gate> was killed for lack of memory`); `wait_detail: memory`; "Merge queue states" rewritten with a reason table (what each reason means and what the agent does: `prepare_failed` base true/false, `prepare_dirty`, `gate_failed` incl. memory kills, `gate_modified_tree`, `key_input_failed`, `member_config_invalid`, `hook_gate_unverified`/`config_changed`, `pre_push_hook`, the rest); the human-only flag; the hook answered from the train; `invalid_config`; the session rules and heavy list. |
| `crates/sluice-gate-mcp/src/lib.rs` | `INSTRUCTIONS` gains the reason guidance, the memory-kill rule and the waiting-for-memory note; `instructions_carry_the_queue_guidance` asserts the new phrases (`prepare_failed`, `gate_modified_tree`, `--trust-member-config`, `KILLED for lack of memory`, `waiting for memory`). |
| `crates/sluice-gate/src/session_start.rs` | Rule 6 of the session context: a gate `KILLED for lack of memory` is not retried unchanged. The context stays under the 1500-character cap (about 1230 with 20 gate names; asserted by `contexts_are_short`). |
| `docs/agents.md` | Session-hook rules list; the paste-in block of section 3 gains the memory-kill and waiting-for-memory bullets; the merge-queue block gains the reason guidance, the `--trust-member-config` rule (never use it, never ask to approve such a train) and the hook note; `cargo check` in the shims block. |
| `plugin/README.md` | Skill and session-hook rows describe the new rules; the heavy-command list was stale (it listed only `cargo build|test|clippy|nextest`): now includes `check`/`doc` and points at the full table. |

Statements that had become false and were fixed: the reference's `failed` state text
(only gate failure and `base_unavailable`); `plugin/README.md` heavy list; the agents
docs' "gates with a `pre-push` hook are used" wording of `[queue] gates`
(`gates-cookbook.md`).

## Part 2: audit, mismatches found and fixed

- `plugin/README.md`: heavy list stale (above).
- `docs/gates-cookbook.md`: `[queue] gates` described as "without it, pre-push gates";
  the code is `[queue] gates` plus every pre-push gate, else every gate. No mention of
  `[queue] prepare`: added.
- `docs/cli.md`: `sluice status` text and JSON memory fields were not documented
  (`memory:` line, `mem x/y` on running jobs, `(budget: memory)` and `mem ~x` on queued
  ones, `gates memory policy:` line, `budget.memory`, `wait_detail = "memory"`,
  `memory_*` fields, `containment.gates_oom_policy`); the doctor containment group list
  missed `gates-oom`, `gates-memory-policy`, `memory-guard`.
- `docs/debugging.md`: intervention causes missed `sluice_memory_guard`; the daemon's
  `hook run request` and `push hook vouched` log lines were undocumented.
- `docs/merge-queue.md`: the `enqueue` synopsis missed `--trust-member-config`.
- `docs/how-it-works.md`: no memory admission paragraph; the daemon environment list
  missed `SLUICE_SESSION_LEAF`, `SLUICE_SHIM_DIR`, `SLUICE_SHIMS`, `SLUICE_GUARD` (the
  code, `env::STRIPPED`, strips them).
- `docs/compatibility.md`: `[queue] prepare*` (one-way, fixture `queue-prepare.toml`),
  the `queue-trust-config` feature, the new optional queue status fields and reasons,
  the nullable queue columns were not listed.
- `README.md`: feature list missed memory admission and `prepare`; the limitations
  bullet said memory is "only accounted" without systemd-oomd 257, which is no longer
  true (admission by memory, kernel preference and the opt-in guard apply everywhere).
- `kb/sluice/design/config-format.md`: `[admission]` memory keys and their validation,
  the `[containment]` table (pointer), `queue.gates` pre-push addition, `key_inputs`
  environment, and the default heavy list (it stopped at `vitest`; the code list has 49
  entries) were missing.
- `kb/sluice/design/protocol.md`: `queue-trust-config` feature, `killed_by = "memory"`,
  `memory_peak_bytes`, a `cached` outcome with a message, and the status memory fields
  were missing.
- `kb/sluice/design/mcp-tools.md`: instructions paragraph, the memory-kill result line
  and `killed_by`, queue status rendering (`prepare:`, `not cached`, `CONFIG FROM BRANCH`,
  `hook_gate_unverified`), `invalid_config`, and the statement that the `status` text
  "mirrors `sluice status`" (it does not print `wait_detail` or the memory lines; they are
  in the structured result).
- `kb/sluice/design/executor.md`, `containment.md`: the `cg-exec` argv lacked
  `--oom-score-adj <N>` and the `-` leaf.
- `kb/sluice/design/adr/0019-memory-safety-of-gate-jobs.md`: piece 5 marked "not
  implemented"; piece 6 names `cargo doc` as well.
- `CHANGELOG.md` `[Unreleased]`: status/doctor additions and the build-time
  `hook_gate_unverified` / `member_config_invalid` failures.

Checked and found consistent (no change): every `.sluice.toml` and user-config key,
default and validation message in `docs/configuration.md` against the structs; the
reasons table of `docs/merge-queue.md` against design sections 3 and 7 and the strings
the queue writes; `--trust-member-config` flags; the heavy-command table; the
memory-guard and admission constants (3 s, 15 s, 10 %, last 5 runs, +20 %, +25 %);
no remaining reference to the base-probe memo (`probe_known`, `probe_remember`),
`TROUBLE_SAMPLES` or a "heavy job" admission exemption (`alone` is the only exemption).

## Suspected code issues (not changed)

1. **ADR-0019 piece 5 is not implemented.** `[containment] gates_memory_max` /
   `gates_memory_swap_max` do not exist in `ContainmentConfig`, while the ADR and the W6
   plan list them (config docs and compatibility fixture). Either implement or drop the
   piece from the ADR and the plan.
2. **MCP `status` text drops the new information.** `sluice-gate-mcp/src/text.rs`
   `render_status` prints neither `wait_detail` nor the `memory:` line nor per-job `mem`;
   an agent reading only the text sees `(budget)` for a gate that waits for memory (the
   structured content has `wait_detail`). Likely an oversight; the agent docs therefore
   name `wait_detail: "memory"` for MCP and `(budget: memory)` for the CLI.
3. `sluice run`'s memory kill prints `KILLED after ...` and then the daemon message, while
   MCP prints `KILLED for lack of memory after ...`: two spellings of one event. The
   agent docs cover both.
4. `cgroup::explain::cause_text` has no entry for `sluice_memory_guard`; it would be shown
   raw if such an intervention ever reached an agent's session note (a gate job's row has
   no session, so today it does not).
5. The MCP/CLI `prepare` status can be `fail`, `timeout`, `killed` or `error` (the
   daemon's outcome status), not only `fail`; docs show `fail` as the example.

## Verification

`cargo fmt -p sluice-gate-mcp -p sluice-gate` and `cargo check -p sluice-gate-mcp -p
sluice-gate --all-targets -j 4`: clean. The full suite is not re-run for this change
(Rust edits are two string literals and one test needle list); it belongs to the final
`verify` of the branch.

## Follow-up fixes

Four of the "suspected code issues" above were then fixed in code (item 5, the `prepare`
status example, stays as documented).

1. **ADR-0019 piece 5 implemented** (issue 1). `[containment] gates_memory_max` and
   `gates_memory_swap_max` (size strings, empty = unset) in `ContainmentConfig`
   (`gates_memory_max_bytes`, `gates_memory_swap_max_bytes`, validated with
   `parse_size_bytes`). `TreeSettings` gains `gates_memory_max` / `gates_memory_swap_max`
   (dropped when the memory controller is not delegated). `Tree::apply_gates_ceiling`
   writes `memory.max` / `memory.swap.max` on `jobs/` only (never on a job leaf, never on
   the scope or above) from `ensure_daemon_scope`, on creation and on reuse, best effort;
   unset writes `max`, so removing the key undoes an earlier value. No `memory.high` key.
   A hit is an in-cgroup kernel OOM kill: the leaf's `memory.oom.group = 1` takes the whole
   job and the leaf's `oom_kill` / `oom_group_kill` counters rise (the victim's cgroup
   counts them), so `memkill::memory_kill_message` reports it unchanged. `sluice doctor`
   shows a `gates-ceiling` line when either key is set (`doctor::containment::gates_ceiling`).
   Tests: config (`the_gates_ceiling_parses_and_is_validated`, defaults), tempdir tree
   (`the_ceiling_is_written_on_jobs_only_and_unset_means_max`), doctor
   (`the_gates_ceiling_is_shown_only_when_set`), compat fixture
   `unreleased/containment-gates-memory-max.toml` (one-way key, noted in
   `docs/compatibility.md`). Docs: ADR-0019 (the "not implemented" mark removed), the
   plan, `docs/configuration.md`, `design/containment.md` sections 4 and 7,
   `design/config-format.md`, CHANGELOG.
2. **MCP `status` text** (issue 2). `render_status` now prints the `memory:` line, the
   per-running-job `mem now/expected` and, for a queued job, `(budget: memory)  mem
   <expected>`. The formats moved into shared helpers of `sluice-gate-proto`
   (`MemoryBudget::status_line`, `RunningJob::memory_text`, `QueuedJob::memory_text`,
   `QueuedJob::wait_detail_text`, `gib_text`) used by both `sluice status` and MCP. Test:
   `status_text_shows_the_memory_budget_and_the_wait_detail`. Docs fixed:
   `plugin/skills/sluice/SKILL.md`, `reference.md`, `docs/agents.md`,
   `design/mcp-tools.md`.
3. **One memory-kill wording** (issue 3). `sluice run` and `sluice exec` print `KILLED for
   lack of memory after <secs>s (job <id>)` (run keeps ` log=<path>`) followed by the
   daemon's message, like MCP `verify`; other KILLED texts are unchanged
   (`run::killed_headline`, `exec::memory_kill_headline`, unit tests). Docs:
   `reference.md`, `docs/debugging.md`, `design/executor.md`.
4. **`sluice_memory_guard` cause text** (issue 4). `explain::cause_text` maps it to "Sluice
   memory guard, gate cancelled for lack of memory"; test
   `every_known_cause_has_friendly_text`.

Verification: `cargo fmt` and `cargo check --workspace --all-targets -j 4`: clean. The
suite, clippy and `fmt --check` belong to the final `verify` of the branch.
