<h1 align="center">Sluice</h1>

<p align="center">
  <strong>Verify each git tree once.</strong><br>
  A stack-agnostic verification queue and cache for parallel coding agents.
</p>

<p align="center">
  <a href="https://github.com/josegarridodigio/sluice/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/josegarridodigio/sluice/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="Rust 2024 edition" src="https://img.shields.io/badge/rust-2024%20edition-orange.svg">
  <img alt="Platform: Linux" src="https://img.shields.io/badge/platform-linux-lightgrey.svg">
  <img alt="Status: pre-alpha" src="https://img.shields.io/badge/status-pre--alpha-red.svg">
</p>

> **Status: pre-alpha.** The design is done and the workspace builds, but no
> command does anything useful yet. Watch the repo or follow the
> [roadmap](#roadmap) if you want to be told when it does.

---

## The problem

You run four Claude Code agents on the same project, each in its own git
worktree. Every one of them triggers the same `pre-commit` and `pre-push`
checks: the full test suite, the linter, the type checker, maybe CodeQL.
Four agents, four copies of everything, at once. The laptop fans spin up,
every agent slows down, and the machine stops answering.

Looking closer, three different things go wrong:

| Cause | What happens | Who should fix it |
|---|---|---|
| **Uncontrolled concurrency** | N agents, N simultaneous suite runs. Hooks live in the shared `.git/hooks`, so nothing coordinates between worktrees. | Sluice |
| **Redundancy** | The same tree is verified by the agent, then by `pre-commit`, then by `pre-push`, then after a no-op rebase, then by the coordinator's merge push. Real-world reports found more than half of suite runs were on trees already proven green, and pushes taking 13–16 minutes mostly because `pre-push` redid `pre-commit`'s work. | Sluice |
| **Excessive scope** | Full suites where a changed-files subset would do. | Your gate commands. Sluice just makes narrow gates easy to declare. |

And one hard constraint: the agent's shell tool has a timeout. A git hook
that sits in a queue runs *inside* the agent's `git commit` call. If it
waits too long, the agent moves on without the result. Any fix has to
respect that.

## What Sluice does

- **Caches results per tree.** The cache key is the exact content of the
  working tree (tracked, staged, unstaged and untracked non-ignored files)
  plus the gate definition. A tree that passed `test` once passes `test`
  everywhere: in another worktree, after a no-op rebase, at push time. Hits
  return in milliseconds and never need the daemon.
- **Bounds concurrency globally.** One per-user daemon runs gates under a
  budget. Each gate declares a weight; heavy suites take more of the budget
  than a linter. On Linux, admission also watches real pressure (PSI), not
  just a slot count.
- **Coalesces identical requests.** Two agents asking for the same tree and
  gate share one run. The second one just waits for the first one's result.
  No other tool in this space does this.
- **Two front-ends, one brain.** `sluice run <gate>` for git hooks, humans
  and scripts. `sluice mcp` for agents, as an MCP server over stdio. Both
  talk to the same daemon and share cache, queue and logs.
- **Never hangs inside an agent.** On a cache miss in a hook invoked by an
  agent, Sluice fails fast with an actionable message ("run the `verify`
  tool first, then retry the commit") instead of waiting in the queue.
  Humans get the queue, because humans are not subject to tool timeouts.
- **Stays out of your stack.** Sluice knows nothing about npm, cargo or
  pytest. It runs named shell commands ("gates") from a small TOML file and
  works with any hook manager: Husky, lefthook, pre-commit/prek, plain
  `.githooks/`.
- **Keeps agent context small.** Full output goes to a log file. Callers
  get one line on success and the tail of the log on failure.

## How it works

```
 git hook ──▶ sluice run ──┐                        ┌──▶ gate (process group)
 terminal ──▶ sluice run ──┼──(Unix socket)──▶ daemon ─┼──▶ gate (process group)
 agent    ──▶ sluice mcp ──┘                        └──▶ …  bounded by budget
                │
                └── cache hit: marker found under .git/, exit 0 in milliseconds,
                    no daemon involved
```

1. The client computes the tree hash from a **private copy of the index**
   (`git add -A` + `git write-tree` against a scratch index and a scratch
   object directory). Your real index, `index.lock` and object store are
   never touched.
2. It derives the cache key from the tree, the gate name, the normalised
   command, the gate's environment, and any extra inputs you declare
   (`key_inputs` such as `node --version`, `key_files` such as `.env.test`).
3. If a marker for that key exists under `$(git rev-parse --git-common-dir)/sluice/`,
   that is a hit. Markers are shared by every worktree of the repository.
4. Otherwise the request goes to the daemon, which either attaches it to an
   identical in-flight run or queues it under the budget.
5. Only a run that exits 0 **and leaves the tree unchanged** writes a
   marker. Auto-fixers that modify files always get re-verified.

A gate definition looks like this:

```toml
# .sluice.toml
version = 1

[defaults]
agent_hook_policy = "fail-fast"   # fail-fast | wait | run-unqueued

[gates.lint]
cmd = "npm run lint"
weight = 1
hooks = ["pre-commit"]

[gates.test]
cmd = "npm test -- --maxWorkers=2"
weight = 4
timeout = "20m"
hooks = ["pre-commit", "pre-push"]
key_inputs = ["node --version"]
key_files  = [".env.test"]
env        = { NODE_ENV = "test" }
```

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

MCP tools for agents: `verify` (blocks until the gates finish, with progress
notifications), `status`, `gate_list`, `logs`.

## Roadmap

| Phase | Scope | Status |
|---|---|---|
| 0 | Research, design, ADRs, workspace | done |
| 1 | Cache only, no daemon: `sluice run`, markers, TTL, hook snippets, a file-lock semaphore | next |
| 2 | Daemon: budget scheduler, coalescing, process groups, timeouts, SQLite metrics, `sluice status` | |
| 3 | MCP server, agent detection, `agent_hook_policy`, `docs/agents.md` | |
| 4 | PSI-based admission, stable `--json` schema, metrics export | |
| 5 | Release builds, install script, real-world numbers | |

Design history lives in [`kb/`](kb/sluice/plan/master-plan.md): the plan,
the architecture, one ADR per decision, and research notes on the projects
Sluice learned from.

## Install

Not published yet. From source, with a stable Rust toolchain:

```sh
cargo install --path crates/sluice-gate
```

The crate is `sluice-gate` because `sluice` is taken on crates.io by an
unrelated library. The binary is `sluice`. Homebrew also ships an unrelated
`sluice` (a Maven index parser); if you have both, check which one is first
in `PATH`.

## Requirements

- Linux. The only supported platform in v1.
- `git` in `PATH`. Nothing else at runtime.

## Things to know before trusting a cache hit

- A hit is a **local optimisation**. CI stays authoritative. Caching is off
  when `CI` is set unless you export `SLUICE_CACHE=1`.
- Inputs git cannot see (tool versions, environment variables, ignored
  files, running services) must be declared with `key_inputs` and
  `key_files`, or a stale pass is possible.
- Agents bypass the queue whenever they can run the test command directly
  through their shell tool. Permission rules such as `Bash(npm test:*)`, or
  running with permissions bypassed, make that easy. Tell your agents to
  verify through Sluice; `docs/agents.md` (Phase 3) ships a ready-to-paste
  `CLAUDE.md` section.

## Prior art

Sluice reimplements ideas from three projects and tries to combine what
each one got right:

- [gate-cache](https://github.com/natelandau/gate-cache): skip a command
  that already passed on the same tree. Sluice takes its tree hashing from a
  private index copy and adds a queue, coalescing and declared key inputs.
- [agent-task-queue](https://github.com/block/agent-task-queue): an MCP
  task queue so agents do not hit shell timeouts. Sluice takes the
  MCP-blocks-instead-of-the-shell insight and the output-to-file design,
  and adds a cache, weights and load awareness.
- [mergetrain](https://github.com/yongjip/mergetrain): a local merge queue
  for agent worktrees. Sluice borrows its lease-fenced single-runner
  discipline for the daemon.

See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

## Contributing

Issues and discussions are open. The quality bar is in
[CONTRIBUTING.md](CONTRIBUTING.md): `cargo fmt`, `cargo clippy -D warnings`,
`cargo test`, and a knowledge-base note for every change.

## License

MIT. See [LICENSE](LICENSE).
