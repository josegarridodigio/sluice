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
