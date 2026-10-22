---
title: "Change: Phase 6, the merge queue crate (sluice-gate-queue)"
tags: [sluice, change, phase-6, merge-queue]
---

# Change: Phase 6, the merge queue crate (2026-10-22)

Core of the local merge queue ([[sluice/design/merge-queue.md]] sections 2
to 7, 10, 11, 14), built on the daemon extension point
([[sluice/changes/phase-6-daemon-extension-point.md]]). The CLI
(`sluice queue …`) and the MCP tools are separate changes.

## What changed

| Where | What |
|---|---|
| new crate `crates/sluice-gate-queue` | `model` (`JOB_TRANSITIONS`, `TRAIN_TRANSITIONS`, `job_transition_allowed`, `train_transition_allowed`, state spellings, `TrainHashInput`, `train_hash`); `store` (`init` with `queue_schema` v1, `JobRow`, `TrainRow`, `MarkerRow`, `TrainSettings`, `Writer` (fenced by `LeaseFence::check`; `insert_job`, `job_to`, `train_to`, `dissolve`, `insert_train`, `insert_marker`, `set_marker_pid`, `delete_marker`, `request_cancel`, `adopt_trains`), `Refusal` + `refuse`/`refusal`, reads `job`, `jobs`, `jobs_in_state`, `train`, `trains`, `train_jobs`, `marker`, `position`, `events`); `target` (`resolve`, `Target`, `TargetError`, `defaults_at`, `endpoint_hash`, `strip_userinfo`); `git` (`Git` runner, `ENV_ALLOWLIST`, `allowlisted`, `worktree_git_dir`); `push` (`push_args`, `spawn`, `PushProcess::wait`, `PushEnd`, `classify`, `PushVerdict`, `land_local`); `reconcile` (`ReconcileInput`, `decide`, `Decision`, `TrainFacts`, `gather`); `runner` (`Core`, `run`, build, gates, push orchestration, `read_config`); `extension` (`QueueExtension`, `QueueOptions`, `on_start` recovery and merge-dir sweep, request handlers, `status_summary`). |
| workspace `Cargo.toml` | member and workspace dependency `sluice-gate-queue`. |
| `sluice-gate-daemon` `server/extension.rs` | `DaemonHandle::store_tx_durable` (one transaction committed with `synchronous = FULL`). |
| `sluice-gate-core` `git.rs` | `git::command` (builds a `git` `Command`, for the push spawned in its own process group) and `git::run_output` (raw output whatever the status). |
| `sluice-gate` `daemon_cmd.rs`, `Cargo.toml` | `extensions: vec![Arc::new(QueueExtension::new(QueueOptions::default()))]`. |
| KB | [[sluice/design/merge-queue.md]] updated to the implementation (sections 2–6, 10, 12, new 15); [[sluice/design/architecture.md]] crate layout, `store_tx_durable` in 5.5, new 5.6. |

## Deviations from the design, and why

- **Durable marker commit**: SQLite refuses `PRAGMA synchronous` inside a
  transaction, so `store_tx` cannot do it; added
  `DaemonHandle::store_tx_durable` instead of fsyncing by hand.
- **Extra columns** (`queue_schema` stays v1, tables are new):
  `queue_jobs.solo` (the split flag of step 7), `queue_jobs.env` (the
  enqueuer's allowlist, for fetches and `ls-remote`),
  `queue_trains.members` (claimed members in order, for the view and the
  hash), `queue_trains.settings` (`push_timeout`, `audit_ref` of the base).
- **Logical refusals** travel as `rusqlite::Error::ToSqlConversionFailure`
  wrapping `store::Refusal`, so `store_tx` rolls back; `store::refusal`
  recovers them and the extension maps them to `not-found`,
  `conflict-state`, `duplicate-branch`, `stale-plan`, `not-allowed`,
  `internal`.
- **Extra transitions** (all recorded in `model.rs` and section 3):
  `merging → failed` (internal error), `approved → failed`
  (`target_checked_out` at landing), `pushing → cancelled` (definitive
  rejection with a pending cancel); `needs_reconcile` jobs, like `pushing`,
  only record `cancel_requested`.
- **Default target**: `[queue] remote`/`target` are read from the enqueued
  head's `.sluice.toml` (only to pick the default target; gates still come
  from the base). The design did not say where the defaults come from
  before a base exists.
- **Own branch** is refused for remote targets too (target branch name ==
  job branch).
- **Endpoint changed at push time** dissolves the train (rebuilt and
  re-approved against the new URL) instead of a bare refusal; reconcile
  still refuses `endpoint_changed`.
- **Hook-declined detection** is a heuristic on Git's output (exit 1, no
  ref line, no `fatal:`, `failed to push some refs`); `LC_ALL=C` is forced
  for every queue git call so the parsing is locale independent.
  Permission denied before any ref line (SSH auth) counts as a definitive
  policy rejection.
- **Merge identity fallback** `Sluice merge queue <sluice@localhost>` when
  git has no committer identity (the daemon may run without one).
- **`enabled = false`**: `welcome.features` still lists `queue` (the trait's
  `features()` is static); requests get `not-supported` naming the setting.
- **Base without `.sluice.toml`**: no gates. Invalid config or failed fetch:
  see "Base failures are recorded" below (originally: jobs stayed `queued`,
  logged only, retried silently every tick).
- **Reconcile row 7** cancels the members with a pending cancel and
  re-queues the others.

## Verification

- Unit tests (32): transition tables, state spellings vs serde, train hash
  determinism and sensitivity (every field, member order, gate key order
  irrelevant), store schema idempotence and fail-closed on a newer
  `queue_schema`, lease fencing, duplicate branch, CAS refusals,
  `dissolve` with a pending cancel, `claim_token` fencing and `adopt_trains`,
  marker round trip, every reconcile row (1–10, pure), porcelain fixtures
  (success, `stale info`, `non-fast-forward`, `[remote rejected]`,
  permission denied, pre-push hook declined, network failure, truncated,
  signal, stale audit ref), push arguments, env allowlist, userinfo
  stripping.
- `tests/target.rs` (4): remote-tracking, local, bare name (remote first),
  `refs/…` spellings, default via `origin/HEAD` and `[queue] target`;
  refusals tag, SHA (full and short), `HEAD`, own branch, checked-out branch
  (linked worktree and main worktree), unknown; endpoint hash follows
  `remote set-url`.
- `tests/queue.rs` (11, real daemon via `server::run` + local bare remote,
  ~4.5 s total): conflict + ready + wrong hash `stale-plan` + MCP
  `not-allowed` + approval → `landed`, remote target == S, audit ref
  present, pin ref and merge worktree gone; MCP approval when allowed,
  fast-forward S, `config_changed`, `status.queue`; marked tree → `cached`
  with no run (counter file); base moved → dissolved, rebuilt, old approval
  `stale-plan`; declining `pre-push` → jobs `failed` `pre_push_hook` with
  the hook output; hanging `pre-push` (FIFO) + `push_timeout = "2s"` →
  `needs_reconcile`, reconcile → `ready`, approval voided, pin ref gone;
  marker without a push across a daemon restart → `needs_reconcile` →
  `ready` → re-approved → landed, and the same with a pending cancel →
  `cancelled`; local target lands by `update-ref` with the audit ref and
  refuses a checked-out branch and `HEAD`; a failing member splits the
  train (A lands alone, B `failed` `gate_failed` with log path);
  `enabled = false` → `not-supported`.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace` pass.

## Bugs found by the acceptance run

Found by `crates/sluice-gate/tests/queue_e2e.rs`
([[sluice/changes/phase-6-acceptance.md]]), fixed, regression tests kept.

| Bug | Fix | Files / symbols | Regression test |
|---|---|---|---|
| After the daemon is killed with SIGKILL (acceptance test 2) no daemon could start for 90 s: the dead daemon's lease row was unexpired and the start refused any other owner (`AlreadyRunning`), so every CLI call failed with "the daemon could not be started", and a parked push could not be reconciled until the TTL ran out. | The starting daemon, which already holds the `flock`, frees an unexpired lease whose `uid:pid` owner is this user's and no longer exists or is a zombie; a live owner (or another user's, or an unparsable owner) is never touched. | `sluice-gate-daemon` `server.rs`: `take_lease`, `reclaim_dead_lease`, `process_is_gone`; KB [[sluice/design/architecture.md]] (single instance). | `tests/daemon.rs` `lease_of_a_dead_daemon_is_reclaimed_on_start` (refused while the owner lives, started once it is reaped); `queue_e2e.rs` `acceptance_2a_…`, `acceptance_2b_…`. |
| `sluice queue approve` reported a train dissolved at push time without the reason ("was dissolved; its jobs were re-queued"), so `endpoint_changed` was invisible to the human. | The message carries the train note: `was dissolved (endpoint_changed); …`. | `sluice-gate` `queue.rs` `report_final`. | `queue_e2e.rs` `acceptance_7a_endpoint_changed_never_pushes`. |

## Base failures are recorded (lead request, after the acceptance run)

Before: when step 2 failed (fetch, local branch, endpoint, or invalid
`.sluice.toml` on the base), the jobs stayed `queued` with nothing visible
and were retried silently every tick, forever. Now:

- Every `queued` job of the group records `reason = base_unavailable` or
  `base_config_invalid` and `detail = {"error": <git or config error>,
  "failures": n}` without a state change (`store::Writer::note_queued_failure`,
  one `queue_events` row per attempt, `from = to = queued`).
- `n` counts consecutive failures (both reasons count; a claim or a
  `queue retry` clears reason and detail). At `runner::MAX_PREPARE_FAILURES`
  (3) the jobs go `queued → failed` (new transition in `JOB_TRANSITIONS` and
  design section 3) with the same reason and detail; `queue retry` re-queues.
- Attempts are paced to one per `[queue] tick` for the group (every queue
  request, `queue status` included, wakes the runner, so without pacing a
  polling client would exhaust the three attempts in a fraction of a second).
- `runner::prepare` returns `PrepareError` (`Unavailable`, `ConfigInvalid`)
  instead of `anyhow::Error`; `record_unprepared`, `is_base_failure`, `tick_ms`.
- Display: CLI `queue status` prints `waiting: <reason> <first error line>`
  on queued lines (also inside a train block) and the error on `FAILED`
  lines (`failure_text` reads `detail.error`); MCP `render_status` prints
  `waiting: <reason>: <error>` and the error after a failed job's reason.
- Tests: `store` unit test
  `base_failures_are_counted_on_queued_jobs_and_reset_by_a_claim`; CLI unit
  `a_queued_job_whose_base_cannot_be_prepared_says_why`; MCP unit
  `status_text_says_why_a_queued_job_waits`; end to end
  `unreachable_base_is_reported_then_fails_after_three_attempts` (remote URL
  that does not exist; retry after fixing the URL builds the train) and
  `invalid_base_config_is_reported_then_fails_after_three_attempts`.
- Design: [[sluice/design/merge-queue.md]] sections 3, 5 (step 2) and 7.

## Open points

- `[queue] allow_agent_approval` in `.sluice.toml` is a config **error**
  today (`QueueConfig` has `deny_unknown_fields`), while section 11 says it
  is ignored with a warning; left for the config owner.
- Not covered by tests: reconcile rows 5/6 against a real remote. Now
  covered end to end ([[sluice/changes/phase-6-acceptance.md]]):
  `endpoint_changed` (push time and reconcile row 2), lease loss before a
  push, SIGKILL of the daemon while a push blocks.
- The daemon reads the user config once at start: enabling
  `allow_agent_approval` needs `sluice daemon stop` (the MCP server reads it
  on every call, so until the restart the MCP check passes and the daemon
  refuses).
- ~~`queue status` keeps every train of the repository, dissolved and landed
  ones included, and renders their members with their *current* state next
  to that train's gates.~~ Fixed in "Review fixes" (superseded dissolved
  trains are hidden in the human view).

## Review fixes (2026-10-22)

Findings of the adversarial review of the merge queue, each with its fix
and regression test. Design updated in [[sluice/design/merge-queue.md]]
(sections 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 15),
[[sluice/design/config-format.md]], [[sluice/design/protocol.md]],
[[sluice/design/mcp-tools.md]], `docs/merge-queue.md`, `docs/agents.md`.
Verified with `sluice run fmt`, `sluice run clippy`, `sluice run test`
(all PASS).

| # | Finding | Fix (files, symbols) | Test |
|---|---|---|---|
| 1 | No network timeout; prepare/watch awaited inline in `step` | `sluice-gate-core` `git::run_output_deadline` (own process group, SIGTERM then SIGKILL after 2 s, reaped; `Error::GitTimeout`); queue `Git::net`, `Git::fetch`, `Git::ls_remote`, `Git::remote_head` with `Git::with_network_timeout`; `[queue] network_timeout` (`QueueConfig.network_timeout`, `DEFAULT_NETWORK_TIMEOUT` 60 s) stored in `TrainSettings.network_timeout_ms`; the base fetch's deadline comes from a trusted commit of the target (`runner::trusted_network_timeout`); `runner::claim_next` spawns one task per group (`prepare_and_claim` + `build`, busy key `group:…`), `step` spawns ready watches (at most once per tick, `Core::due_for_watch`) and pushes as tasks; enqueue resolution uses the head's `network_timeout` | core `git::tests::deadline_kills_a_stalled_transport_group`; queue `a_stalled_remote_times_out_without_blocking_other_repositories` (`core.sshCommand = "sleep 600 #"`, another repository's train is approved and lands meanwhile); config `queue_section_defaults_and_validation` |
| 2 | `[queue] remote` reached git as an option | `config::invalid_remote_name` in `RepoConfig::validate` (no empty, leading `-`, whitespace/control); `target::check_remote` (`invalid_remote`: must be a configured remote) on the default path; `Git::push_url` re-checks; `--` before remote/URL in `fetch`, `ls-remote`, `remote get-url`, `push`; `--end-of-options` in `rev-parse` | `tests/target.rs` `an_option_shaped_remote_is_refused_and_never_runs`; queue `option_shaped_remotes_never_run` (witness file absent); config test above |
| 3 | Endpoint bound to the fetch URL | `Git::push_url` (`git remote get-url --push --all`, several URLs refused); `target::endpoint` (URL + hash) used by `prepare` (fetch from the push URL), `watch_ready` (`bound_url`, `target_at`; a changed endpoint is left to the pre-push check), `pre_push_checks`, `reconcile::gather`/`ancestry` | `tests/target.rs` `endpoint_is_the_push_url` (pushurl, pushInsteadOf, two push URLs); queue `the_push_url_is_the_endpoint` (triangular A/B lands on B with one train; push URL changed after ready → `endpoint_changed`, nothing pushed to B or C) |
| 4 | Reconcile cleaned up after a failed apply | `extension::reconcile`: pin ref and worktree removed only when `apply_decision` committed | queue `a_push_spawn_failure_parks_the_train_and_reconcile_cleans_up_only_on_success` (claim token changed under the train → `refused`, pin ref and worktree kept; then `ready`) |
| 5 | Conflict members kept `train_id` | `Writer::job_to` clears `train_id` on `conflict`/`failed` (audit row keeps the old train) | store `a_conflicting_member_leaves_its_train`; queue `cancelling_a_conflict_keeps_the_ready_train` (cancel B, A stays ready, same hash approves and lands) |
| 6 | Push spawn failure left the train `pushing` | `push_train`: a `push::spawn` error is `PushVerdict::Ambiguous("push_spawn_failed: …")` → `needs_reconcile`; injection point `QueueOptions.push_program` / `Core::with_push_program` / `push::spawn(program, …)` (tests only) | queue `a_push_spawn_failure_parks_the_train_and_reconcile_cleans_up_only_on_success` |
| 7 | MCP dead end after a conflict | MCP tools `queue_retry` (`queue::send_retry` re-reads the branch head) and `queue_cancel` (`queue::send_cancel`), `QueueJobParams`, texts `retry_text`/`cancel_text`; conflict/failed lines name `queue_retry`; `INSTRUCTIONS` updated | MCP units `retry_and_cancel_texts`, `status_text_covers_ready_conflict_and_hint`, `instructions_carry_the_queue_guidance`, `queue_tools_are_listed_with_descriptions_and_output_schemas`, `skipped_fields_are_never_required`; `tests/mcp.rs` `queue_tools_are_listed_with_descriptions_and_instructions`; e2e `queue_e2e.rs` `mcp_retry_and_cancel_after_a_conflict` |
| 8 | CLI `approve` waited forever | `QueueTrainView.push_timeout_ms`/`network_timeout_ms` (optional); CLI `landing_deadline` + `wait_expired_text` (exit 1, hint `sluice queue status`); MCP `approve_wait` replaces the fixed 10 min | CLI unit `landing_wait_is_bounded_by_the_base_timeouts`; MCP unit `approve_wait_follows_the_base_timeouts` |
| 9 | Cancel between the read and the Ready write → `internal` failures | `runner::finish_train` decides step 7 inside the write transaction | runner units `a_cancel_before_the_outcome_requeues_the_others`, `passing_gates_make_the_train_ready_with_a_hash` |
| 10 | `push_env`/job `env` kept forever | `Writer::train_to` clears `push_env` on `landed`/`dissolved`; `Writer::job_to` clears `env` on `landed`/`cancelled` (not on `conflict`/`failed`: `retry` still fetches with it) | store `credentials_are_dropped_when_rows_finish`; queue `cancelling_a_conflict_keeps_the_ready_train` (reads the rows) |
| 11 | `fetch` overwrote `FETCH_HEAD` | `Git::fetch` passes `--no-write-fetch-head` (git ≥ 2.29; Sluice needs 2.31) | queue `cancelling_a_conflict_keeps_the_ready_train` (`FETCH_HEAD` sentinel unchanged) |
| 12 | Every exit-1 without `fatal:` was `pre_push_hook` | `push::classify`: hook-declined only with no git `error:` line besides `failed to push some refs`; `error: src refspec`/`dst refspec`/`unable to push to unqualified destination` → `Rejected` (definitive); other `error:` lines → `Ambiguous` | push units `hook_declined_needs_no_git_error_besides_the_summary`, `a_refused_refspec_is_definitive`, `pre_push_hook_declined` |
| 13 | Untracked gate output dissolved approved trains | `pre_push_checks`: `status --porcelain --untracked-files=no` | queue `cancelling_a_conflict_keeps_the_ready_train` (gate `touch gate-output.txt`, the train lands) |
| 14 | No retention; `step` scanned every train | user `[queue] retention` (default `30d`, `UserQueueConfig.retention`); `Writer::prune` + `store::Pruned`; `runner::maintenance` (prune + `sweep_merge_dir`) at daemon start and every `MAINTENANCE_PERIOD` (1 h); `store::live_trains`, `store::finished_trains_since` (housekeeping window 1 h) | store `retention_prunes_only_finished_rows`; config `user_queue_section` |
| 15 | MCP enqueues audited as `cli` | optional `via` on `EnqueueRequest`, `QueueRetry`, `QueueCancel` (protocol stays 1); `extension::actor_of` | proto `client_round_trip`, `queue_spellings`; queue `cancelling_a_conflict_keeps_the_ready_train`; e2e `mcp_retry_and_cancel_after_a_conflict` (enqueue, retry, cancel all `mcp`) |
| — | `queue status` showed dissolved trains with current member states | CLI `visible_trains`: a dissolved train is hidden once a newer train exists for the same (repo, remote, target); `--json` unchanged | CLI unit `superseded_dissolved_trains_are_hidden` |

Notes:

- The push itself still goes to the remote **name** (`git push … --
  <remote>`), so the repository's `pre-push` hook keeps receiving the remote
  name; the pre-push check verified just before that the name resolves to
  the bound push URL. Fetches and `ls-remote` go to the URL, so remote
  settings keyed by name (`remote.<r>.uploadpack`, `remote.<r>.proxy`) do
  not apply to them.
- A head `.sluice.toml` whose `[queue] remote` is invalid is ignored as a
  whole (built-in defaults, documented behaviour of `defaults_at`), so the
  injection is refused at the config layer; `invalid_remote` is the refusal
  for a valid-looking name that is not a configured remote (or a direct
  caller of `target::resolve`).
