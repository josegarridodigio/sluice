---
title: "W6 container tests: systemd 255, memcg OOM with oom.group, memory admission end to end"
tags: [sluice, research, memory, systemd-oomd, oom-group, admission, docker, w6, adr-0019]
---

# W6 container tests (T-W6-15, T-W6-17, T-W6-19)

Replace the "disposable VM" tests T-W6-15, T-W6-17 and T-W6-19 of section D of
[[sluice/research/memory-gates-w6-analysis.md]] with bounded experiments in Docker
containers. They complete the open items of
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]] after the oomd spike
[[sluice/research/memory-gates-w6d-oomd-spike.md]] (T-W6-14/16). Safety rules:
[[sluice/research/containment-s5-memory-oomd.md]] §3 and §9. Run on 2026-10-26.

## Method

Host: the developer machine (20 threads, 30 GiB RAM, 8 GiB swap), Ubuntu 26.04, kernel 7.0,
Docker 29 with the systemd cgroup driver on cgroup v2. Every container is named
`sluice-w6-ct-*`, has a hard memory limit of at most 2 GiB with an explicit
`--memory-swap`, `--cpus 2` and `--pids-limit 512`, and is removed by a trap on exit; the
three images (`sluice-w6-ct-t15`, `-t17`, `-t19`) were removed at the end. Scripts and build
contexts live outside the repository (session scratchpad).

Safety harness around every container:

- Before each start: host `/proc/pressure/memory` `full avg10` below 5 and `MemAvailable`
  above 8 GiB, otherwise stop.
- A host watchdog (1 s loop) that runs `docker kill` on the container when the host's
  `full avg10` reaches 10. It fired twice during the development of T-W6-15 (10.4 and 10.5)
  and never in the runs reported below. The first allocators were too aggressive: on a quiet
  host the system-wide PSI is dominated by the few tasks that stall (the effect of S5 §2),
  so two throttled allocators alone moved the host's `full avg10` from 0 to 10 within 9 s.
  The final allocator paces itself (below), and the test scripts also abort a round when the
  host reaches 5.5 to 7.
- The watchdog did not fire in T-W6-17 and T-W6-19; the host's `full avg10` stayed at 0.00 in T-W6-19 (the harness logged one sample per second).

| Test | Image | Memory / swap | Other parameters |
|---|---|---|---|
| T-W6-15 | Ubuntu 24.04, systemd 255.4, `systemd-oomd`, python3 | `2g` / `2g` | systemd as PID 1: `--privileged --cgroupns=private --tmpfs /run --tmpfs /run/lock`; `DefaultMemoryPressureDurationSec=2s` |
| T-W6-17 | Ubuntu 24.04, python3 | `1g` / `1g` | `--cgroupns=private --cap-add SYS_ADMIN --security-opt apparmor=unconfined --security-opt seccomp=unconfined` (not privileged: the cgroup tree is mounted read-only by default; the test remounts it read-write inside the namespace) |
| T-W6-19 | Ubuntu 26.04, git, python3, the release binary built on the host (`sluice 0.5.0`, branch `fix/queue-push-and-prepare`) | `2g` / `2g` and `2g` / `3g` | `--cgroupns=private`, normal user with `HOME` set, no systemd, `[containment] enabled = false` |

## T-W6-15: systemd 255 (Ubuntu 24.04)

(a) `systemd-run --scope -p ManagedOOMMemoryPressure=kill -p ManagedOOMMemoryPressureLimit=5%
-p ManagedOOMMemoryPressureDurationSec=1s sleep 1` fails with exit code 1 and
`Unknown assignment: ManagedOOMMemoryPressureDurationSec=1s`. The whole command is rejected,
not just the property: a transient scope that carried it would not start.

(b) Two system scopes (`Delegate=yes`, kill, 5 %, no per-unit duration), each with a leaf
(`memory.high = 64M`, `memory.swap.max = 0`) holding a throttled allocator that asks 200 MiB,
and `DefaultMemoryPressureDurationSec=2s`. Five rounds, 16 s apart (the start order of the
scopes alternates). The victim is read from the oomd journal.

| Round | Start order | First scope acted on | Second scope (after the 15 s post-action delay) | Host `full avg10` peak |
|---|---|---|---|---|
| 1 | A, B | A (6.5 s) | not reached (round aborted by the host guard) | 7.7 |
| 2 | B, A | A (4.8 s) | B (21.0 s) | 6.0 |
| 3 | A, B | A | not reached (round aborted by the host guard) | 7.4 |
| 4 | B, A | B (5.5 s) | A (20.5 s) | 1.5 |
| 5 | A, B | A (7.3 s) | B (22.3 s) | 4.7 |

Result: the first scope acted on was A, A, A, B, A. It does not follow the start order
(round 2 started B first and killed A; round 4 started B first and killed B). With
equal policies and no per-unit duration the order is not controllable; oomd kills one scope per
cycle and the other only after its 15 s delay. This is what the code already assumes: below
systemd 257 Sluice must not create the gates' policy, and the gate is also needed for a
second reason, (a): the scope would fail to start.

Unexpected finding, not in the plan: **oomd 255 only kills when there is reclaim activity.**
The first allocator (anonymous memory only, `memory.swap.max = 0`, so nothing in the leaf is
reclaimable) kept its scope above the 5 % limit for 22 s in one run and 25 s in another
(scope `avg10` up to 76 %, `oomctl` showing `Pgscan: 0`), and oomd never acted. The same
test with 8 MiB of file reads per step (page-cache reclaim, `pgscan` above 0) was killed after
about 6 s, and the journal line says so: `... being 20.90% > 5.00% for > 2s with reclaim
activity`. The spike on systemd 259 killed anonymous-only allocators, so this requirement is
a 255 behaviour that later versions dropped or relaxed (not bisected here). It does not
change Sluice's design (no gates' policy below 257) but it means a vendor policy on 255 does
not protect against a pure anonymous-memory hog with swap disabled.

## T-W6-17: memcg OOM with `oom.group` and `oom_score_adj` (container limit 1 GiB)

Tree created inside the container's cgroup namespace: `t/` with `+memory`, leaf `t/job`
(`memory.oom.group` 1 or 0) with two workers (adj 800, 250 MiB each, touched), leaf `t/other`
with one process (adj 300, 300 MiB), leaf `t/trig` with a trigger (adj 0) that allocates
until the container limit is exceeded. Steady state: container at 819 MiB, `t/job` 501 MiB,
`t/other` 300 MiB. Three trigger shapes were run (8 MiB steps every 20 ms, 1 MiB steps every
50 ms, and one 400 MiB burst); each with `oom.group` 1 and 0.

| Trigger | `t/job` `oom.group` | Died | `t/job` events | `t/other` events | Container root `oom` / `oom_kill` / `oom_group_kill` |
|---|---|---|---|---|---|
| 8 MiB steps | 1 | both workers, `other` | `oom_kill 2`, `oom_group_kill 1` | `oom_kill 1` | 2 / 3 / 1 |
| 8 MiB steps | 0 | both workers, `other` | `oom_kill 2`, `oom_group_kill 0` | `oom_kill 1` | 3 / 3 / 0 |
| 1 MiB steps | 1 | both workers, `other` | `oom_kill 2`, `oom_group_kill 1` | `oom_kill 1` | 2 / 3 / 1 |
| 1 MiB steps | 0 | both workers, `other`, trigger | `oom_kill 2`, `oom_group_kill 0` | `oom_kill 1` | 5 / 4 / 0 |
| 400 MiB burst | 1 | both workers, `other`, trigger | `oom_kill 2`, `oom_group_kill 1` | `oom_kill 1` | 3 / 4 / 1 |
| 400 MiB burst | 0 | both workers, `other` | `oom_kill 2`, `oom_group_kill 0` | `oom_kill 1` | 4 / 3 / 0 |

What the runs show:

1. **Victim selection follows the adj.** The first victim was always an adj-800 worker, then
   the other adj-800 worker, then the adj-300 process; the adj-0 trigger died only in the runs
   where the OOM killer was invoked a fourth time. Order read from the host kernel log
   (read-only; the container cannot read the ring buffer): worker, worker, `other`.
2. **`oom.group = 1` kills the whole leaf in one invocation.** The kernel log says
   `Tasks in .../t/job are going to be killed due to memory.oom.group set`, then both workers
   are killed back to back, and the leaf shows `oom_group_kill 1`. With `oom.group = 0` every
   leaf shows `oom_group_kill 0` and each OOM invocation kills one process (root `oom` equals
   or exceeds `oom_kill`).
3. **The per-leaf `memory.events` is usable as the job-end evidence of piece 4** even though
   the OOM was triggered by a limit two levels above: the kills are counted in the victims'
   own leaf (`oom_kill`, and `oom_group_kill` for the group walk). `oom_kill` counts tasks and
   over-counts by one in some runs (one earlier run showed 3 for two workers), so the
   evidence must be "greater than zero", not an exact count.
4. **Not met: "the adj-300 process survives".** In all six runs (and two earlier ones) the
   process in `t/other` died too. The OOM killer was invoked two to five times within about
   0.1 s: the trigger kept faulting pages while the victims were still releasing memory
   (the sampled `memory.current` fell from 1017 to 285 MiB only 20 ms after the counters
   had already moved), so each failed charge started another invocation and took the next
   highest score. The freed 500 MiB would have sufficed. The group walk and the adj ranking
   work as designed; what does not hold is that a lower adj protects a process absolutely:
   it only moves it later in the queue. A fast-growing hog (a compiler) can take further
   victims after the job leaf is gone. Whether this over-kill also happens in a
   system-wide OOM was not tested.

Caveat for all of T-W6-17: this is a **memcg OOM** (the container limit), not a global one.
The victim selection (`oom_badness`) and the `oom.group` walk are the same code, but
`totalpages` is the container limit (1 GiB) instead of RAM plus swap (38 GiB on the
developer machine), so adj 800 weighs about 820 MiB here and about 30 GiB there. The
ordering (800 above 300 above 0) holds in both, the margin by which a small process with a
high adj beats a large one with a low adj does not.

## T-W6-19: memory-aware admission end to end (Ubuntu 26.04)

Setup inside the container, as a normal user: a git repository with `.sluice.toml` holding
five gates (`g1` to `g5`, `weight = 1`, each running a Python allocator that ramps by 7 MiB every
0.2 s, holds 1 s and exits 0); user config `[daemon] budget = 4`, `[admission]
memory_aware = true`, `memory_reserve = "300M"`, `[containment] enabled = false`. Each gate
runs once alone first so its peak is learned; then a tracked file changes (new tree, no cache
hit) and `g1 & g2` start together; then a second tree change and all five start together.
`sluice status --json` was sampled every second.

`/proc/meminfo` in a container shows the host. Sluice reads it, so the container was run in
two ways: **fake** (a file bind-mounted over `/proc/meminfo`, rewritten every 0.2 s from the
container's cgroup: `MemTotal = memory.max`, `MemAvailable = memory.max - memory.current +
inactive_file`, as lxcfs does) and **raw** (the host's `/proc/meminfo`, as in an ordinary
container). The ramp was first 700 MiB as specified; the 700 MiB pair turned out to fit (see
below), so the ramp was also run at 777 MiB (111 steps).

| Run | Swap | `meminfo` | Ramp | Alone | `g1 & g2` | Five gates | Kills | Container `memory.peak` |
|---|---|---|---|---|---|---|---|---|
| A | none | fake | 700 MiB | 21.4 s each | both ran together, 21.3 s each, no wait | waves of 2/2/1, all done in 65.9 s | none | 1.40 GiB |
| B | 1 GiB | fake | 700 MiB | 21.4 s each | both ran together, 21.4 s | waves of 2/2/1, 65.9 s | none | 1.40 GiB |
| C | none | fake | 777 MiB | 23.6 s each | `g2` ran, `g1` waited with `wait_detail: memory` for 23.6 s, then ran (total 47.7 s) | strictly one at a time: ends at 23.6, 47.2, 70.9, 94.5, 118.0 s (total 118.7 s) | none | 0.78 GiB |
| D | 1 GiB | fake | 777 MiB | 23.7 s each | `g2` waited for `g1` (47.6 s total) | one at a time, 117.8 s total | none | 0.78 GiB |
| E | none | raw | 777 MiB | 23.6 s each | both ran together, no wait | four started at once (budget 4); three gates died with exit 137, two passed (23.6 s and 38.2 s) | 3 (container `oom_kill 3`, `max` 91) | 2 GiB (limit) |

Status samples of run C, `g1 & g2` (every fourth second; `avail` is the daemon's
`memory.available_bytes`, `headroom` its `headroom_bytes`):

| Time | Running | Queued | Container usage | Headroom |
|---|---|---|---|---|
| +4.1 s | `g2` | `g1` (`wait_detail: memory`) | 160 MiB | 816 MiB |
| +12.2 s | `g2` | `g1` (`memory`) | 440 MiB | 542 MiB |
| +20.3 s | `g2` | `g1` (`memory`) | 720 MiB | 262 MiB |
| +24.3 s | `g1` | none | 46 MiB | 946 MiB |
| +44.6 s | `g1` | none | 748 MiB | 248 MiB |

Reading:

1. **Admission does what it promises when `MemAvailable` is true.** With the container's
   figures, a pair that cannot fit runs serially, the second job shows `wait_detail: memory`
   and starts in the pass after the first one ends; nothing is killed, and swap on or off makes
   no difference to admission (swap never counts) nor to the outcome (no run reached the
   limit, so swap behaviour at the limit is untested).
2. **The 700 MiB pair was correctly admitted together.** Learned peak with the 20 % margin is
   about 850 MiB per gate, 1.70 GiB for two against 2.03 GiB available minus the 300 MiB
   reserve (1.73 GiB): it fits by about 27 MiB, and the real peak was 1.40 GiB. The plan's
   expectation "the second gate waits" only holds when the sum of the expectations exceeds
   the headroom, which the 777 MiB ramp does.
3. **No starvation.** In runs C and D five gates finished one after another in
   submission order of the daemon's queue; the longest wait was 94 s for the last of five
   runs of 23.6 s (the sum of the others), and every gate passed.
4. **In a limited container without a container-aware `/proc/meminfo`, admission cannot
   help** (run E): it saw about 23 GiB available, admitted four gates (four times 777 MiB is
   more than the 2 GiB limit) and the container's OOM killer killed three of them. Sluice
   reported `FAIL exit=137` with the log tail `Killed`; with containment off there is no job
   cgroup, so the failure carries no `killed_by = memory` and no peak.

## What a container can and cannot stand for

Can: the kernel's memcg OOM killer and its `oom.group` walk (same code as the global one),
per-leaf `memory.events` accounting inside a nested delegated tree, oomd of an older
systemd userland (255), Sluice's admission and scheduling end to end under a hard memory
limit, with the real binary.

Cannot:

- **Global OOM.** The container limit makes `totalpages` 1 to 2 GiB, the candidates are only
  the container's tasks (no desktop, no agents, no other users), and `constraint` is
  `CONSTRAINT_MEMCG`. The S2 incident's shape (a system-wide shortage with the UI among the
  candidates) is not reproduced, nor is kswapd/direct-reclaim behaviour under a machine-wide
  shortage.
- **A different kernel.** The container shares the host's kernel (7.0). Other kernels
  (an older LTS, a 6.x without the `memory.oom.group` or PSI changes) are not covered, and
  neither is the systemd userland of a host (only the 255 userland of the image, with the
  host's kernel and cgroup layout).
- **zram.** The image has no zram swap and a container cannot configure one safely;
  zram-specific behaviour (compressed swap counting as memory, thrash before OOM) stays open.
- **A real user manager.** T-W6-19 ran with containment off, so the job leaf, `cg-exec`, the
  adj write and the outcome classification of pieces 2 and 4 were not exercised (they are
  covered by T-W6-13 and the unit tests).
- Host-wide PSI, which the container shares with the host: pressure inside the container
  shows up in the host figures, which is why the watchdog and the self-abort were needed.

## Conclusions for ADR-0019

1. **Piece 3 (oomd policy) below systemd 257: confirmed, and stronger than expected.** On 255
   the per-unit duration is not ignored, it makes `systemd-run` fail; with equal policies and
   no per-unit duration the first victim between two scopes is not controllable (A, A, A, B,
   A over five rounds, independent of start order). The version gate in
   `start_transient_scope_args`/`derive_gates_policy` is required, not optional. Also on 255
   oomd acts only with reclaim activity, so even a vendor policy there would miss a pure
   anonymous hog without swap; the Sluice-side guard (`memory_guard`) stays the only
   protection on those systems.
2. **Piece 2 (adj and `oom.group` on the job leaf): works, with a wording correction.** The
   whole job leaf dies together and the per-leaf `memory.events` (`oom_kill`,
   `oom_group_kill`) is a sound job-end evidence for piece 4 (test "greater than zero",
   not equality). But the adj orders the victims, it does not shield the processes with a
   lower adj: in every run the adj-300 process died too, because a fast-growing hog caused
   several OOM invocations in about 0.1 s. ADR-0019 and the docs should say "preferred
   victims", not that agents survive.
3. **Piece 1 (admission): confirmed end to end** with a true `MemAvailable`: wait with
   `wait_detail: memory`, start on release, nothing killed, no starvation, swap irrelevant to
   the decision.
4. **New limitation to decide on (no code changed here):** Sluice trusts `/proc/meminfo`,
   which in a memory-limited container (Docker, Kubernetes, CI) is the host's. Admission
   then never waits and the container's OOM killer chooses. A possible change is to cap
   `MemAvailable` by the cgroup limit of the daemon's own cgroup chain (`memory.max` minus
   `memory.current` of each ancestor) and to make `sluice doctor` warn when the daemon runs
   in a cgroup whose `memory.max` is below `MemTotal`. With containment off, a kill of a gate
   job is also reported as a plain `exit=137`; the daemon's own cgroup `memory.events`
   `oom_kill` delta could attribute it.

## Open items left

- Global OOM with `oom.group` and adj on a real VM or machine, zram and other kernels
  (T-W6-17/19 in their original form), T-W6-18 (variant B under the incident's shape).
