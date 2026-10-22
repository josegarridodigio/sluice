---
title: "Change: fixes from the chaos harness (alpha task X1)"
tags: [sluice, change, daemon, executor, doctor, alpha, x1]
---

# Change: fixes from the chaos harness (2026-10-23)

Task X1 of [[sluice/plan/alpha-plan.md]]. Evidence: the `s4_chaos.py`
failures in [[sluice/changes/alpha-readiness.md]] (section "s3_correctness,
s4_chaos, s6_upgrade"). Design: [[sluice/design/executor.md]] ("Restart
reconciliation", "Daemon death (the gate reaper)", "Unwritable log").

## 1. SIGKILL of the daemon

Root cause. Gates run in their own process groups (`process_group(0)`, so
`killpg` reaches the whole tree on timeout and cancel), and nothing tied
those groups to the daemon's life: no death signal, no supervisor. The
start-up reconciliation (`reconcile_orphans`) was the only cleanup, and it
worked when it ran (reproduced: a restarted 0.2.0 daemon did kill 6 live
orphaned groups); but it only runs when the *next* daemon starts, which
the s4 check (all 18 processes gone within 10 s, no restart) never
reaches, and the harness kills the survivors itself before restarting, so
the restarted daemon found nothing. Reading the code also showed a real
gap in reconciliation: it only signalled a group whose *leader* still
matched its start ticks, so a group whose leader had exited while a
background child kept it alive (a gate that leaves a process behind) was
skipped and survived forever.

Fix.

- `sluice_gate_daemon::reaper` (new): the daemon runs the launcher
  `sluice daemon reaper --detach` (hidden subcommand, `daemon_cmd::reaper`,
  through `/proc/self/exe`) with a pipe on its stdin; the launcher starts
  the reaper (`reaper::detach`, `sluice daemon reaper --daemon <pid>`) and
  exits, so the reaper is reparented and is not part of the daemon's
  process tree (a first version kept it as the daemon's child and the
  s4 census of item 2/cancel, which counts every descendant of the
  daemon, rightly saw it alive). The daemon registers each gate group
  (`Reaper::track` after the spawn, `untrack` after the wait, in
  `server::launch`) and closes the pipe on a clean stop
  (`close_reaper`). The kernel closes the pipe when the daemon dies;
  `reaper::serve` then SIGTERMs every registered group that is still
  ours, waits `KILL_GRACE`, SIGKILLs the rest and exits.
  `DaemonConfig::reaper: Option<ReaperCommand>` (`None` in tests).
  Rationale against `PR_SET_PDEATHSIG` and per-gate supervisors in the
  design note.
- `executor::group_is_ours(pgid, leader_ticks)`: the leader matches its
  start ticks, or the leader is gone and the group still has members (a
  pid number naming a live process group is never reused). A live leader
  with other start ticks (pid reuse) is never signalled.
  `executor::terminate_groups_blocking` terminates several groups with one
  shared grace period.
- `server::reconcile_orphans` uses both and terminates the surviving
  groups together; `Store::close_orphans` (replaces `mark_orphaned`)
  closes running rows of other instances as `killed` and queued ones as
  `orphaned`.
- Clients: `ClientError::is_disconnect`; `sluice run` prints `sluice:
  error: the daemon died while running <gate>; it was killed` (exit 143;
  `… while <gate> was queued; it did not run`, exit 1); `sluice exec`
  prints `the daemon died while running exec '<label>'; it was killed`
  (exit 125).

## 2. Unwritable `logs/` or state directory

The gate never ran: `executor::spawn` returned `cannot create
…/logs/<job>.log` before spawning. Now `sluice_gate_core::logfile::JobLog`
(file, or `Memory` keeping the last 256 KiB, `MEMORY_LOG_CAP`) is the sink
of both paths: `executor::spawn` (gates only; exec spools are still
required), `Finished::{log_error, tail}`, the new optional
`Outcome::log_error` (protocol stays 1), printed once by
`run::report_outcome`; in-process, `run::spawn_and_wait` prints the
warning once and passes the memory tail to `report`. Marker logic is
untouched: a pass marker is written only after exit 0 on an unchanged
tree.

## 3. `sluice doctor`

- `markers`: `fail` when the directory exists and is not writable
  (`nix::unistd::access`, W_OK|X_OK), `warn` with the count and the first
  path of damaged files (`MarkerStore::damaged`: unreadable, not valid
  JSON of their kind, or a key other than the file name; other format
  versions, `.tmp-*` and foreign files are not counted).
- `database` (new): `Store::open_read_only`; an error is `fail` (exit 1)
  with the SQLite message and what to do.

## Files

`crates/sluice-gate-daemon/src/{reaper.rs (new), executor.rs, server.rs,
store.rs, lib.rs}`, `crates/sluice-gate-core/src/{logfile.rs, marker.rs}`,
`crates/sluice-gate-proto/src/{lib.rs, client.rs}`,
`crates/sluice-gate/src/{daemon_cmd.rs, main.rs, run.rs, exec.rs,
doctor.rs}`, tests below, `kb/sluice/design/{executor.md, protocol.md}`,
`docs/debugging.md`.

## Tests

- `reaper::tests::end_of_input_terminates_registered_groups_with_their_children`,
  `a_reused_leader_pid_is_never_signalled`.
- `tests/daemon.rs`: `orphans_are_reconciled_on_start` (now `killed`),
  `a_surviving_group_whose_leader_exited_is_reconciled_too` (plus a
  reused-pid row left alone), `an_unwritable_logs_dir_still_runs_the_gate_with_the_tail_in_memory`.
- `store::tests::orphans_and_newest` (`close_orphans`),
  `logfile::tests::an_unwritable_log_falls_back_to_a_bounded_memory_buffer`,
  `marker::tests::damaged_lists_truncated_garbage_and_misnamed_files`.
- `crates/sluice-gate/tests/chaos.rs` (real binary):
  `a_sigkilled_daemon_takes_its_gates_and_their_children_with_it`,
  `a_restarted_daemon_kills_the_gates_a_dead_daemon_left_behind` (reaper
  SIGKILLed first, so only reconciliation can clean up),
  `an_unwritable_logs_dir_does_not_stop_a_daemon_run`,
  `an_unwritable_logs_dir_does_not_stop_an_in_process_run`,
  `doctor_fails_on_an_unwritable_markers_dir`,
  `doctor_warns_about_damaged_markers`,
  `doctor_fails_on_a_database_the_daemon_cannot_open`.

## Verification

- `scripts/alpha/s4_chaos.py` with the release binary of this change:
  **48/48** in 131 s (before: 43/48). Item 1: all 19 tracked processes
  gone 0.05 s after the daemon's SIGKILL, the 6 clients exit 143 with
  `the daemon died while running long; it was killed`, restart in 0.20 s;
  5/state: PASS with the log error reported once, also on a daemon
  restarted on the read-only state dir; 5/markers and 6/markers: doctor
  `fail` / `warn` (4 damaged files); no other check regressed. A first
  run with the reaper as a child of the daemon scored 47/48 (2/cancel saw
  the reaper alive in the daemon's tree), which led to the detached
  launch.
- `sluice run fmt`, `sluice run clippy`, `sluice run test` from the worktree
  root (see the task report).
