---
title: "Change: two intermittent failures of the fast CI job"
tags: [sluice, change, ci, flaky, executor, reaper, tests]
---

# Change: two intermittent failures of the fast CI job (2026-10-23)

Both failed once each on GitHub (`ci.yml`, job "fmt, clippy, test") and
passed on rerun. One was a real product bug, the other a measuring error
of the test.

## 1. `chaos.rs` `a_sigkilled_daemon_takes_its_gates_and_their_children_with_it`

Run 39868356403: `gate processes survived the daemon: [3700, 3702, 3703]`
at `crates/sluice-gate/tests/chaos.rs:114`, after the full `LIMIT` (20 s).

**Root cause: product bug.** The reaper itself works on GitHub runners
(the test found it, and the sibling test
`a_restarted_daemon_kills_the_gates_a_dead_daemon_left_behind` passed in
the same run); it reacts within milliseconds of the daemon's death, as
soon as the kernel closes the pipe. It never heard of this gate.
`executor::spawn` awaited `apply_priority` (`renice`, then `ionice`, two
child processes with the default `nice = 10` and `ionice_class = idle`)
before returning, and only then did `server::launch` write
`+ <pgid> <ticks>` to the reaper and `mark_started` to the store. The gate
runs from the fork, so `sh` and both sleeps were up while the daemon still
waited for `ionice`; the test saw the three processes and SIGKILLed the
daemon inside that window. The gate was in neither the reaper nor the
store, so nobody would ever kill it (the pid gap 3701 is the `renice`).

**Fix (`crates/sluice-gate-daemon/src/executor.rs`):** `spawn` no longer
applies priorities; `Running` keeps `nice` / `ionice` and `Running::wait`
starts `apply_priority` in a background task. `launch` calls `wait` only
after `track` and `mark_started`, so the registration now strictly
precedes the slow helpers and nothing awaits between the fork and the
`+` line. `server.rs` is unchanged (owned by the scheduler task), so
`spawn` stays `async` (with `#[expect(clippy::unused_async)]`); it can
become a plain function once `server::launch` drops the `.await`.

**Tests (`crates/sluice-gate/tests/chaos.rs`):**

- New `a_daemon_killed_while_it_sets_gate_priorities_still_reaps_the_gate`:
  a slow `ionice` shim (`sleep 3`) first in the daemon's `PATH` holds the
  daemon in the old window; the test waits until the three gate processes
  and the shim run, SIGKILLs the daemon and requires the gate to die.
  Deterministic: fails on the old executor (3 survivors, every run), passes
  on the new one.
- `start_forking_gate` now also waits for the client's
  `sluice: long running (job …)` line (the `Started` event is sent after
  the reaper registration and the store write), so the existing tests kill
  the daemon only once the start is complete, instead of relying on the
  window being short. A `Client` helper collects stderr while the client
  runs.
- The forking sandboxes disable PSI admission: under CPU pressure the gate
  stayed queued past `LIMIT` (`long queued (position 1, 0 of 3 units
  busy)`), seen once in the load loop below.

## 2. `exec.rs` `twenty_concurrent_execs_respect_the_budget`

Run 39869319004: `/proc peak 5 > 4` at `crates/sluice-gate/tests/exec.rs:445`;
the log-based counter peak (`counter peak <= 4`) and the daemon's own
`budget.used <= 8` both held.

**Root cause: the test's `/proc` probe over-counted.** `proc_count` counted
every process whose cmdline is `sh -c <script> <tag>`. Two artefacts make
that more than the number of running jobs: (a) when a job's `sh` forks
`sleep`, the child carries the parent's cmdline until its `exec`, so one
job briefly counts twice; (b) `/proc` is read entry by entry, not as a
snapshot, so a job that ends early in the scan and its successor that
starts late in it are both counted. Both windows widen on a loaded runner.
The budget was never exceeded.

**Fix (test only):** `proc_count` keeps only leaders (a match whose parent
also matches is a fork copy) and re-checks every leader after the scan,
counting those still running; all of them were alive at that same instant,
so the count is a true lower bound of the concurrency and never exceeds it.
The assertion `peak_proc <= 4` is unchanged, as is `peak_proc >= 1`.

## Verification

Load: busy loops (`sh -c 'while :; do :; done'`) pinned with the test to
CPUs 0–3 (`taskset -c 0-3`, 8 loops), each test run alone with
`cargo test -p sluice-gate --test <file> <name> -- --exact` in a loop
(script in the session scratchpad).

| Test | Before | After |
|---|---|---|
| `twenty_concurrent_execs_respect_the_budget` | 1/20 failed (`/proc peak 5 > 4`) | 0/80 |
| `a_sigkilled_daemon_takes_its_gates_and_their_children_with_it` | 0/20 locally (40 loops, all CPUs), 1 CI failure | 0/40 |
| `a_daemon_killed_while_it_sets_gate_priorities_still_reaps_the_gate` | 1/1 failed (3 survivors) on the old executor | 0/20 (1/20 before PSI was disabled in its sandbox) |

The original chaos failure could not be hit by load alone on this machine
(20 fast cores); the shim test reproduces the exact CI symptom on demand.
