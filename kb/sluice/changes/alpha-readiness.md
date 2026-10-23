---
title: "Alpha readiness: harness evidence"
tags: [sluice, alpha, evidence]
---

# Alpha readiness: harness evidence

Harness: `scripts/alpha/` (stdlib Python). Agents are simulated: the CLI
for hooks, `mcp_agent.py` speaking MCP over stdio for `verify`; no AI
session is used. Every scenario runs in its own sandbox (private HOME/XDG,
short runtime dir for the socket, the binary under test first in PATH).
Plan: [[sluice/plan/alpha-plan.md]].

Machine: 12th Gen Intel i7-1280P (6 P-cores with SMT + 8 E-cores, 20
logical CPUs), 30 GiB RAM, Linux 7.0, git 2.53.0, Node 24.13.0, Python
3.14.4. Binary: `sluice 0.2.0` (release build).

Test beds (public, cloned into the scratch area, dependencies installed
once and linked into every checkout with a private `.cache`):
`sindresorhus/slugify` @ 3b17b2e (Node: xo + ava, 25 tests) and
`pallets/itsdangerous` @ 672971d (Python: pytest 297 tests + mypy).

## s1_hooks — real stacks and hook managers (2026-10-22): 36/36

Per manager: human commit on a new tree runs the pre-commit gates; same
tree again hits; push runs the pre-push gate once and the next push of the
same tree hits; an agent commit on an unverified tree fails fast with the
actionable message; after MCP `verify` the agent commit passes from the
cache; a broken change is refused by the gate, stays refused on retry and
is never cached as a pass; content identical to a verified tree hits.

| Stack / manager | Checks | `git commit` on a hit, hook included |
|---|---|---|
| Node / husky | 9/9 | 63 ms |
| Node / lefthook | 9/9 | 17 ms |
| Python / pre-commit framework | 9/9 | 129 ms |
| Python / plain hooks | 9/9 | 20 ms |

```
cd scripts/alpha
ALPHA_TESTBEDS=<scratch>/src ALPHA_PYLIB=<scratch>/pylib ALPHA_OUT=<scratch>/out TMPDIR=<scratch>/runs python3 s1_hooks.py
```

## s2_problem — the original problem (2026-10-22)

N simulated agents in N worktrees of the Node project, same workflow at
the same time: verify the fresh checkout, K iterations of edit → verify
(lint + test) → commit (pre-commit: lint), then push (pre-push: test). One
agent breaks the code in its last iteration. Baseline runs the commands
directly in verify and hooks; Sluice uses MCP `verify` and `sluice run
--hook`. The heavy `test` gate is the real `ava` suite followed by a
synthetic, work-bounded CPU load (calibrated iterations per thread) that
stands in for larger suites.

| Variant | Mode | Heavy suites | Peak concurrent | Peak load (1 min) | Wall | Verify p50 / p95 |
|---|---|---|---|---|---|---|
| 8 agents x 3, 4 threads x ~8 s (time-bounded load) | baseline | 40 | 8 | 16.8 | 80.5 s | — |
| | sluice | 25 | 4 | 15.7 | 70.4 s | — |
| 12 agents x 2, 8 threads x ~8 s (work-bounded, overload) | baseline | 48 | 12 | 74.2 | 147.3 s | 36.0 / 37.5 s |
| | sluice | 25 | 2 | 31.5 | 162.5 s | 35.2 / 79.6 s |

Checks: 11/11 in the normal variant; 10/11 in the overload variant (wall
time worse than baseline). In both variants every healthy agent verified,
committed and pushed, the broken change failed verify and its push was
refused, the base-tree requests coalesced (22 coalesced and 35 hits in the
overload run), and Sluice ran each distinct tree exactly once.

Reading of the overload result: Sluice did 48 % less work and kept the
machine responsive (peak load 31 vs 74), but with weight 8 in a budget of
19 units it admitted only two heavy suites at a time, leaving CPU idle,
and serialised twelve agents: total queue wait 743 s, p95 verify latency
doubled. PSI never blocked (`psi_blocked_ms_total` 0). This is a scheduler
policy problem, addressed by workstream B of the plan.

## Findings that are not Sluice defects but shape the docs

- `git commit` reports any failing hook as exit 1; Sluice's fail-fast exit
  3 is visible only in the message. Agent guidance must key on the text.
- Tools that keep per-checkout state under `node_modules/.cache` (xo writes
  a generated tsconfig there) fail when worktrees share one
  `node_modules`; each worktree needs its own caches.

## Defect found

- A socket path longer than 108 bytes (deep `XDG_RUNTIME_DIR`) makes the
  daemon refuse to start; clients fall back to in-process runs with a clear
  message but lose queue and coalescing. Plan task C.

## s3_correctness, s4_chaos, s6_upgrade (task E1, 2026-10-22, binary 0.2.0)

| Scenario | Checks | Runtime |
|---|---|---|
| s3 correctness invariants (items 1–7 of the plan) | 42/42 | 29 s |
| s4 chaos (items 1–6) | 43/48 | 137 s |
| s6 upgrade with live state (v0.1.0 → v0.2.0 → binary under test) | 50/50 | 15 s |

Every scenario ends with a leak check (no daemon or gate process left).

s3 proved: 16 concurrent `verify` on one tree → one run, same job id and
verdict for all; 16 distinct trees → 16 runs within the budget; a failing
tree is never reported as pass, also next to a passing sibling that
differs by one byte; untracked file, mode change, rename and symlink
target change miss while an ignored file hits; `key_inputs`, `key_files`
and TTL invalidate; the user's index, `index.lock` and `.git/objects` are
byte-identical after 200 cache checks; `CI=1` and `SLUICE_DISABLE=1`
behave as documented.

s6 proved: release checksums verified; v0.1.0 markers and job history
survive the upgrades; queue tables appear on first queue use; `update`
stops an idle daemon and leaves a busy one alone; `doctor` flags a stale
daemon.

s4 failures (Sluice defects, fixed by task X1 of the plan):

| # | Item | Observed | Expected |
|---|---|---|---|
| 1 | SIGKILL of the daemon with 6 gates running | all 18 processes of the gate trees still alive 10 s later; the restarted daemon (0.2 s) neither reaps them nor counts them; clients exit 1 "lost the connection" | no gate process survives; interrupted runs reported as killed |
| 2–3 | `logs/` (and the state dir) read-only | `cannot start the gate: cannot create …/logs/<job>.log: Permission denied`; the gate never runs | the gate runs without a log file and the error is reported once |
| 4 | markers dir read-only | run passes and reports the write error once, but `doctor` says `markers: ok` | `doctor` reports the unwritable markers dir |
| 5 | truncated, garbage and key-mismatched markers | ignored and rewritten correctly, `doctor` still `ok` | `doctor` reports corrupt markers |

Other observations: a corrupt `sluice.db` stops the daemon from starting,
clients fall back to in-process runs and `doctor` exits 0 (to be flagged
by `doctor`, task X1); a truncated WAL is recovered with loss of the most
recent history (SQLite semantics, documented); a gate that leaves a
background child holding its stdout delays "job done" until the child
exits (documented in the cookbook).

## s7_command_control — command control (task E3, binary at 844e95f): 64/64

10 simulated agents issue heavy build-like commands; the PreToolUse input is
built exactly as the agent client does and piped into `sluice guard`, and
the returned command is executed with `sh -c`. Sandbox budget 8, default
exec weight 4 (two concurrent loads), each load about 3 s of single-thread
work.

| Run | Peak concurrent loads | Wall |
|---|---|---|
| Raw, no control | 10 | 2.0 s |
| Guard, bypass mode (allow) | 2 | 8.3 s |
| Guard, default mode (ask) | 2 | 8.5 s |
| Shims with `SLUICE_SHIMS=1` | 2 | 8.9 s |
| Shims, human shell | 10 | 2.1 s (no jobs created) |

All heavy commands were rewritten (allow in bypass mode, ask in default
mode), every build artifact existed afterwards, exit codes propagated
(failing builds returned 7 as the raw run did), output was replayed; a gate
command on a verified tree was rewritten to `sluice run <gate> -v` and
answered from the cache; pipelines, substitutions, heredocs were not
rewritten and `sluice queue approve` was denied.

Finding for the tuning loop (L3): the default exec weight `ceil(budget/2)`
is far too conservative for single-threaded commands — the controlled run
took 4x the raw wall time with most CPUs idle. Learned weights do not apply
to exec by design (ADR-0011); a measured or smaller default exec weight is
evaluated in L3.

## s5_merge_queue — merge queue on the Node project (task E4, binary at 844e95f): 65/65, 74 s

Six simulated agents in six worktrees (two conflicting pairs, one branch
that breaks `ava`, the rest independent) verify over MCP and enqueue
(five over MCP, one through the CLI) against a bare remote.

- One train per target at a time; conflicts are reported with their paths
  and the retry hint; a failing train splits and the failing member shows
  `gate test`.
- The remote's main does not move for 20 s before approval; `queue_approve`
  over MCP is refused (`not-allowed`); a human `sluice queue approve --yes`
  lands exactly the approved members plus the audit ref.
- Conflicting agents rebase, verify and call `queue_retry` over MCP; their
  branches land on a second approval.
- Crash during push (pre-receive blocked on a FIFO, the sandbox daemon
  SIGKILLed while the push is alive): `needs_reconcile`; a reconcile while
  the push is still alive answers `push_in_progress`; released hook →
  `landed`; aborted hook → `ready` with the approval voided, and a new
  approval lands it.
- Gate reuse: single-member fast-forward trains whose tree an agent had
  verified reach `ready` without running the gates again.
- Leak check: no process of the sandbox left.

Not covered: a multi-member train whose combined tree an agent verified
(the reuse check uses fast-forward trains); SSH and HTTP remotes (task H2).

## E2 — the problem scenario after the scheduler work (binary at c381a12)

Same scenario as the first runs (work-bounded synthetic suites behind the
real `ava` suite), three variants, each run with and without Sluice on an
otherwise idle machine (cool-down to load < 2 before each mode). Targets
from the plan (workstream L). `idle_while_waiting` is the daemon's own
metric: the time integral of the idle fraction of its CPUs while at least
one job is queued, compared with the wall time.

| Variant | Mode | Heavy suites | Wall | Verify p50 / p95 | Peak load | Idle while waiting |
|---|---|---|---|---|---|---|
| normal: 8 agents x 3, 4 threads, 20 CPUs | baseline | 40 | 71.2 s | 13.1 / 14.6 s | 25.7 | — |
| | sluice | 25 | 35.8 s | 8.1 / 13.0 s | 10.5 | 5.5 % |
| overload: 12 agents x 2, 8 threads, 20 CPUs | baseline | 48 | 122.2 s | 29.3 / 31.7 s | 64.0 | — |
| | sluice | 25 | 70.9 s | 23.9 / 37.0 s | 13.9 | 13.1 % |
| low-core: 6 agents x 2, 4 threads, 6 CPUs | baseline | 24 | 84.7 s | 21.2 / 23.2 s | 15.1 | — |
| | sluice | 13 | 51.9 s | 10.3 / 32.4 s | 4.5 | 13.1 % |

Targets: wall ≤ baseline (met in all three: 0.50x, 0.58x, 0.61x); p95 ≤
1.2x baseline (met in normal 0.89x and overload 1.17x, missed in low-core
1.40x); suite executions = distinct trees (met in all three); idle CPU
while waiting < 10 % (met in normal 5.5 %, missed in overload and
low-core at 13.1 %); peak load ≤ baseline (met in all three).

Before the scheduler work the overload variant finished later than the
baseline (162.5 s vs 147.3 s with p95 79.6 s vs 37.5 s); it now finishes
in 0.58x the baseline's time with p95 within the target.

## L3 round 1 — overcommit 2.0, interval 2 s (configuration only)

Hypothesis: idle CPUs while jobs wait come from too little over-admission
(cap 1.5x, one over-admission every 5 s). Change: `[admission] overcommit
= 2.0`, `overcommit_interval = "2s"` in the sandbox user config; no code.

| Variant | Wall vs baseline | p95 vs baseline | Idle while waiting | Peak load sluice / baseline |
|---|---|---|---|---|
| overload | 74.1 / 129.6 s = 0.57x | 36.4 / 33.7 s = 1.08x | 9.9 % | 15.4 / 61.9 |
| low-core | 50.6 / 85.1 s = 0.60x | 25.5 / 23.5 s = 1.08x | 11.3 % (miss) | 4.7 / 14.7 |
| normal | 47.8 / 72.5 s = 0.66x | 17.8 / 15.3 s = 1.17x | 4.7 % | 11.8 / 15.7 |

Effect: overload now meets every target and low-core p95 drops from 1.40x
to 1.08x; low-core idle-while-waiting improves but still misses; the
normal variant still meets every target but is slower than in E2 (0.66x
vs 0.50x wall, p95 1.17x vs 0.89x), so the larger cap costs something when
the machine has room. Kept for now as a candidate default; decided after
round 2.

Cause of the remaining idle time, from the low-core job timeline: `test`
demand is learned at about 3.0 CPUs (weight_cfg 4) after three runs, but
jobs queued earlier keep weight 4 for their whole wait, so one test plus
lint run where two tests of weight 3 would fit, leaving about 2 CPUs idle.
Round 2 resolves the learned weight at admission time instead of at
enqueue time.

## L3 round 2 — learned weights resolved at admission (code, ca35df7) + round-1 settings

Change: the planner recomputes each queued gate job's weight from the
current learned statistics on every pass (before: fixed at enqueue). Run
with the round-1 settings (`overcommit = 2.0`, `overcommit_interval = "2s"`).

| Variant | Wall vs baseline | p95 vs baseline | Idle while waiting | Peak load sluice / baseline | Checks |
|---|---|---|---|---|---|
| low-core | 37.4 / 75.9 s = 0.49x | 18.3 / 21.2 s = 0.86x | 4.2 % | 4.8 / 13.2 | 14/14 |
| overload | 78.1 / 138.9 s = 0.56x | 39.7 / 35.4 s = 1.12x | 7.0 % | 18.2 / 68.3 | 14/14 |
| normal | 43.9 / 67.6 s = 0.65x | 16.7 / 14.1 s = 1.18x | 4.7 % | 10.8 / 14.9 | 14/14 |

Every target is met in every variant. Low-core idle-while-waiting fell from
11.3 % to 4.2 % and its p95 from 1.08x to 0.86x. The normal variant is
close to the p95 limit (1.18x; E2 with the default 1.5x cap had 0.89x), so
round 3 measures the code defaults (`overcommit = 1.5`, interval 5 s) with
the round-2 code to choose the shipped defaults.

## L3 round 3 — shipped defaults (`overcommit = 1.5`, interval 5 s) with the round-2 code

No configuration override; the binary at ca35df7 with its defaults.

| Variant | Wall vs baseline | p95 vs baseline | Idle while waiting | Peak load sluice / baseline | Checks |
|---|---|---|---|---|---|
| normal | 42.8 / 66.8 s = 0.64x | 15.7 / 13.7 s = 1.14x | 5.0 % | 10.3 / 14.5 | 14/14 |
| low-core | 49.6 / 88.3 s = 0.56x | 22.6 / 24.1 s = 0.94x | 5.5 % | 5.2 / 14.4 | 14/14 |
| overload | 79.2 / 137.5 s = 0.58x | 39.3 / 35.8 s = 1.10x | 7.8 % | 16.6 / 63.3 | 14/14 |

Every target is met in every variant with the shipped defaults, so the
defaults stay at 1.5x / 5 s: they need no configuration and keep the
normal variant further from the p95 limit (1.14x) than round 2's 2.0x / 2 s
(1.18x). The round-1 settings stay documented as a tuning option for
machines where idle-while-waiting is the priority.

## Tuning loop summary

| Round | Change | Targets met |
|---|---|---|
| E2 (start) | scheduler work of B1/L1 | 13 of 15 (idle-while-waiting in overload and low-core, p95 in low-core missed) |
| 1 | overcommit 2.0, interval 2 s (config) | 14 of 15 (low-core idle-while-waiting 11.3 %) |
| 2 | learned weight resolved at admission (code) + round-1 config | 15 of 15 |
| 3 | round-2 code with the shipped defaults | 15 of 15 (shipped) |

Against the measurement that started the plan (overload, 0.2.0 scheduler):
wall 162.5 s vs 147.3 s baseline (1.10x) → 79.2 s vs 137.5 s (0.58x);
p95 79.6 s vs 37.5 s (2.12x) → 39.3 s vs 35.8 s (1.10x); suites 25 of 48
in both; peak load 31 vs 74 → 16.6 vs 63.3.

## E5 — the whole harness against the release candidate (663edaf)

`scripts/alpha/run_all.sh` ran every scenario with the installed binary
(built from 663edaf, after the R2 review fixes and the test stabilisation),
on this machine with nothing else running. The first pass reported one
failure in s3 item 2: the check still assumed a fixed gate weight
(budget 8 / weight 3 => at most 2 concurrent gates) while ADR-0011 admits
gates with their learned demand. The daemon's own job rows showed the
`wide` test gate (a `sleep`, demand 0.011 CPU) learned the floor weight 1
after three passing runs and the admitted weight peaked at exactly the
budget, 8, with no over-admission: the budget was never exceeded. The check
now asserts the real invariants (process concurrency within what the
admitted weights allow, and the admitted weight from the job rows within
the 1.5x cap) and s3 was rerun.

Results (`python3 scripts/alpha/report.py <out-e5>`):

| Scenario | Passed | Failed |
|---|---|---|
| s1_hooks | 36 | 0 |
| s2_problem-default | 14 | 0 |
| s3_correctness | 43 | 0 |
| s4_chaos | 48 | 0 |
| s5_merge_queue | 65 | 0 |
| s6_upgrade | 50 | 0 |
| s7_command_control | 64 | 0 |


Total: 320 of 320 checks pass across the seven scenarios. s2 here is its
default size (8 agents x 2 iterations); the three sized variants are the
L3 rounds above. s7's numbers: raw 10 concurrent loads in 2.1 s, guarded
or shimmed 2 concurrent in 8.6–8.9 s (sandbox budget 8, exec weight 4).

## R4 — release v0.3.0 and the upgrade rerun (2026-10-23)

- CI and the full compatibility matrix (Ubuntu 22.04/24.04, Debian 12,
  Fedora, Alpine, git 2.31.0, native aarch64) green on the release
  candidate `51beab8`; `scripts/release.sh 0.3.0` bumped every version,
  moved the changelog's Unreleased section into `[0.3.0]`, verified through
  Sluice and tagged; `main` and `v0.3.0` pushed (the first push attempt was
  refused by this repository's own pre-push hook because the shell lacked
  `cargo` in `PATH` — the gate failed as it should; the retry hit the
  cache). The release workflow, the matrix on the tag and CI on `main`
  passed; assets: x86_64 5.2 MB, aarch64 4.7 MB, `SHA256SUMS`, notes taken
  from the changelog.
- `sluice update --yes` on the user's install: 0.2.0 → 0.3.0 (static
  release binary), idle daemon stopped, `doctor` clean, the real database
  already migrated with its backup.
- `s6_upgrade.py` with `ALPHA_TAGS=v0.2.0,v0.3.0` and the published 0.3.0
  as the binary under test: 46/46. Markers and failure records written by
  0.2.0 survive, the history is intact, the 0.2.0 daemon is stopped by
  `update` and 0.3.0 starts, and downgrading to 0.2.0 still opens the
  migrated database. The first run of the rerun exposed two harness
  assumptions, fixed in the scenario: 0.3.0's `status --json` reports
  `daemon: null` instead of starting a daemon, and `sluice daemon` returns
  once it has forked, before the new daemon answers (a product nit noted
  for beta).
- Golden files recorded from the published 0.3.0 binary (58 files) are read
  by the compatibility tests of every crate next to 0.1.0 and 0.2.0.
