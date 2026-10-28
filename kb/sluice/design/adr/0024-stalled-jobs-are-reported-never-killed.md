---
title: "ADR-0024: Stalled jobs are reported, never killed"
tags: [sluice, adr, daemon, observability]
---

# ADR-0024: Stalled jobs are reported, never killed

Status: accepted (2026-10-28, user decision: report only, never kill). Research:
[[sluice/research/queue-scheduling-analysis.md]] (sections 1.5, 2.4, 4.2, 4.4). Plan:
[[sluice/plan/queue-scheduling-plan.md]] (S2).

## Context

All four timeouts in the recorded history (a second machine) were one repository's test suite:
in three, the work had finished and one test file sat stuck for 25+ minutes, holding its memory
and its weight until the 30-minute gate timeout; 415 job-minutes of other jobs' waiting fell
inside those windows. Sluice samples a job's CPU, working set and log bytes every second but has
no notion of "no progress". Killing on silence risks false positives (network waits, locks,
`sleep`-based tests) that no recorded data can rule out yet.

## Decision

1. **Detection.** Per running job, every 5-10 s, the daemon reads the job's own counters (the
   leaf cgroup when contained, the process group otherwise): CPU time (`cpu.stat`), block I/O
   (`io.stat`), log bytes, page faults (`memory.stat`) and process count. A job whose counters
   all stay flat for `max(5 min, 3 x expected duration)` is `stalled`; a job starved of CPU
   (its CPU pressure shows waiting) is not stalled. The window is configurable per gate
   (`stall_timeout`, `"off"` for gates that legitimately idle). Progress resets it.
2. **Report only.** `status`, `verify` results, MCP and `client.log` show `stalled` with how long,
   the memory and weight it holds and which signals are flat; `daemon.log` records the start and
   end of each stall and, per finished job, its longest quiet window (the data to judge the rule).
3. **Never killed.** Sluice does not end a stalled job; the gate `timeout` stays the only
   automatic limit. There is no setting to kill on stall (user decision).
4. **Scheduling.** A stalled job counts as ending at an unknown time in the backfill shadow time
   (ADR-0023), so no promise is built on it.

## Consequences

- An agent or a person sees the hang minutes after it starts, with its cost, instead of after
  the timeout, and can cancel it or fix the test; nothing is ever lost to a false positive.
- The queue still waits for the timeout on a hang no one acts on; the agent-facing text tells
  agents to cancel their own stalled job and look at the test.

## Alternatives considered

- Report, then an opt-in kill after a week of traces (recommended by the analysis): declined.
- Releasing a stalled job's CPU weight for admission: possible later if traces show long benign
  quiet periods; not now.
