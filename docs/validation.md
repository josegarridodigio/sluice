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
