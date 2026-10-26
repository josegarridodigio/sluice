---
title: "ADR-0019: Gate jobs are admitted by learned memory and are the preferred victims of any OOM"
tags: [sluice, adr, containment, memory, scheduler, systemd-oomd]
---

# ADR-0019: Gate jobs are admitted by learned memory and are the preferred victims of any OOM

Status: accepted (2026-10-26). The user asked for the definitive solution (no
temporary patches) and, on 2026-10-26, approved both open decisions: the bounded live
tests on the developer machine (S5 §9 safeguards) and the opt-in Sluice-side guard
(piece 7). The shape of piece 3 is still decided by those tests.
Analysis: [[sluice/research/memory-gates-w6-analysis.md]]. Amends
[[sluice/design/adr/0017-agent-resource-containment.md]] point 4 and
[[sluice/design/containment.md]] §4, §5, §7. Builds on
[[sluice/research/containment-s5-memory-oomd.md]]. Plan: W6 of
[[sluice/plan/queue-trust-plan.md]].

## Context

Incident (a second machine, Ubuntu 26.04, systemd 259, kernel 7.0, 12 threads, 30 GiB
RAM, 8 GiB swap, five agents, Sluice 0.5.0), measured read-only:

- One `pnpm test` gate (`node --test`, one worker per core minus one, 1.5–2.7 GiB each)
  peaked at 18 GiB in its job leaf; the daemon scope at 24.5 GiB. Two or three such gates
  (weight `auto`→6, budget 12) were admitted together, one over-admitted; memory PSI was
  about 0 at each admission (the previous gate was still ramping up).
- `oomctl` monitored only `sluice-agents.scope` (30 %, 5 s). systemd-oomd killed agents'
  `tools` leaves; the kernel's global OOM killer killed browsers in agents' leaves
  (`oom_score_adj` 300) and an unrelated user service. GNOME showed "Application stopped";
  load average reached 153.
- Cumulative counters afterwards (T-W6-0): `oom_kill` = 0 in `sluice-daemon.scope`, 18 in
  `sluice-agents.scope`, 26 under `user@UID.service`. **The culprit was never the victim.**
  The daemon scope's `full` memory stall total (466 s) exceeded the agents scope's (438 s)
  and its `pgscan` (19.7 M) matched the agents' (19.8 M): an oomd policy on the gates' scope
  would have met both of oomd's trigger conditions. No ancestor has `memory.oom.group = 1`.
  The daemon runs with `oom_score_adj` 0, agents with 100.

Root causes: (1) admission knows CPU weights and PSI, not memory, and PSI lags a ramping
job; (2) the only oomd policy Sluice creates is on the agents' scope, so oomd can only
choose agents; (3) nothing makes the kernel prefer gate processes; (4) a gate killed for
memory is reported, if at all, as an ordinary failure (the interventions watcher misses
kills of leaves removed at job end, and oomd kills never reach `memory.events`).

S5 constraints kept: no `memory.high` by default (throttling creates pressure without
page scanning, which made oomd kill the desktop in the S2 incident); no absolute ceiling
on by default; never kill the agent; memory experiments only bounded or in a VM.

## Decision

1. **Memory-aware admission (prevents the incident).** The daemon learns each gate's
   working set (`anon` + swap of its job leaf, sampled every second; `memory.peak` is
   recorded for display only, it includes page cache) per `(repo, gate, cmd hash)`, in a
   new table of the containment schema group (older binaries ignore it). A job is
   admitted only when `MemAvailable − reserve − Σ headroom(running)` covers its expected
   peak, where `headroom = max(0, expected − current)` of each running job and jobs started
   earlier in the same pass count in full; or when nothing at all runs and nothing started earlier in
   the pass (`alone`: progress is always possible; a running job, however light, withdraws
   it, and there is no "heavy" notion any more). `reserve` defaults to 10 % of `MemTotal`. Unknown first runs use the
   weight as prior (`w/B × (MemTotal − reserve)`). Over-admission additionally requires a
   learned peak that fits. The overdue-head reservation extends to memory-blocked heads.
   Swap never counts as available. CPU weights and ADR-0015 are unchanged. Implemented in W6b
   ([[sluice/changes/queue-trust-w6b-memory-admission.md]]).
2. **Gates are the preferred victims of the kernel.** `sluice cg-exec` raises the job's
   `oom_score_adj` to `max(configured, inherited)` (default 800; unprivileged) before
   `exec`, and each job leaf gets `memory.oom.group = 1`, so a global or in-cgroup OOM that
   picks a gate process kills that whole job (kernel source: `mem_cgroup_get_oom_group`
   walks to the highest ancestor with the flag, so `doctor` checks every ancestor). Works
   without systemd-oomd and with partial containment (adj only: the daemon then
   passes the leaf `-` to `cg-exec`, which skips the move and still raises the adj; with
   `[containment] enabled = false` nothing is wrapped). Implemented in W6a
   ([[sluice/changes/queue-trust-w6a-memory-outcome.md]]).
3. **Gates are the first candidates of systemd-oomd.** `sluice-daemon.scope` gets its own
   `ManagedOOMMemoryPressure=kill` with a shorter duration than the agents' derived one
   (`max(1 s, agents/2)`), only where the agents' policy exists (oomd active, systemd ≥
   257); the `daemon/` leaf carries `user.oomd_omit`. Measured on two machines
   ([[sluice/research/memory-gates-w6d-oomd-spike.md]]): with both scopes over their
   limit oomd acts on the shorter-duration one first; `omit` excludes a leaf (never
   chosen), `avoid` only reorders. Variant B (one policy on `sluice.slice`) is rejected.
   *Implemented in W6c* ([[sluice/changes/queue-trust-w6c-oomd-policy.md]]): the limit is
   the agents' own, the duration `max(1 s, ceil(agents / 2))`.
4. **A gate killed for memory says so.** At job end, while the leaf still exists, the
   daemon reads its `memory.events` (`oom_kill`, `oom_group_kill`), `memory.peak` and the
   sampled working set; for SIGKILL-shaped endings without a counter it checks the oomd
   journal once (bounded). Such a job ends `Killed` with `killed_by = "memory"` and a
   message (`killed for lack of memory after 41s; job peak 18.1 GiB, 1.4 GiB available`),
   an interventions row, and every client (MCP `verify`, `sluice run`, `sluice exec`, the
   queue) prints the message instead of treating it as a cancel. Never inferred from the
   exit code alone.
5. **Optional ceiling.** `[containment] gates_memory_max` (with `gates_memory_swap_max`)
   on `jobs/`, empty by default. No `memory.high` key is offered for gates.
   *Implemented in the docs-sync follow-up
   ([[sluice/changes/queue-trust-docs-sync.md]]): written on `jobs/` only when the daemon
   scope is created or reused, `max` when unset; a hit is an in-cgroup kill of one whole
   job, classified by piece 4 from the leaf's counters.*
6. **Commands that escape the budget.** `cargo check` (and other compilers found equally
   heavy) join the built-in heavy list. Implemented: `cargo check` and `cargo doc`
   (`DEFAULT_HEAVY_COMMANDS`).
7. **Sluice-side guard (opt-in, `[admission] memory_guard = false` by default).** Under
   real exhaustion (`full` memory pressure of the daemon scope and `MemAvailable` below
   the reserve for at least 3 s of wall time, whatever the sampling period) the daemon cancels its own running gate
   job with the largest working set, reported as `killed_by = "memory"` like piece 4.
   Never an agent's process, never a job alone in the machine when the agents cause the
   shortage (it only acts while gates hold more than the reserve). The only protection
   without systemd-oomd or below systemd 257.
   *Implemented in W6e* ([[sluice/changes/queue-trust-w6e-memory-guard.md]]).

**Amended in 0.6.1 (first-run estimate).** Measured on a 12-thread, 30 GiB machine:
three first-run gates (`fmt`, `lint`, `secrets`) that really used about 20 MB each
waited more than 5 minutes behind a test suite, because the weight proxy guessed 2.3 GiB
for each. Learned peaks on two machines: weight 1 gates 0.02-1.3 GiB, weight 2 gates
0.2-2.2 GiB, weight 6-16 suites 4-13 GiB. A gate with no learned peak is now estimated
at `min(proxy, prior(w))`, `prior(w)` being the largest expected peak among the learned
keys of the daemon whose weight is at most the job's effective weight; without such a
key the proxy is used as before. The figure stays `guessed`, so over-admission still
needs the gate's own learned peak. No schema change: the weight of a key comes from
`jobs.weight` of the runs in `gate_memory`
([[sluice/changes/memory-cold-start-prior.md]]).

## Consequences

- Two 18 GiB suites never start together on a 30 GiB machine; the second waits with
  `wait_detail = memory` and starts when the first ends.
- When memory still runs out (a gate outgrows its history, agents grow), the kernel and
  oomd pick the gate first, which dies whole and explains itself. This orders the victims;
  it does not guarantee that nothing else dies: in a fast OOM the kernel acts several
  times within a fraction of a second, before the first victim's memory is freed, and can
  take the next candidates too (measured in containers,
  [[sluice/research/memory-gates-w6-container-tests.md]]). Admission (piece 1) is what
  keeps the machine out of that state.
- The memory admission and the guard use the memory the gates may really have: the
  machine's `MemAvailable` capped by every limited cgroup above the daemon (a container,
  a CI job, a unit with `MemoryMax=`) and by the gates' own ceiling (found by the
  container tests: with the machine's figure the daemon in a 2 GiB container admitted
  four gates that needed 3 GiB).
- ADR-0017 point 4 becomes: "no memory limit by default; gate jobs are admitted by learned
  memory, are the preferred victims (`oom_score_adj`, `oom.group` on the job leaf, an oomd
  policy acting before the agents'), and the agents' policy is the second line". S5's
  objection to `oom.group` concerned session cgroups that contain the agent; a job leaf
  never does.
- New config keys are one-way (older binaries reject them): documented in
  `docs/compatibility.md` with an unreleased fixture.
- `status`/JSON gain memory fields (additive, schema 1); the status fixture is updated
  deliberately.

## Alternatives considered

- **A default hard ceiling computed from `MemTotal`**: an absolute threshold that kills
  legitimate suites and cannot know the agents' share; kept as opt-in only.
- **`memory.high` on gates**: the S2 incident's mechanism; rejected.
- **Throttling the agents instead**: they are not the cause and include the user's UI.
- **Relying on oomd alone**: absent on Arch/Debian by default, needs systemd ≥ 257 for
  ordering, ignores `oom_score_adj`; admission and the kernel preference are needed anyway.
- **Fixing it in each repository (lower test concurrency)**: helps that repository, does
  not stop the next one; the product must protect the machine by construction.

## Open items (decisions and measurements)

- ~~Shape of the oomd policy~~: decided by T-W6-14/16 on two machines (2026-10-26),
  [[sluice/research/memory-gates-w6d-oomd-spike.md]].
- ~~T-W6-15 (systemd 255), T-W6-17 (memcg OOM with `oom.group` and adj), T-W6-19 (admission end to
  end, swap on/off)~~: done in containers, 2026-10-26,
  [[sluice/research/memory-gates-w6-container-tests.md]]. Confirmed: systemd 255 rejects the per-unit
  duration (`systemd-run` fails) and its first victim between two scopes is not controllable, so the
  policy stays off below 257; `oom.group` kills the whole job leaf and the leaf's `memory.events`
  is sound evidence, but the adj only orders victims (the adj-300 process also died); admission waits
  with `wait_detail: memory` and nothing starves. New: Sluice trusts the host's `/proc/meminfo`, which
  is wrong inside a memory-limited container (admission never waits there).
- Still open: global OOM (not memcg) with `oom.group` and adj, zram, other kernels, and T-W6-18; they
  need a disposable VM or a dedicated machine.
