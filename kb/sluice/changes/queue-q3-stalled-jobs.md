---
title: "Queue Q3: stalled jobs are reported, never killed (ADR-0024)"
tags: [sluice, changes, queue, daemon, observability, stall]
---

# Queue Q3: stalled jobs are reported, never killed

Wave Q3 of [[sluice/plan/queue-scheduling-plan.md]] (S2). Decision:
[[sluice/design/adr/0024-stalled-jobs-are-reported-never-killed.md]]; evidence:
[[sluice/research/queue-scheduling-analysis.md]] sections 1.5, 2.4, 4.2, 4.4. Status:
implemented; verification through Sluice pending (the lead runs `fmt`, `clippy` and `test`).
Isolated runs during the work: the new `stall` module (13 tests), the shadow tests of
`scheduler/tests/backfill.rs`, the two daemon integration tests, the config fixture, the
proto, MCP, status and session-context tests, all green.

## What changed

1. **Detection** (`crates/sluice-gate-daemon/src/stall.rs`, pure decision plus readers).
   Every 5 s (`EVERY_MS`, a coarser cadence than the 1 s sampler; the sampler tick calls
   `server::stalls::due`) the daemon reads, per running job (gates and `exec`), either the
   leaf cgroup (`cpu.stat` `usage_usec`, `cpu.pressure` `some total=`, `io.stat`
   rbytes+wbytes and rios+wios summed over devices, `memory.stat` `pgfault` and
   `pgmajfault`, `pids.current`, else the lines of `cgroup.procs`) or, without a readable
   leaf, the process group (`utime+stime+cutime+cstime` of every process, the run delay of
   their tasks from `schedstat`, `/proc/<pid>/io` `read_bytes+write_bytes` and `syscr+syscw`
   where readable, `minflt`/`majflt` from `stat`, the process count), plus the log bytes
   the executor already counts (`RunningHandle::log_bytes`).
2. **Progress and thresholds** (`moved`). Beyond noise, per read interval: CPU time above
   1 % of one CPU (a timer tick is about 0.1 %); waiting for a CPU above 10 % of the
   interval (a starved job is runnable but not hung: it is not quiet; this is the
   `cpu.pressure` `some total` growth, or the `schedstat` run delay in the fallback); block
   I/O of 16 KiB or 16 operations (four pages: a heartbeat file write is below it);
   128 minor page faults (512 KiB of pages) or one major fault; any log byte; any change
   of the process count. A counter the machine does not give is not judged. Readings of
   different sources (leaf then group) are not compared.
3. **The window** (`StallWatch::observe`). A monotonic `last_progress_at` per job
   (`mono_ms`, `Instant`-based, never the wall clock) starts at the first sample; progress
   sets it to now; the job is `stalled` when `now - last_progress_at >= window`. The window
   is only ever measured from samples the daemon took: after a daemon restart the watch
   starts again (no prior knowledge, no stall before one full window), a sample that could
   not be read counts as not quiet, and a hole of more than four cadences between two
   samples restarts the window, so a stall is never decided over an unobserved stretch. The
   longest quiet window of the job is kept (`longest_quiet_ms`, the open one included).
4. **Window length** (`window_ms`). Default `max(5 min, 3 x expected)`, expected being the
   learned duration (the median of the last passing runs). With no history it is the 5 min
   floor alone: the floor is the evidence-backed figure (the hangs on record sat idle for
   25 minutes or more, and 5 min of total silence is far from any recorded healthy run), a
   first run has nothing to scale with, and a longer guess would only delay the report.
   Per gate `stall_timeout` (a duration or `"off"`, `StallTimeout` in `config.rs`, wire
   `GateSpec::stall_timeout_ms`, `Some(0)` = off): a duration is raised to a 1 min floor
   (so 12 reads at least are in any configured window); `"off"` never reports.
5. **Test knobs.** `stall::Tuning` (`default_floor_ms`, `configured_floor_ms`, `every_ms`)
   is in `DaemonConfig::stall`; the default is production. Only tests lower it: an
   in-process daemon sets the field, the daemon command reads
   `SLUICE_TEST_STALL_FLOOR_MS` (both floors) and `SLUICE_TEST_STALL_EVERY_MS` (cadence).
6. **Report only.** Never cancels, kills or changes the job. `JobEntry::stall`; transitions
   are `Change::{Stalled, Resumed}`.
   - `status`: `running[].stalled` (`StalledJob { for_ms, flat }`, additive, schema 1), text
     `  STALLED 6m12s (flat: cpu io output)` after the memory (`RunningJob::stalled_text`,
     CLI and MCP); the memory and the weight are the job's own columns on the line.
   - Subscribers: `ServerMessage::Progress::stalled_ms` (additive; `JobEvent::Progress`
     carries it). The daemon sends a `progress` at the transition (so the notice is
     immediate) and every 5 s with the periodic one while it lasts. `sluice run`
     (`run.rs::stall_notice`) prints `sluice: <gate> looks stalled: no CPU, I/O or output
     for 6m12s (job J); it is probably hung, ...` once, and `is active again` when it
     goes; the MCP `verify` progress message (`JobState::describe`) says `; looks STALLED:
     ...`. This is the existing progress mechanism (there is no separate notice channel).
   - `daemon.log`: `job X stalled: no progress for 5m0s while holding N unit(s) of budget;
     reported only, Sluice never ends it` (warn; fields `quiet_ms`, `weight`,
     `memory_bytes`, `flat`), `job X active again after a stall of ...` (info) and, per
     finished job, `job X finished: longest quiet window ...` (`stalls::log_finished`,
     called from `close_job`; fields `longest_quiet_ms`, `stalled`, `outcome`).
7. **Scheduling.** `RunningSnap::stalled` (from `Scheduler::set_stalled`, set by `admit`
   from the watches). In `Pass::new` a stalled holder gets `end = UNKNOWN_END`; `shadow()`
   never releases it: when the head does not fit once everything that ends at a known time
   is gone, `Shadow::at == UNKNOWN_END`. Consequences: the room (`room()`) counts the
   stalled job as a survivor, so it only shrinks to what fits beside it; `ends_in_time`
   refuses every job against an unknown shadow time; `Plan::shadow` and the head's
   `reserved_eta_ms` are `None` (no ETA is shown for a head that waits for a hung job);
   `note_shadow` does not record an unknown first shadow (the head-delay counter ignores
   it). A head that fits beside the stalled job keeps a known shadow.
8. **Agent text** (one rule each, nothing loosened): `plugin/skills/sluice/SKILL.md`,
   `reference.md`, `INSTRUCTIONS` of the MCP server (`sluice-gate-mcp/src/lib.rs`), rule 8
   of the session context (`session_start.rs`, kept under its 1,500 character cap),
   `docs/agents.md`, `plugin/README.md`.

## Deviations and interpretations

- **Cancel path.** ADR-0024 says an agent should "cancel its own stalled job". There is no
  command or tool that cancels a running gate job (`queue_cancel` and `sluice queue cancel`
  are the merge queue's). The existing path is cancelling the `verify` call (the daemon then
  ends the job when its last subscriber is gone, `last_subscriber_gone = "cancel"`, the
  default) or interrupting `sluice run`. The agent text says exactly that ("cancel your own
  `verify` call"). A dedicated cancel command is a possible follow-up; none was added.
- **The notice is the progress message.** The MCP `verify` call blocks until the gates end;
  the one-line notice reaches the agent only through progress notifications (when the host
  passes a progress token) and through `status`. The final `verify` result does not repeat it
  (no `Outcome` field was added). `sluice exec` prints no stall notice (its jobs show in
  `status` and `daemon.log`).
- **Longest quiet window: log only.** The per-job table (`jobs` plus `FinishMeta`) would
  need a migration for a column; the ADR asks for a log line and says a column only if
  trivial. It is the `longest quiet window` line of `daemon.log` (`grep`-able, per job).
- **`stall_timeout` floor.** A configured value below 1 min is raised to 1 min by the daemon
  (not a config error, so a repository config cannot be rejected by a daemon knob); with
  `"off"` and the default the ADR's text holds.
- **Any log byte is progress.** A hung process that keeps printing (a repeating "still
  pending" line) is never reported; that is the conservative side (no false stall). The
  recorded hangs each have a log whose last line is such a message, but their rate is not
  known (the log mtime is the timeout footer): the longest-quiet-window log line is the
  data to decide whether output of a bounded size should count less.
- **A stalled-to-active flap when a read fails.** An unreadable sample ends a stall
  (`Resumed`) and a new window starts; a job whose counters keep failing is never reported.
- **Weight of a stalled job is not released** for admission (ADR alternatives: later, only if
  traces show long benign quiet periods).
- Wire names chosen here: `running[].stalled { for_ms, flat }`, `progress.stalled_ms`,
  `gate.stall_timeout_ms`; signal spellings `cpu`, `cpu-wait`, `io`, `faults`, `output`,
  `processes`.

## Files and symbols

- `crates/sluice-gate-daemon/src/stall.rs` (new) and `stall/tests.rs`: `Tuning`,
  `window_ms`, `Source`, `Readings`, `Sample`, `Signal`, `moved`, `observed`, `Change`,
  `StalledView`, `StallWatch`, `mono_ms`, `read_leaf`, `read_group`.
- `crates/sluice-gate-daemon/src/server/stalls.rs` (new): `due`, `tick`, `stalled_ids`,
  `stalled_for`, `view`, `log_finished`.
- `crates/sluice-gate-daemon/src/server.rs`: `DaemonConfig::stall`, `Shared::stall_last`,
  `JobEntry::stall`, `JobEvent::Progress::stalled_ms`, `sample_tick`, `admit`, `close_job`,
  `status_report`, the progress tick of the run loop, `Subscriber::message`.
- `crates/sluice-gate-daemon/src/scheduler.rs`, `scheduler/shadow.rs`: `RunningSnap::stalled`,
  `Scheduler::{stalled, set_stalled}`, `UNKNOWN_END`, `shadow()`, `ends_in_time`,
  `Plan::shadow`.
- `crates/sluice-gate-daemon/src/sampler.rs`: `stat_fields` is `pub(crate)`.
- `crates/sluice-gate-core/src/config.rs`: `StallTimeout`, `Gate::stall_timeout`;
  `lookup.rs`: `RunMaterial::stall_timeout_ms`; fixture
  `tests/fixtures/unreleased/gate-stall-timeout.toml`, test in `tests/compat.rs`.
- `crates/sluice-gate-proto/src/lib.rs`: `GateSpec::stall_timeout_ms`,
  `ServerMessage::Progress::stalled_ms`, `RunningJob::{stalled, stalled_text}`, `StalledJob`.
- `crates/sluice-gate/src/{run.rs, status.rs, daemon_cmd.rs, session_start.rs}`,
  `crates/sluice-gate-mcp/src/{lib.rs, text.rs}`, `crates/sluice-gate-queue/src/{runner.rs,
  prepare.rs}` (the new `GateSpec` field).
- Docs: `docs/how-it-works.md`, `configuration.md`, `cli.md`, `debugging.md`,
  `compatibility.md`, `agents.md`, `plugin/README.md`, `plugin/skills/sluice/SKILL.md`,
  `reference.md`, `CHANGELOG.md`.

## Tests

`stall/tests.rs`: window (default, 3 x expected, configured floor, off, test overrides);
each signal beyond its threshold and each just below it; CPU starvation (never stalled over
ten minutes of waiting); different sources; the stall at exactly the window; progress and
`Resumed`; `"off"`; a new watch (restart) needs a whole window; an unreadable sample and a
gap restart the window; the leaf reader and the group reader over fake trees.
`scheduler/tests/backfill.rs`: a stalled holder is never released (unknown shadow, known
when the head fits beside it, memory), no job passes an unknown shadow by duration, the
evidence case with a stalled suite (no ETA, closed room), a head beside a stalled job, the
scheduler's flag. `tests/daemon.rs`: a real job that sleeps without output under a
tiny `stall_timeout` is reported stalled (progress and `status` with its signals and weight),
wakes up and is reported active again, and passes (never killed); `"off"` never reports.
Also: proto additivity, status text (CLI and MCP), the MCP progress line, the config fixture,
the session-context and instructions tests.

Measure afterwards (ADR-0024): the `longest quiet window` lines of passing jobs against the
window (the false-stall rate), and how many stalls ended by themselves.

Lead review: deviations accepted (an agent cancels a stalled job by cancelling its own `verify`,
which ends the job when nobody else waits for it; any log byte counts as progress, judged later
from the longest-quiet-window log lines). One clippy fix (`needless_borrow` in
`server.rs`). Full fmt, clippy and test through Sluice: pass.
