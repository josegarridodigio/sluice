---
title: "R1: adversarial review of the alpha work"
tags: [sluice, alpha, review]
---

# R1: adversarial review of the alpha work (2026-10-23)

Read-only review (Opus) of everything added since v0.2.0: command control
(exec, guard, shims), the reaper, the scheduler and timeline, path-scoped
keys, the socket fallback, queue transports and the MCP early verdict.
Plan: [[sluice/plan/alpha-plan.md]] task R1. Fixes: task R2,
[[sluice/changes/alpha-review-fixes.md]].

Result: 13 findings, none critical or high. Rows 1–5 and 9–10 were also
reproduced with a scratch build; the reaper and scheduler rows come from
reading the code.

| # | Severity | Area | Finding |
|---|---|---|---|
| 1 | medium | guard | `cd ..` resolved on the physical path while the shell uses the logical one, so the wrong `.sluice.toml` can be matched |
| 2 | medium | guard, queue | "approval is reserved to humans" relies on a text prefix; `'sluice' queue approve … --yes`, `env sluice …` and `sluice exec -- sluice queue approve …` pass; the daemon refuses agent approvals only over MCP (same gap in 0.2.0) |
| 3 | medium | exec, shims, guard | a non-UTF-8 environment variable makes `sluice exec` and every shim panic, also on the direct path |
| 4 | medium | scheduler | a planning pass right after a nominal start can over-admit on stale utilisation and PSI samples |
| 5 | medium | scheduler | fast failures teach a low learned weight, so the next full run is admitted at the floor |
| 6 | low-medium | scheduler | under a cgroup CPU quota below the CPU mask, idle CPUs are over-counted and over-admission is always granted |
| 7 | low | daemon | a job admitted between the "still queued" check and its cancellation keeps running unsupervised |
| 8 | low | executor | the final SIGKILL can hit a reused process group after the grace period |
| 9 | low | exec | the in-process fallback signals only the leader; grandchildren survive a timeout |
| 10 | low | guard | a leading `PATH=…` assignment also applies to the inserted `sluice` |
| 11 | low | migrations | an existing backup is trusted even if a crash truncated it |
| 12 | low | doctor | `doctor` connects to the fallback socket without the ownership check normal clients do |
| 13 | low | reaper | the microsecond window between fork and reaper registration (accepted, documented) |

Checked and found correct: guard rewrites on about 30 crafted inputs
(quoting, redirections, substitutions, globs), shim resolution and
recursion, exec exit codes and signal handling, reaper matching by start
ticks, budget accounting and starvation guards, the sampler, migration in
one transaction behind the lock and 0.2.0 binaries opening the migrated
database, path-scoped keys (unscoped golden vector unchanged, modes,
renames, case changes, gitlinks, a differential test against git
pathspecs), the socket fallback against pre-created, symlinked or
writable directories, credential stripping and deadlines on every queue
network call, and the MCP early verdict never killing a job another
client waits on.
