---
title: "Containment spike S6: why the agent UI stalls under outside load"
tags: [sluice, research, containment, cgroups, cpu, weights, spike, latency]
---

# Containment spike S6: why the agent UI stalls under outside load

Measured 2026-10-24 on the 20-CPU machine of S2/E1 (i7-1280P hybrid, kernel 7.0,
systemd 259, cgroup v2). Task S6 of [[sluice/plan/containment-plan.md]]; open
item of E1 ([[sluice/changes/containment-proof.md]]); layouts come from
ADR-0017 ([[sluice/design/adr/0017-agent-resource-containment.md]]) and
[[sluice/design/containment.md]]; probe and method from
[[sluice/research/containment-s2-weights.md]]. CPU only: no memory limit,
pressure or hog; every process and unit was started by the spike and removed.

## Summary

- **H1 (the `ui` leaf inherits the low weight of the agents scope) is not
  supported.** Moving `ui` to its own scope at weight 100 (L1) did not remove
  the stalls (ui rounds with a sample over 10 ms: 3 of 8 against 4 of 8 in L0).
  A control probe placed like a normal app (weight 100 scope, nothing to do
  with Sluice) stalled at the same rate in every layout (2 to 3 rounds of 8,
  max up to 80 ms).
- **H3 holds, with a precise form: the stalls come from oversubscription by the
  outside load pattern, not from Sluice's weights or from the probe.** They
  appear when the busy loops (tools plus outside) outnumber the CPUs (20 + 10,
  20 + 4 and 20 + 20 loops on 20 CPUs) and hit every sleeping task, including
  an ordinary app, whatever its weight. With exactly 20 loops and nothing
  outside they do not happen (ui p99 74 us, max 1.0 ms). The same probe code
  also measures idle floors of 250 us and loaded medians of 60 us, so Python
  and timer slack are not the cause.
- **H2 (hierarchy depth) is weak.** The flat layout (L3, two levels) had the
  smallest tails in the main cell (ui p99 median 0.97 ms, 4 samples over 10 ms
  in 8 rounds, against 2.5 ms and 14 for L0), but L2b, which is as deep as L0,
  was as quiet (3 samples), and with 3 to 8 rounds per stall class the
  difference does not reach significance.
- The one clear effect of the layout: **L2 (ui and a nested work slice under
  one parent slice at weight 100) is the worst for the UI (6 of 8 rounds with
  stalls, 50 samples over 10 ms) and weakens the protection of the machine**
  (tools take about 45 % of the contended CPU against about 29 % in L0).
- **Recommendation: keep L0** (the layout of ADR-0017). No variant measured
  here improves the UI beyond noise without costing protection or complexity.
  Change the E1 acceptance bound instead (section "Consequences for the
  harness").

## Method

Scratch harness (not product code), `sluice-test-s6-*` scopes in a test slice
tree `sluice_test_s6*.slice`. Slices use underscores on purpose: systemd nests
slices by dash (`sluice-test-s6.slice` would sit under the real `sluice.slice`).
The machine's `sluice.slice`, `sluice-daemon.scope` and `sluice-agents.scope`
were never touched. Every scope is `Delegate=yes` with a `holder/` leaf (the
scope's own sleeper) and cpu enabled in `cgroup.subtree_control`.

```
user@1000.service/sluice_test_s6.slice            (default weight 100; the "machine")
  sluice-test-s6-outside.scope  w100  loops/      outside competitor: N busy loops, a normal app
  sluice-test-s6-desk.scope     w100  probe/      control probe, placed like a desktop app
  layout subtrees (all built once, only one is loaded per round, idle ones cost nothing)
```

| Layout | Tree under the test slice (weights) | Mimics |
|---|---|---|
| L0 | `sl0` (50) / agents scope (30) / `s/ui` (400), `s/tools` (25) | the current design; `sl0` stands for `sluice.slice`, `outside` for `app.slice` |
| L1 | `sl1` (50) / agents scope (30) / `s/tools` and uis scope (100) / `s/ui` (400) | ui leaves in a separate scope of normal weight, same slice |
| L2 | `sl2` (100) / uis scope (100) / `s/ui`, and nested `sl2-work` (30) / agents scope (100) / `s/tools` | slice weight 100, tools under a lower-weight nested slice |
| L2b | `sl2bw` (50) / agents scope (30) / `s/tools`, and sibling `sl2bu` (100) / uis scope (100) / `s/ui` | extra: two sibling slices instead of nesting |
| L3 | agents scope (50) / `ui` (400), `tools` (25), no slice and no session level | L0 with fewer levels (H2): 2 levels instead of 4 |

- **Probes**: S2's probe (wake every 10 ms on an absolute schedule, spin 1 ms,
  record wake-up delay) with the Python GC disabled, 700 samples per round
  (7 s). One probe in the layout's `ui` leaf, one control probe in the `desk`
  scope, always concurrently. Abort if the control probe reports a 1 s stall
  (never happened).
- **Load**: `timeout -k 1 9 sh -c 'while :; do :; done'`, each placed by a wrapper
  that writes its pid to the leaf and `exec`s. N outside loops in `outside/loops`
  plus 20 tools loops in the layout's `tools` leaf. Rounds last about 9 s of
  load; total loaded wall time of the spike 459.6 s (50 rounds) against the 8
  minute budget.
- **Order**: the layouts of the main cell were interleaved in rotated order
  (8 passes) so slow drift of the shared machine hits all of them alike.
  Before each round the harness waits up to 25 s for the processes outside the
  test slice to use under 5 % of the machine; it records their share during the
  round (`other %`, median 4.0 to 4.6 per cell, max 10.3).
- **Share**: `cpu.stat usage_usec` delta of the tools leaf and of the outside
  loops over the probe window; `share = tools / (tools + outside)`.
- Stall metrics: p50, p95, p99, max, and the count of samples over 10 ms (one
  stall of D ms delays about D/10 consecutive samples, so a few samples over
  10 ms mean one or two stalls, not a general slowdown).

## Results

### 1. Main cell: 10 outside loops, 20 tools loops, 8 rounds per layout

Medians over 8 rounds, spread in brackets (min to max), microseconds unless
stated.

| Layout | ui p50 | ui p95 | ui p99 | ui max (ms) | rounds with a ui stall >10 ms | ui samples >10 ms (sum of 8 rounds) |
|---|---|---|---|---|---|---|
| L0 | 60 | 80 (69-966) | 2 451 (316-5 021) | 10.8 (3.4-50.0) | 4 of 8 | 14 |
| L1 | 61 | 116 (65-921) | 2 933 (81-79 378) | 9.1 (3.0-133.4) | 3 of 8 | 25 |
| L2 | 62 | 516 (65-2 223) | 6 593 (98-102 998) | 42.4 (4.8-155.0) | 6 of 8 | 50 |
| L2b | 61 | 84 (68-1 783) | 2 062 (137-4 800) | 6.2 (2.0-16.8) | 2 of 8 | 3 |
| L3 | 60 | 73 (62-83) | 967 (104-1 967) | 5.0 (0.9-23.0) | 3 of 8 | 4 |

Control probe (a weight-100 app scope next to the layout, same rounds):

| Layout round set | desk p99 | desk rounds with a stall >10 ms | desk samples >10 ms | ui and desk stalled in the same round |
|---|---|---|---|---|
| with L0 | 1 520 (95-32 538) | 2 of 8 | 17 | 1 |
| with L1 | 2 116 (790-4 905) | 3 of 8 | 8 | 2 |
| with L2 | 2 935 (969-12 148) | 3 of 8 | 15 | 3 |
| with L2b | 2 114 (1 913-5 571) | 3 of 8 | 5 | 1 |
| with L3 | 1 923 (933-9 959) | 3 of 8 | 17 | 0 |

(The desk probe was fixed in place in the outside group, so its numbers differ
only because the rest of the tree differs and by noise.) Pooled over every
round that had outside load (49 rounds): the ui probe stalled in 21, the
control in 17; the stall rate is the same order, and the two seldom coincide
(1 to 3 of 8 rounds), which fits per-CPU events: a sleeper that wakes on a CPU
that is oversubscribed waits several slices. Stall rounds were 16 of 38 when
outside load was under 5 % and 5 of 11 when it was 5 % or more: no relation to
the "outside used more than about 7 %" observation of E1 (here the outside
load is mostly the 10 controlled loops).

### 2. Other cells (fewer repeats, budget)

| Cell | Layout | n | ui p99 (range) | ui rounds with stall | desk rounds with stall |
|---|---|---|---|---|---|
| outside 0, tools 20 | L0 | 1 | 74 | 0 of 1 | 0 of 1 |
| outside 4, tools 20 | L0 | 2 | 5 691 (2 398-8 983) | 1 of 2 | 2 of 2 |
| outside 4, tools 20 | L1 | 2 | 1 514 (770-2 257) | 0 of 2 | 0 of 2 |
| outside 20, tools 20 | L0 | 1 | 346 | 0 of 1 | 0 of 1 |
| outside 20, tools 20 | L1 | 1 | 16 135 | 1 of 1 | 0 of 1 |
| outside 20, tools 20 | L2 | 1 | 1 962 | 0 of 1 | 0 of 1 |
| outside 20, tools 20 | L2b | 1 | 2 993 | 1 of 1 | 0 of 1 |
| outside 20, tools 20 | L3 | 1 | 932 | 0 of 1 | 1 of 1 |

Not run for budget: tools 40 and outside 20 with repeats (E1 already measured
40 loops without outside load). These cells have one or two rounds, they only
corroborate: no stalls without oversubscription (outside 0), stalls in both
probes with it. The "outside 4" cell has 24 loops on 20 CPUs and already
stalls the control in 2 of 2 rounds.

### 3. Protection of the machine: share of CPU taken by tools

With 10 or 4 outside loops the machine is not saturated by the outside
competitor (it needs 10 or 4 of 20 CPUs), so work conservation gives tools
whatever is left: shares of 0.53 to 0.57 and 0.79 do not measure protection.
The saturating cell (20 outside loops against 20 tools loops, one round per
layout) does:

| Layout | tools share vs outside (20 + 20 loops) | Weight prediction | Main cell (10 outside), median and range |
|---|---|---|---|
| L0 | 0.294 | 50 vs 100: 0.33 | 0.526 (0.504-0.545) |
| L1 | 0.381 | 0.33 (ui idle) | 0.549 (0.511-0.575) |
| L2 | 0.447 | 100 vs 100: 0.50 | 0.569 (0.553-0.588) |
| L2b | 0.348 | 50 vs 100: 0.33 | 0.546 (0.536-0.571) |
| L3 | 0.295 | 50 vs 100: 0.33 | 0.526 (0.511-0.558) |

Single round each, so +-0.05 is noise (L0 and L3 agree at 0.29, and S2 measured
20.6 % for slice weight 30 against 23 % predicted). The mechanism is clear
and matches the prediction: **the share against the outside is decided by the
weight of the highest group that contains the tools loops and no mostly-idle
sibling.** A nested lower-weight slice (L2) does nothing while the `ui` sibling
is asleep, because the parent slice then behaves as one busy group of weight
100: tools get about 45 % instead of about 30 %. L2 worsens the protection of
the rest of the machine, against the requirement that it must not be worse than
L0 (measured about 21 to 33 % in S2 and here). L1 and L2b keep the tools at the
weight of the slice (L1 0.38 is within noise of 0.33, the other rounds of L1
under 10 outside loops are within the range of L0).

## Which hypothesis holds

| | Evidence | Verdict |
|---|---|---|
| H1: `ui` inherits the low weight | L1 (ui at weight 100, own scope) is not better than L0 in stall rounds (3 vs 4 of 8) or in samples over 10 ms (25 vs 14); L2 and L2b, ui in a weight-100 group, are worse and equal respectively; the control at weight 100 stalls as often as `ui` | rejected |
| H2: depth | L3 (2 levels) 4 samples against L0 (4 levels) 14 and a lower p99 (0.97 vs 2.45 ms); but L2b (4 levels) also has 3 samples, and L1/L2 (also 4 levels) have 25 and 50 | not supported as the cause; a small gain from flattening cannot be excluded (not significant at n=8) |
| H3: probe or outside load pattern | probe unchanged (GC off) and a control probe outside Sluice shows the same stalls; no stalls without oversubscription; the pattern appears whenever tools plus outside loops exceed the CPU count | holds, as oversubscription of the machine, not the Python probe |

What this does not explain is why a stall lasts 20 to 150 ms with 30 runnable
loops on 20 CPUs (a few slices is the expected wait). The data say it is a
property of the scheduler under oversubscription on this hybrid P/E-core
machine, independent of cgroup layout; it was not investigated with `perf
sched` or the kernel's scheduler debug files (not available without privileges
here). A person using the agent would notice 50 to 150 ms hiccups on a
heavily loaded machine, as with any other app on it, which is what the control
shows.

## Recommendation

**Keep L0.** Unit layout and weights unchanged from [[sluice/design/containment.md]]:

```
sluice.slice                   CPUWeight=50
  sluice-daemon.scope          CPUWeight=100
  sluice-agents.scope          CPUWeight=30
    s-<id>/ui     cpu.weight=400   agent + MCP servers
    s-<id>/tools  cpu.weight=25    shells and everything they start
```

Reasons: none of L1, L2, L2b, L3 improved the UI beyond what a normal app
experiences next to the same outside load; L2 is clearly worse and weakens the
protection of the desktop; L1 adds a scope per session for nothing. L2b and L3
looked best in the tails but cannot be separated from L0 at 8 rounds, and each
costs something:

- **L2b (ui in a separate slice at weight 100)** would make the agent and its
  MCP servers compete with the desktop as a normal app (weight 100 against
  `sluice.slice`'s 50 for tools). The MCP servers live with the agent in `ui`:
  a CPU-hungry one (a headless browser from a Playwright MCP, an indexer) would
  no longer be bounded by the slice weight, so "no process of an agent session
  can saturate the machine" would hold for tools but not for MCP servers. L0
  and L1 keep MCP servers inside `sluice.slice` (50), where the only protection
  is the weight; if MCP servers become a problem the answer is a separate
  `mcp` leaf with a normal weight inside the agents scope, not moving `ui`
  out of the slice.
- **L3 (flat)** merges the slice weight into the scope (50) and drops the
  per-session level, which removes the gates-versus-agents split that
  ADR-0017 uses (agents 30 against gates 100 inside `sluice.slice`) and the
  per-session accounting and pruning cgroup (`s-<id>`). Gains of 1.5 ms in p99
  do not justify it.

If a later measurement with many more rounds (or on an 8-core machine, still
open from S2/E1) shows the flat layout reproducibly better, the cheap change
is dropping the `s-<id>` level (sessions as direct children of the agents
scope, `ui`/`tools` named per session) rather than changing weights; that
needs its own spike and ADR amendment.

## Consequences for the harness (E1)

The p99 over 12 s of one probe is the wrong criterion on a shared machine and
under oversubscription: a single stall decides it, and an ordinary app next to
the same load fails it as often as `ui` does. Proposal for `s9_containment.py`:

- Judge `ui` **relative to the control probe** of the same round: `ui` is not
  worse than the control by more than a stall or two, for example `ui samples
  over 10 ms <= control samples over 10 ms + 7` (1 % of 700) and `ui p95 < 1 ms`,
  and keep the absolute p99 only as a warning.
- With these rules, in the main cell, L0 passes 8 of 8 rounds on `over10 < 7`
  (values 0, 6, 0, 0, 4, 0, 2, 2) and 8 of 8 on `p95 < 1 ms` (worst 966 us). The
  current absolute bounds fail on this load: ui p99 under 5 ms fails on 1
  round of 8 (5 021 us) and the 2 ms control p99 fails on 2 of 8 (5 743 and
  32 538 us) for the control itself.
- The quiet check (`wait_quiet`, 8 % threshold) can stay, but it is not the
  cause of the stalls; do not tune it further.

## Caveats

- 8 rounds per layout in the main cell and 1 to 2 elsewhere; the p99 and max
  are heavy tailed (one round gives 79 ms, the others 3 ms). Medians with the
  range, and the count of rounds with stalls, are the reliable columns.
- The shape of the "machine" is a test slice: the outside competitor is a
  sibling of `sl*` inside the test slice, like `app.slice` next to
  `sluice.slice` under `user@1000.service`; the real desktop and other agents
  also ran (3 to 10 % of the machine during rounds, recorded as `other %`).
- 20 CPUs on a hybrid CPU: the stalls may depend on P/E-core placement; a
  uniform 8-core machine is still unmeasured (open item from S2/E1).
- The protection share was a single round per layout.
- The `ui` probe measures wake-up delay, not rendering throughput.

## Cleanup and safety

Total loaded time 459.6 s in 50 rounds (cap 15 s per round, 8 min total); the
first run was stopped on purpose after round 44 of 48 planned (to leave budget
for the saturating cell), and the second ran the saturating cell. Both stopped
all `sluice-test-s6-*` scopes and the `sluice_test_s6*` slices themselves.
After the runs: no `sluice-test*` or `sluice_test*` unit active and no process
under a `sluice-test*` cgroup. No memory knob was touched; the abort threshold
(desktop probe above 1 s) was never reached (worst control max 79.6 ms).
