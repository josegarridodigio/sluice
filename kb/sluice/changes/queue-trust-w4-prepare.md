---
title: "Queue trust W4: [queue] prepare for the merge worktree"
tags: [sluice, changes, merge-queue, runner, init]
---

# Queue trust W4: `[queue] prepare` for the merge worktree

Decision: [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]] section 5
(GitHub issue #3). Plan: [[sluice/plan/queue-trust-plan.md]] wave W4. Design amended:
[[sluice/design/merge-queue.md]] sections 3, 4, 5 (new step 4b), 7 and 11,
[[sluice/design/config-format.md]].

## What changed

- **Config.** `[queue]` gets `prepare` (string, not empty after trimming), `prepare_timeout`
  (duration, default `10m`, must be > 0) and `prepare_weight` (a gate's `weight`: integer
  >= 1 or `"auto"`, default `"auto"`), validated at parse like every `[queue]` key. Older
  binaries reject the file (`deny_unknown_fields`); `docs/configuration.md` says "needs
  0.6.0". Unreleased fixture `tests/fixtures/unreleased/queue-prepare.toml` and
  `queue_prepare_fixture_parses` (like `auto-weight.toml`).
- **Runner (`build_inner`).** After the merges and the snapshot assertion, a base with
  `prepare`: (a) derive keys and look them up as before; all hit, or no gate: skip the
  prepare (fast path). A key derivation that fails here just means "prepare first". (b)
  Otherwise submit the prepare through `submit_run_owned` (`queue:prepare`, key/tree/committed
  tree none, `no_marker`, `cache = false`, weight/timeout from the config, timeout never 0,
  cwd = merge worktree, daemon env, caller human, session `queue:<train>`) and await it in
  `prepare::await_run` (a tick-driven check for a cancelled member; dropping the
  subscription, also on daemon stop, kills the job). (c) After a pass the worktree must snapshot
  to `T` again; a different tree is `dirty` (`git status --porcelain` paths). (d) Keys are derived
  after the prepare (`derive_views`, extracted from the old inline loop; a key input that fails
  is the existing `key_input_failed` path). Without `prepare` the behaviour is the old one.
- **Attribution (`runner::attribute`, `prepare::probe_base`).** A failure (`fail`, `timeout`,
  `killed`, `error`, or `dirty`) is probed on the base: `git worktree add --detach
  <state>/merge/<train>-base <base_sha>`, locked, hooks disabled (the `Git` wrapper), the same
  command with the same limits through the scheduler, cleanliness checked, worktree removed.
  The result is not remembered (see "Review correction").
  Base fails too: `requeue_unprepared` dissolves the train (not split) and
  `record_queued_failure_with` puts every member back to `queued` with `prepare_failed`
  (`error`, `job_id`, `status`, `exit_code`, `log_path`, `message`, `base: true`, `base_job_id`,
  `failures`), paced by `is_base_failure`, `failed` after `MAX_PREPARE_FAILURES`; the count
  spans attempts (`claim` keeps the reason, `clear_key_failures`, now for `prepare_failed`
  too, clears it once the keys derive). Base prepares: a `Failure` with reason `prepare_failed`
  (or `prepare_dirty` with `paths`), `base: false`, through the W2 `finish_train`: single
  member `failed`, several split.
- **Sweep.** `sweep_merge_dir` and the finished-train housekeeping know `<train>-base`
  (`prepare::probe_path`): owned by its train, kept while the train is active or busy,
  removed otherwise.
- **Store.** Nullable `queue_trains.prepare` JSON column added by `store::add_column_if_missing`
  (`PRAGMA table_info`, then `ALTER TABLE`) in `init`; no `queue_schema` bump. `TrainRow.prepare`,
  `TrainEdit.prepare`, `Writer::note_queued_failure_with` (extra detail keys; `prepare_failed`
  counts as a base reason). Not part of the train hash.
- **Proto, CLI, MCP.** `TrainPrepare {status, job_id?, duration_ms?, log_path?}` and
  `QueueTrainView.prepare` (`default`, `skip_serializing_if`), mirrored by `PrepareSchema` in the
  MCP schema. CLI `queue status` and the approval plan print `prepare: pass 41.2s` /
  `running (job ...)` / `fail (job ...)`; MCP the same line. `failure_text` / `detail_error`
  show a prepare failure's `message`, and `prepare_dirty` its paths. `waiting:` needs nothing
  new: the generic queued-reason line already shows `prepare_failed`.
- **`sluice init`.** Detects the prepare command at the root (`detect::node_prepare`,
  `python_prepare`; `Stack.prepare`) and writes a `[queue]` table with a comment, before the gates
  (a carried gate table would otherwise swallow it on `--force`): pnpm, npm, yarn (berry with
  `.yarnrc.yml`), bun, uv, poetry; nothing for cargo, go, a Node project without a lockfile or
  unknown stacks. Several detected stacks are joined with ` && `.

## Files and symbols

- `crates/sluice-gate-core/src/config.rs`: `QueueConfig`, `DEFAULT_PREPARE_TIMEOUT`, `validate`
- `crates/sluice-gate-queue/src/prepare.rs` (new): `Spec`, `Run`, `End`, `Probe`, `submit`,
  `await_run`, `dirty_paths`, `probe_base`, `probe_path`
- `crates/sluice-gate-queue/src/runner.rs`: `build_inner`, `snapshot_of`,
  `derive_views`, `prepare_train`, `store_prepare`, `member_cancelled`, `attribute`,
  `requeue_unprepared`, `record_queued_failure_with`, `clear_key_failures`, `claim`,
  `is_base_failure`, `sweep_merge_dir`, `step`
- `crates/sluice-gate-queue/src/store.rs`: `add_column_if_missing`, `TrainRow.prepare`,
  `TrainEdit.prepare`, `note_queued_failure_with`; `model.rs` trigger strings; `extension.rs` `train_view`
- `crates/sluice-gate-proto/src/lib.rs`: `TrainPrepare`, `QueueTrainView.prepare`
- `crates/sluice-gate-mcp/src/{queue,lib}.rs`, `crates/sluice-gate/src/queue.rs`: rendering
- `crates/sluice-gate/src/init/{detect,render,mod}.rs`
- Docs: `kb/sluice/design/merge-queue.md`, `kb/sluice/design/config-format.md`,
  `docs/merge-queue.md` ("Preparing the merge worktree"), `docs/configuration.md`, `docs/cli.md`

## New reasons, fields, column

- Job reasons `prepare_failed` (detail as above) and `prepare_dirty` (`paths`).
- `QueueTrainView.prepare` (optional), `queue_trains.prepare` (nullable), job name `queue:prepare`.

## Edge cases and decisions

- A pass that left only git-ignored files is clean; `dirty` also covers a prepare that edits a
  tracked file (the snapshot differs).
- A prepare that dirties the base as well (deterministic) is an environment failure: requeued
  with `prepare_failed` and the paths, not `prepare_dirty`.
- No probe is remembered: each prepare failure costs one probe more, the price of never
  blaming a member for an environment failure. A probe that cannot be set up (worktree creation fails)
  requeues the train as an environment failure (nothing to attribute by).
- No gate selected: the fast path always holds, so such a base never prepares.
- The learned `auto` weight of `queue:prepare` is keyed by (repo, name, command) like any gate's
  and never collides with a gate (names match `[a-z0-9][a-z0-9_-]*`).
- A cancelled member while the probe runs dissolves the train as during the main prepare.

## Verification

`cargo check --workspace --all-targets` clean; `cargo fmt` applied. New tests: core
(`queue_prepare_defaults_and_validation`, `queue_prepare_fixture_parses`); proto
(`the_prepare_view_is_optional_on_the_wire`); queue unit (`prepare.rs` spec and run text; `store.rs`
`the_prepare_column_is_added_in_place_and_idempotently`,
`the_prepare_view_round_trips_and_stays_when_the_state_changes`,
`prepare_failures_are_counted_with_their_extra_detail`; `runner.rs`
`a_passing_prepare_clears_the_failure_remark_and_keeps_config_changed`,
`a_prepare_failure_splits_a_pair_like_a_gate_failure`); queue integration (`tests/queue.rs`:
`prepare_provides_the_ignored_files_the_gates_need`,
`a_prepare_failing_on_the_base_requeues_the_members_then_fails_them` (one probe, three trains,
never solo), `a_prepare_failing_only_with_a_member_splits_the_train`,
`a_prepare_that_dirties_the_tree_is_reported_with_the_paths`,
`a_train_whose_gates_all_hit_skips_the_prepare`, `cancelling_a_member_kills_a_running_prepare`);
init (`detect.rs` `the_prepare_command_follows_the_lockfile`,
`cargo_go_and_unknown_stacks_need_no_prepare`; `tests/init.rs` pnpm, yarn classic, cargo and go
fixtures). The full suite, clippy and fmt check run once after the last wave through Sluice `verify`.

## Review correction

The base-probe memo is removed (`Core.probes`, `probe_known`, `probe_remember`, `ProbeKey`).
A pass remembered per (repo, base sha, command) blamed members for later environment
failures on the same base: a registry that went down, a timeout or a memory kill after the
base had once prepared fine looked like the member's fault and failed it. Every prepare
failure now probes the base afresh (`prepare::probe_base` no longer looks anything up).
`a_prepare_failing_on_the_base_requeues_the_members_then_fails_them` already counted one
probe per attempt and now says why. Docs: ADR-0018 section 5,
`kb/sluice/design/merge-queue.md` step 4b and failure modes, `docs/merge-queue.md`.
