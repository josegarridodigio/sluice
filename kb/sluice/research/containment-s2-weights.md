---
title: "Containment spike S2: session layout and CPU defaults, measured"
tags: [sluice, research, containment, cgroups, cpu, weights, spike]
---

# Containment spike S2: session layout and CPU defaults, measured

Measured 2026-10-24 on a 20-CPU Ubuntu desktop (kernel 7.0, systemd 259,
cgroup v2, `cpu memory pids` delegated to the user manager). Task S2 of
[[sluice/plan/containment-plan.md]]; feeds ADR-0017
([[sluice/design/adr/0017-agent-resource-containment.md]]) and tasks C1/C2/C3.
CPU only: no memory limit, memory hog or `memory.*` file was touched (rule
from [[sluice/research/containment-s2-incident.md]]; memory belongs to S5).
Related: S1 (sandbox) [[sluice/research/containment-s1-sandbox.md]], S4
(nice) [[sluice/research/containment-s4-fallbacks.md]].

## Summary

- **What protects the desktop is the weight of the slice that holds the
  agents scope, not the weight of the scope.** A scope that is the only child
  of its slice competes with the desktop with the slice's weight. Measured with
  20 desktop busy loops against 20 agent loops: slice weight 30 gave the agents
  20.6 % of the CPU (weights predict 23 %), slice weight 100 gave 49 %.
- Both ways of protecting the agent's UI work: layout A (`ui` 400 / `tools` 25)
  and layout B (load at `nice` 19 in one leaf). A is the tightest (p99 below
  1 ms in every round); B is close (p99 0.1 to 3 ms) and needs only a
  `renice` of the shell.
- **Recommendation: "A unsandboxed, B sandboxed"**, because A is impossible
  when the agent's Bash sandbox is on (S1) and B is the only thing that still
  works there; the numbers show B is a good enough fallback.
- No CPU ceiling by default. Weights are work-conserving (agents used 95 to
  98 % of the machine when it was idle) and a ceiling only adds throttling
  stalls to the agent's own UI (p95 8 to 17 ms) without helping the desktop.
- `pids.max` per session: 4096 (tasks, threads count).

## Method

Harness: bash plus a small Python probe (scratch, not product code). All
units are `sluice-s2-*` in a `sluice_s2.slice` created for the test, outside
`sluice.slice`; everything was stopped at the end.

```
user@1000.service/
  app.slice/.../<my terminal scope>        desktop probe + desktop loops (round R0, D-*)
  sluice_s2.slice                          CPUWeight 30 or 100 (set-property --runtime)
    sluice-s2-agents.scope  Delegate=yes   holder/ one/ ui/ tools/
    sluice-s2-gates.scope   Delegate=yes   holder/ jobs/
```

- **UI probe** (`probe.py`): wakes every 10 ms on an absolute schedule, spins
  about 1 ms of work, records wake-up latency (actual minus intended wake),
  1 500 samples per 15 s round; p50/p95/p99/max in microseconds. One probe
  inside the agents scope ("ui": in the `ui` leaf for layout A, in the single
  leaf otherwise) and an identical **desktop probe** outside the scope, in
  the cgroup of my own shell. Idle baseline: p50 250 us, p99 290 us (timer
  slack; under load the CPUs never enter idle states, which is why loaded
  p50 can be lower, about 58 us).
- **Load**: `timeout 15 sh -c 'while :; do :; done'`, 20 copies (and 40 for
  the oversubscription rounds). Each is placed by a wrapper that writes its
  own PID to the leaf's `cgroup.procs` and `exec`s `nice -n N timeout ...`.
  Allocates nothing.
- Weights: `cpu.weight` written to the leaves; slice and scope weights and
  `CPUQuota` through `systemctl --user set-property --runtime`.
- CPU shares from `cpu.stat` `usage_usec` deltas of each scope (hierarchical,
  includes the probe). 20 CPUs x 15 s = 300 000 ms available per round.
- Safety: `trap` stops all units on exit; a round aborts the run if the
  desktop probe max exceeds 1 s (never happened; the worst desktop max was
  26 ms); total load time about 8.5 minutes in 29 rounds.
- Single run per configuration, machine otherwise idle but with a real
  desktop session running. `max` values vary by 10x between identical rounds
  (repeats below) and must not be used as a pass/fail criterion; p50/p95/p99
  are stable enough to rank layouts, differences under about 0.5 ms are noise.

## Results

### 1. Latency, 20 busy loops, agents slice weight 30 (microseconds)

| Round | desktop p50 | p95 | p99 | max | ui p50 | p95 | p99 | max |
|---|---|---|---|---|---|---|---|---|
| R0 no containment (probes and load in my cgroup) | 543 | 547 | 1 626 | 4 120 | 717 | 720 | 1 729 | 20 740 |
| R1 one leaf, no nice | 57 | 60 | 81 | 1 148 | 799 | 802 | 2 799 | 17 803 |
| R1 repeat (X) | 58 | 59 | 61 | 1 254 | 528 | 531 | 534 | 6 343 |
| A 100/100 | 56 | 58 | 271 | 2 273 | 58 | 67 | 672 | 2 813 |
| A 200/50 | 58 | 62 | 81 | 6 123 | 57 | 62 | 875 | 3 642 |
| A 400/25 | 58 | 70 | 99 | 2 056 | 58 | 64 | 69 | 1 769 |
| A 400/25 repeat (X) | 59 | 62 | 416 | 833 | 58 | 61 | 68 | 86 |
| B nice 10 | 58 | 73 | 669 | 3 708 | 59 | 346 | 5 365 | 59 577 |
| B nice 19 | 58 | 66 | 102 | 10 746 | 59 | 94 | 1 775 | 4 127 |
| B nice 19 repeat (X) | 58 | 59 | 224 | 432 | 59 | 65 | 3 049 | 9 177 |

Reading: on 20 CPUs, 20 busy loops alone are not a latency disaster even with
no containment (p99 under 2 ms): there is almost always a CPU that frees up
within a scheduler slice. What containment buys is the tail and, in the next
sections, the share when the machine is oversubscribed. The `ui` probe in a
shared leaf (R1) waits a slice behind a loop (p50 0.5 to 0.8 ms); with a
`ui` leaf at weight 400/25 or with `nice` 19 on the load it wakes in 58 us
like on an idle machine.

### 2. Oversubscription: 40 busy loops (2x the CPUs), weight 30

| Round | desktop p50 | p95 | p99 | ui p50 | p95 | p99 | ui max |
|---|---|---|---|---|---|---|---|
| none (my cgroup) | 517 | 4 516 | 5 517 | 2 261 | 6 261 | 8 262 | 10 483 |
| one leaf, no nice | 58 | 64 | 88 | 2 453 | 7 457 | 10 458 | 72 489 |
| A 400/25 | 58 | 69 | 198 | 59 | 68 | 128 | 10 632 |
| B nice 19 | 58 | 66 | 606 | 58 | 69 | 2 112 | 17 129 |
| B nice 10 | 59 | 64 | 80 | 60 | 1 514 | 2 518 | 6 540 |

This is where the layouts separate: a single unweighted leaf gives the agent's
own UI p50 2.5 ms, p99 10 ms; A keeps it at 59 us / 128 us; B nice 19 keeps
the median but its p99 is 2 ms; nice 10 is clearly worse at p95 (1.5 ms).

### 3. Desktop competing: 20 desktop loops against 20 agent loops

| Round | agents scope CPU (ms of 300 000) | agents share of busy CPU | desktop probe p95 | ui p50 / p95 / p99 |
|---|---|---|---|---|
| slice 100, one leaf | 147 882 | 49 % | 2 374 | 381 / 387 / 2 386 |
| slice 30, one leaf | 61 522 | 20.6 % | 444 | **3 504 / 10 501 / 31 508** |
| slice 30, A 400/25 | 49 622 | 16.6 % | 929 | 58 / 64 / 984 |
| slice 30, B nice 19 | 66 673 | 22 % | 506 | 58 / 553 / 2 555 |

(Desktop loops were in the cgroup of my terminal, together with a few
unrelated processes, so "share of busy CPU" is the agents' CPU divided by the
sum of both groups' CPU; weights predict 23 % for 30 vs 100.) Slice weight 30
is what moves the share from 49 % to about 21 %. Under that contention the
agent's UI in a single leaf is the loser (p50 3.5 ms, p99 31 ms, max 322 ms in
that round), while A and B keep it at 58 us median.

### 4. Where the weight must be: slice or scope (20 loops, one leaf)

| Slice weight / agents scope weight | agents scope CPU (ms) | desktop p99 | ui p50 |
|---|---|---|---|
| 30 / 100 | 294 127 | 81 | 799 |
| 100 / 100 | 295 832 | 434 | 61 |
| 100 / 30 | 295 077 | 246 | 113 |

These rounds had no competitor, so they only show the scope weight changes
nothing for the desktop (the scope is the slice's only runnable child).
Section 3 shows the slice weight is the effective knob. In the final design
`sluice.slice` holds both scopes, so its weight sets agents-versus-desktop
and the scope weights (gates 100, agents 30) set gates-versus-agents.

### 5. CPU ceiling `CPUQuota=1600%` on the agents scope (80 % of 20 CPUs)

| Round | agents CPU (ms) | desktop p95 / p99 | ui p50 / p95 / p99 |
|---|---|---|---|
| slice 30, one leaf, quota | 240 330 | 436 / 1 444 | 200 / 13 494 / 13 539 |
| slice 30, A 400/25, quota | 240 132 | 343 / 2 698 | 62 / 7 979 / 17 992 |
| slice 100, one leaf, quota | 240 460 | 1 427 / 1 428 | 945 / 17 258 / 17 297 |
| slice 100, one leaf, no quota | 295 537 | 65 / 693 | 237 / 1 239 / 2 241 |

The ceiling works (16.0 CPUs, 240 s) but it throttles by 100 ms periods: the
agent's UI, inside the throttled group, stalls 8 to 18 ms in 5 to 10 % of its
wake-ups. The desktop probe did not improve (p95 0.3 to 1.4 ms, worse than
without, the idle CPUs sit in deep sleep). On a 20-CPU machine the ceiling
protects nothing the weights do not already protect.

### 6. Gates versus agents (scopes in one slice, weights 100 and 30)

| Round | gates CPU (ms) | agents CPU (ms) | gates share | agents share |
|---|---|---|---|---|
| 8 gate loops + 20 agent loops | 119 166 | 178 016 | 40 % | 60 % |
| 20 gate loops + 20 agent loops | 192 345 | 96 842 | 66.5 % | 33.5 % |

With 8 loops the gates get everything they ask for (8 CPUs, below their
entitlement of about 15 CPUs) and the agents take the other 12 CPUs. With 20
against 20, gates get twice the agents' CPU, not the 3.3x that 100:30 predicts
(per-CPU hierarchical scheduling compresses the ratio when each group has
roughly one thread per CPU; the 30-versus-100 desktop test above came out at
the predicted 21 vs 23 %). Single run: treat the ratio as 2x to 3x.

### 7. Work conservation

Every round without a ceiling and with only agent load had the agents scope
use 286 800 to 297 000 ms of the 300 000 ms available (95 to 99 %): a weight
never slows a session down on an otherwise idle machine, and layout B nice 19
and layout A lose at most 4 % of throughput against a single leaf.

## Layout decision

| | A: `ui`/`tools` leaves | B: one leaf, `nice` on the shell |
|---|---|---|
| ui p99, 20 loops | 69 to 875 us | 1.8 to 3.0 ms (nice 19) |
| ui p99, 40 loops | 128 us | 2.1 ms (nice 19), 2.5 ms (nice 10) |
| under desktop contention | 984 us | 2 555 us |
| works with Bash sandbox on | no (S1: cgroupfs read-only) | yes if the renice is applied by the hook/env line, S4: unprivileged, inherited, irreversible by the child |
| across cgroups | n/a (weights) | nice has no effect between cgroups (S4), so it only protects the UI inside the session leaf; agents-vs-desktop and gates-vs-agents still rely on slice and scope weights |
| cost | two leaves per session, two moves | one `renice` line |

Recommendation: **A when the agent is not sandboxed, B when it is.** The
plugin chooses at session start by testing whether it can write
`cgroup.procs` (S1/S3 fail-open pattern). B is not much worse (p99 2 ms is
still invisible to a person) so a user who runs sandboxed loses little.
The nice level for B is 19: nice 10 had p95 1.5 ms and a 59 ms outlier in the
20-loop round, nice 19 had a clean p95 (65 to 94 us). A shell at nice 19 still
gets 1.7 % of a core against a busy sibling (S4) and all of the idle machine.

## Recommended defaults

| Knob | Value | Basis |
|---|---|---|
| `sluice.slice` `CPUWeight` | 50 (30 to 100 range, measured 30 and 100) | section 3; share scales linearly: 30 gave 21 %, 100 gave 49 % of a fully contended machine, so 50 is about 33 %. It also bounds gates, since the gates scope lives in the same slice |
| `sluice-agents.scope` `CPUWeight` | 30 | section 6; weight against the gates scope only |
| `sluice-gates.scope` `CPUWeight` | 100 | gates get 2x to 3x the agents' share, and everything when the agents are idle |
| `ui` leaf / `tools` leaf (layout A) | 400 / 25 | sections 1 to 3; 200/50 left p99 at 0.9 ms; 100/100 is no better than one leaf under oversubscription |
| nice for the shell (layout B) | 19 | sections 1 to 3 |
| `CPUQuota` ceiling | off by default; optional config key (suggest 80 % of CPUs) for laptops where fans and temperature matter | section 5 |
| `pids.max` per session | 4096 | below |

Note the second gates question: the weights make gates win only against the
agents; against the desktop gates share `sluice.slice`'s weight. If the team
prefers gates to compete at desktop level (100) while agents stay low, put
the scopes under two slices (`sluice_gates.slice` weight 100,
`sluice_agents.slice` weight 30; avoid dashes in slice names, systemd nests
them) instead of one. That layout was not measured but follows from the same
mechanism and is a configuration change only.

## Memory and pids observation (read-only)

Only one `claude` process was running (this spike's own parent): its process
tree (agent plus five MCP servers, `npm exec` wrappers and `sh` shims)
before the harness started:

| Item | Observed |
|---|---|
| processes in the tree | 11 (64 while my 22 load/probe processes ran) |
| tasks (threads) | about 35 idle |
| agent RSS | 491 MB (31 threads) |
| MCP servers | 4 to 168 MB each, 5 of them, about 330 MB together plus `npm exec` wrappers about 105 MB each |
| tree RSS | about 1.4 GB in total during the measurement (includes my harness) |

No memory limit is proposed here (S5). `pids.max` counts tasks, threads
included: an idle session is about 35, a 20-job build (cargo with rustc
threads, jest or webpack workers, each with 10 to 20 threads) reaches the low
hundreds. The user manager has `pids.max = max` and the user slice 82 328.
**Proposal: `pids.max = 4096` on each session's cgroup** (100x idle, about
10x a heavy build), which stops a fork bomb long before it matters and leaves
real builds untouched. Builds above that would show up as `EAGAIN` on fork;
make it a config key and have `sluice status` show `pids.peak` so the
default can be tuned. I did not measure a heavy build's task peak.

## Caveats

- 20 CPUs: with 20 loops the machine is exactly saturated and a waking task
  usually finds a CPU quickly, so latency under 20 loops is mild even with no
  containment. On a 4 to 8 core laptop the same 20 loops oversubscribe 3x to
  5x, which resembles the 40-loop table (section 2) or worse; the layout
  ranking should hold but absolute numbers will be worse, and the CPU-share
  results scale with the weights, not with the CPU count. Re-run on an
  8-core machine before fixing the Definition of Done bound.
- Suggested bound for the harness and E2: ui probe p99 under 5 ms and
  desktop probe p99 under 2 ms with 20 busy loops at 2x oversubscription;
  `max` is too noisy (6 to 60 ms outliers appear in every layout and in the
  desktop probe).
- Single runs; repeats of three configurations (X rows) differed by up to
  2x in p99 and 10x in max.
- The probe measures scheduling delay of a sleeping process waking up, not
  throughput of a UI under CPU-bound rendering, and not I/O or memory
  contention (not measured by design).
- cgroup `cpu.weight` is relative: all conclusions assume the desktop slices
  keep their default weight 100 (as seen here: `app.slice` 100).
- The gates ratio (2x measured, 3.3x predicted) was measured once.
- Layout A needs the agent outside the Bash sandbox; the sandboxed case was
  not re-measured here, S1 covers it.
- Desktop loops in section 3 shared a cgroup with the terminal and the
  agent's own session; the shares include their small background use.
- Cleanup verified: `systemctl --user list-units 'sluice*' --all` empty, no
  `sluice_s2.slice` directory under the user manager, no `sluice-s2` process.
