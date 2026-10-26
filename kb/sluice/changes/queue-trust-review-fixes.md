---
title: "Queue trust: review fixes (Q1-Q5, D1-D4)"
tags: [sluice, changes, merge-queue, scheduler, guard, review]
---

# Queue trust: review fixes

Findings confirmed by a reviewer and accepted by the lead, on the uncommitted
`fix/queue-push-and-prepare` work. Decisions: [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]],
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]].

## Queue

- **Q1, no base-probe memo.** Removed `Core.probes`, `probe_known`, `probe_remember`, `ProbeKey`.
  A remembered pass blamed members for later environment failures on the same base. Docs: ADR-0018
  section 5, design merge-queue step 4b, `docs/merge-queue.md`, W4 note ("Review correction").
- **Q2, `hook_gate_unverified`.** New `hookcheck` module and `runner::reject_hook_gates`, called in
  `build_inner` after the snapshot assertion. Members that changed the configuration fail with
  `hook_gate_unverified` (detail `gates`, `hint`) or `member_config_invalid` (merged file does not
  parse); the others are requeued, not split. Skipped when no member changed the configuration.
  Rendering: CLI `failure_text` (`hint`), MCP `job_line`; reasons in `model.rs` trigger text,
  `docs/merge-queue.md`, design sections 3, 5 (step 5b) and 7. Tests: `hookcheck` unit tests;
  `a_member_adding_a_pre_push_gate_fails_at_build_time`,
  `the_other_member_of_such_a_train_is_requeued_and_lands`,
  `a_member_breaking_the_config_fails_with_member_config_invalid`; CLI
  `an_unverified_hook_gate_names_the_gates_and_the_way_out`.
- **Q3, cancel check not starved.** `prepare::await_run` and `runner::await_gates` use one
  `tokio::time::interval` (`MissedTickBehavior::Delay`) created before the loop, not a `sleep` per
  iteration (the former was starved by queued `progress` messages).
- **Q4, `trust::read_member_config`.** Absence is `git ls-tree <head> -- .sluice.toml` printing
  nothing; any git error is an error (`member_config_invalid` with the git error), never "no gates".
  Test `an_absent_file_is_no_config_but_a_git_error_is_an_error`.
- **Q5, `await_gates` JoinError.** A panicked receiver task is mapped (task id to gate index) to that
  gate's `error` outcome with a failure detail, so the train fails instead of going ready with a gate
  "running". Not unit-tested: a panic in the receiver task cannot be provoked from the public API.
