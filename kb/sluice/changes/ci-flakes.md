---
title: "Change: intermittent CI failures of the fast job and the matrix"
tags: [sluice, change, ci, flaky, executor, reaper, tests, git-maintenance, psi]
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

# Change: v0.3.0 stabilisation, after the scheduler rounds (2026-10-23)

Three CI failures on commits `ca35df7` and `1a7d17f`, plus one more found
in the logs of the same runs. Every reproduction below ran with
`TMPDIR=/var/tmp/slx` (this machine's `/tmp` is a quota-limited tmpfs; the
`update.rs` tests copy the 170 MB debug binary and hit `EDQUOT`, an
artefact of the local machine, not of the suite).

## 3. `run.rs` `real_index_and_object_store_are_never_touched`

Run 39881190063 (fast CI, git 2.55.0 on `ubuntu-26.04`):
`objects were written to .git/objects` at `crates/sluice-gate/tests/run.rs:97`.
The `left` (after the two `sluice run`) lacks `.git/objects/maintenance.lock`,
the `right` (the listing taken *before* Sluice ran) has it: the lock was
there before Sluice started and was gone afterwards.

**Root cause: the test's own setup commit.** `git commit` ends with
`git maintenance run --auto --quiet --detach` (`GIT_TRACE`). Up to git 2.53
the foreground `maintenance run` takes `objects/maintenance.lock`, forks the
detached child and removes the lock before `commit` returns (strace: the
child's own `unlink` gets `ENOENT`). With git 2.56 (`alpine:edge`, used as
a stand-in for the runner's 2.55) the detached child owns the lock and
removes it when it finishes, after `commit` has returned. Probe (100 fresh
repositories, one commit each, lock checked right after `commit`):
git 2.53 0/200 (also under load); git 2.56 94/100; git 2.56 with
`-c maintenance.auto=false` 0/100.

**Did Sluice ever touch the object store?** Not on the cache path.
`GIT_TRACE` of a miss and a hit shows only `rev-parse`, `add -A`,
`ls-files -s -z`, `ls-files -v -z` and `write-tree`, with the private index
and object directory; none of them runs auto-maintenance, and the real
`.git/objects` kept the same four objects. The merge queue, however, runs
`fetch` and `merge` against the user's repository (its worktrees share the
user's object store), and both start `git maintenance run --auto`; so
Sluice could trigger git's auto-maintenance (repack, prune of loose
objects) in the user's repository, outside the cache-check promise.

**Fix.**
- Product (`crates/sluice-gate-core/src/git.rs`): new
  `NO_AUTO_MAINTENANCE = ["-c", "maintenance.auto=false", "-c", "gc.auto=0"]`
  and a private `git_binary()`; `output` (behind `run`, `run_raw`,
  `run_output`) and `command` (behind `run_output_deadline` and the queue's
  push) start every git with it. `-c` also reaches git's own
  subprocesses through `GIT_CONFIG_PARAMETERS`. New unit test
  `spawned_git_never_starts_auto_maintenance` (`add`, `commit`, `checkout`,
  `merge`, `fetch` through `run`, and a `commit` through `command`, under
  `GIT_TRACE`); it fails without the options (the trace shows
  `git maintenance run --auto --quiet --detach`).
- Tests: the setup git of the shared helpers starts with the same options
  (`crates/sluice-gate/tests/common/mod.rs` `Sandbox::git_in`,
  `crates/sluice-gate-queue/tests/common/mod.rs` `git_in`,
  `crates/sluice-gate-daemon/tests/daemon.rs` `git_in`, and
  `test_support::git_with` in `sluice-gate-core`, which also backs
  `tree.rs`'s `real_index_and_objects_are_untouched`). Only the setup calls
  get them, not the sandbox config or environment, so what Sluice spawns
  still runs with git's defaults and the object-store assertion stays
  strict.

## 4. `chaos.rs` `a_restarted_daemon_kills_the_gates_a_dead_daemon_left_behind`

Run 39881190103 (matrix, git 2.43): `left: None, right: Some(0)` at
`chaos.rs:276`, i.e. `status --json` had no `running` array.

**Root cause: test timing.** `sluice daemon` returns as soon as it has
forked. `server::run` reconciles orphans (kills the dead daemon's groups,
closes their rows) *before* it binds the socket (reaper start, sanitised
environment through git, extensions in between). The test waited for the
orphans to die and for the row to close, then called `status --json`
once; in the gap it got `{"schema":1,"daemon":null}`. The startup order is
intended (no client is served before orphans are gone).

**Fix (test only):** wait until `status --json` reports a daemon object
before asserting `running == []`; the fixed 500 ms sleep before the
restart is replaced by waiting until the killed daemon is gone (with its
reaper already gone, nothing else can kill the orphans).

## 5. `daemon_cli.rs` `stop_waits_for_the_drain_and_the_next_run_gets_a_fresh_daemon`

Run 39879186725 (fast CI, `ca35df7`): `the gate never started` at
`daemon_cli.rs:288` (10 s wait for `sleep 1.05`).

**Root cause: PSI admission in the test sandbox, not the scheduler
change.** The default `Sandbox` had no user config, so PSI admission was on
and read the machine-wide `/proc/pressure/cpu`. Whenever `some avg10`
passes 60 % (a loaded runner), the daemon holds every gate until the
5-minute starvation guard. Reproduced with 80 busy loops (PSI 85–91 %): the
binary took 301.9 s and failed both `the gate never started` assertions
(this test and `killing_the_client_kills_the_gate_and_the_daemon_survives`,
`daemon_cli.rs:166`); every other test of the file waited the full
starvation guard. With budget 1 and weight 1 the new admission-time
weight resolution cannot hold the job (`queued_weight` clamps to
`1..=w_cfg`), and the job ran at once whenever PSI was calm.

**Fix (tests only):** `Sandbox::new` writes a user config and the new
`Sandbox::write_user_config(body)` (used by every test that writes one:
`daemon_cli`, `run`, `mcp`, `queue_cli`, `queue_e2e`,
`queue_transports`, `shim`, `debugging`) adds `[admission]
psi_enabled = false` unless the body has its own `[admission]` table.
`exec.rs`, `shim.rs`'s `setup` and `chaos.rs` already did this by hand.
PSI admission keeps its own tests (`psi.rs`, scheduler tests,
`sluice-gate-daemon` tests with a fake PSI root).

## 6. `shim.rs` `ten_concurrent_shimmed_makes_stay_within_the_budget`

Run 39879186669 (matrix, `ca35df7`): `/proc peak 3 > 2` at `shim.rs:504`;
the start/end counter peak was within the budget.

**Root cause: the test's `/proc` probe over-counted** — the artefact fixed
in `exec.rs` (section 2) but not in the shim test's copy of `proc_count`:
the fake `make` script forking `sleep` is two processes with the same
cmdline until the `exec`, and the scan is not a snapshot.

**Fix (test only):** `proc_count` keeps leaders (parent does not match)
and re-checks them after the scan, as in `exec.rs`. The test now also
asserts that no run fell back in-process (a fallback holds the `flock`
budget, not the daemon's, so the peaks would prove nothing about it); it
never did in any reproduction.

## Verification (this round)

Targeted loops (`one.sh` in the session scratchpad: one test binary
repeated; "pinned" = test and 4 busy loops restricted to CPUs 0–1 with
`taskset`; "heavy" = 80 busy loops on all 20 CPUs, PSI 85–91 %):

| Test | Before | After |
|---|---|---|
| `real_index_and_object_store_are_never_touched` | not reproducible with local git 2.53 (lock 0/200); 94/100 lock probe on git 2.56 | lock probe 0/100 with the options; product unit test fails without them |
| `a_restarted_daemon_kills_the_gates_a_dead_daemon_left_behind` (pinned) | 4/40 (`left: None, right: Some(0)`, the CI symptom) | 0/60 |
| `daemon_cli` binary (heavy) | 3 of 41 binary runs failed (`the gate never started`, one or both tests above) | 0/25 |
| `ten_concurrent_shimmed_makes_stay_within_the_budget` (pinned) | 8/30 (`/proc peak 3 > 2`) | 0/40 |

Whole-suite stress loop (`cargo test --workspace --no-fail-fast -q`,
failures grepped for `FAILED|panicked`; "load" = `nproc` (20)
`sh -c 'while :; do :; done'` loops started and killed by the script):

| Run | Result |
|---|---|
| Before the fixes, with load (2 iterations; a first attempt on `/tmp` hit `EDQUOT` in `update.rs` and was discarded) | 2/2 clean |
| After the fixes, with load | 5/5 consecutive clean (168–184 s each) |
| After the fixes, without load | 5/5 consecutive clean (100–103 s each) |

On this 20-core machine plain `nproc` load neither pushes the system-wide
PSI past 60 % nor slows the daemon enough, so the whole-suite loop did not
reproduce any CI flake on its own; the targeted loops above did (pinning to
2 CPUs, or 80 loops). No other flaky test showed up in any loop.
