---
title: "ADR-0029: The cache key can name the environment and the installed state a gate depends on"
tags: [sluice, adr, cache-key, init, doctor]
---

# ADR-0029: The cache key can name the environment and the installed state a gate depends on

Status: accepted (2026-11-06). Plan: [[sluice/plan/memory-containment-goal.md]]. Contract:
[[sluice/design/cache-key.md]].

## Context

A user asked whether a shared in-flight run is safe when the git tree does not describe
the whole test environment: lockfiles, toolchain, test configuration. The key already
covered the working tree (lockfiles and tracked config included), the command, the gate's
`env` table, `key_inputs` (toolchain versions, written by `sluice init`) and `key_files`
(untracked files). Three gaps remained, all silent: a change to them reused an old pass,
and a request coalesced into a run made with other values.

1. **Variables of the caller's environment** (`RUSTFLAGS`, `NODE_OPTIONS`, `PYTHONPATH`,
   `GOFLAGS`, ...) reach the gate but not the key; only the gate's own `env` table was
   keyed.
2. **Installed dependencies.** The lockfile is in the tree; `node_modules` is not. A
   worktree whose install drifted from the lockfile reused another worktree's pass.
3. **Path-scoped gates** that leave out a toolchain or test configuration file:
   `doctor` warned about lockfiles and manifests only.

## Decision

1. **`key_env`**, a list of variable names per gate. Their values are read from the gate's
   environment (the sanitised caller environment plus the gate's `env` table: the one the
   command and its `key_inputs` see, so a merge train, which builds it from the human's
   login environment, derives the same key a client would). Each name is hashed once, in
   the order written, after the `key_files`: `env:NAME \0 =value \0`, or
   `env:NAME \0 unset \0` when the variable is not set (unset and empty differ). Without
   `key_env` the key bytes are exactly those of every earlier release (golden tests:
   `key::tests::key_env_extends_the_layout_and_tells_unset_from_empty` and the release
   fixtures of `tests/compat.rs`). Coalescing uses the cache key, so a request with other
   values never joins a run made with different ones.
2. **`sluice init` writes them.** From the toolchain each gate's `key_inputs` name:
   Node: `NODE_OPTIONS`, `NODE_ENV`; Rust: `RUSTFLAGS`, `RUSTDOCFLAGS`,
   `CARGO_BUILD_TARGET`; Python: `PYTHONPATH`, `PYTHONOPTIMIZE`; Go: `GOFLAGS`,
   `CGO_ENABLED`, `GOOS`, `GOARCH`. For Node it also writes the file the package manager
   rewrites on every install as a `key_file` (npm `node_modules/.package-lock.json`, pnpm
   `node_modules/.modules.yaml`, Yarn Berry `.yarn/install-state.gz`, Yarn Classic
   `node_modules/.yarn-integrity`; Bun has none). Gates carried from an existing config
   keep their table.
3. **`sluice doctor` warns** (never fails) when a cached gate names a toolchain in
   `key_inputs` but its `key_env` or installed-state `key_files` leave something out
   (`key-env` check), and its path-scope check now also knows the toolchain pin files
   (`.node-version`, `.python-version`), the package manager settings (`.npmrc`,
   `.yarnrc.yml`, `bun.lock`), lint and format settings (`rustfmt.toml`, `clippy.toml`)
   and test runner configuration (`angular.json`, `jest.config.*`, `vitest.config.*`,
   `playwright.config.*`, `karma.conf.*`, `pytest.ini`, `tox.ini`, `conftest.py`).

## Consequences

- A configuration that does not use `key_env` keeps every key and marker.
- `key_env` is a one-way key: older binaries reject the gate table
  (`docs/compatibility.md`); a repository adopting it needs every machine on 0.11.0.
- Adding `key_env` to a gate changes its key once (one re-run per gate).
- What is still not keyed is deliberate: system libraries, the OS, services the tests
  talk to, the time. A gate that depends on one of them declares it with `key_inputs`.

## Alternatives considered

- **Key the whole environment.** Every shell differs (`PWD`, `SHLVL`, agent session ids):
  no two runs would ever share a pass. Rejected.
- **Hash `node_modules`.** Correct but slow on every lookup (hundreds of thousands of
  files); the manager's own state file changes on every install. Rejected.
- **Make `doctor` fail.** It cannot know every variable a project depends on; a warning
  with the fix is the right strength.

## Open items

- Zero-config hook units (repositories without `.sluice.toml`) key no toolchain and no
  environment today; extending them changes their keys and is a separate decision.
