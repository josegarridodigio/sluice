---
title: "Scheduler throughput and latency (B0 + L design)"
tags: [sluice, design, scheduler, latency, alpha]
---

# Scheduler throughput and latency (B0 + L design)

Status: design for alpha tasks B1, L1, L2 and L3 of
[[sluice/plan/alpha-plan.md]]; sections 2–7 are the source of the ADR-0011
draft. B1, L1 and L2 (M4–M7) are implemented and tuned by L3; section 12 lists
where the code differs from this text, and
[[sluice/design/scheduler.md]] describes the current behaviour. Extends [[sluice/design/scheduler.md]],
[[sluice/design/psi-admission.md]] and
[[sluice/design/adr/0007-scheduler-fairness.md]]. Evidence:
[[sluice/changes/alpha-readiness.md]] (`s2_problem` overload run).

## 1. What the overload run measured, and why

`scripts/alpha/s2_problem.py`, overload variant: 12 agents in 12 worktrees,
`ITER=2`. Each agent: MCP `verify` of the base tree (gates `lint` then
`test`; `verify` runs gates **sequentially**), then twice edit → verify →
`git commit` (pre-commit lint: a hit, the tree was just verified), then
`git push` (pre-push test: a hit; the broken agent's push fails fast from
its failure record). `test` = `npx ava` (a single-threaded prelude) +
`alpha-burn.js`, 8 worker threads, work-bounded (8 × a calibrated 8 s = 64
"units"; one unit = one idle core-second at calibration), `weight = 8`.
Budget `max(2, cpus − 1)` = 19 on 20 logical CPUs (i7-1280P: 6 P-cores
with SMT = 12 logical, plus 8 E-cores). Sluice ran 25 suites: one for the
shared base tree (12 requests coalesced) plus 24 distinct iteration trees.

A model that reproduces every measured number:

- Saturated capacity: the baseline ran 48 suites (≈ 69 units each with the
  prelude) in 147 s → **C ≈ 21 units/s**.
- One suite with spare CPUs: ≈ 3.5 s prelude at ~1.5 runnable threads +
  ≈ 8.5 s burn at 8 → **t0 ≈ 12 s**, W ≈ 69 units, average demand
  W / t0 ≈ **5.75 CPUs** (peak 8).
- `⌊19/8⌋` → **k = 2** concurrent suites; throughput min(k·W/t0, C) = 11.5
  units/s (55 % of C). Iteration phase 24 × 69 / 11.5 = 144 s, plus ≈ 15 s
  for the base verify → **wall ≈ 160 s** (measured 162.5).
- A wave of 12 iteration verifies arrives together: ⌈12/k⌉ = 6 rounds ×
  12 s + lint ≈ 75 s for the last one (measured p95 **79.6 s**). Queue wait
  per wave k × (0 + 12 + … + 60) = 360 s, two waves 720 s plus lint
  (measured **743 s**).
- Idle logical CPUs while jobs waited ≈ 45 % (4 idle during two burns, ~14
  during each prelude) → ≈ 60 s of whole-machine idle (≈ 37 % of wall).
  PSI never blocked: 16 burn threads on 20 CPUs keep `cpu some` far
  below 60.

Lessons. (1) The loss is not queue order (the jobs are homogeneous) but
**how many** run: weight 8 is the peak, not the average demand, and the
budget refuses a third suite while a third of the machine idles. (2) p95
has a work-conservation floor: a wave of 12 distinct suites needs
12 × 69 / 21 ≈ 39 s of saturated machine in any order (the baseline's own
wave takes ≈ 37 s). The target p95 ≤ 1.2 × 37.5 = 45 s is reachable only
by keeping the machine saturated during waves (k ≈ 4); ordering policies
cannot buy it.

## 2. Decisions at a glance

| # | Mechanism | Decision | Default | Task |
|---|---|---|---|---|
| M1 | Budget = online CPUs | adopt | `max(2, cpus)` | B1 |
| M2 | Learned weights (measured demand) | adopt, gates only | on after 3 runs | B1 |
| M3 | PSI-driven over-admission | adopt | cap 1.5 × budget | B1 |
| M4 | Parallel `verify` + early verdict | adopt | on, `fail_fast = true` | L2.1 |
| M5 | Per-lane fairness | adopt | on | L2.2 |
| M6 | Human-waiting priority | adopt | on | L2.3 |
| M7 | Shortest expected job first (HRRN) | implement, keep off | `order = "fifo"` | L2.4 |
| M8 | Supersede queued older trees | design only, defer | — | after v0.3.0 |
| L1 | Job timeline + utilisation sampler | adopt | always on | L1 (with B1) |

New user-config keys (optional): `[scheduler] learned_weights = true`,
`fairness = "lane"|"off"`, `human_first = true`, `order = "fifo"|"sjf"`;
`[admission] overcommit = 1.5` (1.0 disables), `cpu_some_avg10_low = 25.0`,
`memory_some_avg10_low = 5.0`, `overcommit_interval = "5s"`;
`[mcp] verify_parallel = true`. Each mechanism has a switch, so L3 measures
each one on the same binary (section 9).

## 3. M1 — budget = online CPUs

`DaemonConfig::default().budget = max(2, available_parallelism())`.
`available_parallelism` honours the daemon's affinity mask and cgroup
quota, so a daemon autostarted under `taskset -c 0-5` (E2 low-core run)
gets 6. The headroom that `cpus − 1` meant to leave is already provided by
`nice = 10`, `ionice` best-effort level 7 and the PSI block. Overload
effect: none (`⌊20/8⌋ = ⌊19/8⌋ = 2`); normal variant (weight 4): 4 → 5
slots. 6-CPU laptop: 5 → 6; a weight-8 gate is clamped to 6 either way, so
heavy concurrency stays 1 and only weight-1 jobs gain a slot. Docs: weight
≈ peak threads the gate uses; Sluice learns the average (M2).

## 4. M2 — learned weights

**Measurement.** A 1 s job sampler (same task as the L1 utilisation
sampler) scans `/proc/[0-9]*/stat`, keeps pids whose `pgrp` (field 5) is
the recorded `pgid` of a running gate job, and reads each of their tasks'
`/proc/<pid>/task/<tid>/schedstat` (`cpu_ns run_delay_ns slices`); deltas
accumulate per job, keyed by `(tid, starttime)` against pid reuse. It also
keeps the leader's `utime+stime+cutime+cstime` (`/proc/<leader>/stat`
fields 14–17) from the last tick. At job end:

```
cpu_ms   = max(Σ sampled cpu_ns, leader cumulative ticks)   # both lower bounds
delay_ms = Σ sampled run_delay_ns
demand   = (cpu_ms + delay_ms) / wall_ms                    # mean runnable threads
```

Plain `(utime+stime) / wall` is not used alone because it **falls under
contention** (threads wait, wall grows, CPU time does not): learning it
would lower weights exactly when the machine is full, a positive feedback
loop. Running + runnable time stays near the gate's intrinsic parallelism
and drifts toward the peak under contention, the safe direction. Without
`schedstat` (`CONFIG_SCHED_INFO` off) the fallback is `cpu_ms / wall_ms`,
kept only when `cpu some avg10 < 10` at both ends of the run. Known
undercount: processes that leave the group (`setsid`, `dockerd`/buildkit)
and short-lived ones exiting between ticks (the leader's `cutime` recovers
those waited for). Hence learning applies to **gates only**; `sluice exec`
jobs (A1) keep their declared weight.

**Use.** Stats key `(repo identity, gate, cmd_hash)`: repo identity is the
main worktree path (as for the repo cap); `cmd_hash` = 16 hex of SHA-256
over the normalised `cmd`, `\0`, and the `extra` args joined by `\0` (a new
command or a `--grep` filter starts a new history). Samples from `pass`
runs only (alpha review finding 5: a failing run often stops early, a
compile error or the first failed test, and taught a weight far below what
the suite needs once it passes; timeouts, kills, cached are partial);
`gate_stats` rows of failed runs written by earlier releases are ignored
when the statistics are loaded; window = last 10; learned after **3**.
`w_eff = clamp(round_half_up(P75(demand)), max(1, ⌈w_cfg/4⌉), w_cfg)`,
`w_cfg` = configured weight after the budget and repo-cap clamp. Never
above `w_cfg`; the floor stops a sleeping or memory-heavy gate from
collapsing to 1 (weight is also a memory proxy). **Resolved at
admission, not at enqueue:** every planning pass recomputes the
effective weight of each queued gate job from the current learned
statistics (the snapshot carries the learned weight per stats key, each
queued job its `w_cfg` and key), so a job queued before its gate had
learned is admitted with the learned weight as soon as one exists. A
running job keeps the weight it was admitted with until it finishes (the
units it holds and releases never change); `jobs.weight` records that
admitted weight (the enqueue-time weight until then). `status` shows
both, the effective one of a queued job being the current one. `exec`
jobs have no stats key and always use `w_cfg`.

**Overload effect.** demand ≈ (3.5 × 1.5 + 8.5 × 8) / 12 ≈ 6.1 (≈ 6.4 with
three suites running) → **w_eff = 6** → `⌊20/6⌋` = **k = 3**: 17.3
units/s (82 % of C), iteration phase 24 × 69 / 17.3 ≈ 96 s → wall ≈ 114 s;
p95 ≈ 4 rounds × 12.5 s + lint and jitter ≈ 55 s; Σ wait ≈ 470 s;
idle-while-waiting ≈ 16 % of wall.

## 5. M3 — PSI-driven over-admission

A job that does not fit the nominal budget starts anyway when all hold:

1. PSI enabled and the last sample has both values (no PSI → budget only);
2. not `blocking` (high-water hysteresis unchanged) and at least
   `overcommit_interval` since the last resume;
3. `cpu some avg10 < cpu_some_avg10_low` (**25**) and
   `memory some avg10 < memory_some_avg10_low` (**5**);
4. mean idle CPUs over the last two utilisation samples (7.1)
   `≥ max(1, ⌈w_eff/3⌉)`;
5. `used + w_eff ≤ cap = ⌊overcommit × budget⌋` (**1.5**); the repository
   cap still applies strictly;
6. at most **one** over-admission per `overcommit_interval` (**5 s**), so
   each step shows in `avg10` before the next;
7. the newest utilisation sample was taken at least one sampler tick
   (`psi_poll_interval`) after the latest admission of any kind
   (`Scheduler::last_start_at`), so the idle CPUs it reports do not predate
   jobs that just started (alpha review finding 4).

Interactions: over-admitted jobs are never preempted; while
`used > budget` nothing is admitted nominally, so the system drains back.
The `psi_max_wait` starvation guard is unchanged and admits only within
the nominal budget. While the head is reserved only the head may be
over-admitted (shortens the reservation, never bypasses it). Status:
`admission: "over"`, `budget.used` may exceed `budget.total`,
`budget.cap` reported.

Why both signals: PSI `some` measures contention (low with 16 burn threads
on 20 CPUs, ≈ 20–40 % with 24, ≈ 100 % with the baseline's 96) but not
idle capacity, and lags ~10 s; the sampler sees idle CPUs within 1–2 s but
not contention. "There is room" and "nobody waits for a CPU".

**Overload effect.** w_eff 6, cap 30: three suites (18.3 demand) leave ≈ 2
CPUs idle on average, more during preludes → a 4th starts; four (24.4)
leave none → no 5th. **k ≈ 4**: throughput ≈ C, iteration phase
24 × 69 / 21 ≈ 79 s → wall ≈ 97–100 s; p95 ≈ 3 rounds × 13.1 s + lint and
jitter ≈ 46 s; Σ wait ≈ 335 s; idle-while-waiting ≈ 4 %. If L3 sees k
stuck near 3, raise `cpu_some_avg10_low` (25 → 35) first.

**6-CPU laptop** (`taskset -c 0-5` = 3 P-cores with SMT): budget 6, test
clamped to 6 and learned 6, cap 9 → a second heavy suite (12) is never
over-admitted; only weight-1 jobs use prelude idle time, and only below
PSI 25. Worst case 3 units over budget, PSI block at 60 still applies.

## 6. Queue order and the L2 policies

### 6.1 Priority order (pure, recomputed every pass)

```
rank(j) = ( overdue(j) ? 0 : 1,            // waited ≥ head_reserve_after
            overdue(j) ? j.queued_at : 0,   // overdue: strict FIFO
            human_first ? class(j) : 0,     // 0 human, 1 agent
            fairness ? lane_rank(j) : 0,
            order == sjf ? −hrrn(j) : 0,
            j.queued_at, j.id )
lane_rank(j) = running jobs of j's lane + index of j among its lane's queued jobs
hrrn(j)      = (waited_ms + expected_ms) / expected_ms
```

ADR-0007's loop runs unchanged over this order (the "head" is the first
ranked job): backfill while the head is young, reservation once it is
overdue and blocked by the budget, then M3's step. `head_reserve_after`
(30 s) is the **single ageing bound**: overdue jobs outrank everything, are
FIFO among themselves, and the first gets the reservation, so the ADR-0007
bound (`head_reserve_after` + longest running job) holds for every job
whatever M5–M7 do. Under sustained overload the order degrades to FIFO +
reservation, the fair choice when everything is late.

### 6.2 Who is a "session": the lane

The daemon never sees the MCP client session; `CLAUDE_CODE_SESSION_ID` is
absent in the harness and for other agents, and shared by subagents in
different worktrees; `client_pid` differs between an agent's `sluice mcp`,
hooks and `exec`. The **worktree toplevel** is always present and is one
agent in the parallel-agent workflow. Decision: **lane = worktree
toplevel** (`run.repo.toplevel`, already `jobs.repo`); `session` stays
recorded but unused for scheduling.

### 6.3 M4 — parallel `verify` and early verdict (L2 step 1)

`verify` resolves every gate's hit check, then submits all misses at once
(one connection per gate, as today) instead of one after another, and
returns when all passed or **one failed** (fail, timeout, error, killed), sending
`cancel` on the other pending connections. The existing detach logic does
the rest: at 0 subscribers a queued job is dropped and a running one
killed under `last_subscriber_gone = cancel`; a job another agent
coalesced into keeps going ("nobody else waits" is exactly
`subscribers == 0`); under `finish` it completes and caches. Those gates
report `cancelled` with message `not finished: <gate> failed first`. New
optional `verify` parameter `fail_fast` (default `true`; `false` waits for
all and reports every failure); `[mcp] verify_parallel = false` restores
sequential runs. The tool description must say gates run concurrently.
Overload effect: lint (≈ 3 s, weight 1, always fits) overlaps the test's
wait: −3 s on every verify, ≈ −6 s on wall. On a lint failure the test is
cancelled (up to ≈ 3.3 s of saturated machine saved); s2's broken change
fails `test`, so this is not exercised there.

### 6.4 M5 — per-lane fairness (L2 step 2)

`lane_rank` interleaves lanes: a lane with jobs running or several queued
goes behind lanes with nothing running. Effect in s2 ≈ 0 (one test queued
per lane; with M4 a lint too, which backfills anyway). It protects p95
from bursts s2 does not produce: one agent issuing several `exec` builds
through the A2 rewrite, or a merge-queue train. No starvation risk (6.1).

### 6.5 M6 — human-waiting priority (L2 step 3)

Class 0 = `caller == human`: a human request always has someone waiting
(`human_hook_policy` cannot be fail-fast; `sluice run` blocks a terminal).
Agent hooks are fail-fast and never queue, so agent traffic is `verify`
and `exec`. Effect in s2: none (no humans); covered by tests. Risk: a
script misdetected as human floods the queue; bounded by the overdue rule.

### 6.6 M7 — shortest expected job first with ageing (L2 step 4)

HRRN is SJF with built-in ageing and no parameter (a waiting job's ratio
grows without bound), plus the hard overdue rule. `expected_ms` = median
`wall_ms` of the key's last 10 passing runs; unknown → 60 s. Effect in
s2: **none expected** (homogeneous tests; lints backfill because 1 unit is
almost always free). It helps mixed queues only when the budget is full
and the next slot must be chosen. Implement and unit-test; default
`order = "fifo"`; flip only if an L3 run with mixed gate lengths shows a
p50 gain without p95 loss.

### 6.7 M8 — supersede (deferred, designed)

With default policies nothing stale stays queued: `verify` blocks the
agent, a cancelled call detaches and `cancel` drops the orphan, agent
hooks fail fast. Only `last_subscriber_gone = finish` and backgrounded
`exec` leave any. Not in v0.3.0; reconsider if L1 shows jobs finishing
with zero subscribers above 5 % of runs. Design then: scope = (lane, gate,
cmd_hash); a newer key cancels an older **queued** job of that scope only
if all its subscribers are from the lane and none is attached, or all set
an optional run field `follow_latest: true` (moved to the new job; nobody
waiting on tree X gets tree Y's verdict); the rest get `killed`,
`killed_by: "superseded"`, optional `superseded_by` (no new status value).

## 7. L1 — instrumentation

### 7.1 Utilisation sampler and `idle_while_waiting_ms`

Every 1 s (the PSI tick task, running even with PSI off) the daemon reads
the `/proc/stat` lines of the CPUs in its affinity mask
(`Cpus_allowed_list` of `<proc_root>/self/status`: a `taskset` run counts 6,
not 20);
`idle_j = idle + iowait`, `total_j` = sum of all fields except
`guest`/`guest_nice` (already inside user/nice):

```
idle_frac = Σ Δidle_j / Σ Δtotal_j          # 0..1
idle_cpus = idle_frac × N_allowed           # input of M3
# under a cgroup CPU limit L < N_allowed (alpha review finding 6):
idle_cpus = max(0, L − (N_allowed − idle_frac × N_allowed))
idle_frac = idle_cpus / L
if the queue was non-empty at the previous sample:
    waiting_wall_ms       += Δt_ms
    idle_while_waiting_ms += Δt_ms × idle_frac
```

Definition: **the time integral of the idle fraction of the daemon's CPUs
while at least one job is queued**, in ms. Preferred to the CPU-ms
integral (the same × N) because it compares directly with wall time (L3
target < 10 % of wall) and with `waiting_wall_ms`; preferred to "ms with
any idle CPU" because that saturates at the first idle CPU. The harness
computes the same number independently (poll `/proc/stat` and
`sluice status --json` `queued` every second, same formula); agreement
within sampling noise is an L3 check. Convention as for
`psi_blocked_ms_total`: the state before the sample decides; per daemon
instance, in memory. Caveat recorded with the numbers: idle logical CPUs
here are often SMT siblings of busy P-cores (≈ 25 % of a core), so the
metric overstates lost capacity at moderate load.

### 7.2 Job timeline

Per job (new `jobs` columns, 7.4): `requested_at` (client send time, from
an optional `run` field), `queued_at`, `admitted_at` (decision),
`started_at` (spawn, after the marker double-check), `finished_at`,
`weight_cfg`, `weight` (effective), `admission` (`nominal|over|guard`),
`expected_ms`, wait accumulators `wait_budget_ms`, `wait_psi_ms`,
`wait_repo_cap_ms`, `wait_reserved_ms`, `wait_order_ms` (fit but
outranked; each pass adds the elapsed time to each queued job's current
reason), `cpu_ms`, `run_delay_ms`, `demand_milli`, `killed_by`
(`last-subscriber|early-verdict|stop|superseded`). Per request (table
`job_requests`): who waited, since when, whether they stayed. The
dominant wait reason is the largest accumulator. `sluice logs --timeline
[--since 10m]` prints one line per job (`queued +0.0 admitted +31.2
started +31.3 done +43.9 pass; waited budget 29.0s, order 2.2s; demand
6.1/8; 3 request(s)`; an optional `requested -0.1` precedes `queued`, an
over-admission shows `admitted +31.2 (over)`, a job with no measured demand
and a weight below the configured one shows `weight 6/8`, detached requests
`(1 left)`, a killed job `killed by <who>`).

### 7.3 Exposure: optional fields only (protocol 1, status schema 1)

Every field below is `Option` + `skip_serializing_if`, so the frozen
`status-schema-1.json` fixture stays byte-identical; each gets an additive
test like `log_level_is_an_additive_optional_field`. `reason` keeps its
frozen four values (an outranked job reports `budget`); detail goes in a
new string field, because a new enum variant breaks old clients.

| Message | New optional fields |
|---|---|
| `run` request | `requested_at` |
| `queued` event | `weight_cfg`, `expected_ms` |
| `done.outcome` | `queue_wait_ms` (this subscriber: `started_at − attached_at`, 0 if it joined a running job), `wait_reason` (dominant), `admission`, `killed_by` |
| `status.budget` | `cap`, `cpus`, `idle_cpus` |
| `status.running[]` | `weight_cfg`, `waited_ms`, `admission`, `demand` |
| `status.queued[]` | `lane`, `rank`, `expected_ms`, `weight_cfg`, `wait_detail` (`outranked`, `lane`, `human-first`) |
| `status.totals` | `idle_while_waiting_ms`, `waiting_wall_ms`, `over_admissions` (per instance) |
| MCP `verify` | `GateResult.queue_wait_ms`, `GateResult.wait_reason`, `VerifyReport.wall_ms` |

Text: `PASS 12.1s (job j1) (queued 31.0s: budget)` in the `verify` line (not
for cached results; `sluice run -v` does not print it); `sluice status` shows
`budget:   24 of 20 units busy (cap 30, 3.0 of 20 cpus idle)` (the cap only
when above the budget; `budget.cap` is always in the JSON), each queued job
as `#<rank> ... (<reason>[: <wait_detail>])  expected <duration>`, and a
`waiting:` line with the time jobs were queued, how much of it CPU-idle and
the over-admissions.

### 7.4 SQLite: additive migration from v0.2.0

Today `schema.version = 1` in v0.1.0 and v0.2.0, a mismatch is a hard
error and nothing is migrated; the merge queue already added `queue_*`
tables with their own `queue_schema` version. Decision: **keep
`schema.version = 1`** (no existing column changes meaning) and version
the additions in a new `sched_schema` table, as the queue does. Reason:
`sluice update --version 0.2.0` must keep working; v0.2.0 opens a migrated
file (its version check passes, its `INSERT` names columns so new ones
stay NULL, it ignores unknown tables). `schema.version = 2` would make
every v0.2.0 binary, `doctor` included, refuse the file.

Migration 0 → 1 at daemon start, after the existing check, in one
`IMMEDIATE` transaction (SQLite DDL is transactional: a crash leaves
version 0 and nothing applied):

```sql
CREATE TABLE IF NOT EXISTS sched_schema (version INTEGER NOT NULL);
-- absent row → 0; version > 1 → hard error "sched_schema N is newer than this build"
-- one ALTER TABLE jobs ADD COLUMN per column, all nullable:
--   INTEGER: requested_at, admitted_at, weight_cfg, expected_ms, wait_budget_ms,
--            wait_psi_ms, wait_repo_cap_ms, wait_reserved_ms, wait_order_ms,
--            cpu_ms, run_delay_ms, demand_milli
--   TEXT:    admission, killed_by
CREATE TABLE job_requests (
  id INTEGER PRIMARY KEY, job_id TEXT NOT NULL, caller TEXT NOT NULL,
  session TEXT, client_pid INTEGER, via TEXT,       -- mcp | cli | hook | exec | queue
  requested_at INTEGER, attached_at INTEGER NOT NULL, finished_at INTEGER,
  coalesced INTEGER NOT NULL, detached INTEGER NOT NULL DEFAULT 0);
CREATE INDEX job_requests_job ON job_requests(job_id);
CREATE TABLE gate_stats (
  repo TEXT NOT NULL, gate TEXT NOT NULL, cmd_hash TEXT NOT NULL,
  job_id TEXT NOT NULL, finished_at INTEGER NOT NULL, outcome TEXT NOT NULL,
  wall_ms INTEGER NOT NULL, cpu_ms INTEGER NOT NULL, run_delay_ms INTEGER,
  demand_milli INTEGER NOT NULL);
CREATE INDEX gate_stats_key ON gate_stats(repo, gate, cmd_hash, finished_at);
INSERT INTO sched_schema (version) VALUES (1);
```

`gate_stats` keeps the newest 20 rows per key (pruned on insert); keys
unseen for 90 days go in the daily prune; the daemon caches the last 10
per key in memory. Rows written by a downgraded v0.2.0 have NULL timeline
columns, shown as unknown. `Store::open_read_only` checks `sched_schema`
before selecting new columns. B1 tests: a database written by the v0.2.0
release binary (H3 golden file) migrates and its rows read back; a second
migration is a no-op; a failure mid-migration rolls back; the v0.2.0
binary runs a job on the migrated file (`s6_upgrade` covers real
binaries). B1 updates [[sluice/design/executor.md]] and the `store.rs`
module doc ("never migrated").

## 8. Pure scheduler API (for B1 unit tests)

`scheduler.rs` keeps its I/O-free contract; the decision becomes one pure
function over a snapshot, and `Scheduler` only owns state and applies it.

```rust
pub struct QueuedSnap { id, repo, lane: PathBuf,
    weight_cfg: u32, learn_key: Option<LearnKey>, class: Class /* Human|Agent */,
    queued_at: u64, expected_ms: Option<u64> }   // effective weight: queued_weight()
pub struct RunningSnap { id, repo, lane: PathBuf, weight: u32 }
pub struct Pressure { blocking: bool, guard_due: bool,
    calm: bool /* section 5, conditions 1-3 */,
    idle_cpus: Option<f64>, sampled_at: Option<u64> /* newest utilisation sample */ }
pub struct Policy { budget, cap: u32, head_reserve_after_ms, overcommit_interval_ms,
    sample_tick_ms: u64 /* sampler period */, order: Order /* Fifo|Sjf */,
    fairness: bool, human_first: bool, learned_weights: bool,
    repo_caps: BTreeMap<PathBuf, u32> }
pub struct Snapshot<'a> { now: u64, policy: &'a Policy, pressure: Pressure,
    last_overcommit_at: Option<u64>, last_start_at: Option<u64>,
    running: &'a [RunningSnap], queued: &'a [QueuedSnap],
    learned: &'a Learned /* learned weight per stats key */ }
pub enum Admission { Nominal, Over, Guard }
pub struct Plan { start: Vec<(String, Admission)>, reserved: Option<String>,
    waits: Vec<Wait /* id, cause: WaitCause, detail */>,
    order: Vec<String> /* rank order */, weights: Vec<u32> /* parallel to order */ }

pub fn rank(s: &Snapshot) -> Vec<usize>;                           // 6.1
pub fn plan(s: &Snapshot) -> Plan;
pub fn queued_weight(s: &Snapshot, j: &QueuedSnap) -> u32;         // 4, per pass
pub fn effective_weight(w_cfg: u32, demands_milli: &[u32]) -> u32; // 4
pub fn expected_ms(walls_ms: &[u64]) -> Option<u64>;               // 6.6
```

`plan`: (1) PSI blocking → only the guard may start the first ranked job,
within the nominal budget; (2) nominal pass over `rank` with backfill and
reservation; (3) at most one over-admission (the reserved head, else the
first ranked job under the cap) when section 5 allows. Starts are checked
against the running set as it grows within the pass; `waits` feeds
`status` and the accumulators. Unit tests: ADR-0007's existing tests pass
unchanged with every switch off; weight 8 in budget 20 → 2, w_eff 6 → 3;
a 4th only when calm, idle ≥ 2 and the interval passed; never over cap,
never while blocking or without PSI; the guard never over-admits; under
reservation only the head over-admits; an overdue agent job outranks a
fresh human one; lanes interleave; HRRN order and its ageing crossover;
learned-weight floor, ceiling and the < 3 samples case; laptop snapshot
(budget 6, cap 9): two w = 6 jobs never run together.

## 9. Order of introduction and predicted numbers (overload, 20 CPUs)

Each row is a user-config change on the same v0.3.0 binary, one E2 run
each. R0 reproduces v0.2.0 policy: `budget = 19`, `learned_weights =
false`, `overcommit = 1.0`, `verify_parallel = false`, `fairness = "off"`,
`human_first = false`, `order = "fifo"`.

| Run | Adds | k | Wall (s) | p95 verify (s) | Σ wait (s) | idle-while-waiting / wall |
|---|---|---|---|---|---|---|
| baseline | — | 12 | 147 | 37.5 | — | — |
| R0 | v0.2.0 policy (measured) | 2 | 162.5 | 79.6 | 743 | ≈ 37 % (est.) |
| R1 | M1 budget = CPUs | 2 | ≈ 160 | ≈ 79 | ≈ 740 | ≈ 37 % |
| R2 | M2 learned weights | 3 | ≈ 114 | ≈ 55 | ≈ 470 | ≈ 16 % |
| R3 | M3 over-admission | ≈ 4 | ≈ 100 | ≈ 46 | ≈ 335 | ≈ 4 % |
| R4 | M4 parallel verify, early verdict | ≈ 4 | ≈ 95 | ≈ 43 | ≈ 330 | ≈ 4 % |
| R5–R7 | M5 lanes, M6 human first, M7 HRRN (one run each) | ≈ 4 | ≈ 95 | ≈ 43 | ≈ 330 | ≈ 4 % |

Targets: wall ≤ 147 s (met from R2); p95 ≤ 45 s (met from R4, R3
borderline: floor = 39 s work conservation + lint + admission lag); suites
= 25 at every step; idle < 10 % (from R3); peak load ≤ the baseline's 74
(k = 4: ≈ 25 runnable burn threads plus agents, ≈ 35). R5–R7 are expected
neutral on s2 and kept for workloads s2 does not generate ("keep only if
it helps E2" read as "keep unless it hurts E2"), except M7, off by
default.

Measured (the tuning rounds L3, [[sluice/changes/alpha-readiness.md]]): the
shipped defaults (`overcommit = 1.5`, `overcommit_interval = "5s"`, every
policy of section 2 on, `order = "fifo"`) meet every target in the normal,
overload and low-core variants (overload: wall 0.58x the baseline, p95
1.10x, idle while waiting 7.8 %, peak load 16.6 vs 63.3). Round 2 added the
resolution of learned weights at admission (section 4); a 2.0x / 2 s
setting also met every target but left the normal variant nearer the p95
limit (1.18x against 1.14x), so the defaults stayed. The predictions above
are the design estimates; the low-core prediction below was pessimistic
(measured p95 0.94x).

Low-core variant (6 logical CPUs, budget 6): k = 1 heavy suite throughout.
Suite alone ≈ 64/6 + 3.5 ≈ 14 s; Sluice wall ≈ 25 × 14 + base ≈ 370 s vs
baseline ≈ 48 × 69 / 6 ≈ 550 s (met); p95 ≈ 12 × 14 + 3 ≈ 170 s vs the
baseline wave ≈ 140 s → 1.2–1.25×, at or just over the target. Cause if
missed: the single-threaded prelude holds the whole budget for ≈ 25 % of
each suite, and a second heavy suite is never over-admitted on 6 CPUs by
design. Record it as the expected gap; raising the cap on small machines
is exactly the plan's overload risk.

## 10. Risks

- Under-measured CPU (escapes the group) → floor `⌈w_cfg/4⌉`, gates only,
  never above `w_cfg`, PSI block as backstop.
- Over-admission oscillation → one step per 5 s, idle condition, none
  right after a resume, cap 1.5×.
- Starvation from M5–M7 → one ageing bound (6.1); property test: under any
  switch combination a weight-B agent job behind an endless stream of
  weight-1 human jobs in other lanes starts within `head_reserve_after` +
  the longest running job.
- Sampler cost: one `/proc` scan per second (1–5 ms) while gates run.

## 11. Open questions and how they were decided

1. Additive `sched_schema` instead of `schema.version = 2`? Yes (downgrade
   safety, queue precedent); decided in ADR-0011 item 8, and ADR-0013 (H3)
   states it as the database rule.
2. Supersede in v0.3.0? No (6.7, deferred in ADR-0011); revisit with L1 data.
3. SJF on by default? No (`order = "fifo"`, unchanged by the L3 rounds); a
   mixed-length E2 variant was not needed.
4. Parallel `verify` with `fail_fast = true` by default? Yes (implemented, M4).
5. Learned weights for `exec` jobs? No (their CPU escapes the group).
6. ADR-0011: extracted from sections 2–7 (accepted); ADR-0007 is marked
   amended and [[sluice/design/scheduler.md]] was updated when B1 landed.

## 12. Implementation notes and deviations (B1 + L1)

Change note: [[sluice/changes/scheduler-throughput.md]]. Config names and
defaults are exactly those of section 2. Where the code differs:

1. **Over-admission waits for a quiet pass.** `plan` over-admits only when
   nothing started nominally in the same pass: the idle-CPU reading predates
   those starts. The next pass (≤ 1 s) may over-admit.
2. **Unsampled runs teach nothing.** A gate run that ended before the
   sampler saw it (shorter than one tick) records no `gate_stats` row; with
   section 4's formula it would record a demand near 0 and pull the weight
   to its floor. Learning therefore needs runs of about a second or more.
3. **Lane of `exec` jobs** is the request's `cwd` (the exec request carries
   no worktree toplevel); gates use `repo.toplevel`.
4. **`job_requests.via`**: `mcp` when the client's `hello` name starts with
   `sluice-gate-mcp`, `hook` when the run request names a hook, `queue` for
   merge-queue runs, `exec` for exec, else `cli`. `requested_at` is sent by
   `sluice run` and the MCP server; the queue runner and `exec` leave it
   unset.
5. **Status order.** `status.queued` is listed in rank order (with every
   switch off this is arrival order, as before) and gains `rank`.
   `wait_detail` is set only for the `order` cause. `idle_cpus` and
   `demand` are rounded to 0.1. `budget.cpus` is the daemon's affinity mask
   size (falling back to the affinity/cgroup count of the core).
6. **Sequential `verify`** (`[mcp] verify_parallel = false`, or a single
   gate) is exactly the pre-0.3.0 loop: every gate runs, `fail_fast` is
   ignored. The early-verdict failure set is fail, timeout, error and
   killed. In parallel mode each pending gate sends its own progress
   notification per interval. `(queued 31.0s: budget)` is appended to the
   `verify` text line; `sluice run -v` does not print it yet.
7. **Migration framework.** `crate::migrate` (numbered `Migration`s per
   version table, one `IMMEDIATE` transaction, `VACUUM INTO
   <db>.bak-<version>` before migrating an existing file, an existing
   backup is kept). A newer `sched_schema` refuses a read-write open
   (`Error::NewerSchema`) but a read-only open reads it (only known columns
   are selected). The merge queue keeps its own `queue_schema` code.
8. **`killed_by = superseded`** is reserved (M8 deferred) and never set.
9. **Downgrade check** is a test of the v0.2.0 statements on a migrated
   file (version check, named `INSERT`, start/finish updates) plus a daemon
   started with the v0.2.0 user-config fixture; the real-binary check stays
   with `s6_upgrade`.
10. **CPU budget source.** `sluice_gate_core::cpus` reads
    `Cpus_allowed_list` and the cgroup v2/v1 limits itself (injectable root,
    unit-tested) and falls back to `available_parallelism`; the daemon's
    utilisation sampler uses the mask from `<proc_root>/self/status`, and
    the cgroup limit read under the parent of `proc_root`
    (`sampler::cgroup_cpu_limit`: `/proc` → `/`, a fake `<root>/proc` →
    `<root>`): when the limit is below the mask, idle CPUs are the limit
    minus the busy CPUs of the mask, never below 0 (alpha review finding 6).
11. **Fresh samples for over-admission.** `Pressure::sampled_at` is the
    time of the newest utilisation reading; `plan` over-admits only when
    it is at least `Policy::sample_tick_ms` after `Snapshot::last_start_at`
    (set by every admission: nominal, guard or over). Note 1 above remains
    (no over-admission in a pass that started a job nominally).
