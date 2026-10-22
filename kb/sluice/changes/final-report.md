---
title: "Final report: Sluice through Phase 6"
tags: [sluice, report, final]
---

# Final report (2026-10-22)

Sluice is a verification queue and result cache for parallel coding agents
on Linux: git hooks and agents ask for a gate (tests, linters), a per-user
daemon runs it once per tree under a weighted budget, coalesces identical
requests, answers repeats from markers shared by every worktree, and a
local merge queue lands verified branches only after a human approves.

State at commit `c50cee5`: every phase of [[sluice/plan/master-plan.md]]
is done with an acceptance document; release `v0.1.0` is published; the
full suite (401 tests) passes through Sluice itself; the final
reconciliation lists no open discrepancies
([[sluice/changes/final-reconciliation.md]]).

## Per phase

| Phase | Built | Evidence |
|---|---|---|
| 0 | Research of four reference projects, ADR-0001..0005, workspace | [[sluice/changes/phase-0-scaffold.md]] |
| 1 | `sluice run` with tree hashing from a private index and object dir, cache keys, markers and miss records with TTL, agent detection, flock semaphore, `cache`, `init`, `hook print` | [[sluice/changes/phase-1-acceptance.md]], [[sluice/changes/hit-path-latency.md]] |
| 2 | Daemon: socket protocol v1, fenced SQLite lease, budget scheduler with backfill and head-of-line reservation, coalescing, executor with process groups, timeouts, orphan reconciliation, `status`, `logs` | [[sluice/changes/phase-2-acceptance.md]] |
| 3 | MCP server (`verify` blocking with progress, `status`, `gate_list`, `logs`), agent hook policy (fail-fast for agents) | [[sluice/changes/phase-3-acceptance.md]] (real sessions) |
| 4 | PSI admission with hysteresis and starvation guard, status JSON schema 1 pinned by a fixture | [[sluice/changes/phase-4-acceptance.md]] |
| 5 | Release workflow (static musl x86_64/aarch64, checksums), installer, `v0.1.0`, multi-worktree benchmark, README demo | [[sluice/changes/packaging.md]], [[sluice/changes/phase-5-validation.md]] |
| 6 | Merge queue in the daemon (`sluice-gate-queue`): trains of up to 4, no-ff merges in a disposable worktree, gates through the scheduler, approval bound to a train hash, write-ahead marker and pin ref before an atomic push, deterministic reconcile, local-branch targets; CLI `sluice queue …`, MCP `queue_*` | [[sluice/changes/phase-6-acceptance.md]], [[sluice/changes/phase-6-reconciliation.md]] |
| 6a | `[debug] level`, `daemon restart --debug`, persistent MCP and client logs, `daemon.log` rotation, `sluice doctor [--bundle]`, `sluice update`, release tag guard and `scripts/release.sh` | [[sluice/changes/debugging.md]], [[sluice/changes/self-update.md]] |
| 6b | Agent plugin in this repository (marketplace, MCP registration, `sluice` skill, guard hook) | [[sluice/changes/agent-plugin.md]] |

## Measured numbers

| Measurement | Result |
|---|---|
| Cache-hit latency, 5 000-file repository (median of 40) | 29.1 ms |
| Benchmark, 4 worktrees x 2 commits: gate runs without / with Sluice | 16 / 10 (6 coalesced) |
| Same benchmark, total wall time without / with Sluice | 4.6 s / 3.6 s |
| Real sessions: a 90 s gate under a 60 s MCP idle window | completes with progress every 15 s; aborts without |
| Dogfooding since 2026-10-22 19:18 UTC (this repository's own gates) | 34 runs, 30 cache hits, 308 s saved of 365 s run |
| Commit through the Sluice pre-commit hook on a verified tree | 88 ms |
| Test suite | 401 passed, 0 failed (`sluice run test`) |

The dogfooding figures come from `sluice status --json` on the user's
daemon while Phase 6 and the post-phase blocks were developed with several
agents in parallel worktrees; coalescing stayed at 0 there because each
agent verified a different tree. On one occasion the agent-detected
pre-commit hook refused an unverified commit by the lead agent and the
retry after `sluice run` passed from the cache, which is the designed
behaviour.

## Open limitations

- Linux only; git ≥ 2.31; submodules are not part of the tree hash.
- A cache hit is a local optimisation: CI stays authoritative, and inputs
  git cannot see must be declared (`key_inputs`, `key_files`).
- Agents can still run raw commands or push directly when their
  permissions allow it; the plugin's guard hook and forge branch
  protection are the enforcement, not the daemon.
- The guard hook splits compound commands without parsing quotes, so it
  can deny more than necessary, and it does not see commands wrapped in
  `sh -c`.
- Merge queue: bisection of a failing train is not implemented (failing
  multi-job trains split into single-job trains); the daemon reads the
  user config only at start; reconcile rows 5 and 6 and a lease lost in
  the middle of a running push are covered by unit tests only.
- Runtime rotation of `daemon.log` (the daily path) has no integration
  test; rotation at start does.
- The crates are not published to crates.io (needs the user's explicit
  decision).

## Install and configure (new user)

```sh
# 1. Install the binary (Linux x86_64 or aarch64) into ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/josegarridodigio/sluice/main/scripts/install.sh | sh
sluice update --check                    # later: sluice update

# 2. In a repository
cd your-repo
sluice init                              # writes .sluice.toml, prints hook snippets
sluice hook print husky                  # or lefthook | pre-commit | plain
sluice doctor                            # checks git, PSI, daemon, configs, hooks

# 3. Coding agents (Claude Code): install the plugin from this repository
#    /plugin marketplace add josegarridodigio/sluice
#    /plugin install sluice@sluice
#    Other agents: the instructions block in docs/agents.md.

# 4. Merge queue (optional)
sluice queue enqueue                     # from a worktree with a committed branch
sluice queue status
sluice queue approve <train-id>          # humans only
```

Release process for maintainers: `scripts/release.sh X.Y.Z`, review, then
`git push origin main vX.Y.Z`; the workflow refuses a tag that does not
match the workspace version.
