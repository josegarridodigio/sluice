---
title: "Queue scheduling analysis: backfilling, stalled jobs and first-run memory (S1, S2, S4)"
tags: [sluice, research, scheduler, backfill, stall, memory, admission, queue-scheduling]
---

# Queue scheduling analysis: backfilling, stalled jobs and first-run memory

Read-only analysis for items S1, S2 and S4 of [[sluice/plan/queue-scheduling-plan.md]]
(S3 is out of scope). Nothing was run except read-only queries (`sqlite3` URI `mode=ro`)
and reads of the daemon logs; no daemon was touched. Code lines are those of `main` at
release 0.7.0 (`cfb7d72`). Evidence is marked **measured** (recorded database/log),
**simulated** (replay over recorded data, method and limits in section 2.6), **source**
(code) or **cited** (external source, section 3).

Machines: "machine A" is a 20-thread, 32 GiB developer machine; "machine B" is a second
machine, 12 threads, 30 GiB (the one of the plan's evidence). Both ran 0.6.x on 2026-10-28.

## 0. Summary

| Item | Root cause (measured/source) | Recommended design |
|---|---|---|
| S1 | A head blocked by memory or budget reserves **everything**, even jobs that fit the free budget and memory and would end in seconds (`scheduler.rs:827-833`, `818`). On machine B that day: 359 job-minutes of `head-reserved` wait (48 % of all queued time), 91 % of it small jobs (expected < 60 s, weight <= 2), median 371 s, worst 1597 s. | EASY-style reservation (the head's *shadow start* computed from running jobs) **plus an "extra resources" rule**: a later job may start if, after the head's reservation at the shadow time, budget and memory still fit it, without any duration estimate; else it may start when its learned duration (x margin) ends before the shadow time. Unknown durations use only the extra-resources rule. |
| S2 | Four `pnpm test` runs on machine B hung until their timeout (1800/1800/900/1800 s, expected about 250 s), holding weight 6 and about 8 GiB; 415 job-minutes of other jobs' waiting fell inside those windows. The only supervision is the wall-clock timeout (`executor.rs:327`, 30 min default, `config.rs:291`). | Stall detector in the sampler tick over signals it mostly has already (cgroup/task CPU, working set, log bytes) plus cheap new ones (`io.stat`, `memory.stat` activity, D-state tasks). Phase 1: report `stalled` (no kill). Phase 2: opt-in `stall_kill_after`, window not below 5 min, never on a job whose tree shows I/O or CPU progress. |
| S4 | The first-run estimate is the **weight proxy** `w/B x usable` (13.8 GiB for weight 6 on machine B), not the learned prior (the prior, 14.4 GiB, was larger and `min` picked the proxy, `scheduler.rs:438-441`). Actual peaks of those guessed runs: 0.0-1.0 GiB. The proxy over-estimates by more than 4x in 73 % / 69 % of recorded runs and under-estimates in 2 of 416. A failing gate teaches nothing (`server.rs:1520`, `1709-1722`). | Replace "guess = full proxy" by a **bounded, self-correcting estimate**: for a never-passed key, admit when free memory covers a small share of the guess (the *probe*, with one probe per key at a time and no over-admission), and record **failed-run peaks as lower bounds** so the estimate rises when evidence says so. |

Combined effect on machine B, day 2026-10-28 (simulated, section 2.6): waiting time summed
over jobs 631 min (current policy replayed; recorded 753) -> 282 min with the extra-resources
rule (-55 %) -> 134 min with the extra-resources rule **and** stall detection at 5 min (-79 %).
Stall detection is the only lever that shortens the *time the queue is non-empty* (177 -> 92
min) and the CPU idle while waiting (15.9 -> 5.0 idle CPU-hours); backfilling shortens the
small jobs' waits but not that, because the serialised heavy suites are the critical path.

## 1. Code map

### 1.1 Where decisions are made

- **Pure decision**: `scheduler::plan(&Snapshot) -> Plan` (`scheduler.rs:903`). Passes:
  PSI blocking (`Pass::blocked`, `:783`), nominal pass with backfill and head reservation
  (`Pass::nominal`, `:806`), at most one over-admission (`Pass::over_admit`, `:864`).
  No clocks, no I/O: callers pass `now`.
- **State and application**: `Scheduler::admit_with` (`:1450`) accounts each queued job's
  elapsed wait to its previous cause (`Job::account`, `:1143`), plans, moves started jobs
  to `running`, notes memory-blocked/started lines (`:1470-1499`).
- **Driver**: `server.rs::admit` (`:1413`) gathers PSI (`blocking`, `calm`, guard), idle
  CPUs (`Utilisation`), memory facts (`memory::facts`, capped by cgroups) and the running
  jobs' working sets, then calls `admit_with`. `scheduling_loop` (`:1492`) runs it on every
  wake and every `ADMIT_TICK` (1 s, `:92`).
- **Config**: `[scheduler] head_reserve_after` default 30 s (`config.rs:985`);
  `[admission] memory_aware`, `memory_reserve = "auto"` (10 % of `MemTotal`, `config.rs:906-920`),
  `memory_guard = false`; gate `timeout` default 30 min (`config.rs:291`).

### 1.2 Queue order, weights, budget

- Order (`compare`, `:580`, `rank`, `:606`): jobs overdue by `head_reserve_after` outrank
  everything and are FIFO among themselves; otherwise `human_first`, then lane fairness
  (round-robin by worktree), optionally HRRN (`order = sjf`, off), then arrival. "The head" is
  the first job of this order. `Order::Sjf` already uses `expected_ms` with
  `DEFAULT_EXPECTED_MS = 60 s` for jobs without history (`:19`, `:556`).
- Budget: units; default = online CPUs (ADR-0011). Effective weight per pass
  (`queued_weight`, `:931`): configured weight (ceiling; `auto` = `ceil(budget/2)`, ADR-0015)
  lowered by the learned weight (P75 of the last 10 passing runs' demand, used after 3 runs,
  clamped to `[ceil(w_cfg/4), w_cfg]`, `:957-980`). A job fits when `used + w <= budget`
  (`:815`).
- Over-admission up to `cap = overcommit x budget` (1.5) when PSI is calm, CPUs idle,
  nothing started nominally, and the candidate has a **learned** memory peak that fits
  (`:864-891`).

### 1.3 Memory admission (ADR-0019)

- `MemPass` (`:614-673`): `free = MemAvailable - reserve - held`, where
  `held = sum over running of max(0, expected - current)` (the *running headroom*,
  `running_headroom_of`, `:677`) plus the full expected peak of jobs started earlier in the
  same pass. A job fits if `peak <= free`, **or nothing runs and nothing started in the pass**
  (progress exemption, `:657-659`).
- `current` is the job's last sampled working set: `anon` + swap of its cgroup leaf, or the
  summed RSS of its process group (`memory.rs:150-190`, `server.rs:1259-1284`).
- Expected peak (`MemoryFacts::estimate_primed`, `:422-442`): learned `expected_peak` of the
  key (max of the newest 5 working sets, +20 %, a memory-killed run +25 % more:
  `memory.rs:206-221`; `PEAK_WINDOW = 5`) -> `Expected{learned: true}`; else
  `min(proxy, prior)` -> `learned: false` ("guessed"). `proxy = usable x min(w,B)/B`,
  `usable = MemTotal - reserve`. `prior = prior_peak(priors, w)` = the largest expected peak
  among learned keys whose last weight is <= `w` (`:42-44`; `PeakCache::priors`,
  `memory.rs:265-277`; added in 0.6.1).
- Learning: `learned_memory` (`server.rs:1709`) records passing runs and runs killed for
  lack of memory; other failures teach nothing. Rows live in `gate_memory`, newest 20 per
  key (`store.rs:269`, `1194`); the in-memory window is 5 (`memory.rs`).
  `announce_closed` (`server.rs:1739`) pushes the new peak and priors to the scheduler.
- Logging: first memory-blocked pass and start produce the lines
  "waits for memory: expected X learned|guessed, free Y, running headroom Z" and
  "started after waiting N s for memory" (`scheduler.rs:1048-1069`). Memory waits are folded
  into the frozen `jobs.wait_budget_ms` column and the wire reason `budget`
  (`WaitCause::Memory` shares accumulator 0, `:105`, `:123`); the wire detail is `memory`.

### 1.4 The head reservation and why a small job waits as `head-reserved`

`Pass::nominal` (`scheduler.rs:806-859`), per job in rank order:

1. If no reservation exists yet and the job fits budget, repo cap and memory, it starts
   (`:818-823`).
2. Otherwise, the **first waiting** job becomes the reserved head when it is *overdue*
   (waited >= 30 s) and is blocked by the budget, or by memory with its cap fine
   (`:827-833`).
3. Once `reserved` is set, step 1 is skipped for **every later job** (`reserved.is_none()`),
   so a later job that fits is left with cause `Reserved` (`:848-849`), shown as
   `head-reserved`.

What the reservation protects: the heavy head cannot be starved by a stream of lighter
jobs (ADR-0007): without it, `used + w_head <= budget` (or the memory test) might never
hold because every freed slot is refilled. What it costs: it blocks on **any** inability of
the head, including memory, where the head typically waits for a *whole running suite* to
end (minutes), and it never asks whether the later job would disturb the head at all.
Note the only bounded-wait guarantee is "head_reserve_after + longest running job": a
reservation held by a head whose memory estimate is over-conservative extends it.

A head blocked by its repository cap alone does not reserve (`:827-830` and the comment),
so other repositories are never stopped by it.

### 1.5 Expected durations and progress observation

- Expected duration: `StatsCache` (`server.rs:199-222`) keeps the last 10 passing runs
  `(demand_milli, wall_ms)` per `(repo identity, gate, cmd hash)`; `expected_ms` is their
  **median** (`scheduler.rs:984`). It lands in `JobSpec.expected_ms` at enqueue, in the
  `jobs.expected_ms` column and in `status`. `learns_from` (`server.rs:1520`) is
  `status == Pass` only. Running jobs have `expected_ms` and `started_at` available but
  `RunningSnap` does not carry them (`scheduler.rs:477-491`): a backfill planner has to add
  them.
- Progress observation (once per second, `sample_tick`, `server.rs:1107-1145`, only for jobs
  with a stats key: gates and `exec`): per task `schedstat` (CPU ns and run-delay ns) of
  every process of the job's process group plus the leader's `utime/stime/cutime/cstime`
  (`sampler.rs:300-390`, `JobUsage::update`), and the working set (`observe_memory`). The
  executor counts log bytes (`RunningHandle::log_bytes`, `executor.rs:262`). **Not read
  today**: cgroup `cpu.stat`, `io.stat`, `*.pressure` of the job leaf, `/proc/<pid>/io`,
  process state (`D`/`S`), `wchan`. `cpu.stat` appears only in the containment sampler for
  the whole scope (`containment.rs:483`).
- Timeouts and kills: the executor `select!`s the child, a deadline `started + timeout` and
  a cancel `Notify` (`executor.rs:327-331`); on timeout or cancel `terminate_group` sends
  SIGTERM then SIGKILL after a grace (`:30`, `:732`), outcome `Timeout`
  (`server.rs:2118`, `2221`: "timed out after ..."). `killed_by` is set by Sluice only:
  `memory` (guard or classifier, `memkill`, `server.rs:1229`), `early-verdict`,
  `last-subscriber`, `stop`; `merged_killed_by` (`:1538`) lets only `memory` survive
  `close_job` for non-Sluice cancels. A `Timeout` has `killed_by = NULL` (verified in the
  data). There is no stall notion anywhere.

### 1.6 Cold-start prior (S4) in one place

`estimate_primed` (`scheduler.rs:422`) = learned, else `min(proxy(w), prior(w))`. With
`weight = auto` (budget/2) or an `exec` job of default weight (`ceil(budget/2)`, ADR-0015),
`proxy` = half of the usable memory (13.8 GiB on machine B). The prior only lowers it. A
failing gate never produces a `gate_memory` row (`learns_from`/`learned_memory`), so the key
stays guessed on every run until one passes.

## 2. Evidence

### 2.1 Waiting per day (measured)

Union of `[queued_at, started_at]` intervals = "time with jobs queued" (what `status`
reports). Sum = job-minutes of waiting. Reasons come from `wait_*_ms`; memory is split out
of the budget column using the "started after waiting N s for memory" log lines.

Machine B (12 threads, 30 GiB), jobs that ran (cache hits are not in the table):

| Day | jobs | time with jobs queued | sum of waits | budget-only | memory | PSI | head-reserved | jobs waiting > 5 min | p95 wait |
|---|---|---|---|---|---|---|---|---|---|
| 10-24 | 233 | 47 min | 79 min | 2 | 0 | 77 | 1 | 3 | 141 s |
| 10-25 | 446 | 48 min | 64 min | 10 | 0 | 53 | 0 | 0 | 70 s |
| 10-26 | 392 | 129 min | 250 min | 54 | 11 | 159 | 26 | 17 | 267 s |
| 10-27 | 256 | 39 min | 84 min | 2 | 38 | 22 | 22 | 5 | 164 s |
| 10-28 | 199 | 194 min | 753 min | 13 | 322 | 59 | 359 | 49 | 1269 s |

(`status` since the daemon restart on 10-26: 4h16m with jobs queued, 1h55m of it CPU-idle.)
Memory admission shipped on 10-26: before it, waits were PSI. On 10-28 memory plus
head-reserved are 90 % of all waiting; both come from the same event, a heavy suite
waiting for memory (the head) with everything behind it reserved.

Machine A (20 threads, 32 GiB): days up to 10-26 have 0-11 min of waiting; 10-28 has 64
min with jobs queued, 88 min summed: memory 68, head-reserved 17, budget-only 3, PSI 0
(the test suite's learned peak is 20.5 GiB, so two never coexist; 39 memory-blocked jobs,
all `learned`, none guessed). The daemon on machine A was stopped when this was written, so
its live `status` totals were not available.

**Head-reserved waits** (machine B, 10-28, 40 jobs): 17 jobs had an expected duration < 10 s
and 20 had 10-60 s; 37 of 40 (91 % of the reserved minutes) were weight <= 2 jobs expected
< 60 s. Per job: median 371 s, p90 1359 s, max 1597 s; the median job waited 40x its own
expected run time (p90 2868x; a 105 ms linter waited 15-23 min). The blocking head (the
oldest job still waiting when the small job started) was a weight-6 `test` gate in 27 of 40
cases, a guessed `pytest` in 3, a guessed `docker build` exec in 2. 10-27: 13 jobs, all
`typecheck` (w2) behind a weight-6 `test`. Machine A 10-28: 10 jobs, 17 min, median 97 s.

### 2.2 How reliable are learned durations as a bound (measured)

`jobs.expected_ms` at queue time vs actual wall (finished - started) of passing runs:

| | machine B | machine A |
|---|---|---|
| passing runs with an expected duration | 1217 of 1269 (96 %) | 692 of 901 (77 %) |
| actual/expected p10 / p50 / p90 / p99 | 0.64 / 1.03 / 1.90 / 3.43 | 0.33 / 0.99 / 1.99 / 5.54 |
| > 1.5x expected | 19 % | 18 % |
| > 2x expected | 9 % | 10 % |
| exceeding `1.25x + 5 s` / `1.5x + 5 s` / `2x + 5 s` / `3x + 5 s` | 10.6 / 6.5 / 3.0 / 0.8 % | 11.1 / 7.1 / 3.5 / 2.6 % |
| expected < 10 s: p90 / p99 | 2.10 / 4.17 | 2.63 / 5.61 |
| expected 1-5 min: p90 / p99 | 1.62 / 2.67 | 1.41 / 1.86 |
| expected > 5 min: p90 / p99 | 1.31 / 1.67 | n/a |

Short jobs are noisy in ratio but tiny in absolute terms (absolute overrun p50 about 0,
p90 about 18-28 s, p99 85-170 s); long jobs are tight. Consequence: a multiplicative margin
alone is wrong for short jobs, an additive pad alone is wrong for long ones; and about 3-7
% of runs will exceed any reasonable bound, so the design must tolerate an under-estimate
(section 4, S1).

### 2.3 Guessed memory (measured)

Log of machine B: 71 memory-blocked jobs (3 + 7 guessed on 10-27 and 10-28, the rest
learned). The 10 guessed jobs waited 7481 s in total (125 min), individually 137-1614 s
(`pytest` twice: 1614 s and 1023 s; `docker build` exec 1274 s and 165 s; `pnpm install`
exec 160 s). Their real peaks, where a later passing run recorded one:

| Guess | Real working-set peak |
|---|---|
| 13.8 GiB (pytest, w6) | 0.3 GiB (100 s run) |
| 13.8 GiB (docker build exec, w6) | 0.0 GiB (the client; the memory is in the engine) |
| 13.8 GiB (pnpm install exec, w6) | 0.5 GiB |
| 2.7 GiB (run build exec, w2) | 1.0 GiB |

Correction to the plan's account: the 13.8 GiB came from the weight proxy
(`6/12 x (30.6 - 3.1)`), not from the other repository's 12 GiB suite; the prior there was
14.4 GiB, so `min` selected the proxy. In the failing-gate case (`pytest`, first run
failed after 100 s, 1614 s of wait) the key converged at the second run (passed after
1023 s of wait, peak 0.3 GiB); it did not "never converge" in this instance, but each
non-passing run costs a full wait, and a key that keeps failing (a test gate on a broken
branch) keeps paying.

Quality of the proxy over all learned runs (`gate_memory` joined to the run's weight):

| | machine B | machine A |
|---|---|---|
| runs | 204 | 212 |
| proxy over-estimates by > 4x | 140 (69 %) | 154 (73 %) |
| proxy under-estimates | 0 | 2 |
| peak p50 / p90 / max by weight | w1 0.04/0.05/0.05; w2 0.97/2.16/2.22; w6 4.9/12.1/12.9 GiB | w1 0.03/1.29/1.69; w3 0.31/0.83/1.17; w10 2.2/3.2/7.2; w16 10.5/14.9/17.2 GiB |
| within-key max/median peak (n >= 5) | p50 1.08, max 1.21 | p50 1.63, max 3.45 |

(Only the newest 20 rows per key are kept, so older runs are not in this sample.) The proxy
is almost never too low and very often far too high; a weight-6 first run is the
worst-case figure. Learned peaks are stable once known (machine B), less so on machine A.

### 2.4 Stalled jobs and timeouts (measured)

Four timeouts in the whole history (none on machine A), all machine B, all the same
`pnpm test` gate (weight 6) of two repositories, all on 10-28:

| wall | CPU time | expected | log size | last lines of the log |
|---|---|---|---|---|
| 1800 s | 22 s | 215 s | 11.6 KB | a test file reported as running 1789 s, "Promise resolution is still pending but the event loop has already resolved" |
| 1801 s | 1932 s | 253 s | 65 KB | same message, a file running 1651 s |
| 900 s | 1945 s | none | 66 KB | same, 754 s |
| 1800 s | 1999 s | 260 s | 64 KB | same, 1615 s |

The CPU time of three of the four (about 1.9-2.0 k CPU-s) is what a normal run of that
suite consumes within the first few minutes (expected 250 s at demand 8-16 CPUs): the
suite did its work, then one test file never finished, and the process tree sat idle for
the remaining 25 minutes or more (the first one never got past 22 CPU-s). The memory was
held throughout (the plan measured 7.6 GiB resident; the memory guard is off, and
admission counted the gate's learned peak, 14 GiB, against everything behind it).
Cost: 76 + 155 + 107 + 77 = **415 job-minutes** of other jobs' waiting started and ended
inside those four windows. Limits: the log's mtime is the timeout footer, so the last real
output instant cannot be recovered; "no CPU" is inferred from the cumulative CPU figure,
not from a time series; `jobs` keeps no per-second CPU/log growth. No stall was observed
on a job that was later fine, because Sluice never looked.

Other endings: 36-41 `killed` per machine, all `early-verdict` or `last-subscriber`
(client gone), none stalled. No `timeout` has `killed_by`.

### 2.5 Interaction evidence

- A head waiting for memory because of **another weight-6 suite** plus the reserve is the
  standard picture: the free memory printed in the log at the memory-block line (11-24 GiB)
  was always larger than any small job's need, e.g. "expected 14.6 GiB learned, free 15 GiB,
  running headroom 2 GiB" with `reserve = 3 GiB`.
- Stalled jobs inflate the head's shadow time: while a hung suite holds weight 6 and its
  learned peak, every heavy suite behind it is blocked by memory, and the small jobs behind
  those are `head-reserved`. All 359 reserved minutes and 322 memory minutes of 10-28
  overlap the four windows heavily (section 2.4).
- A guessed head (13.8 GiB) is blocked for a whole suite even though it needs 0.3 GiB: its
  reservation then freezes the queue behind it (3 of the 27+ blocking heads above).

### 2.6 Simulation (method and limits)

Replay of one day of recorded jobs (`queued_at`, actual `finished - started`, effective
weight, expected duration, learned/guessed memory estimate per the daemon's rules,
actual peak from `gate_memory`) under alternative policies, 1 s steps while the queue is
non-empty. Common model: budget = CPUs; memory rule of ADR-0019 with `reserve = 10 %`, a
fixed amount of memory used by everything that is not a gate (machine B 8 GiB, A 9 GiB,
read off the `free` figures of the log), a running job's working set ramping to its peak
over the first 20 % of its run; FIFO with the 30 s reservation; PSI, over-admission, lanes
and humans-first ignored; open-loop arrivals (agents' follow-up requests do not change).
Jobs whose key was never learned and that the log does not show as guessed are treated as
small.

Calibration, 10-28: replayed current policy 631 min summed wait vs 753 recorded on
machine B (84 %), 52 vs 88 on machine A (60 %); CPU-idle while queued 15.9 idle CPU-hours
(about 1.3 h on 12 CPUs; `status` showed 1h12m of idle earlier the same day). Earlier days
are not calibrated (PSI waits, pre-0.6.1 estimates) and are not used.

| Policy (10-28) | B: sum wait | B: time queued | B: p95 | B: waits > 30 s | B: idle CPU-h | A: sum wait | A: waits > 30 s |
|---|---|---|---|---|---|---|---|
| recorded | 753 | 194 | 1269 s | 78 | - | 88 | 34 |
| current policy, replayed | 631 | 177 | 1170 s | 66 | 15.9 | 52 | 17 |
| EASY by learned duration (margin 1.25 / 1.5 / 2, pad 5 s) | 356-359 | 176.5 | 998 s | 37-40 | 15.8 | 38 | 12 |
| EASY with exact durations (upper bound) | 279 | 172 | 545 s | 32 | 15.3 | 31 | 10 |
| **extra-resources rule only** (no durations) | **283** | 176 | 545 s | 33 | 15.7 | 48 | 14 |
| extra-resources rule + EASY by duration | 283 | 176 | 545 s | 33 | 15.7 | 36 | 11 |
| current policy + "guessed job admitted on 25 % of its guess" | 570 | 170 | 1017 s | 63 | 15.2 | 52 | 17 |
| EASY (margin 2) + guess bound | 339 | 175 | 906 s | 40 | 15.5 | 38 | 12 |
| current policy, **hung jobs cut at 1.5 x expected + 10 / 5 / 2 min** | 279 / 216 / 172 | 109 / 92 / 82 | 541 / 396 / 311 s | 48 / 46 / 42 | 7.5 / 5.0 / 3.5 | - | - |
| EASY (margin 2) + hung jobs cut at +5 min | 134 | 92 | 314 s | 29 | 4.9 | - | - |

Findings:

1. In no variant was a heavy job started later than under the current policy
   (EASY variants: 0 of 63 heavy jobs delayed by more than 5 s), including real
   under-estimates, because the jobs that backfill are seconds long while the head waits
   minutes. The result is insensitive to the margin between 1.0 and 2.0.
2. Duration-based EASY recovers about 77 % of the exact-knowledge gain; the
   extra-resources rule recovers all of it (it needs no durations).
3. Backfilling does not reduce "time with jobs queued" or idle CPU on machine B: the heavy
   suites are serialised by memory, that chain is the critical path. Cutting the hangs does
   (177 -> 92 min, 15.9 -> 5.0 idle CPU-hours); the two compose.
4. The guess bound alone gives -10 % here because the replay treats most unlearned jobs as
   small; the guessed-job evidence is better read from section 2.3 (125 min of waiting on
   10 jobs).

Limits: cut-off of the hung jobs is a heuristic (the real instant the CPU stopped is
unknown); the replay's memory model is coarse; durations are unaffected by contention (the
real wall of a backfilled job may be longer under load); no I/O or network wait is
modelled; two machines, one calibrated day each.

## 3. Prior art

**Backfilling.** EASY backfilling (Lifka, ANL/IBM SP) reserves resources for the first
queued job only and lets later jobs start if they do not delay that reservation;
conservative backfilling (Maui, Slurm's default) reserves for every queued job. Slurm's
`sched/backfill` plans start times for queued jobs and starts a lower-priority job only if
that does not delay the expected start of any higher-priority job; it depends on job time
limits ("reasonably accurate time limits are important"), looks ahead `bf_window` (default
1440 min, at least the longest time limit to avoid starvation), at granularity
`bf_resolution` (60 s) every `bf_interval` (30 s) over `bf_max_job_test` (100) jobs
([Slurm scheduling configuration](https://slurm.schedmd.com/sched_config.html)). Runtimes
there are **user estimates, enforced**: a job that exceeds its time limit is killed
(after `OverTimeLimit`, same page), which is why an under-estimate is harmless to the
reservation and over-estimates just waste backfill opportunity. Users inflate estimates
because of the kill. Tsafrir, Etsion and Feitelson, "Backfilling using system-generated
predictions rather than user runtime estimates", IEEE TPDS 18(6), 2007
([paper](https://www.cse.huji.ac.il/~feit/papers/Pred07TPDS.pdf)): history-based
predictions (the mean of the user's last two jobs) beat user estimates, but production
schedulers did not adopt them because an under-prediction would kill jobs; their fix is to
**separate the kill time from the prediction** and to **correct the prediction adaptively**
when a job outlives it (raise it step by step), keeping EASY semantics for users. Sluice's
position is exactly that of their scheme: no kill limit for a backfilled job (the gate has
its own timeout), a learned prediction, and a head that must not be delayed more than a
bounded amount when the prediction is wrong. EASY's other branch (the "extra nodes" rule:
a backfilled job may run past the shadow time if it uses only resources the head will not
need) is duration-free and is the part the replay shows is enough here. (From memory, not
re-fetched this session: Mu'alem and Feitelson 2001 on user estimates in IBM SP2
backfilling.)

**Stall/hang detection in CI.** Wall-clock deadlines are universal; "no output" deadlines
exist in a minority and are always about output, never about process activity:

- CircleCI `no_output_timeout`: default 10 min, "elapsed time the command can run without
  output" ([configuration reference](https://circleci.com/docs/reference/configuration-reference/)).
- Travis CI: a build with no output for 10 min "is assumed to have stalled" and is killed;
  `travis_wait` prints a line every minute to extend it, at the price of hiding real hangs
  ([common build problems](https://docs.travis-ci.com/user/common-build-problems/)).
  The workaround shows the weakness of the output-only signal: any heartbeat defeats it.
- Bazel: per-target wall-clock timeouts by size (60 s / 300 s / 900 s / 3600 s) and
  `--test_timeout`; on timeout the runner "may choose to kill the test and treat the run as a
  failure" ([test encyclopedia](https://bazel.build/reference/test-encyclopedia)). No
  activity notion. GitHub Actions (`timeout-minutes`, default 360), GitLab and Buildkite
  (`timeout_in_minutes`) are also wall-clock only (from memory, not re-fetched).
- Jenkins Build Timeout plugin offers absolute, elastic (a percentage over the average of
  past builds) and "no activity" (console-output) strategies (from memory; the plugin page
  fetched this session did not describe them,
  [plugin page](https://plugins.jenkins.io/build-timeout/)). The elastic strategy is the
  CI analogue of a learned bound.
- Go `go test -timeout` (default 10 min) panics the test binary and dumps all goroutine
  stacks (from memory). pytest-timeout offers a signal method that fails only the test and
  a thread method that terminates the process, both dump all thread stacks, and it tries to
  stay quiet when a debugger is attached (trace function or `--pdb`), switchable with
  `--disable-debugger-detection`
  ([pytest-timeout](https://pypi.org/project/pytest-timeout/)). Per-test timeouts inside the
  runner are the better remedy for the repositories' bug that started S2, but Sluice cannot
  impose them.

**Distinguishing "waiting on I/O" from "stuck".** Output silence is a poor signal (long
compiles, linkers, container pulls, test runners that buffer). Better kernel-side signals,
all per cgroup: `cpu.stat` `usage_usec` (CPU progress), `io.stat` counters
(`rbytes/wbytes/rios/wios`), `memory.stat` activity (page faults, `pgfault`/workingset), and
the pressure files `cpu|memory|io.pressure` (`some`/`full` stall shares,
[kernel PSI doc](https://docs.kernel.org/accounting/psi.html)); per task, state `D`
(uninterruptible, I/O) vs `S` and `wchan`. The Linux hung-task detector (khungtaskd) uses
state `D` for 120 s as its criterion (from memory). A job that blocks on a lock or a
network read sleeps in `S` with no I/O counters moving, which is indistinguishable from a
hang without application knowledge: this is why the window must be long, the kill opt-in,
and the stack/state snapshot recorded. Debuggers, REPLs and `sleep`-style tools are not
gate workloads: gates run with stdin null (`executor.rs`), so interactive sessions are not a
case, but `--watch` modes and servers waiting for a client are (they never end by design;
they also hit the wall-clock timeout today).

## 4. Options

### 4.1 S1: backfilling behind a reserved head

**Needed plumbing** (all small): `RunningSnap` gets `started_at` and `expected_ms`;
`QueuedSnap.expected_ms` already exists; a `shadow()` function that releases running jobs
in order of predicted end and returns the head's earliest start and the set still
running then. `Wait`/`QueuedView` can carry the shadow time (an ETA in `status`).

**Option S1-A: EASY by learned duration only.** Later job starts if it fits now and
`now + expected x margin + pad <= shadow`. Unknown duration (no history): never backfills
(`DEFAULT_EXPECTED_MS` must not be used as a bound). Margin 1.5-2 and pad 5-10 s
(section 2.2: 3-7 % of runs exceed 1.5-2x + 5 s). Overruns: the job is not killed; the
head's delay is bounded by that job's overrun, and the shadow is recomputed every pass, so
a long overrun shows up at once and no further backfill starts. Replay: -43 % summed wait
on machine B, -28 % on A; no heavy job delayed.
Risks: jobs without history never benefit (first runs, 23 % of passes on machine A);
estimates of short jobs are multiplicatively noisy (use additive pad).

**Option S1-B: extra-resources rule only (no durations).** Later job starts if, counting
the head at its shadow time, the jobs still running at that time (conservatively including
the candidate and already backfilled jobs for the whole interval) fit budget and memory:
`sum(w of survivors) + w_k + w_head <= budget` and
`sum(expected peak of survivors) + peak_k + peak_head <= usable`. It cannot delay the head
whatever the candidate's duration. Replay: -55 % on machine B (equal to the exact-duration
bound), -9 % on machine A (there only the head's 20.5 GiB leaves little room, so durations
matter). Works for first runs. Risks: relies on **memory expectations** (a candidate that
outgrows its expected peak can push the head's start; mitigations already exist: running
headroom is recomputed from real working sets every pass, and the guard/oom ordering); on
guessed candidates it is only as good as the guess (a guessed 13.8 GiB candidate rarely
fits, which is conservative).

**Option S1-C: conservative backfilling (reservation for every queued job).** Rejected:
state and `status` complexity for little extra at this scale (not measured: queue lengths were not
recorded); Slurm itself needs `bf_max_job_test` and
planning horizons that Sluice has no use for.

**Recommendation: S1-B first, S1-A on top (S1-AB).** Order of tests: (1) extra-resources
(duration-free, robust); (2) else duration rule with `max(1.5 x expected, expected + 10 s)`
for known durations. The reservation's guarantee changes from "nothing behind starts" to
"nothing starts that is not shown not to delay the head", keeping ADR-0007's no-starvation
property; a head's *measured* start delay relative to its shadow must be logged and
alarmed (counter in `status.totals`: `backfill_head_delay_ms`), and a kill-switch
`[scheduler] backfill = false` kept. Reasons: it removes the dominant cause (small jobs
waiting 40x their run time), it is evidenced to not delay heads even with an
imperfect estimate, and it needs no new knowledge beyond what the daemon already learns.
Do not backfill an `Over` admission (keep over-admission as is) and do not backfill while PSI
blocks. Do not let backfill count for lane fairness (a backfilled job still keeps its rank
for later passes).

What to measure after: summed and p95 queue wait, waits > 30 s, `idle_while_waiting_ms`,
**head delay** (actual start - first computed shadow), backfilled count, share of
backfilled jobs that overran their bound, memory-killed count (must not rise), on the same
recorded workload via the existing `Scheduler` unit-test replays (the decision is a pure
function: golden tests from the recorded days).

### 4.2 S2: stalled-job detection

**Signal set.** Per running job, per tick (sampler already runs at 1 s; use a coarser
internal cadence, 5-10 s, for the new reads): CPU progress (already: task `schedstat` sums;
better: leaf `cpu.stat` `usage_usec`, which also covers processes that left the process
group), working-set change (already), log bytes (already, executor), plus new cheap reads:
leaf `io.stat` (`rbytes+wbytes`, `rios+wios`), `memory.stat` (`pgfault`, `pgmajfault`) and
`pids.current`. A tick is *quiet* when none of: CPU time grew by more than a threshold
(say > 1 % of one CPU over the window), I/O bytes grew, log bytes grew, page faults grew
beyond noise, task/pid count changed. A job is `stalled` when quiet for the whole window.
The reading must not depend on the process group alone (the leaf is the truth for jobs
under containment; the group is the fallback, as in `read_job_samples`).

**Option S2-A: report only** (`stalled` in `status`/`verify`/MCP with the window, memory held
and the quiet signals), no kill. Zero false-kill risk; gives agents and the user the fact
and the cost (it holds weight w and X GiB). Also lets the scheduler treat the job
differently (see S2-C).

**Option S2-B: report, then opt-in kill** (`[daemon] stall_kill_after`, off by default as
the plan says): the daemon cancels the job through `RunningHandle::cancel` (like
`cancel_for_memory`, `server.rs:1217`), `killed_by = "stalled"`, message
"KILLED: stalled (no CPU, I/O or output for N min; held X GiB)", never cached, recorded as
an intervention, and **teaches nothing** (neither weight nor duration nor peak, via the
existing `learns_from`). Before the kill, capture a state snapshot into the log (process
tree with state `S/D` and `wchan`, open `io` counters; a SIGQUIT-style stack dump is not
generally possible). Window floor 5 min and at least `max(5 min, 3 x expected)`; scale
with the learned duration so slow-but-alive gates are not touched.

**Option S2-C: a stalled job stops being counted as live.** When a job is stalled, the
scheduler treats its *expected remaining time* as unbounded for the shadow time (so
backfill is not planned around it) and, optionally, releases its CPU weight (not its memory)
for admission, while keeping the job running. This improves utilisation without any kill
and without a false-kill risk, but it changes admission accounting (the weight is a CPU
unit; the stalled job holds none) and over-subscribes memory only if it wakes up and grows:
its working set is already counted at the learned peak.

**Risks.** False stalls: a linker or `docker pull` over the network (I/O counters or `S`
state with no counters), a test waiting on a lock or a slow remote (genuinely silent),
`sleep`-based jobs, a watch/serve gate (never ends), a debugger/REPL (not in gate
workloads, stdin null). `io.stat` only counts block I/O of the leaf, not network or tmpfs;
CPU time of the job under heavy contention can look flat only if it is starved (check
run-delay: `cpu.pressure` of the leaf or `schedstat` run_delay: starved is not stalled).
Daemon restarts reset the window (store it as a monotonic "last progress" instant, not a
derived value, and do not kill without having sampled the whole window). Never act on a job
whose first sample is missing.

**What to measure first**: add the quiet-window computation in observation mode
(S2-A) on both machines for a few days and log per job the *longest quiet window* and the
job's outcome; the false-positive rate is the share of passing jobs whose longest quiet
window exceeded the proposed threshold. The recorded data has no time series (section 2.4),
so thresholds cannot be tuned from it: a one-week trace is required before enabling S2-B.
From the four timeouts, with the proposed floor of 5 min the hung jobs would have been
reported about 5-6 minutes after their last activity instead of after 25+.

**Recommendation: S2-A in the first release, S2-B (opt-in) after one week of traces, S2-C
as a follow-up only if the traces show long benign quiet periods that still block the
queue.** Defaults: window `max(5 min, 3 x expected)`, configurable per gate
(`stall_timeout`, `"off"` for watch/serve gates), `stall_kill_after` global, kill
disabled by default. Reasons: detection costs nothing and is already worth it (it names
the cost of the hang), the kill is the only dangerous part, and the evidence says the
benefit when enabled is large (replay: queue-non-empty time 177 -> 92 min, idle CPU
-69 %), so the opt-in is worth documenting for repositories known to hang.

### 4.3 S4: first-run memory estimate that does not converge

Facts that constrain the design: the proxy is far too high in 70 % of runs and too low
in under 1 %; learned peaks are stable (machine B) or vary up to 3.5x within a key
(machine A); a failed run stopped early gives a lower bound only; guessed jobs cannot
over-admit (needs a learned peak) and the memory guard (opt-in) and `oom_score_adj` /
`oom.group` already make gates the preferred victims if a guess is too low.

**Option S4-A: bounded share when nothing of the key runs.** A guessed job starts if
`share x guess <= free` (share 0.25) and no job of the same key runs (and at most one such
probe at a time on the machine), with the ordinary rule afterwards. Cheap, local to
`MemPass::fits`; the first run is a probe whose outcome (pass or memory kill) teaches. Risk:
an under-estimate means a probe that needs more than `share x guess` and may be memory-killed
or push others (mitigations above); the share bounds the harm. Replay: -10 % on machine B
(the replay does not know the real peaks of unlearned jobs; the log evidence of section 2.3
says the saving would have been the 125 minutes of guessed waits).

**Option S4-B: decay the guess to a data-driven prior.** Replace `proxy` by the median (or
P75) of learned peaks of the machine's keys with weight <= w, falling back to the proxy
only with no data; the estimate decays with the number of days the machine has data. Fixes
the over-estimate systematically (machine B: median of weight-<=6 peaks 0.97 GiB, p90 12
GiB). Risk: a machine whose weight-6 gates are all heavy has the same figure as today;
a median is too low for the one heavy first run (under-estimate then relies on the guard).

**Option S4-C: failed runs as lower bounds, estimate = `max(lower bound, capped prior)`.**
Record the peak of failing/timed-out runs in `gate_memory` with a `lower_bound` flag
(schema group `containment`, additive; older binaries ignore it as with `gate_memory`
itself). The estimate becomes `max(learned expected if any, max lower bound x 1.2, min(prior or
proxy, cap))` where `cap = clamp(P75 of learned peaks of weight <= w, 1 GiB, share x
usable)`. A failing gate then converges after its first run to at least its real
early-stop peak; later runs are admitted on that, and passing runs replace it. Risk: a
failed run's peak is a lower bound, so a key that always fails early stays under-estimated
until it passes (then the first pass at full size is the probe, protected by the guard and
oom ordering); recording failures also needs the timed-out/killed distinction (an OOM-kill is
already learned).

**Recommendation: S4-A + S4-C together, S4-B as the fallback value of the cap.** Order of
estimate for a key: learned (unchanged) > lower bound from failed runs (x 1.2) > capped
prior. The probe rule (S4-A) then applies only while no pass exists, with `share = 0.25`
of the guess and "one probe per key". Reasons: a pure estimate cannot be right on a first
run (70 % over, rare but costly under), so the first run must be allowed to *measure*
rather than to *wait*; failed runs carry information the current rule throws away; and the
guard/oom ordering make the downside of a bad probe a single gate kill that explains itself
(`killed for lack of memory`, ADR-0019 piece 4). Never apply the probe to `over-admission`
(still needs learned). What to measure: guessed-wait minutes per day (log lines), share of
probes that were memory-killed (target 0), peak of the probe vs the guess, number of runs
until a key has a learned peak.

### 4.4 Interactions

- **S2 -> S1.** A hung job holds weight and its (learned) peak; the head's shadow time
  computed from "expected remaining = expected - elapsed" is meaningless for it (it already
  overran). Rule: a running job past `max(2 x expected, expected + 5 min)` or flagged
  `stalled` counts as "ends at unknown time" in the shadow computation; with the
  extra-resources rule this only shrinks the backfill room to what fits beside it, never
  to a wrong promise. Until S2 exists, S1-A's overrun correction (recompute every pass,
  pessimistic remainder `max(expected - elapsed, 0.1 x expected + 2 s)` as in the replay)
  is enough because the replay's shadow stayed valid for the hung jobs only in the memory
  sense (they were weight-6 holders that never released).
- **S4 -> S1.** A guessed head (13.8 GiB) blocks for a whole suite and, through the
  reservation, freezes everything; S1's extra-resources test uses the head's *expected
  peak* in `sum(...) + peak_head <= usable`, so an inflated guess also shrinks backfill
  room. S4 therefore improves S1's room (the head gets a small estimate) and S1 limits the
  damage of a still-wrong guess. A guessed *candidate* is only backfilled by the
  extra-resources rule on its (capped) estimate, never by the duration rule (unknown
  duration).
- **S4 -> S2.** A failing-run lower bound for a *stalled* run is real information about the
  memory it held (7.6 GiB in the incident) but its duration is not: record the peak, never
  the wall time, of a `stalled`-killed run.
- **S1 -> S2.** More small jobs run beside a long job, so CPU of "the job" is a smaller
  share of the machine; use the job's own leaf counters, not machine-wide idle, for the
  stall decision.
- **Shared prerequisites**: carry `started_at` and `expected_ms` in `RunningSnap`; add a
  per-job `last_progress_at` to `JobEntry`; extend `status`/JSON additively (schema 1): the
  shadow time on the reserved head, `stalled` and `stalled_for`, `memory_expected_basis`
  (`learned | lower-bound | guessed`). New config keys are one-way for older binaries
  (`docs/compatibility.md`), as in ADR-0019.

### 4.5 Suggested order of work

1. S4-C and S4-A (small, local to memory admission, directly measurable in the log).
2. S1-B then S1-A (pure-function change in `plan`, golden tests from the recorded days).
3. S2-A (observation) shipped with the traces, then S2-B opt-in.
Each needs its own ADR before code (plan); S1 amends ADR-0007's "reservation" paragraph and
ADR-0019 piece 1's "overdue-head reservation extends to memory-blocked heads"; S4 amends the
0.6.1 note of ADR-0019.

## 5. Open points and limits of this analysis

- The replay is open-loop, ignores PSI and over-admission, and was calibrated on one day per
  machine; the figures are for ranking designs, not predictions.
- Timeouts: four events, one root cause (a repository's own hung test); no time series, so
  thresholds for S2 are a guess until traces exist.
- Machine A's daemon was not running at analysis time (no live `status`).
- Not found in the code or data: any use of `io.stat`, `cpu.pressure` of job leaves, or
  `/proc/<pid>/io`; cost and availability of reading them every 5-10 s per job should be
  measured before committing to S2-B.
- Items marked "from memory" in section 3 were not re-verified this session.
