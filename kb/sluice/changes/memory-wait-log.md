---
title: "Change: log memory waits"
tags: [sluice, change, memory, scheduler, observability, adr-0019]
---

# Change: memory waits in the daemon log

Rule: [[sluice/design/scheduler.md]] (memory rule). ADR: [[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]].
Written without running tests (the `cargo test` hook): `cargo fmt` and
`cargo check --workspace --all-targets -j 4` only.

## Why

Memory waits are folded into the budget accumulator (`jobs.wait_*_ms`) and the wire
reason (`budget`), both frozen, and nothing was logged. After a week of use there was
no way to tell how long jobs waited for memory, which the review of memory-aware
admission needs.

## What changed

- `scheduler.rs`: `Job.memory_wait_ms` (time under `WaitCause::Memory`, added in
  `Job::account` next to the unchanged budget accumulation) and
  `Job.memory_blocked_at` (first pass that left the job memory-blocked, the once-flag).
  `MemoryNote`, `memory_blocked_line`, `memory_started_line` (pure formatting, GiB via
  `sluice_gate_proto::gib_text`), `Scheduler::take_memory_notes`. `admit_with` queues a
  note on the first memory block and when a job with `memory_wait_ms > 0` starts;
  `plan` is untouched. Headroom for a start note is taken before the pass starts jobs,
  for a block note after (the jobs the pass started count as running).
- `server.rs` `admit`: drains the notes under the same lock and logs them at `info`.
- No schema change, no wire change, no `done` outcome field.

## Log lines

```text
job <id> (<repo basename>:<gate>) waits for memory: expected <x.x GiB> learned|guessed, free <y.y GiB>, running headroom <h.h GiB>
job <id> (<repo basename>:<gate>) started after waiting <s>.<d>s for memory (expected <x.x GiB> learned|guessed, free at start <y.y GiB>, reserve <z.z GiB>)
```

`free` is `MemAvailable`. Docs: `docs/debugging.md` ("A gate that waits for memory").

## Verification

Tests in `scheduler/tests.rs`: `memory_waits_are_accumulated_apart_and_noted_twice`,
`budget_waits_leave_no_memory_note`, `the_memory_log_lines_have_a_fixed_format`. Not run
yet (hook); `cargo check` passes.
