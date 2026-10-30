# Sluice on its own repository

Sluice verifies itself. The repository declares three gates in
`.sluice.toml` and wires them through plain git hooks in `.githooks/`.

| Gate | Command | Hook | Weight |
|---|---|---|---|
| `fmt` | `cargo fmt --all --check` | pre-commit | 1 |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` | pre-commit | 4 |
| `test` | `cargo test --workspace` | pre-push | 6 |

All three are path-scoped (`crates/**`, the manifests and lockfiles, the
toolchain file and `plugin/**`), so a documentation-only change keeps their
cached passes.

## Set it up

```sh
cargo install --path crates/sluice-gate     # puts `sluice` in ~/.cargo/bin
git config core.hooksPath .githooks         # per clone; worktrees share it
```

Without `sluice` in `PATH` the hooks run the cargo commands directly, so
the repository stays protected. For coding agents working on this repo,
install the Claude Code plugin (see [agents.md](agents.md), section 2) or
register the MCP server by hand and paste the rules of section 3:

```sh
claude mcp add sluice -- sluice mcp
```

The agent then calls `verify` before committing instead of running cargo.

## Run the benchmark

```sh
scripts/validation-bench.sh [worktrees] [commits]    # defaults: 4 2
```

It simulates N agents in N worktrees, each making `commits` commits that
trigger the pre-commit gates (`fmt`, `clippy`), all in parallel, twice:

- **A, without Sluice:** the hook runs the cargo commands directly.
- **B, with Sluice:** the hook runs `sluice run --hook pre-commit <gate>`
  against a private daemon with `budget = 8`.

Every hook command is timed with `scripts/measure-hook.sh` and summarised
with `scripts/validation-report.sh`. The first commit is identical in all
worktrees (same notes line, same comment appended to
`crates/sluice-gate-core/src/lib.rs`), so their trees coincide and Sluice
should verify them once; the second commits differ.

The script is isolated: private `HOME` and `XDG_*` directories in a
temporary directory, never your real daemon; it stops its daemon, removes
the worktrees, branches and temp files on exit or Ctrl-C. Build artifacts
go to `target/validation-bench` (or `SLUICE_BENCH_TARGET`) and are
warmed before timing. Needs a few minutes the first time (cold build).

## Result

Machine: 12th Gen Intel i7-1280P, 20 logical CPUs, Linux. Load average at
the start of the run (1, 5, 15 min): 2.01 0.82 0.68. Command:
`scripts/validation-bench.sh 4 2`.

| Scenario | Wall (s) | Hook runs | Suite runs | Hits | Coalesced | Mean hook (s) | Wall per commit (s) |
|---|---|---|---|---|---|---|---|
| A without Sluice | 4.6 | 16 | 16 | - | - | 1.1 | 0.6 |
| B with Sluice | 3.6 | 16 | 10 | 0 | 6 | 0.9 | 0.5 |

"Hook runs" are gate invocations in the hook log (fmt + clippy per commit);
"suite runs", hits and coalesced come from the daemon totals
(`sluice status --json`). Sluice executed 10 of the 16 gate invocations:
one `fmt` and one `clippy` for the four identical first commits (the other
three requests of each were coalesced) and one of each per second commit.

Reading it honestly: the saving is real but small here, 22 % less wall time
and 6 fewer runs out of 16. The gates are short on a warm target
directory (`clippy` about 2 s) and the machine has 20 CPUs, so there is
little contention to remove; with a suite measured in minutes, or with more
agents on fewer cores, the coalescing share is what grows. The commits are
synthetic and agents never think between them. Raw outputs and caveats:
`kb/sluice/changes/phase-5-validation.md`.

## Agents in parallel (CI)

The chart in the README. `scripts/bench-agents.sh` simulates N agents (1, 2,
4 and 8), each in its own git worktree of this repository with its own
`target/` (a copy of one warmed directory, as an agent's worktree has). Every
agent makes two commits, all agents at the same time, and each commit runs the
repository's pre-commit gates (`fmt`, `clippy`):

- the first commit has the same content in every worktree (all agents start
  from the same tree);
- the second commit is the agent's own change;
- both change a source file, so `clippy` really recompiles.

Each agent count runs twice: without Sluice (the hook runs the gate commands
directly) and with Sluice (the hook runs `sluice run --hook pre-commit
<gate>`, private daemon, default budget). The order of the two alternates
between agent counts so a warm page cache never favours the same one, and each
starts from the same warmed `target/` and a fresh daemon state. The measure is
the wall time until the last agent finished its last commit.

It runs on demand in the `Bench` workflow (`.github/workflows/bench.yml`) on a
GitHub-hosted runner, three rounds; `scripts/bench-chart.py` draws the medians.
On a workstation the scenario without Sluice deliberately overloads the
machine, so run it on a CI runner or a VM.

### Result

Runner: GitHub-hosted `ubuntu-latest`, 4 CPUs, 16 GiB. Three rounds
(2026-10-30, release 0.9.0); raw data in
[`docs/assets/bench-agents.csv`](assets/bench-agents.csv).

| Agents | Without Sluice (s) | With Sluice (s) | Change | Gate runs with Sluice |
|---|---|---|---|---|
| 1 | 27 (27, 28, 27) | 28 (28, 29, 27) | +1 % | 4 of 4 |
| 2 | 49 (49, 53, 48) | 40 (40, 42, 40) | −18 % | 6 of 8 |
| 4 | 103 (103, 106, 100) | 72 (72, 73, 71) | −30 % | 10 of 16 |
| 8 | 213 (213, 207, 214) | 119 (119, 116, 120) | −44 % | 18 of 32 |

Reading it honestly: one agent has nothing to coordinate, so Sluice only adds
its own overhead (about a second). From two agents on, two effects add up: the
shared first commit is verified once and the other agents are coalesced onto
that run (the runs column), and the commits that do differ wait in the queue
instead of compiling at the same time on four CPUs. The gates here are short
(a `clippy` of about 10 s); a test suite measured in minutes widens the gap,
and agents that think between commits narrow it.
