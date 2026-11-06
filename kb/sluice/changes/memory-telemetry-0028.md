---
title: "Change: per-job memory telemetry and the session's pressure (ADR-0028 part A)"
tags: [sluice, change, daemon, memory, telemetry, psi]
---

# Change: per-job memory telemetry and the session's pressure (ADR-0028 part A)

Why: the 2026-11-05 desktop hang could not be attributed
([[sluice/research/desktop-hang-2026-11-05.md]]): the daemon kept each job's working
set, but not its memory pressure, swap or escaped processes, nor the desktop's pressure.
Decision: [[sluice/design/adr/0028-gate-jobs-do-not-degrade-the-desktop.md]] decision 1.

## What changed

- `crates/sluice-gate-daemon/src/telemetry.rs` (new): `parse_pressure`, `read_pressure`,
  `read_leaf` (`LeafReading`: pressure, `memory.swap.current`, `anon`, `file`),
  `leaf_procs`, `cgroup_of`, `find_escapees` (descendants of the leaf's processes living
  in another cgroup, plus known escapees by pid and start time), `session_cgroup`
  (`session.slice` of the user manager the daemon runs under), `JobTelemetry` (peaks and
  the notes worth a log line), `Summary`, `SessionWatch` (warning at `some avg10 >= 10 %`,
  clear under 5 %).
- `sampler.rs`: `ProcEntry`, `parse_proc_entry`, `scan_processes`, `members_of`; one
  `/proc` pass per tick now serves the CPU sampler and the escape detection
  (`group_members` kept as a wrapper).
- `memory.rs`: `rss_bytes`.
- `server.rs`: `JobEntry::telemetry`, `Shared::{session_dir, session}`, `session_tick`
  (logs "the desktop session is under memory pressure" / "... back to normal" with the
  running jobs), `telemetry_tick` (logs "a process of the job left its cgroup" and "the
  job is stalled on memory"), `job_pressure` (stored at job end, "job memory telemetry"
  summary line when notable), status fields.
- `migrate.rs`: `containment_schema` 5, table `job_memory_pressure`; `store.rs`:
  `JobPressure`, `insert_job_pressure`, `job_pressure_recent`, `JOB_PRESSURE_KEEP`.
- `sluice-gate-proto`: `RunningJob::{memory_swap_bytes, memory_pressure_some,
  memory_pressure_full, escaped_processes, escaped_bytes}` and
  `MemoryBudget::{session_some_avg10, session_full_avg10}` (additive); the text status
  (`sluice status` and MCP `status`) prints `swap`, `psi` and `N process(es) outside its
  cgroup` per job and the desktop pressure on the `memory:` line. `MemoryBudget` no longer
  derives `Eq` (it holds `f64`).

## How it was verified

- Unit tests: `telemetry::tests` (pressure parsing, leaf reading, escapees in a fake
  `/proc` including orphaned and reused pids, session cgroup, peaks and notes, session
  hysteresis), `store::tests::job_pressure_round_trips_and_a_file_without_the_table_ignores_it`,
  `tests/compat.rs::a_database_at_containment_v4_gains_job_memory_pressure_in_place` and the
  older migration tests updated to drop the new table.
- Real user manager: `crates/sluice-gate/tests/containment_daemon.rs`
  (`an_escaping_process_is_moved_back_and_dies_with_the_job`,
  `with_reclaim_off_an_escapee_is_only_reported`) check the escape log line and the
  telemetry summary line.
- The full gates through Sluice (`verify`: fmt, clippy, test).
