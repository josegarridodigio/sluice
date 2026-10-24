---
title: "Change: harness s9 proves agent containment end to end (task E1)"
tags: [sluice, change, containment, cgroups, harness, e1]
---

# Change: harness s9, containment proof (2026-10-24)

Task E1 of [[sluice/plan/containment-plan.md]]. Decision
[[sluice/design/adr/0017-agent-resource-containment.md]], design
[[sluice/design/containment.md]], measured values
[[sluice/research/containment-s2-weights.md]]. No AI session is involved, CPU
load only (rules after [[sluice/research/containment-s2-incident.md]]).

## What changed

- `scripts/alpha/s9_containment.py` (new, stdlib only, `Report` JSONL like the
  others, picked up by `run_all.sh`): scenario `s9_containment`, 30 checks.
  It also writes `<ALPHA_OUT>/s9_containment_numbers.json`.
- `scripts/alpha/README.md`: s9 row, run line. `run_all.sh`: comment.
- `scripts/alpha/s8_zero_config.py`: the check "the hook is rewired in place"
  expected the pre-0.4.1 hook text. Since `f79b912` (hooks keep working without
  Sluice) the rewired lines are `if command -v sluice ...; then sluice run
  --hook pre-commit lint; else ng lint; fi`; the expectation follows. s8 went
  from 36/37 to 37/37; nothing else of s8 changed.
- No product code changed.

## Method

The release binary of this tree, a sandbox HOME/XDG tree (as s8), and a unit
tree `sluice-test-e1_<pid>` on the real systemd user manager, selected through
`SLUICE_TEST_CONTAINMENT_PREFIX` (so the machine's `sluice.slice`,
`sluice-daemon.scope` and `sluice-agents.scope` are never touched). A sandbox
daemon (`SLUICE_TEST_MANAGER_RUNTIME_DIR` points it at the real user manager)
owns the daemon and agents scopes. The "agent" is a `sh` with a `sleep` child
(`CLAUDE_PID` is the `sh`). `plugin/hooks/session-start.sh` runs with hook JSON
on stdin and a temp `CLAUDE_ENV_FILE` (twice: startup, resume). Agent shells
are `bash` with `CLAUDECODE=1`, started from a parent that is first moved into
`ui` (as Claude's Bash tool is a child of the agent), and source the env file
before the command.

Latency probes are the S2 probe (wake every 10 ms on an absolute schedule, 1 ms
of spin, wake-up delay recorded; 1 200 samples per round of 12 s). One probe
sits in `ui` (it writes its own pid to `ui/cgroup.procs`), a second runs
outside as the desktop. Loops are `timeout -k 1 15 sh -c 'while :; do :; done'`,
started by one agent shell so they are placed by the env line, not by the
harness. A round never lasts more than 15 s; the whole scenario loads the CPU
for about 1.5 minutes (`load_seconds_total` is wall time of the run, 83 s).
Safety: no memory limit, pressure or hog; only processes the scenario started
are killed (by the cgroups of its own prefix and its process groups); it aborts
when the desktop probe sees a stall above 1 s (never happened; worst desktop
max in any run 26 ms); everything stops in `finally`.

Because the machine is shared (other agents run builds), every round first waits
up to 90 s for the processes outside the test tree to use less than 8 % of the
machine, measures that outside load during the round (`/proc/stat` minus the
test slice's `cpu.stat`), and a latency round is repeated up to three times
before it counts as failed. Every attempt is printed and stored.

## Results (final run, 30/30 checks, 20 CPUs)

| # | Check | Result |
|---|---|---|
| 1 | Hook exits 0 and prints context twice | rc 0/0 |
| 1 | Fake agent and its child in `s-alpha-s9/ui` | yes, both |
| 1 | Env file holds the containment line once after startup and resume | 1 line |
| 1 | Weights of the created tree | ui 400, tools 25, agents scope 30, slice 50 |
| 1 | Sourced agent shell lands in `tools` | yes |
| 1 | Daemon scope and agents scope exist under the test slice | yes |
| 2 | Script file, tool by absolute path (copy of `sh`), `python3 -c subprocess.run`, background command, `setsid nohup` child: loop in `tools` | 5 of 5 (read from `/proc/<pid>/cgroup` while running) |
| 2 | Loops ended by their timeout | yes |
| 3 | 40 loops (2x) in `tools` while measuring | 81 processes (sh + timeout) |
| 3 | 2x: ui probe p99 < 5 ms | 1.8 ms (p50 65 us, p95 90 us, max 9.8 ms) |
| 3 | 2x: desktop probe p99 < 2 ms | 1.2 ms (p50 63 us, p95 109 us, max 3.3 ms) |
| 3 | 1x (20 loops): both bounds | passed on attempt 2 of 3: ui p99 1.06 ms, desktop p99 1.995 ms (bound 2 ms); attempt 1 failed on a ui stall, see below |
| 3 | Idle probe floor | ui p99 265 us, desktop p99 250 us |
| 3 | Gates scope against agents scope, 20 loops each, `cpu.stat` over 8 s | gates 92.9 s CPU, agents 51.2 s: share 64.5 %, ratio 1.81x (weights predict 3.3x, S2 measured 2x) |
| 4 | `tools` leaf unwritable (env line pointed at a read-only dir) | shell stays in `ui`, nice 19, no output, command runs |
| 5 | `enabled = false`: daemon starts, a gate runs, no systemd unit, agent and child do not move, no cgroup or renice line in the env file, an agent shell runs its command where it started | all true |
| - | Nothing left: no `sluice-test` unit, no process in the test cgroups | yes |

The 1x row of the final run: first attempt ui p99 6.9 ms (11 samples over
10 ms, 15 % of the machine used outside the test), second attempt passed
(ui p99 1.06 ms, desktop p99 1 995 us, a hair under the 2 000 us bound).

### Reading the latency numbers: stalls decide the p99

Across the development runs the pattern is consistent. Medians stay at 60 to
65 us with 40 loops (idle is 70 to 160 us), the p95 stays under 0.5 ms, and the
desktop probe is almost always fine. What fails an attempt is a **stall of the
`ui` probe of 20 to 150 ms** (one stall of D ms delays about D/10 consecutive
samples, so one stall decides the p99 of a 12 s window; `stats` therefore also
records `over_10ms` and `worst_at_s`). The desktop probe does not see them at
the same moment. They happened mid-round (not at its start) and only when
processes outside the test used more than about 7 % of the machine.

| Test tree depth | Runs | 2x first attempt passes | 1x first attempt passes |
|---|---|---|---|
| `sluice-test-e1-<pid>`: 4 implicit slice levels below the real `sluice.slice` | 5 | 2 of 5 (one run failed all 3 attempts, one passed on the 3rd) | 1 of 5 (one run passed on the 3rd) |
| `sluice-test-e1_<pid>`: 3 levels (final) | 4 | 3 of 4 | 3 of 4 |

The flatter prefix looked better, which suggests extra group levels add
scheduling jitter, but the samples are small and the outside load varied; do
not read it as proven. The production tree (`sluice.slice` / agents scope /
session / leaf) is one level shallower than the flat test tree.

## Machine

Intel Core i7-1280P (12th gen, hybrid: 6 P-cores with SMT plus 8 E-cores, 20
logical CPUs), 31 GB RAM with swap, kernel 7.0.0-38-generic, systemd 259
(259.5-0ubuntu3.4), cgroup v2, desktop slices `app.slice` and `session.slice`
at `cpu.weight` 100, `background.slice` 30, a live desktop session and other
agents' builds running. Load average before the final run: 15 (the previous
run's own load).

## Open items

- **ui stalls under outside load** (to look at before release): with
  more than about 7 % of the machine used by other processes, the `ui` probe
  suffers isolated stalls of 20 to 150 ms with 20 or 40 loops in `tools`, while
  the desktop probe does not. S2 did not see them (machine idle, flat unit
  tree). Hypotheses: per-CPU group entity weights of a mostly sleeping `ui`
  group decaying (PELT), or the extra slice levels of the test tree. Repro:
  `scripts/alpha/s9_containment.py` on a machine that is not quiet; read
  `over_10ms` and `worst_at_s` in the numbers file. A bound on p99 over 12 s is
  too fragile for a shared machine; consider "samples over 10 ms <= 1 % and p95
  < 1 ms", or a longer window, once the cause is known.
- 8-core rerun (the S2 caveat): not done. The bound must be confirmed on an
  8-core machine, where 40 loops are 5x oversubscription.
- Gates share is 1.8x to 2.0x in every run (predicted 3.3x), same as S2.
  Gate jobs run at the daemon's default nice 10; not separated from the weight
  effect here.
- Never measured here (by rule): memory behaviour, `pids.max` enforcement.
- The scenario fails (rather than skips) on a host that is not fully
  containment capable; add a skip if it must run in CI containers.

## Verification

`python3 scripts/alpha/s9_containment.py` (about 2 minutes) 30/30 on the final
tree. `run_all.sh` limited to s8 and s9 (scratch directory with symlinks to
those two scripts and the helpers): s8 37/37, s9 30/30, "2 scenario(s)
passed". After every run: no `sluice-test*` unit, no process in a
`sluice-test*` cgroup.
