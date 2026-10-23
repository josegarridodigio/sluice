---
title: "Change: sluice init detects the stack and rewires the hooks (zero-config task T3)"
tags: [sluice, change, init, hooks, zero-config]
---

# Change: `sluice init` with detection and hook rewiring (2026-10-23)

Task T3 of [[sluice/plan/zero-config-plan.md]]. Decision:
[[sluice/design/adr/0016-init-detection-and-hooks.md]] (the one user-invoked
exception to "never write the working tree"). Builds on the heavy
classification of T1 (`HeavyMatcher::classify`, package scripts) and on
`weight = "auto"` of T4 ([[sluice/design/adr/0015-automatic-weights.md]]).

## What changed

- `sluice init [--force] [--dry-run]` is a new module, `crates/sluice-gate/src/init/`;
  `main.rs` only wires the CLI. Root is `git rev-parse --show-toplevel` (through
  `sluice_gate_core::git::discover`); outside a repository: error, exit 1.
  Existing `.sluice.toml` without `--force`: error, exit 1. Usage errors: exit 2 (clap).
- **Gates from hooks first** (`hooks.rs`): husky `.husky/<hook>`, plain hooks in
  `core.hooksPath` (not `.husky/_`) and `.githooks/`, `lefthook.yml` / `.lefthook.yml`
  (`run:` values under `pre-commit:` / `pre-push:`), `.pre-commit-config.yaml` (`entry:` of
  `repo: local` hooks, stage from `stages`). A line is a candidate only if it is one simple
  command (no pipes, redirections, substitutions, lists) with optional leading `NAME=value`
  words and, in scripts, an `|| exit` tail. The words go through `canonical_words` and
  `HeavyMatcher::classify` (so `npm run lint` is read through `package.json`); mutating
  commands (`--fix`, `--write`, `-w` with prettier, `pretty-quick`, `lint-staged`, or a package
  script that runs one) and non-heavy lines are left untouched. Lines that already call
  `sluice run` are recognised (`wired_gate`).
- **Naming and merging**: the gate is named from the package script, else from the matching heavy
  entry (`gate_name`: `ng test` -> `test`, `run typecheck` -> `typecheck`, `cargo clippy` ->
  `clippy`). The same command in two hooks is one gate with both hooks; a different command
  under a taken name gets `-2`, `-3`. Bare local binaries (`ng`, `jest`, `tsc`, ...) get the
  package manager's launcher (`npx`, `pnpm exec`, `yarn`, `bunx`). Leading variables become the
  gate's `env`; `CI = "1"` is added to Node test gates (not `ng`, which has `--watch=false`).
- **Stack detection** (`detect.rs`) for the names the hooks did not give: Angular
  (`angular.json` targets), Node (lockfile picks the manager; `lint`, `typecheck`/`type-check`,
  `test` scripts; the placeholder test script is skipped), Rust (fmt, clippy, test with
  `CARGO_BUILD_JOBS = "4"`), Python (ruff and mypy when configured, pytest, `.venv/bin/`
  prefix), Go (vet, `test -p 4`), nothing -> the starter template. Projects up to two levels
  below the root are listed in a comment. Weights: lint, vet, type checks 2, fmt 1, tests,
  builds and clippy `"auto"`; timeouts 10m lint-like (clippy 20m), 30m otherwise; builds get
  `cache = false`; `key_inputs` carry the tool versions.
- **Rewiring** (`hooks::rewire`): each rewritten line becomes `sluice run --hook <hook> <gate>`
  at the same position with the same indentation, terminator (LF, CRLF or none on the last
  line), `|| exit` tail and YAML trailing comment; file mode is untouched because the file is
  rewritten in place. A local pre-commit hook gets `pass_filenames: false` (inserted or
  replaced). A symlinked hook is edited at its target when the target is inside the repository;
  otherwise, and for pre-commit hooks with `args`, a language other than `system`/`script`, or an
  `entry:` on the last line without a newline, the line is left alone and the snippet of its
  manager is printed.
- **No hook manager**: `.githooks/pre-commit` and `.githooks/pre-push` (0755, `set -e`, one
  `sluice run --hook ...` per gate; lint-like gates in pre-commit, tests and builds in pre-push)
  and `git config --local core.hooksPath .githooks` through the git wrapper. If `.git/hooks`
  already has hooks and `core.hooksPath` is unset, or `core.hooksPath` points to a directory
  `init` does not own, the plain snippet is printed with the reason. When a hook manager exists,
  detected gates that no hook calls keep `hooks = []` and a note says so.
- **Idempotence**: with `--force`, gates that hook lines already call are carried over
  verbatim from the previous `.sluice.toml` (their table text), so a second run changes no hook
  and no config.
- **Output** (`render.rs`): the gate table (name, cmd, weight, hooks, source), the files, a
  unified diff (LCS, two lines of context) of the hook edits and of the created hook files, the
  notes and snippets. `--dry-run` prints the same and writes nothing.
- `starter_repo_config` documents `weight = "auto"` and uses it for `test`.
- `sluice init` in the CLI smoke tests now runs inside a repository.

## Files and symbols

`crates/sluice-gate/src/init/{mod,detect,hooks,render}.rs`, `crates/sluice-gate/src/main.rs`
(`Command::Init { force, dry_run }`, old `init` removed), `crates/sluice-gate-core/src/config.rs`
(`starter_repo_config`), `crates/sluice-gate/tests/init.rs`,
`crates/sluice-gate/tests/fixtures/init/*`, `crates/sluice-gate/tests/cli.rs` (`repo_dir`),
`docs/cli.md`, `docs/gates-cookbook.md`, `README.md`, `CHANGELOG.md`.

## Why

An Angular project with husky needed hand edits after `init` (command, watch flag, weights) and its
hooks kept running the suites outside Sluice. See section 1 of the plan.

## How it was verified

`crates/sluice-gate/tests/init.rs` builds each fixture (angular-husky, node-pnpm,
node-yarn-lefthook, rust-plain, python-precommit, go, empty, mixed with CRLF and no final newline)
in a temporary repository and checks: `--dry-run` leaves files, modes, `git status --porcelain`
and `core.hooksPath` unchanged; the config parses; nothing is staged; `sluice doctor` has no
`fail`; `sluice hook print` for every manager names the same `(hook, gate)` pairs as the config and
as the hook files; a second `init --force` changes no file. More cases: symlinked hooks inside
and outside the repository, `.git/hooks` with its own hooks, a heavy line in a `.git/hooks`
hook, subdirectory invocation, outside a repository, `--force`, usage error. Unit tests cover the
line parser, the YAML scanners and rewriting, naming, kinds and the diff. Full gates:
`sluice run fmt`, `clippy`, `test`.
