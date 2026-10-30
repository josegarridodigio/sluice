---
title: "ADR-0026: Memory admission keeps what it learned, and the agents' oomd window outlasts a gate kill"
tags: [sluice, adr, daemon, memory, admission, containment, systemd-oomd]
---

# ADR-0026: Memory admission keeps what it learned, and the agents' oomd window outlasts a gate kill

Status: accepted (2026-10-30, user decision; decision 7: a 15 s window, not `omit`). Research: [[sluice/research/memory-review-2026-10-30.md]]. Amends
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]] (decision 1),
[[sluice/design/adr/0022-first-run-memory-estimate.md]] (decision 3) and
[[sluice/design/adr/0017-agent-resource-containment.md]] (item 4, the derived duration).

## Context

This is the one-week review of memory admission. On the 12-thread, 30 GiB machine, running
4 to 10 agents, 0.8.0 had 11 global OOMs (13 recorded kills) and 7 systemd-oomd kills in 37 hours. 16 gate
jobs were memory-killed and one agent session was killed. The developer machine had none.
Each OOM had one of two causes:

1. **Two heavy jobs of the same gate admitted together** (2026-10-29, gates held
   19-22.7 GiB). This came from three gaps in admission:
   - The learning key includes the extra arguments, and agents vary them: 7 hashes of one
     gate in a day, and 29 of another overall. A new hash is guessed at 1.3-1.7 GiB for a
     gate that needs 11-14 GiB.
   - A first-run probe needs only a quarter of that guess free, and it may run beside a job
     of the same gate under another hash.
   - A `lower-bound` estimate also probes at a quarter, which lets a second heavy job in
     beside it.
2. **One heavy job plus growth outside gates** (2026-10-30). Gates held 11-15 GiB, and
   everything else held 14.5-18.5 GiB: 8-10 agent sessions and 290-400 processes. Swap was
   full at every OOM. Admission measured free memory at the start of the job, and the rest
   of the machine then grew by several GiB during its 8-10 minutes.

Three things made it worse:

- After a kill, a killed peak counts 1.25 x 1.2 = 1.5 x. After kills at 12.7 GiB the
  estimate was 19.5 GiB, while the runs that followed peaked at 11-13 GiB.
- With the estimate inflated, jobs only started through the idle-machine exemption (15
  times, 3 of them killed), and the log does not say when it is used.
- The PSI starvation guard started two jobs 3-4 s after a kernel kill, while the gates'
  scope pressure still showed the dead job. oomd then killed both.

Finally, the agents' scope policy (30 % for 5 s, derived as a quarter of the vendor's 20 s)
killed an agent's `ui` leaf 6 s after a gate kill, at 30.14 %. `user.oomd_avoid` only
reorders candidates (W6d spike, T-W6-16b), and `tools` leaves exist only while a tool runs.

The agents' reaction fed the loop. Rule 6 of the agent policy says to lower a killed gate's
footprint, so agents added `--test-concurrency=1` or filters. That made a new hash and
therefore a fresh guess, and it barely changed the peak (11.3-13.6 GiB against 12-14 GiB).

## Decision

1. **An unknown hash inherits its gate's memory.** Learning keeps the key
   `(repo, gate, cmd_hash)`, so durations still separate a filtered run from a full one
   (ADR-0011 M2). When a key has no runs, its memory estimate is the largest expectation
   among keys of the same `(repo, gate)`, with basis `family`. A `family` estimate is
   evidence: it never probes, and it does not over-admit (over-admission still needs the
   key's own learned peak). The rule "no probe beside a running job of the key" compares
   `(repo, gate)`, not the hash.
2. **Only a guess probes.** The ADR-0022 probe, which needs a quarter of the estimate free,
   applies to `guessed` estimates only. A `lower-bound` estimate is admitted on the whole
   of it and holds the whole of it while it runs.
3. **A margin is applied once.** The expected peak becomes
   `max(1.2 x largest passing peak, 1.25 x largest killed peak)` over the newest five runs.
   The killed margin no longer multiplies the general one.
4. **Admission settles after a memory kill.** After any kernel or oomd kill of a gate job,
   no job is admitted until 10 s have passed and the memory `some avg10` of the gates'
   scope is below `[admission] memory_some_avg10_low`. The PSI starvation guard cannot
   override this settling.
5. **The reserve follows what runs outside gates.** Every second, the sampler records memory
   outside gates: `MemTotal − MemAvailable − Σ working sets of running jobs`. Each finished
   job stores how much that figure grew during its run, from its start to its maximum. For
   jobs whose own peak reached at least 1 GiB, the reserve becomes
   `max(configured reserve, P90 of that growth over the last 20 such jobs)`, capped at 25 %
   of `MemTotal` so admission never freezes; with fewer than five such jobs the configured
   reserve applies. `status` shows the reserve and its basis (`configured | observed`).
6. **The idle-machine exemption stays, and says so.** Progress needs it: 12 of its 15 starts
   finished, and free memory never reached the inflated estimates. When a job starts beyond
   its estimate, the daemon writes an `info` line ("starts alone beyond its estimate:
   expected X, free Y") and `status` flags the job.
7. **The agents' oomd window outlasts a gate kill.** The derived duration of the agents'
   scope becomes `min(15 s, 3/4 x the user manager's)`, never below 1 s (15 s under the
   vendor's 20 s); it was `min(5 s, 1/4)`. The limit is unchanged. The gates' scope keeps
   `ceil(agents / 2)`, which gives 8 s. `ui` leaves keep `user.oomd_avoid`. An explicit
   `[containment] memory_pressure_duration` still overrides.
8. **The kill message stops sending agents into the loop.** `KILLED for lack of memory`
   now states the gate's measured peak, the free memory when it was killed and the
   memory outside gates. When extra arguments did not lower the peak of earlier runs, it
   says so, and it advises to retry unchanged once memory is free, or to tell the human.
   The agent policy's rule 6 and the skill change to match.

## Consequences

- Two heavy runs of the same gate cannot start together on a guess. In the recorded
  2026-10-29 sequences, decisions 1 and 2 hold the second test back: 4 of the 11 kernel OOMs
  and 4 of the 7 oomd kills.
- Estimates come within about 20 % of real peaks after a kill (16 GiB instead of 19.5 GiB),
  so fewer jobs need the exemption.
- Decision 5 holds back heavy gates on machines where agents grow. It addresses the
  2026-10-30 kills, but its value is a P90 that is still to be measured. The 25 % cap means
  a machine whose agents use more than that is still at risk, and decision 8 tells the
  human.
- oomd acts later on both scopes: 8 s for gates instead of 3 s, and 15 s for agents instead
  of 5 s. A runaway `tools` leaf lives about 10 s longer. The global OOM killer still
  prefers gates (`oom_score_adj` 800), and the vendor policy (50 % for 20 s) still comes
  last.
- New data: a table `outside_growth` (containment schema 4, additive; older binaries never
  read it), the `family` basis, `reserve_basis` and `memory_beyond_estimate` in `status`
  and JSON.
- Measured afterwards (research section 10): memory kills per day, kills with two heavy
  jobs of one gate running (target 0), agent `ui` kills (target 0), jobs started within
  10 s of a kill (target 0), and estimate against peak per family.

## Alternatives considered

- **Drop the extra arguments from `cmd_hash`.** Simpler, but a filtered run's duration
  would pollute the full run's expected duration, which backfilling relies on (ADR-0023).
  The family fallback keeps both.
- **Remove the idle-machine exemption.** The recorded data says it would have stalled the
  heavy gate for hours on the second machine (free memory never reached its inflated
  estimate) without preventing the three kills, which came from growth outside gates.
- **`user.oomd_omit` on `ui` leaves.** Agents would never be oomd candidates. When only
  `ui` leaves are under pressure, though, the vendor policy on the user manager would pick
  another leaf, possibly a desktop service, as in the S2 incident. The user chose the
  longer window instead.
- **Turn on the memory guard by default** (ADR-0019 decision 7). It kills the same gate the
  kernel would, slightly earlier. It does not stop the growth outside gates, so it stays
  opt-in.
- **A per-job `memory.max`.** It turns global OOMs into in-job OOMs, but needs a limit per
  gate that nobody configured. It stays opt-in (`gates_memory_max`).
