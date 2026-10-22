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

![Six requests, one run](docs/assets/demo.gif)

*Six requests for the same tree: one run, five coalesced, and a repeat served from the cache.*

Sluice runs each gate (tests, linters, type checks) once per git tree, under a
global concurrency budget, and caches the result. Git hooks call `sluice run`;
coding agents call the MCP server started by `sluice mcp`.

## Why

You run four coding agents on the same project, each in its own git worktree,
and every one of them triggers the same `pre-commit` and `pre-push` checks.
Four copies of the full suite run at once and the machine stops answering.
Most of those runs verify a tree that already passed: the agent ran it, then
`pre-commit`, then `pre-push`, then again after a no-op rebase. Hooks live in
the shared `.git/hooks`, so nothing coordinates the worktrees. And an agent's
shell tool has a timeout, so a hook that waits in a queue inside `git commit`
makes the agent move on without the result. Sluice bounds the concurrency,
removes the redundancy and never leaves an agent hanging.

## Features

- **Per-tree cache.** The key is the exact content of the working tree plus the
  gate definition, shared by every worktree. Hits return in milliseconds and
  never need the daemon.
- **Daemon with budget and coalescing.** A per-user daemon runs gates under a
  weighted budget; identical simultaneous requests share one run.
- **PSI admission.** New work is held while the machine is under real pressure
  (Linux PSI).
- **Git hooks** for Husky, lefthook, pre-commit/prek and plain `.githooks/`,
  failing fast inside agents instead of hanging.
- **MCP tools and an agent plugin.** `sluice mcp` gives agents a blocking
  `verify` tool; the Claude Code plugin adds a skill and a guard hook.
- **`sluice exec` and queue-only gates** put any command (`docker build`,
  `make`) under the same budget, without cache.
- **Path-scoped gates** key on part of the tree, so unrelated changes keep the
  cached pass.
- **Local merge queue.** Agents enqueue committed branches; a human approves
  one atomic landing.
- **`sluice doctor` and `sluice update`** check the installation (with a
  redacted bug-report bundle) and upgrade in place.

## Install

Linux only, with `git` 2.31 or newer in `PATH`. Prebuilt static binaries for
x86_64 and aarch64 are on the
[releases page](https://github.com/josegarridodigio/sluice/releases).

```sh
curl -fsSL https://raw.githubusercontent.com/josegarridodigio/sluice/main/scripts/install.sh | sh
```

This installs the latest release into `~/.local/bin` (`SLUICE_VERSION` pins a
tag, `SLUICE_INSTALL_DIR` relocates). Later, `sluice update` upgrades in place,
checksum-verified. From source, with a stable Rust toolchain:

```sh
cargo install --path crates/sluice-gate
```

The crate is `sluice-gate` because `sluice` is taken on crates.io; the binary
is `sluice`. For Claude Code, install the agent plugin (with `sluice` in your
`PATH`):

```
/plugin marketplace add josegarridodigio/sluice
/plugin install sluice@sluice
```

## Quick start

```sh
sluice init                  # writes .sluice.toml, prints the hook snippet
sluice hook print husky      # or lefthook | pre-commit | plain
git commit -m "my change"    # the hook runs `sluice run` on a cache miss
sluice status                # daemon, budget, queue and totals
```

A gate is a named shell command in `.sluice.toml`:

```toml
version = 1

[gates.test]
cmd = "npm test -- --maxWorkers=2"
weight = 4
hooks = ["pre-commit", "pre-push"]
```

Run `sluice run test` twice on the same tree and the second call returns from
the cache. Recipes for Node, Python, Rust, Go, Docker and monorepos are in the
[gates cookbook](docs/gates-cookbook.md).

## Documentation

| Document | What it covers |
|---|---|
| [How it works](docs/how-it-works.md) | Architecture, cache key, scheduling, what to know before trusting a hit, prior art |
| [CLI reference](docs/cli.md) | Every command with its full `--help`, exit codes, environment variables |
| [Configuration](docs/configuration.md) | Every field of `.sluice.toml` and the user config, with defaults |
| [Gates cookbook](docs/gates-cookbook.md) | Ready-to-adapt gate files per stack |
| [Agents](docs/agents.md) | Using Sluice from coding agents; the [Claude Code plugin](plugin/README.md) |
| [Merge queue](docs/merge-queue.md) | Handing agents' branches to a local queue and landing them |
| [Debugging](docs/debugging.md) | Logs, debug levels, `sluice doctor --bundle` |
| [Compatibility](docs/compatibility.md) | What stays stable across releases: cache, config format, protocol, upgrades and downgrades |
| [CHANGELOG](CHANGELOG.md) | What changed in each release |
| [Design knowledge base](kb/sluice/plan/master-plan.md) | The plan, the architecture and one ADR per decision |

## Status and limitations

Sluice is pre-alpha. The gaps you will hit first:

- **Linux only**, git 2.31 or newer; other platforms are not planned.
- **Verdicts, not artifacts.** A hit says a gate passed on this tree; it does
  not restore build outputs.
- **Only declared gates and `sluice exec` are controlled.** Raw commands bypass
  the queue; the plugin's guard hook and your forge's branch protection are
  the enforcement.
- **A hit is a local optimisation.** CI stays authoritative, and inputs git
  cannot see must be declared (`key_inputs`, `key_files`).
- **Per-worktree caches.** Tools that keep state inside the checkout, such as
  `node_modules/.cache`, need one copy per worktree.

The full list is in the [final report](kb/sluice/changes/final-report.md).

## Contributing

Issues and discussions are open; use the issue forms (a bug report asks for
`sluice doctor --bundle`). The quality bar is in
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
