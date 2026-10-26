---
title: "Queue trust W2: real train results, corrections and observability"
tags: [sluice, changes, merge-queue, runner]
---

# Queue trust W2: real train results, corrections and observability

Decision: [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]] sections 4 and 7.
Plan: [[sluice/plan/queue-trust-plan.md]] wave W2. Design amended:
[[sluice/design/merge-queue.md]] sections 3, 5 (steps 2, 5, 6, 7), 6 and 7.

## What changed

- **A passing gate must leave the tree alone (`gate_modified_tree`).** After every gate
  that had to run passed, the runner runs `git status --porcelain --untracked-files=no`
  in the merge worktree (queue `Git` wrapper). A non-empty answer fails the train with
  reason `gate_modified_tree`, detail `{"gates": [gates that ran], "paths": [...]}`,
  handled exactly like a gate failure: a single surviving member is `failed`, several
  split into solo trains. Before, the pre-push check dissolved such a train and the
  rebuild repeated it forever.
- **A pass without a marker is said.** When a gate's outcome has `marker_written = false`
  and a `not_cached_reason`, the train's gate view gets the new optional
  `TrainGate.note = "not cached: <reason>"` (proto, `skip_serializing_if`; mirrored in the
  MCP `GateSchema`). CLI `queue status` / plan and MCP text render `pass (not cached: ...)`.
  The note is not part of the train hash (`finish_train` and `current_hash` hash only the
  gate names and keys; unit test `a_gate_note_does_not_change_the_train_hash`).
- **Train gate jobs die with their subscription.** `DaemonHandle::submit_run_owned` returns
  a `RunSubscription` with `kill_on_drop`: its `Drop` calls `detach_with(.., force = true)`,
  so the last subscriber going away cancels the job even under
  `last_subscriber_gone = "finish"`. No wire change. Used for the train's gates.
- **`config_changed` is three-dot and is kept.** `git diff --quiet <base>...<head> --
  .sluice.toml` (merge-base of base and head against head: what the member changed since
  it forked). The flag is merged into the job detail with the new `runner::merge_detail`
  (the Merging to Verifying transition, and the gate / modified-tree failure, no longer
  overwrite each other). Rendered as `changes .sluice.toml (this train runs the base's
  gates)` on the job line of CLI `queue status` and of the approval plan (`render_plan`
  includes the job lines), and in MCP `job_line`.
- **`target::defaults_at` returns a `Result`.** An existing but unparsable `.sluice.toml`
  on the enqueued head is `TargetError` code `invalid_config`; `enqueue` without a base
  answers `bad-request` with a message starting `invalid_config:`. With an explicit base the
  head's config is not needed (built-in defaults, as before); an absent file still gives
  defaults.
- **`TrainSettings` has real defaults.** Hand-written `Default` (10 min push timeout,
  `audit_ref = true`, 60 s network timeout from `QueueConfig::default()`) and
  `#[serde(default)]` on the struct; `store::from_json` logs `tracing::warn!` when a
  stored column does not parse (it used to fall back to zeros silently, a 1 ms push timeout).
- **The `push_timeout` note carries the output.** After `terminate_group` the readers of
  the push's pipes get a bounded 2 s to drain (`push::drained`); `PushEnd::TimedOut` now
  holds `stdout`/`stderr` and `push::timeout_note` builds `push_timeout: <last 20 lines of
  stderr, else stdout>` (at most 4000 characters; plain `push_timeout` when nothing was
  printed). It is stored as the train's `needs_reconcile` note like any ambiguous end.
- **`key_input_failed`.** A `derive_key` error in the merge worktree no longer fails every
  member with `internal`: the train is dissolved (`Writer::dissolve`, not split) and the
  members go back to `queued` through `record_queued_failure` / `note_queued_failure` with
  reason `key_input_failed`, detail `{"error": "gate <name>: <error>", "failures": n}`,
  paced by `is_base_failure`, `failed` after `MAX_PREPARE_FAILURES`. Because a claim
  clears reason and detail, `claim` keeps both for a job whose reason is
  `key_input_failed` so the count spans the attempts; `clear_key_failures` drops them once
  the keys derived (new `Writer::set_job_remark`).

## Files and symbols

- `crates/sluice-gate-proto/src/lib.rs`: `TrainGate.note`
- `crates/sluice-gate-daemon/src/server.rs`: `detach_because`, new `detach_with`
- `crates/sluice-gate-daemon/src/server/extension.rs`: `DaemonHandle::submit_run_owned`,
  `RunSubscription::kill_on_drop`, `Drop`
- `crates/sluice-gate-queue/src/runner.rs`: `Failure`, `merge_detail`, `modified_tree`,
  `key_input_failed`, `clear_key_failures`, `record_queued_failure`, `is_base_failure`,
  `claim`, `build_inner`, `await_gates`, `finish_train`, `merge_members`, `push_train`
- `crates/sluice-gate-queue/src/store.rs`: `TrainSettings` (`Default`, `serde(default)`),
  `from_json`, `Writer::set_job_remark`, `note_queued_failure`
- `crates/sluice-gate-queue/src/push.rs`: `PushEnd::TimedOut { stdout, stderr }`, `drained`,
  `timeout_note`
- `crates/sluice-gate-queue/src/target.rs`: `defaults_at`; `extension.rs`: `enqueue`
- `crates/sluice-gate-queue/src/model.rs`: transition trigger strings
- `crates/sluice-gate/src/queue.rs`, `crates/sluice-gate-mcp/src/queue.rs`: rendering
- Tests: `crates/sluice-gate-queue/tests/{queue,target,common/mod}.rs`; unit tests in
  `runner.rs`, `store.rs`, `push.rs`, and the two rendering modules
- Docs: `kb/sluice/design/merge-queue.md` sections 3, 5, 6, 7; `docs/merge-queue.md`
  (status notes, reasons table, `invalid_config`, `push_timeout` note)

## New reasons and fields

- Job reasons: `gate_modified_tree` (detail `gates`, `paths`), `key_input_failed` (detail
  `error`, `failures`). Enqueue error: `bad-request` `invalid_config: ...`.
- `TrainGate.note` (optional). Job `detail.config_changed` (three-dot) now survives gate
  failures.

## Why

ADR-0018 context, "Latent bugs found on the way": a `pass` was treated as verified without
looking at the marker or the tree, `config_changed` was wrong and invisible, an invalid head
config silently picked the wrong target, zero settings meant a 1 ms push timeout, the
hook's output was thrown away, a failing key input failed every member as `internal`, and a
train's aborted gates kept running in a worktree being removed under `finish`.

## Verification

`cargo check --workspace --all-targets` clean; `cargo fmt` applied. New tests: unit tests
(`from_json` defaults, `merge_detail`, `finish_train` with `gate_modified_tree` for one
member and for a pair, gate note outside the hash, `timeout_note`, CLI/MCP rendering) and
integration tests in `tests/queue.rs` (`a_gate_that_edits_a_tracked_file_fails_the_train`,
`a_modified_tree_splits_the_train_then_fails_each_member`,
`config_changed_ignores_what_the_base_changed`, `a_gate_failure_keeps_config_changed`,
`the_push_timeout_note_carries_the_hooks_output`,
`failing_key_inputs_requeue_then_fail_after_three_attempts`,
`a_train_kills_its_other_gates_under_the_finish_policy`, plus the note assertion in
`cancelling_a_conflict_keeps_the_ready_train` and the `invalid_config` branch of
`option_shaped_remotes_never_run`) and `tests/target.rs`
(`defaults_at_reports_an_unparsable_config_instead_of_hiding_it`). The full suite, clippy and
fmt check run once after the last wave through Sluice `verify`.
