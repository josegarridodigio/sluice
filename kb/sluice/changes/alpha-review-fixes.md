---
title: "Change: fixes of the R1 adversarial review of the alpha"
tags: [sluice, change, alpha, review, guard, scheduler, executor, merge-queue, exec]
---

# Change: fixes of the R1 adversarial review of the alpha (2026-10-23)

Task R2 of the alpha plan: every row of the R1 adversarial review, fixed
(or, for row 13, accepted and documented), each with a regression test.
No protocol field was added; the protocol version stays 1.

| # | Severity | Finding | Fix | Files and symbols | Regression test |
|---|---|---|---|---|---|
| 1 | MEDIUM | `cd ..` was resolved on the canonical path while the shell uses the logical one, so the guard picked the wrong `.sluice.toml` | `cd` targets are joined lexically on the logical cwd (the hook's `cwd`) and only then canonicalised; no rewrite when `CDPATH` is set and the command changes directory, nor when a `cd` argument has a `..` component while the logical and canonical current directories differ | `guard/matcher.rs`: `lexical_join`, `has_parent_component`, `ShellContext` (`from_env`), `plan` (new `shell` argument); `guard/mod.rs`: `respond` | `guard::tests::cd_targets_resolve_on_the_logical_directory`, `guard::tests::cdpath_disables_rewrites_that_change_directory` |
| 2 | MEDIUM | Approval was denied only on a plain-text prefix (guard), and the daemon accepted any CLI approval | Guard: `runs_approve` tokenises, takes the program's base name, looks through `env`, `command`, `builtin`, `exec`, `nohup`, `time`, `setsid`, `sluice exec --`; opaque wrappers (`sh -c`, `eval`, `xargs`, `sudo`, `timeout`, ...) and unparseable commands are denied when the text names `queue` and `approve`. Daemon: a `queue-approve` with `via: cli` whose env is detected as an agent's (`agent::detect` with the daemon's `[agent_detection]` rules) is refused with `not-allowed` unless `[queue] allow_agent_approval = true`. The CLI now adds the set agent-detection variables (`SLUICE_AGENT`, `env_any_set`, `env_any_equals` keys) to the existing `env` map of the request; only the allowlist is stored | `guard/matcher.rs`: `runs_approve`, `words_approve`, `mentions_approve`, `PLAIN_WRAPPERS`, `OPAQUE_WRAPPERS`; `sluice-gate-queue/src/extension.rs`: `agent_approval_refusal`, `approve`; `sluice-gate/src/queue.rs`: `approve_env`, `do_approve` | `guard::tests::approval_is_found_through_quotes_paths_and_wrappers`; `sluice-gate-queue/tests/approve_policy.rs`: `an_agent_cannot_approve_through_the_cli_unless_allowed`, `an_agent_approves_through_the_cli_when_allowed`; `queue::tests::approve_env_carries_the_agent_detection_variables` |
| 3 | MEDIUM | `std::env::vars()` panics on a non-UTF-8 entry (exec, shim, guard and every other CLI entry point) | `sluice_gate_core::config::process_env` (`vars_os`, non-UTF-8 entries skipped) replaces every product use; `sluice exec` checks `process_env_is_utf8` and, when false, runs in-process, where the command inherits the raw environment; a shim then still runs the real binary (directly or through that in-process exec) | `sluice-gate-core/src/config.rs`: `process_env`, `process_env_is_utf8`, `utf8_entries`; `exec.rs`: `exec`; `shim.rs`: `env_map`; `guard/mod.rs`: `run_hook`; `cache.rs`, `daemon_cmd.rs`, `doctor.rs`, `logs.rs`, `main.rs`, `queue.rs`, `run.rs`, `status.rs`, `update.rs`; `sluice-gate-daemon/src/server.rs` | `config::tests::non_utf8_environment_entries_are_skipped_not_fatal`; `tests/exec.rs`: `a_non_utf8_environment_runs_in_process_with_the_raw_environment`; `tests/shim.rs`: `a_non_utf8_environment_still_runs_the_real_binary`; `tests/guard.rs`: `a_non_utf8_environment_still_gets_an_answer` |
| 4 | MEDIUM | Over-admission could use utilisation samples older than the latest admission | The scheduler records `last_start_at` on every admission (nominal, guard, over); `plan` over-admits only when the newest utilisation sample (`Pressure::sampled_at`) is at least one sampler tick (`Policy::sample_tick_ms` = `psi_poll_interval`) after it | `scheduler.rs`: `Policy::sample_tick_ms`, `Pressure::sampled_at`, `Snapshot::last_start_at`, `Scheduler::last_start_at`, `samples_after_last_start`, `plan`, `admit_with`; `sampler.rs`: `Utilisation::sampled_at`; `server.rs`: `admit`, `policy` | `scheduler::tests::over_admission_waits_for_a_sample_taken_after_the_latest_start` (and `scheduler_counts_over_admissions_and_drains_back` updated) |
| 5 | MEDIUM | Fast failures taught low learned weights | Choice: learn from passing runs only. A failing run often stops early (compile error, first failed test) and its demand is not the suite's; failures no longer write `gate_stats` rows, and rows of failed runs written by earlier releases are ignored on load (`WHERE outcome = 'pass'`). Expected times (HRRN) follow the same samples | `server.rs`: `learns_from`, `close_job`; `store.rs`: `gate_stats_recent` | `server::tests::only_passing_runs_teach_the_demand`; `store::tests::gate_stats_keep_twenty_per_key_and_prune_unseen_keys` (extended) |
| 6 | LOW-MED | Under a cgroup CPU quota below the affinity mask, idle CPUs were over-counted | `Utilisation` takes the cgroup limit (read under the parent of `proc_root` by `sampler::cgroup_cpu_limit`); when it is below the mask, idle = `max(0, limit − busy CPUs of the mask)` and the idle fraction is taken over the limit | `sampler.rs`: `cgroup_cpu_limit`, `Utilisation::with_cpu_limit`, `Utilisation::observe`; `server.rs`: `utilisation` | `sampler::tests::a_cgroup_quota_below_the_mask_bounds_the_idle_cpus` (fake `proc/stat`, `proc/self/cgroup`, `sys/fs/cgroup/.../cpu.max`) |
| 7 | LOW | A job admitted between the `is_queued` check and `finish_job` (detach, drain) kept running unsupervised | `finish_job` is split into `close_job` (under the state lock) and `announce_closed`; `detach_because` and `drain` drop a queued job under the same lock as the check. In `launch`, a job whose entry is gone after the spawn is cancelled (group SIGTERM, then SIGKILL), awaited and untracked from the reaper | `server.rs`: `Closed`, `close_job`, `announce_closed`, `finish_job`, `detach_because`, `drain`, `launch` (`gate_name` removed) | `tests/daemon.rs`: `a_queued_job_dropped_by_its_last_subscriber_never_starts` |
| 8 | LOW | `terminate_groups_blocking` could signal a pgid again after it was seen empty (pgid reuse) | The sequence is `terminate_groups_with` over injected operations: a group is dropped the first time it is seen empty and never signalled again; only groups never seen empty get SIGKILL | `executor.rs`: `terminate_groups_blocking`, `terminate_groups_with` | `executor::tests::a_group_seen_empty_is_never_signalled_again` |
| 9 | LOW | The in-process exec fallback signalled the leader only | The command is spawned in its own process group (`process_group(0)`); timeout and forwarded signals use `killpg` (SIGTERM, SIGCONT, SIGKILL after the grace); when the client owns the terminal, the terminal is handed to the command's group (`tcsetpgrp` with SIGTTOU blocked) and taken back afterwards. `nix` gains the `term` feature in `sluice-gate` | `exec.rs`: `in_process`, `group_alive`, `Terminal`, `with_sigttou_blocked`; `crates/sluice-gate/Cargo.toml` | `tests/exec.rs`: `an_in_process_timeout_kills_the_whole_group`, `an_in_process_forwarded_signal_reaches_the_whole_group` |
| 10 | LOW | A leading `PATH=...` assignment also applied to the inserted `sluice` | No rewrite when a leading assignment of the segment sets `PATH` | `guard/matcher.rs`: `plan_segment` | `guard::tests::a_path_assignment_is_never_rewritten` |
| 11 | LOW | The migration backup was written in place and an existing one trusted blindly | `VACUUM INTO` a temporary file next to it, fsync, rename, fsync the directory; an existing backup is kept only when it opens read-only and passes `PRAGMA quick_check`, otherwise it is replaced | `migrate.rs`: `backup`, `backup_is_sound`, `persist`, `io_error` | `migrate::tests::a_backup_is_written_whole_and_kept_when_sound`, `migrate::tests::a_damaged_backup_is_replaced` |
| 12 | LOW | `doctor` (checks and bundle) connected to the fallback socket before checking its directory | `socket_to_contact` refuses the short fallback when its directory exists and `unsafe_socket_dir` objects; the `daemon` check fails with `not contacted: ...`, the bundle skips the status | `doctor.rs`: `socket_to_contact`, `check_daemon`, bundle | `tests/socket_fallback.rs`: `doctor_never_connects_through_an_unsafe_fallback_dir` |
| 13 | LOW | Fork-to-registration window of the reaper | Accepted and documented as a known limitation (reconciliation at the next start covers it; no marker can be written) | `kb/sluice/design/executor.md` | none (documentation) |

## Notes

- Finding 2, protocol: instead of a new optional field in `queue-approve`
  (which would also have broken the struct literals of the shared queue
  test helpers), the detection variables travel in the request's existing
  `env` map; an older daemon drops them through its allowlist, an older
  client sends none and is treated as a human, as before. The check is a
  speed bump: an agent controls its own environment. The guard's deny, the
  agent's permission rules and the forge's protected branches remain.
- Finding 3: `sluice-gate-core/src/git.rs` and a test helper in
  `sluice-gate-queue/src/runner.rs` still call `std::env::vars()` in test
  code only (`git.rs` was owned by a parallel task).
- Finding 4: no over-admission ever happens in a pass that started a job
  nominally (unchanged), and now also not before a full sampler tick after
  any start.

## Docs updated

`kb/sluice/design/command-control.md` (sections 2.6, 4.1, 4.3),
`kb/sluice/design/scheduler-throughput.md` (sections 3, 5, 6.6, 7.1, 9,
12), `kb/sluice/design/executor.md` (reaper, known limitation, job finished
while spawning), `kb/sluice/design/merge-queue.md` (transition table,
agent approval through the CLI), `docs/merge-queue.md`, `docs/agents.md`,
`docs/configuration.md`.

## Verification

From the worktree root, dogfooding: `sluice run fmt`, `sluice run clippy`,
`sluice run test` (results in the task report).
