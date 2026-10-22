---
title: "Phase 5 validation evidence"
tags: [sluice, acceptance, phase-5]
---

# Phase 5 validation evidence (2026-10-22)

Sluice run on its own repository: four simulated agents in four git
worktrees, two commits each, pre-commit gates `fmt` and `clippy` from
`.sluice.toml`, without and with Sluice. Plan: [[sluice/plan/master-plan.md]].

Command (release binary built by the script, private HOME and XDG dirs,
`budget = 8`, shared warmed target dir):

```
scripts/validation-bench.sh 4 2
```

Machine: 12th Gen Intel i7-1280P, 20 logical CPUs, Linux 7.0. Load average at
the start of the run (1, 5, 15 min): 2.01 0.82 0.68 (other work was running
on the machine).

## Comparison

| Scenario | Wall (s) | Hook runs | Suite runs | Hits | Coalesced | Mean hook (s) | Wall per commit (s) |
|---|---|---|---|---|---|---|---|
| A without Sluice | 4.6 | 16 | 16 | - | - | 1.1 | 0.6 |
| B with Sluice | 3.6 | 16 | 10 | 0 | 6 | 0.9 | 0.5 |

Hook runs: gate invocations in the CSV log (fmt and clippy per commit, 4 x 2
x 2 = 16). Suite runs, hits, coalesced: totals of the private daemon. The 10
suite runs are one fmt and one clippy for the four identical first commits
(3 + 3 requests coalesced) plus one of each for the four distinct second
commits. Hits are 0 because the four first commits arrived concurrently, so
they coalesced instead of finding a marker.

## validation-report.sh, scenario A (no Sluice)

```
label            runs  runs/day    mean(s)  median(s)   fail
fmt                 8       8.0        0.2        0.2      0
clippy              8       8.0        2.1        2.1      0
```

## validation-report.sh, scenario B (with Sluice)

```
label            runs  runs/day    mean(s)  median(s)   fail
fmt                 8       8.0        0.2        0.2      0
clippy              8       8.0        1.5        1.5      0
```

## sluice status --json, totals of scenario B

```json
"totals": {
  "since": "2026-10-22T16:12:40Z",
  "runs": 10,
  "passes": 10,
  "failures": 0,
  "timeouts": 0,
  "hits": 0,
  "coalesced": 6,
  "wait_ms_total": 4026,
  "run_ms_total": 6050,
  "time_saved_ms": 2997,
  "psi_blocked_ms_total": 0
}
```

Budget at the end: total 8, used 0, no job running or queued, PSI not
blocking.

## What this does and does not show

It shows that, on this repository, the mechanism works end to end with real
cargo gates and real git hooks: identical trees requested by four worktrees
at once cost one fmt run and one clippy run, the others waited and got the
result, the second commits ran separately and the budget was never
exceeded. It does not show a large speed-up and should not be quoted as
one: the commits are synthetic (a notes file plus a comment line), the
target directory is warm (each worktree was pre-compiled, so clippy takes
about 2 s instead of minutes), the gates are short, the machine has 20 CPUs
and little contention to remove, and the load average was not zero. Scenario
A is also softened because concurrent cargo runs on one shared target
directory already serialise on cargo's build-directory lock, so the
"uncoordinated" baseline is not a free-for-all. Real agents also think
between commits, which spreads requests in time: fewer would coalesce and
more would be answered by hits on markers. There is a single run, with no
repetitions or variance estimate. The gain that matters in practice (a
pre-push `test` suite of minutes, repeated on trees already proven) is not
measured here. The `test` gate was deliberately left out of the benchmark.
The first-commit text includes a per-run id so markers left in `.git` by
earlier runs cannot turn the first commits into hits.

See also [[sluice/plan/master-plan.md]].
