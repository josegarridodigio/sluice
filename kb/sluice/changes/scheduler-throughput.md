---
title: "Change: scheduler throughput and job timeline (alpha B1 + L1)"
tags: [sluice, change, alpha, scheduler, latency, b1, l1]
---

# Change: scheduler throughput and job timeline (B1 + L1, 2026-10-23)

Implements alpha tasks B1 and L1 of [[sluice/plan/alpha-plan.md]] (and the
M4–M7 policies of L2, each behind its switch) from
[[sluice/design/scheduler-throughput.md]] and
[[sluice/design/adr/0011-scheduler-throughput.md]]; storage follows
[[sluice/design/adr/0013-compatibility-promise.md]] (G9). Current behaviour:
[[sluice/design/scheduler.md]]. Protocol stays 1, `status` schema stays 1,
`schema.version` stays 1; every wire addition is optional.

## What changed

- **M1 budget = CPUs** (`sluice-gate-core/src/cpus.rs`: `parse_cpu_list`,
  `allowed_from_status`, `cgroup_cpu_limit_in`, `available_cpus_in`,
  `available_cpus`, `default_budget`): `daemon.budget` defaults to
  `max(2, available CPUs)`, the affinity mask bounded by the cgroup v2/v1
  CPU limit, read under an injectable root.
- **Pure admission** (`sluice-gate-daemon/src/scheduler.rs`): `Policy`,
  `Pressure`, `Snapshot`, `QueuedSnap`, `RunningSnap`, `Plan`, `Wait`,
  `WaitCause`, `Admission`, `Class`, `Order`; `rank`, `plan`,
  `effective_weight`, `expected_ms`. `Scheduler` owns state only
  (`with_policy`, `admit_with`, `head`, `record`, `cap`, `over_admissions`);
  `Scheduler::new` and `admit` keep the ADR-0007 behaviour (every switch off).
  Tests in `scheduler/tests.rs`.
- **M2 learned weights**: `sampler.rs` (`group_members`, `read_tasks`,
  `leader_ticks`, `JobUsage`, `Demand`) samples each running gate's tasks
  every tick; `finish_job` stores pass/fail demand in `gate_stats`; a
  per-daemon `StatsCache` (last 10 per `(repo, gate, cmd_hash)`, `cmd_hash`
  in `server.rs`) feeds `JobSpec.demands_milli` / `expected_ms` at enqueue.
- **M3 over-admission**: `psi::Admission::calm_for_overcommit` (conditions
  1–3, cooldown after a resume via `last_resume_ms`), `Policy::cap_for`,
  step 3 of `plan`; `status.budget.cap`, `totals.over_admissions`.
- **M4 parallel `verify`** (`sluice-gate-mcp/src/lib.rs`: `verify`,
  `verify_parallel`, `verify_gate`, `early_line`, `name_early_verdicts`;
  `daemon.rs`: `RunEnd::EarlyVerdict`, `run(.., early, ..)`): gates start
  together; with `fail_fast` (new optional parameter, default true) the first
  failure cancels the pending ones with `cancel.reason = "early-verdict"`.
  Daemon side: `ClientMessage::Cancel.reason`, `detach_because`,
  `JobEntry.killed_by`, `Outcome.killed_by`.
- **M5–M7**: lane fairness, humans first, SJF (HRRN) in `rank`, with
  `head_reserve_after` as the single ageing bound.
- **L1 timeline**: `RunRequest.requested_at` (sent by `sluice run` and
  `sluice mcp`), `admitted_at`, wait accumulators per cause, `admission`,
  `killed_by`, `job_requests` rows (`register`/`coalesce_into`,
  `close_request`), `done.outcome` fields `queue_wait_ms`, `wait_reason`,
  `admission`, `killed_by`; `queued.weight_cfg`/`expected_ms`; `status`
  fields per [[sluice/design/protocol.md]]; MCP `GateResult.queue_wait_ms`,
  `wait_reason`, `VerifyReport.wall_ms`.
- **L1 utilisation sampler**: `sampler::Utilisation` over `/proc/stat` of the
  daemon's CPUs; `server::sampler_loop` (replaces `psi_loop`, runs with PSI
  off too, every `psi_poll_interval`); `totals.idle_while_waiting_ms`,
  `waiting_wall_ms`, `budget.idle_cpus`, `budget.cpus`.
  `DaemonConfig.proc_root` (new field, normally `/proc`).
- **Storage**: `sluice-gate-daemon/src/migrate.rs` (`Migration`,
  `SCHED_MIGRATIONS`, `version`, `backup_path`, `backup`, `apply`);
  `Store::open` migrates (`open_with`, `migrate`, `sched_version`) after a
  `sluice.db.bak-<version>` copy; `Error::NewerSchema`; store methods
  `set_queued_meta`, `set_admitted`, `set_finish_meta`, `insert_request`,
  `close_request`, `insert_gate_stat`, `gate_stats_recent`,
  `prune_gate_stats`, `timeline`, `timelines_since` (`QueuedMeta`,
  `FinishMeta`, `RequestRow`, `GateStat`, `Timeline`).
- **CLI**: `sluice logs --timeline [<job>] [--since 10m]` (`logs::timeline`,
  `timeline_line`; reads the database read-only, works without a daemon);
  `sluice status` shows `(cap N, X of Y cpus idle)`, each queued job's rank,
  wait detail and expected time, and a `waiting:` line.
- **Config** (`sluice-gate-core/src/config.rs`): `[scheduler]
  learned_weights = true`, `fairness = "lane"` (`Fairness`), `human_first =
  true`, `order = "fifo"` (`QueueOrder`); `[admission] overcommit = 1.5`,
  `cpu_some_avg10_low = 25.0`, `memory_some_avg10_low = 5.0`,
  `overcommit_interval = "5s"`; `[mcp] verify_parallel = true`; validation of
  each. Documented in `docs/configuration.md` and
  [[sluice/design/config-format.md]].
- Docs: [[sluice/design/scheduler.md]] (rewritten to current behaviour),
  [[sluice/design/scheduler-throughput.md]] section 12 (deviations),
  [[sluice/design/protocol.md]], [[sluice/design/mcp-tools.md]],
  [[sluice/design/executor.md]] (migrations and new tables).

## Deviations from the design

See [[sluice/design/scheduler-throughput.md]] section 12: over-admission
only in a pass without nominal starts; runs never sampled teach nothing;
`exec` lane = cwd; `via` derivation; status queued list in rank order;
sequential `verify` ignores `fail_fast`; `sluice run -v` does not print the
queue wait yet; read-only opens tolerate a newer `sched_schema`;
`superseded` reserved; the downgrade check replays the v0.2.0 statements
rather than the real binary (`s6_upgrade` covers that).

Two existing MCP tests now pass `fail_fast: false`
(`parallel_verifies_coalesce_and_a_failure_does_not_stop_the_next_gate`,
`an_unavailable_daemon_is_an_error_per_gate_not_an_mcp_error`): their
assertion (every gate runs after a failure) is the documented
`fail_fast: false` behaviour. `config::tests::user_defaults` checks the new
budget default.

## Tests added

- core: `cpus::tests` (CPU lists, affinity alone, cgroup v2 ancestor quota,
  cgroup v1 quota, real machine); `throughput_switches_have_documented_defaults_and_parse`.
- proto: `scheduler_wire_fields_are_additive`;
  `status_schema::scheduler_fields_are_additive_optional_fields` (fixture
  unchanged).
- daemon unit: scheduler (learned-weight warm-up/floor/ceiling, 2 → 3
  suites, median; over-admission calm/idle/interval, cap, blocking, guard,
  reservation head only, first ranked under the cap, laptop budget 6/cap 9,
  counting and drain-back; overdue agent vs fresh human, human-first
  detail, lane interleaving and detail, HRRN ordering/crossover/FIFO when
  off, no starvation under all 16 switch combinations, wait accumulators,
  views); psi `calm_for_overcommit_conditions_and_cooldown`; sampler
  (`/proc/stat` parsing and mask, idle-while-waiting integral, task
  sampler on a fake proc tree, tid reuse, no-schedstat fallback); store
  (fresh database without backup, backup + idempotence, failing migration
  rolls back, newer `sched_schema`, timeline/requests, gate stats pruning).
- daemon integration (`tests/daemon.rs`):
  `timeline_fields_are_present_and_monotonic`,
  `idle_while_waiting_grows_when_the_budget_leaves_cpus_idle` (fake proc
  root), `an_early_verdict_cancel_kills_only_a_job_nobody_else_waits_for`.
- daemon compat (`tests/compat.rs`): `the_v020_database_migrates_without_losing_history`,
  `a_v020_daemon_still_opens_and_writes_the_migrated_file`,
  `a_daemon_with_the_v020_user_config_runs_on_the_migrated_database`.
- CLI: `status::render_shows_the_scheduler_fields_when_present`,
  `logs::timeline_line_reads_like_the_design`; MCP integration
  `parallel_verify_early_verdict_cancels_an_orphaned_sibling` (also checks
  `sluice logs --timeline` shows `killed by early-verdict`) and
  `parallel_verify_keeps_a_sibling_another_client_waits_for`.

Timing: no test asserts a duration bound; the sibling gate is ordered by a
pid file, the utilisation test drives a fake `/proc/stat`, and waits poll
with generous limits.

## Verification

From the worktree root, `PATH="$HOME/.cargo/bin:$PATH"`:

```
sluice: fmt PASS 0.5s (job 1792711022186-3052613-1fa040)
sluice: clippy PASS 0.1s (job 1792711022838-3052613-003a1c)
sluice: test PASS 96.8s (job 1792711026680-3052613-8b2307)
```

(clippy ran as a daemon job; its 0.1 s is cargo reusing the results of the
identical `cargo clippy` run just before.)
