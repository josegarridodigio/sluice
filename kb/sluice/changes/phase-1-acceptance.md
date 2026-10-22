---
title: "Phase 1 acceptance evidence"
tags: [sluice, acceptance, phase-1]
---

# Phase 1 acceptance evidence (2026-10-22)

Every criterion from [[sluice/plan/master-plan.md]] Phase 1 is covered by
an automated test in `crates/sluice-gate/tests/run.rs` (throwaway
repositories, real git, real hooks) or by the measurement in
[[sluice/changes/hit-path-latency.md]].

| Criterion | Evidence |
|---|---|
| A gate that passed is skipped on the same tree from another worktree | `first_run_executes_then_hits_and_other_worktree_shares_the_hit`: run in repo (counter 1), run again (cached), `git worktree add`, run there (cached, counter still 1). |
| Any change to a tracked or untracked non-ignored file invalidates; an ignored file does not | `tracked_and_untracked_changes_invalidate_but_ignored_files_do_not`: unstaged edit → run, untracked file → run, `.gitignore`d file → cached. Different gates have different keys. |
| Real index, `index.lock` and `.git/objects` untouched | `real_index_and_object_store_are_never_touched`: index bytes and mtime equal before/after a miss and a hit with an untracked file present; no `index.lock`; object file set unchanged. Core unit tests in `tree.rs` check the same at module level. |
| Works inside `pre-commit` and `pre-push` with git's hook environment | `works_inside_real_pre_commit_and_pre_push_hooks`: plain hooks under `core.hooksPath` call the built binary with `--hook`; a `git commit` runs `lint` and `test` once each; `git push` to a bare remote hits; a partial commit with a dirty worktree (temporary `GIT_INDEX_FILE`) works; markers carry `committed_tree`. `hook_environment_variables_from_a_subdirectory_still_hit`: `GIT_DIR`, `GIT_PREFIX`, `GIT_INDEX_FILE` set, cwd in a subdirectory → hit. |
| Hit latency measured on ≥ 5 000 files, target < 30 ms | median 29.1 ms under load 21; see [[sluice/changes/hit-path-latency.md]]. |
| `CI` disables the cache unless `SLUICE_CACHE=1`; `SLUICE_DISABLE=1` bypasses | `ci_disables_the_cache_unless_sluice_cache_is_set_and_disable_bypasses`. |

Additional behaviour proven by tests:

| Behaviour | Test |
|---|---|
| Agent in a hook fails fast on a miss (exit 3, actionable message), hits normally, `SLUICE_AGENT=0` override, `run-unqueued` policy | `agent_in_hook_fails_fast_on_miss_and_hits_on_pass` |
| A pass that modified the tree is not cached and leaves a miss record | `a_gate_that_modifies_the_tree_is_not_cached` |
| Failure returns the gate's exit code with the log tail; miss record visible in `cache status`; `sluice logs` prints the interleaved log | `failure_returns_the_gate_exit_code_with_log_tail_and_records_a_miss` |
| Timeout kills the gate tree and exits 124 promptly | `timeout_kills_the_gate_and_exits_124` |
| `budget = 1` serialises two concurrent runs | `budget_of_one_serialises_concurrent_runs` |
| `cache clear [--gate]`, `cache status --all` | `cache_clear_removes_markers` |
| `key_inputs` and `key_files` change the key | `key_inputs_and_key_files_change_the_key` |
| Extra arguments reach the gate | `extra_arguments_reach_the_gate` |
| Unknown gate and non-repository errors | `unknown_gate_and_missing_config_are_reported` |

Command and result:

```
cargo test --workspace
  sluice-gate-core: 84 passed
  sluice-gate (unit): 8 passed
  sluice-gate tests/cli.rs: 8 passed
  sluice-gate tests/run.rs: 15 passed
```
