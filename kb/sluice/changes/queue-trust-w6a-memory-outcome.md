---
title: "Change: W6a, gate jobs are the kernel's preferred victims and say when memory killed them"
tags: [sluice, change, queue-trust, memory, containment, w6]
---

# Change: W6a, outcome and kernel preference (ADR-0019 pieces 2, 4, 6)

Plan: W6a of [[sluice/plan/queue-trust-plan.md]]. Decision:
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]]. Analysis:
[[sluice/research/memory-gates-w6-analysis.md]] (A3, A4, A5, A8, B2, B4, B7; tests T-W6-6..9, 12).
Written without running tests (the `cargo test` hook): `cargo fmt` and
`cargo check --workspace --all-targets -j 4` only; the full verification is the plan's final step.

## What changed

1. **`oom_score_adj` of gate jobs.** `sluice cg-exec <leaf> --oom-score-adj N -- <argv>`
   (`crates/sluice-gate/src/cg_exec.rs`, `main.rs`): after writing its pid to the leaf and
   before `exec`, reads `/proc/self/oom_score_adj` and writes `max(N, current)`
   (`raised_adj`, `apply_oom_score_adj`); any failure is a debug log, never an error
   (both the move and the adj fail open, independently). A leaf of `-` (`NO_LEAF`) skips
   the move. `Containment::job_prefix` passes `--oom-score-adj` (`cg_exec_prefix`);
   `job_wrapper` in `server.rs` falls back to the leaf `-` when containment is not running
   but `[containment]` is enabled (no cgroup v2, no user manager, leaf creation failed), so
   the kernel preference also holds where cgroups are unavailable. With
   `[containment] enabled = false` nothing is wrapped (0.4.1 behaviour, kept). Config
   `[containment] gates_oom_score_adj = "auto" | 0..=1000` (`GatesOomScoreAdj`,
   `auto` = 800 = `AUTO_GATES_OOM_SCORE_ADJ`, validated, one-way: older binaries reject the
   key; fixture `tests/fixtures/unreleased/containment-gates-oom-score-adj.toml`,
   `docs/compatibility.md`).
2. **`memory.oom.group = 1` on each job leaf** (`Tree::job_leaf`, tolerates a missing
   file). Never on `jobs/` or above. The doctor check `gates-oom`
   (`doctor/containment.rs`: `GatesOomFacts`, `ancestors_with_oom_group`, `gates_oom`)
   reads `memory.oom.group` of `jobs/` and every ancestor up to the cgroup root and warns
   when one is `1`; it also shows the gate adj (floor, "auto") and whether oomd is active.
3. **Outcome classification at job end** (`crates/sluice-gate-daemon/src/memkill.rs`, new;
   `server.rs` `launch`, `job_memory_end`, `memory_end`, `conclude`, `start_exec`,
   `close_job`/`merged_killed_by`). While the job's leaf still exists the daemon reads its
   `memory.events` and `memory.peak` (`Containment::read_job_end`) on a blocking thread;
   a `SIGKILL`-shaped ending with no counter triggers one bounded journal query
   (`Containment::oomd_killed_job`: waits 400 ms, `journal_oomd(user=false)` since the job
   start minus 5 s, `find_job_kill` by leaf `jobs/<id>`). The pure rule is
   `memkill::memory_kill_message`:
   the leader ended with a signal or exit 137 (never `Timeout`/`Killed`/other exits), and
   either `oom_kill` or `oom_group_kill` of the leaf is above zero, or the ending is signal 9
   or exit 137 and a journal line names the leaf. Result: `OutcomeStatus::Killed`,
   `killed_by = "memory"`, `memory_peak_bytes`, message `<gate> was killed for lack of
   memory after 41.2s (job peak 18.1 GiB; 1.4 GiB available at the end); running it again
   unchanged is likely to end the same way: lower its concurrency or free memory first`
   (MemAvailable from `<proc_root>/meminfo`). The miss record reason is `killed for memory`.
   `close_job` kept `killed_by` only from the cancel entry: it now keeps the classifier's
   value when Sluice did not cancel (`merged_killed_by`).
4. **Interventions rows** for kills inside the job (cause `oom_kill`/`oom_group_kill`) and
   for the oomd line (`oomd_memory_pressure`/`oomd_swap`, leaf `jobs/<id>`) are recorded by
   `record_interventions` (extracted from `containment_tick`). Dedup: the watcher's
   `settle_job` only reports what grew beyond what its poll had seen; the journal row keeps
   the journal's own timestamp, so the later journal read hits the unique
   `(at_ms, leaf, cause)` index.
5. **`MemoryWatcher` backstop** (`interventions.rs`: `with_backstop`, `backstop`,
   `settle_job`): reads `memory.events` of `jobs/` and the daemon scope (they count the kills
   of removed descendants). Growth of `jobs/` not explained by a leaf row (poll or settle),
   still unexplained at the next 10 s pass, becomes "a gate job died of OOM (job unknown)"
   (leaf `jobs`); growth of the scope outside `jobs/` is reported against `daemon`.
6. **Protocol (additive).** `killed_by = "memory"` documented (`KILLED_BY_MEMORY`,
   `Outcome::killed_for_memory`), `Outcome.memory_peak_bytes: Option<u64>`
   (`#[serde(default, skip_serializing_if)]`); the frozen status schemas are untouched.
7. **Clients print the message.** `sluice run` (`run.rs`, `Report.memory_kill`), `sluice
   exec` (`exec.rs`, before the signal and "daemon stopped" arms), MCP `verify`
   (`KILLED for lack of memory ...` line, `message`, new optional `killed_by` in the gate
   result), the queue runner (`gate_failure`: `what` "gate X was killed for lack of memory",
   `detail.killed_by`) and the prepare job (`Run.killed_by`, `detail.killed_by`, the message
   in `error_text`).
8. **W6 addendum.** `cargo check` and `cargo doc` join `DEFAULT_HEAVY_COMMANDS`
   (`heavy.rs` tests); `docs/configuration.md`, `config-format.md`, `command-control.md`.

## Docs

`kb/sluice/design/containment.md` (§4, §5, §7), `docs/configuration.md`,
`docs/debugging.md` (how to read `killed_by = memory`), `docs/merge-queue.md`,
`docs/compatibility.md`, ADR-0019 (piece 2 note).

## Decisions

- No-leaf `cg-exec` implemented (small, safe): the setup (`exe`) already exists whenever
  `[containment]` is enabled, even when `Containment::start` failed. Cost: one extra exec hop
  on machines without cgroup v2.
- The classification reads the leaf of every job at its end (two small cgroup files) so kills
  inside a job whose leader survived are recorded too; the `killed` status needs the leader to
  die (with `oom.group = 1` it does).
- One journal query, not one per user/system journal: oomd is a system service, so the system
  journal; a user not in `adm`/`systemd-journal` sees no line (the counters still classify
  kernel kills). No retry; a 400 ms pause lets oomd's line land first.
- Job peak includes page cache (`memory.peak`); it is displayed only. The learned working set
  is W6b.
- Counters start at 0 for a job (its leaf is created for it), so the end value is the delta.

## Tests (none creates memory pressure)

- `memkill::tests` (T-W6-6): counters + kill ending gives the message with peak and available;
  group counter alone and any signal; exit code alone never; journal hit only for signal 9/137;
  cancels, timeouts and normal exits never; unknown figures omitted; journal asked only when
  nothing else explains a SIGKILL; meminfo parsing and byte formatting.
- `server::tests::the_classifiers_killed_by_survives_close_job_unless_sluice_cancelled`.
- `interventions::tests` (T-W6-7): backstop reports unexplained growth once after it persists;
  a settled job explains it; a leaf the poll reported is not reported again at the end; kills
  outside `jobs/` go to `daemon`; no backstop or unreadable files report nothing;
  `find_job_kill`.
- `cg_exec::tests` and `tests/containment.rs::cg_exec_raises_the_oom_score_adj_and_never_lowers_it`
  (T-W6-8): raised, never lowered (inherited value kept), `-` leaf, unwritable leaf fails open,
  out-of-range value is a usage error.
- `tree::tests::a_job_leaf_gets_oom_group_and_nothing_above_it_does` (T-W6-9, W6a part).
- `doctor::containment::tests::an_ancestor_with_oom_group_is_a_warning` (T-W6-12, W6a part).
- Config: `gates_oom_score_adj_accepts_auto_or_a_number`, defaults; compat fixture test.
- Heavy list: `cargo check`/`cargo doc` match, `cargo checkout`/`cargo docs` do not.
- Prepare: `a_memory_kill_travels_in_the_detail_and_the_error_text`; proto: round trip of
  `memory_peak_bytes`.

## Not done here (W6b to W6e)

Memory-aware admission, the oomd policy on the gates scope and `user.oomd_omit` on `daemon/`,
the optional ceiling, the guard, the bounded live tests; the doctor lines for the gates policy,
the omit xattr and learned peaks (they belong to W6c/W6b). Verification: `cargo check
--workspace --all-targets -j 4` clean; the suite runs at the end of the plan.

## Verification (lead)

First `verify` found: `nonminimal_bool` in `memkill` (rewritten with an `explained`
binding), `too_many_lines` in MCP `outcome_line` (the status words moved to
`status_text`), `assert_is_empty` in a doctor test, the job-leaf test comparing
`"1\n"` with `"1"` (the helper writes a newline; the test trims), and an interventions
test that called `settle_job` twice for one job (the daemon calls it once, at job end,
and drops the entry so the map cannot grow; the test now follows that). Then fmt,
clippy and the whole suite passed through Sluice (2026-10-26).
