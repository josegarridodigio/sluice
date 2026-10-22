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
