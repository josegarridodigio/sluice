---
title: "Queue trust W3: the daemon vouches for its own push"
tags: [sluice, changes, merge-queue, hooks, extension]
---

# Queue trust W3: the daemon vouches for its own push

Decision: [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]] section 3
(GitHub issue #4). Plan: [[sluice/plan/queue-trust-plan.md]] wave W3. Design amended:
[[sluice/design/merge-queue.md]] sections 5 (step 5), 6, 11, 12 and 14.

## What changed

- **Extension API.** `Extension::vouch_run(&RunRequest, peer_pid: Option<u32>) ->
  BoxFuture<Option<Vouch>>` (default `None`) and `pub enum Vouch { Verified { train_id,
  job_id }, Refused { message } }`, exported from `sluice_gate_daemon::server`. `handle_run`
  consults the enabled extensions in order, **only when `request.hook.is_some()`** (a train
  push hook always has `--hook`; `verify` and plain runs pay nothing), before `register`,
  the scheduler and the marker double-check. The first `Some` wins.
- **Reply shape.** One `done` and nothing else (no `queued`, no `started`): `Verified` is
  `OutcomeStatus::Cached`, `exit_code 0`, `marker_written false`, message `<gate>: verified
  by merge train <id> (job <job>)`; `Refused` is `OutcomeStatus::Error` with the message. The
  client (`proto::client::Client::run`) loops on `recv()` until `done` and only uses `queued`
  / `started` for its progress lines, so a direct `done` is accepted: `Cached` prints
  `PASS (cached)` and exits 0, `Error` goes through `fail(message)` (exit 1). Nothing is
  registered: no job row, no request row, no totals. The daemon logs `push hook vouched:
  train=.. gate=.. verdict=..` at `info`. No wire change.
- **Client output.** For `done{cached}` the client now also prints the outcome message on
  stderr (`sluice: <gate>: verified by merge train ...`), so the hook's output says why
  nothing ran (it also shows the existing "a pass appeared while queued" message).
- **Queue implementation** (`crates/sluice-gate-queue/src/vouch.rs`, called from
  `QueueExtension::vouch_run`): (a) a `pushing` train whose canonical worktree is the
  request's `toplevel` and canonical repo the `common_dir`; (b) the peer descends from the
  marker's `push_pid` (`descends_from` over `/proc/<pid>/stat`, `parse_ppid` after the last
  `)`, at most 64 steps, stops at pid 1) and `executor::process_matches(push_pid,
  push_start_ticks)` holds; a push pid not recorded yet is polled for up to 2 s (it is
  written right after the spawn); (c) off the async threads, `git rev-parse HEAD == S` and
  no tracked change (`status --porcelain --untracked-files=no`, the pre-push checks' rule;
  a first version compared a full snapshot with `T`, which refused a push whose gates had
  left untracked output, corrected in review); a different HEAD or a tracked change is `Refused` (`the merge worktree of train <id> no longer holds the verified
  tree ...`); (d) `judge`: the train's gate view named `request.gate.name` with status `pass`
  or `cached` is `Verified`, else `Refused` (`gate '<g>' runs in the pre-push hook but merge
  train <id> did not verify it: ...`, naming `.sluice.toml` changes by a member when
  `config_changed`, otherwise hinting `[queue] gates` / the `pre-push` hook). No train,
  not a descendant, an inconclusive worktree check (a git error) or
  a database error is `None` (normal path; the last two log a `warn`).
- **The refusal fails the push definitively.** The hook exits non-zero at once; `git push`
  prints its output and `error: failed to push some refs`; `push::classify` (exit 1, no
  porcelain ref line, no `fatal:`, no other `error:` line: the client's lines start with
  `sluice:`) gives `HookDeclined`, so the jobs are `failed` with reason `pre_push_hook` and
  the hook output in `detail.output`, in seconds, never `push_timeout`.
- **`select_gates`.** `[queue] gates` set: that list (as written) then the gates whose hooks
  contain `pre-push` (by name), no duplicates; not set: the `pre-push` gates; none: every
  gate (unchanged). Unknown names in the list are still ignored.

## Files and symbols

- `crates/sluice-gate-daemon/src/server/extension.rs`: `Extension::vouch_run`, `Vouch`
- `crates/sluice-gate-daemon/src/server.rs`: re-export, `handle_run` (new `peer_pid`
  parameter), `ask_extensions`, `answer_vouch`
- `crates/sluice-gate-queue/src/vouch.rs` (new), `lib.rs`, `extension.rs`
  (`QueueExtension::vouch_run`), `runner.rs` (`select_gates`)
- `crates/sluice-gate/src/run.rs`: `report_outcome` (message of a cached outcome)
- `crates/sluice-gate-core/src/config.rs`: doc comment of `QueueConfig::gates`
- Docs: `kb/sluice/design/merge-queue.md` (sections 5, 6, 11, 12, 14),
  `docs/merge-queue.md`, `docs/configuration.md`

## Why

ADR-0018 context: the hook of the train's push re-derived the cache key in a different
environment (`PATH`-dependent `key_inputs`, `CI`), missed, and re-ran the suite inside
`push_timeout`. The daemon now states a fact about its own locked worktree (it holds
exactly the verified `S`/`T`, the asker descends from the push it spawned) and answers from
the train's record; it cannot be used to skip a gate anywhere else.

## Verification

`cargo check --workspace --all-targets` clean; `cargo fmt` applied. New tests:

- unit (`vouch.rs`): `ppid_is_read_after_the_last_parenthesis`,
  `the_walk_follows_parents_stops_at_init_and_is_bounded`,
  `the_real_process_tree_is_walked` (parent chain of the test process, a child, a
  non-ancestor), `a_gate_the_train_verified_is_vouched_with_its_job`,
  `an_unverified_gate_is_refused_with_an_actionable_message`; `runner.rs`:
  `queue_gates_are_extended_with_the_pre_push_gates`.
- daemon integration (`crates/sluice-gate-daemon/tests/extension.rs`):
  `an_extension_can_vouch_for_a_hook_run` (direct `done`, cached/error shapes, peer pid,
  extension not asked without a hook, unclaimed hook run goes the normal way).
- end to end with the real binary (`crates/sluice-gate/tests/queue_e2e.rs`):
  `the_pre_push_hook_of_the_trains_push_is_answered_from_the_train` (hook runs the real
  `sluice run --hook pre-push test`; `key_inputs = ["tool --version"]` with a different `tool`
  first in the approver's `PATH` and `CI=1`: the train lands, the counter shows one
  execution, the hook output says `verified by merge train`),
  `a_hook_gate_the_train_did_not_verify_fails_the_push_at_once` (member adds a pre-push gate
  to `.sluice.toml`: `failed`, `pre_push_hook`, well under `push_timeout = 60s`, message in
  `detail.output`, the gate never ran), `a_run_in_the_merge_worktree_outside_the_push_is_not_vouched`
  (the push is held in a hook; a `sluice run --hook pre-push` with the cache off, started from
  the merge worktree by the test, runs the gate and gets no vouch).

The full suite, clippy and fmt check run once after the last wave through Sluice `verify`.
