---
title: "Memory review after a week of parallel agents (0.8.0, two machines)"
tags: [sluice, research, memory, admission, systemd-oomd, oom, scheduler, review]
---

# Memory review after a week of parallel agents (0.8.0, two machines)

The one-week review of memory admission planned in
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]] and
[[sluice/design/adr/0022-first-run-memory-estimate.md]]. The decisions that follow from it
are in [[sluice/design/adr/0026-memory-admission-keeps-what-it-learned.md]].

Nothing was changed on either machine. Sources, all read-only: `daemon.log`, a copy of
`sluice.db` (`jobs`, `job_requests`, `gate_memory`, `gate_memory_bound`, `interventions`,
`queue_*`), the job logs, `sluice status` and `sluice doctor`, the kernel journal (OOM
reports with the per-process table) and the `systemd-oomd` journal. Evidence is marked
**measured** (recorded data), **source** (code of `main` at v0.8.0) or **inference**.

Machines:

- **Developer machine**: 20 threads, 30 GiB RAM, 8 GiB swap. Daemon 0.8.0 since
  2026-10-29 07:11 UTC (25 h analysed, full history since 2026-10-22).
- **Second machine**: 12 threads, 30 GiB RAM, 8 GiB swap. Daemon 0.8.0 since
  2026-10-28 18:37 UTC (37 h analysed, full history since 2026-10-24). Four to ten agent
  sessions at a time, many git worktrees of one pnpm monorepo, plus a local LLM server,
  a desktop agent app and a terminal app outside Sluice.

Projects are named generically: **project A** is a pnpm monorepo whose `test` gate
(`pnpm test`, `node --test` workers) peaks at 11-18 GiB; **project B** is a smaller
TypeScript monorepo; **project C** is an Angular application whose `test` gate peaks at
about 11.5 GiB.

## 1. Summary

| | Developer machine | Second machine |
|---|---|---|
| Jobs (window) | 204 (158 pass, 42 fail, 4 early-verdict) | 704 (546 pass, 116 fail, 16 memory-killed, 21 abandoned, 5 early-verdict) |
| Kernel OOM events / oomd kills | 0 / 0 | 11 / 7 (one of them an agent's `ui` leaf) |
| Memory waits (all history) | 56, 107 min, median 104 s | 278, 954 min, median 100 s |
| Admission blocked by PSI | 1 episode | 235 episodes, 9.9 h; 234 triggered by CPU |
| Cache savings (window) | 128 min | 671 min |
| Stalled reports | 0 | 0 (longest quiet window 52 s) |

The developer machine is healthy: the three heavy project C test runs that waited for
memory (80-103 s) then ran and passed. Everything below is about the second machine,
except the key fragmentation (section 3), which both show.

## 2. Every memory kill on the second machine was a global OOM

**Measured** (kernel journal): all 11 OOM events since 0.8.0 (13 recorded kills of gate jobs) are
`constraint=CONSTRAINT_NONE, global_oom`; the allocating task was unrelated each time (a
Python process in a container, the desktop agent app, a browser, an image decoder).
`oom_score_adj = 800` and `memory.oom.group = 1` worked as designed: the kernel always
picked a gate process and took its whole job. Swap was full (free swap under 200 kB) at
every OOM.

The per-process table of each OOM splits memory into gate jobs (`oom_score_adj` 800) and
everything else:

| OOM (UTC) | Gates | Everything else | What happened |
|---|---|---|---|
| 10-29 16:36 | 19.0 GiB | 10.7 GiB | two project A tests: a learned one (12.7 GiB) and a **probe** of a new key |
| 10-29 18:01 | 20.0 GiB | 9.8 GiB | two project A tests: one of them a new key |
| 10-29 19:36 | 22.7 GiB | 7.8 GiB | two project A tests: a **lower-bound probe** and a second admitted beside it |
| 10-29 19:41 | 22.6 GiB | 7.8 GiB | same pair, second kill |
| 10-29 19:47 | 11.1 GiB | 19.0 GiB | one test; 11 GiB of node processes outside gates |
| 10-29 23:03 to 10-30 03:12 (6) | 11.0-15.2 GiB | 14.5-18.5 GiB | one test alone; 6-10 agent sessions, 290-400 processes outside gates |

On 2026-10-29 the cause was **two heavy tests admitted together**; on 2026-10-30 it was
**one heavy test plus the growth of everything else** while it ran (8-10 agent sessions,
each with its CLI, MCP servers and type checkers: 1-4 GiB per knowledge-base MCP server
group, 1.4-1.9 GiB per out-of-queue `tsc`, and 29-74 node processes outside gates).

## 3. The learned peak is lost whenever an agent changes the command

**Source**: the learning key is `(repo identity, gate, cmd_hash)` and `cmd_hash` covers
the command *and the extra arguments* (`server.rs`, `cmd_hash`; ADR-0011 M2).

**Measured**: project A's `test` gate had **7 distinct hashes in one day** on the second
machine; project C's had **13 in the window and 29 overall** on the developer machine.
The job logs show where they come from: agents add `--filter <package>`,
`--test-concurrency=1|2` or `--workspace-concurrency=1` to `verify`.

Each new hash has no history, so its estimate is a guess from *other* gates of the same
weight (`prior`, P75 of lighter gates clamped to 1 GiB..share): **1.3-1.7 GiB** for a
gate that needs 11-14 GiB. ADR-0022 then lets it start as a probe that needs a quarter of
that (0.3-0.4 GiB) free, beside a running test of the same gate (the "no probe beside a
running job of the key" rule compares hashes, and the hashes differ).

Agents add these arguments **because** a run was killed for memory (rule 6 of the agent
policy: lower the footprint). It did not lower it: `--test-concurrency=1` peaked at
11.3-13.6 GiB against 12-14 GiB without it (`pnpm -r` still runs every package). The
reaction to a kill produces the next kill.

## 4. A lower bound starts as a probe

**Source**: `probe_ok` allows a probe whenever the estimate is not `Learned`, so a
`LowerBound` estimate (1.2 x the largest peak of failed runs) probes too, at a quarter.

**Measured**: 2026-10-29 19:28, project A test, estimate 16.3 GiB lower-bound: started as
a probe holding 4.1 GiB; at 19:32 a second test (estimate 16.3 GiB lower-bound, 21.5 GiB
free) was admitted beside it because the probe only held its quarter. Both reached
11-13 GiB; global OOM at 19:36 and 19:41. A lower bound is evidence that the gate needs
*at least* that much: probing it at a quarter discards that evidence.

## 5. A killed run inflates the estimate by 50 %

**Source**: `expected_peak` adds 25 % to a killed run's peak and then 20 % to the
maximum, so a killed run counts `1.25 x 1.2 = 1.5 x`.

**Measured**: after kills at 12.7 GiB the estimate of the plain hash was 19.0-19.5 GiB,
while the 12 runs that followed peaked at 11.1-13.3 GiB. With the margin applied once
the estimate would have been about 16 GiB.

## 6. The idle-machine exemption started jobs known not to fit

**Source**: `MemPass::fits` admits the first ranked job when no gate runs, however large
its estimate (ADR-0019, "progress is always possible").

**Measured**: 15 starts since 0.8.0 had a learned estimate above the free memory at start
(for example 21.8 GiB expected, 17.5 GiB free). 3 were memory-killed and 12 finished
(peaks 11-13 GiB, below the inflated estimates of section 5). Free memory with agents
running never reached the inflated estimate during the window, so removing the exemption
alone would have stalled the gate rather than saved the three runs. Those three died of
section 2's second cause (growth outside gates), not of the exemption itself. The log
does not say that a job started under the exemption; it was reconstructed from
"expected X, free at start Y".

## 7. Collateral kills right after an OOM

**Measured**: 6 oomd kills of gate jobs on the gates' scope (30 % for 3 s). Four were the
second test of a pair (section 2). Two were type checks of 2.7 GiB started **3-4 s after**
a kernel kill by the PSI starvation guard; the scope's pressure average (41-72 %) still
reflected the dead job, and the new job was the only candidate.

## 8. systemd-oomd killed an agent

**Measured**: 2026-10-30 01:22:50, six seconds after a kernel kill of a gate, oomd killed
`s-<session>/ui` under `sluice-agents.scope` at 30.14 % > 30 % for 5 s: an agent session
ended. Two more `ui` kills happened on 2026-10-26 (0.5.0, the incident that led to
ADR-0019).

**Source and earlier measurement**: `ui` leaves carry `user.oomd_avoid`, which only
reorders candidates; the W6d spike ([[sluice/research/memory-gates-w6d-oomd-spike.md]],
T-W6-16b) measured that an avoided leaf is killed when no other candidate exists.
`tools` leaves exist only while a tool runs (one of seven sessions had one at the time of
the check), so under the agents' policy the `ui` leaf is often the only candidate. The
policy that killed it is the one Sluice installs (ADR-0017 item 4), stricter than the
vendor's 50 % for 20 s on the user manager.

## 9. Other findings (not memory admission)

- **CPU pressure blocks admission for hours**: 235 PSI episodes, 9.9 h in 37 h, 234 of
  them on CPU. `sluice status`: 8 h 51 min with jobs queued, of which only 33 min had idle
  CPUs. The CPU comes from outside the queue (a local LLM server at 180 % at the time of
  the check, agents' own processes). Follow-up: tune the PSI thresholds with this data.
- **Waits longer than the caller's timeout**: 21 jobs abandoned by their last subscriber;
  the `exec` ones gave up at 2.0 min (the agent's default shell timeout), 4.7-5 min or
  9.7-10 min (the maximum), most before they started. Follow-up: tell the caller the
  expected wait so it can run in the background or wait longer.
- **Diagnostics missing from the log**: the extra arguments and `cmd_hash` of a job, its
  peak at the end (`gate_memory` keeps only the newest five runs per key, so older peaks
  are lost), memory and swap outside gates over time, which processes cause CPU pressure,
  and an explicit line when the idle-machine exemption is used. The user decided on
  2026-10-30 to keep the current log level for now.
- **Possible test runs outside the queue**: at the 2026-10-30 OOMs, 29-74 `node` worker
  processes outside gates (`oom_score_adj` 100) were alive. **Inference**: some may be
  test runs that bypassed Sluice; the OOM table has no cgroup, so this is unverified.

## 10. Measures to track after the fix

Memory-killed jobs per day; kills with two heavy jobs of the same gate running; probes
memory-killed; estimate versus peak per family; agent `ui` kills (target 0); jobs started
within 10 s of a memory kill (target 0).
