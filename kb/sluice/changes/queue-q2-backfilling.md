---
title: "Queue Q2: backfilling behind a reserved head (ADR-0023)"
tags: [sluice, changes, queue, scheduler, backfill]
---

# Queue Q2: backfilling behind a reserved head

Wave Q2 of [[sluice/plan/queue-scheduling-plan.md]] (S1). Decision:
[[sluice/design/adr/0023-backfilling-behind-a-reserved-head.md]]; evidence and replay method:
[[sluice/research/queue-scheduling-analysis.md]] sections 1.2, 1.4, 1.5, 2.1, 2.2, 2.6, 4.1, 4.4.
Status: implemented; verification through Sluice pending (the lead runs `fmt`, `clippy` and
`test`). Isolated runs during the work: the new scheduler module (34 tests), the starvation
bound, the memory property tests and the differential test, all green.

## What changed

1. **Snapshots.** `RunningSnap` carries `started_at`, `expected_ms` and `backfilled`.
2. **`shadow()`** (`scheduler/shadow.rs`, pure). The holders of budget and memory (the
   running jobs, plus the jobs the pass started before the head) are released in order of
   predicted end until the reserved head fits budget, repository cap and memory. The
   predicted end is `started_at + expected` (`DEFAULT_EXPECTED_MS` when the job has no history:
   it only feeds this prediction, never a candidate's bound); for a job at or past its
   expectation the remainder is the pessimistic `0.1 x expected + 2 s` (which equals
   `max(expected - elapsed, 0.1 x expected + 2 s)` there). Memory uses the admission accounting
   of ADR-0019 with Q1's estimates: a released job returns what it holds now plus what it is
   still expected to grow by (`current + headroom`, a quarter of the estimate for a probe);
   the head needs its estimate (learned, lower bound or guessed) to be at most the free memory
   plus what the released jobs return, or nothing may be left running (the progress exemption).
3. **`room()`** (pure). What a job started now may take without delaying the head at its
   shadow time: budget `budget - (jobs still running then + every running job flagged
   backfilled) - head weight`; memory `free now + what the released, non-backfilled jobs return
   - head estimate`. The room is closed when the head does not fit beside the counted jobs
   (it would need the progress exemption, or backfilled jobs already take its memory), and
   candidates of the head's repository are refused when that repository has a cap.
4. **The two rules** (`Pass::backfill_rule`), applied in `Pass::nominal` to a job that fits
   budget, cap and memory now while a head is reserved and `[scheduler] backfill` is on:
   (a) `ExtraResources`: it fits the room (units and its estimate; a guessed candidate uses its
   estimate); (b) `EndsInTime`: it has a learned duration and
   `now + max(1.5 x expected, expected + 10 s) <= shadow`. Rule (a) is tried first. Every job
   started behind the reservation, by either rule, takes its units and memory out of the room
   for the rest of the pass, and later passes count a still-running one in full through the
   `backfilled` flag, as the ADR asks.
5. **Measured.** `Plan::shadow` and `Plan::backfills`; `Scheduler::backfill_stats()`
   (`backfilled`, `head_delay_ms`): the first shadow of each reserved head is remembered
   (`first_shadows`), and when the head starts the delay `max(0, start - first shadow)` is added
   up. The log says `job X (repo:gate) starts before the reserved head job Y (repo:gate)
   (extra resources | ends in time): expected 3.4s, the head is predicted to start in 244.0s`
   (`BackfillNote`, `backfill_line`, `take_backfill_notes`, drained in `server::admit`).
6. **Visible.** `QueuedView::shadow_at` (the reserved head only) becomes
   `queued[].reserved_eta_ms` in `status --json` and `  reserved, eta 4m05s` in the text status
   (CLI and MCP); `totals.backfilled` and `totals.backfill_head_delay_ms` in `--json`, and a
   `backfill:` line in the text totals once something backfilled. All additive; `schema` stays 1.
7. **Switch.** `[scheduler] backfill` (default `true`; `SchedulerConfig::backfill`,
   `Policy::backfill`). `false` restores the strict reservation (nothing behind the head
   starts); the shadow and ETA are still computed and shown.

## Probes and backfill do not compound

A memory probe starts a job on a quarter of its estimate; a backfill needs the job to be shown
not to delay the head, counted at its full estimate. Mixed, a guessed 14 GiB job would be
counted as 3.5 GiB beside the head and could hold four times that. So:

- Behind a reserved head `Pass::nominal` never grants a probe (`probe` needs no reservation);
  a candidate must fit its **whole** estimate now, and rule (a) counts that whole estimate
  (`MemPass::peaks`) in the room. A job that fits only as a probe waits with cause `Memory`.
- A probe that started before the head became reserved is a holder like any other: it hands
  back `current + max(0, quarter - current)`, the same figure admission holds back for it, and it
  is recomputed from its real working set every pass; one probe per machine and per key still
  holds (a candidate of a probing key does not start either).
- A probe is not flagged `backfilled`, so no later pass counts it as taking room.
- Property: the memory-admission property test (`started_jobs_never_exceed_free_memory_unless_alone`)
  now runs with backfill on in half of its 32 switch combinations; probes stay bounded by one
  per pass.

## Deviations and interpretations

- `Policy::fifo` (ADR-0007 policy, used by most scheduler tests) sets `backfill: false`; the
  daemon policy takes it from the config (default `true`). That keeps every ADR-0007 test at
  its strict-reservation meaning.
- Over-admission is unchanged, so a pass that backfills starts a job nominally and, as before,
  does not over-admit the head in that same pass (the existing rule: nothing started nominally).
- The head delay is measured for every reserved head, also with `backfill = false` (a baseline
  of how good the shadow is) and with no backfilled job; it is summed and clamped at zero, per
  daemon instance like `over_admissions`. The count and the delay are not split per head.
- The shadow is fixed when the head is reserved, once per pass. A backfilled job's own
  predicted end is irrelevant to it: by construction the head fits at the shadow beside every
  counted job.
- `backfilled` flag on `RunningSnap` is an addition to the plumbing the ADR lists (needed to
  count a job backfilled by an earlier pass for the whole interval).
- Wire names chosen here: `reserved_eta_ms` (milliseconds from the report, not a timestamp),
  `backfilled`, `backfill_head_delay_ms`.
- A job of a head's capped repository never takes room by rule (a) (conservative; rule (b)
  still applies).
- The test run time of `no_starvation_under_any_switch_combination` is already about 45 s, so
  its switch combinations were not doubled; the replay property test, the bound test (half of
  its combinations backfill) and a new module cover backfill starvation instead.

## Files and symbols

- `crates/sluice-gate-daemon/src/scheduler/shadow.rs` (new): `BackfillRule`, `Holder`, `Head`,
  `Machine`, `Shadow`, `Room`, `predicted_end`, `shadow`, `room`, `ends_in_time`.
- `crates/sluice-gate-daemon/src/scheduler.rs`: `Policy::backfill`, `RunningSnap::{started_at,
  expected_ms, backfilled}`, `Plan::{shadow, backfills}`, `QueuedView::shadow_at`, `Pass::{holders,
  reservation, reserve, backfill_rule, wait_cause, start, nominal}`, `Reservation`,
  `MemPass::cost`, `headroom_of`, `running_release`, `BackfillNote`, `BackfillStats`,
  `backfill_line`, `Scheduler::{note_shadow, record_start, take_backfill_notes, backfill_stats}`,
  `Job::backfilled`.
- `crates/sluice-gate-daemon/src/server.rs`: `policy`, `admit` (log), `status_report`.
- `crates/sluice-gate-core/src/config.rs`: `SchedulerConfig::backfill`; fixture
  `tests/fixtures/unreleased/scheduler-backfill.toml`, test in `tests/compat.rs`.
- `crates/sluice-gate-proto/src/lib.rs`: `QueuedJob::{reserved_eta_ms, reserved_text}`,
  `Totals::{backfilled, backfill_head_delay_ms}`.
- `crates/sluice-gate/src/status.rs`, `crates/sluice-gate-mcp/src/text.rs`: eta and totals text.
- Docs: `docs/how-it-works.md`, `docs/configuration.md`, `docs/cli.md`, `docs/debugging.md`,
  `docs/compatibility.md`, `kb/sluice/design/scheduler.md`, `CHANGELOG.md`.

## Tests

`scheduler/tests/backfill.rs`: `predicted_end` (not overdue, no history, overrun remainder),
`shadow()` (order of release, already fits, budget-bound, memory-bound, too big for the machine,
repository cap, overrun), `room()` (survivors, head, backfilled counted, closed room, capped
repository, take), the duration rule's margins; the pass: the evidence case of section 2.1 (a
105 ms linter behind a memory-blocked suite starts; strict reservation leaves it
`head-reserved`), a job that does not fit now never backfills, extra resources refuse a job that
would take the head's memory, duration accepts and refuses (including a suite past its
expectation), a guessed candidate on its estimate only, a probe never backfills (and still
probes without a reservation), a running probe's release, every job backfilled counts (same
pass and earlier pass), the switch, PSI blocking, a non-overdue head, no over-admission as
backfill, the capped repository; the scheduler: log line, counters, ETA in the views, head delay
(5 s late, 0 when early), counting across passes; a replay property test: 12 random
open-loop days (70 jobs, learned and first-run, 12 units, 30 GiB, heavy 18 GiB suites that
serialise by memory), strict against backfill: no heavy job starts more than 15 s later (the
overrun margin of a duration-backfilled job plus the tick), the switch off backfills nothing,
and more than 20 jobs backfill, so the replay exercises the rules. Also: proto additive fields,
status and MCP text, the config fixture.

Measure afterwards (ADR-0023): summed and p95 wait, waits over 30 s, `backfill_head_delay_ms`
against `backfilled`, share of backfilled jobs that overran their bound, memory kills (must not
rise).

Lead review: deviations accepted. Clippy fixes: `ServerMessage::Status` carries
`Box<StatusReport>` (`large_enum_variant`; `serde(flatten)` keeps the wire format); the
`backfill: bool` of `Policy` became `reservation: HeadReservation` (`Strict | Backfill`,
`HeadReservation::backfill(on)`) for `struct_excessive_bools`; `server.rs::status_with_queue`
was extracted from `handle_connection` (`too_many_lines`). Full fmt, clippy and test through
Sluice: pass.
