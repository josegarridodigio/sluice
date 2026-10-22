# Sluice

A stack-agnostic verification queue and cache for parallel coding agents.

> Status: pre-alpha. Phase 0 (research and scaffolding) is complete; no
> command is implemented yet. See the [plan](kb/sluice/plan/master-plan.md).

## The problem

Several coding agents working in parallel on the same project, each in its
own git worktree, trigger the same expensive `pre-commit` and `pre-push`
checks independently. The machine saturates, every agent slows down, and
most of those runs verify trees that were already proven green.

## What Sluice does

- **Verify each tree once.** Gate results are cached by the exact content
  of the working tree plus the gate definition. Hits return in milliseconds
  and never need the daemon.
- **Bounded concurrency.** A per-user daemon runs gates under a global
  budget with per-gate weights and Linux PSI load awareness.
- **Coalescing.** Two requests for the same tree and gate share one run.
- **Two front-ends, one brain.** `sluice run <gate>` for git hooks, humans
  and scripts; `sluice mcp` for agents (MCP over stdio). Both talk to the
  same daemon.
- **Hooks never hang inside an agent.** On a cache miss in a hook invoked by
  an agent, Sluice fails fast with an actionable message instead of waiting.
- **Stack-agnostic.** Sluice only runs named shell commands ("gates") from
  `.sluice.toml`. Any language, any hook manager (Husky, lefthook,
  pre-commit/prek, plain `.githooks/`).

## Command surface

```
sluice run <gate> [-- extra args]   # thin client: hooks, terminal, scripts
sluice mcp                          # MCP stdio server spawned by the agent
sluice daemon                       # one per user, Unix socket
sluice status [--json]              # queue, in-flight, cache stats, time saved
sluice cache status|clear [--gate]  # inspect / clear markers for the current repo
sluice init                         # write a starter .sluice.toml and print hook snippets
sluice hook print <manager>         # husky | lefthook | pre-commit | plain
sluice logs [<job-id>]              # show job logs
```

## Install

Not published yet. From source:

```sh
cargo install --path crates/sluice-gate
```

The crate is named `sluice-gate` because `sluice` is taken on crates.io by
an unrelated library. The binary is `sluice`. Note that Homebrew also ships
an unrelated `sluice` binary (a Maven index parser); if you have both
installed, make sure the right one comes first in `PATH`.

## Requirements

- Linux (the only supported platform in v1).
- `git` in `PATH`. Nothing else at runtime.

## Caveats you should read

- A cache hit is a **local optimisation**. CI stays authoritative. Caching
  is disabled when `CI` is set unless `SLUICE_CACHE=1`.
- Inputs git does not see (tool versions, environment variables, ignored
  files, running services) must be declared per gate with `key_inputs` and
  `key_files`, or a stale pass is possible.
- A gate that exits 0 but modifies tracked files (an auto-fixer) is never
  cached; the new tree must be verified again.
- Agents bypass the queue whenever they can run the test command directly
  through their shell tool. Tell them not to; see `docs/agents.md` (Phase 3).

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

Design history, ADRs and research notes live in `kb/` (a Pando knowledge
base, versioned with the code). Reference repositories studied during
research are cloned into `refs/` (gitignored).

## License

MIT. See [LICENSE](LICENSE). Ideas were taken from
[gate-cache](https://github.com/natelandau/gate-cache) (MIT) and
[agent-task-queue](https://github.com/block/agent-task-queue) (Apache-2.0);
see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
