---
title: "ADR-0016: sluice init detects the stack and rewires existing hooks"
tags: [sluice, adr, zero-config, init, hooks]
---

# ADR-0016: `sluice init` detects the stack and rewires existing hooks

Status: accepted (2026-10-23). Plan: [[sluice/plan/zero-config-plan.md]]
task T3.

## Context

`sluice init` wrote a fixed Node template in the current directory and
printed hook snippets for the user to paste. On a real Angular project with
husky, the result needed hand edits (command, watch flag, weights) and the
hooks kept running the suites outside Sluice. The user wants one command
that leaves the repository ready, and wants existing hooks edited, not
snippets printed.

`CLAUDE.md` says product code never modifies the user's index or working
tree. That rule protects the user from side effects of verification
(`run`, `verify`, the daemon, the guard, shims, the merge queue).

## Decision

- `sluice init` is an explicit, user-invoked setup command and is the one
  exception to the working-tree rule: it may write `.sluice.toml` at the
  repository root (`git rev-parse --show-toplevel`), edit existing hook files
  and create `.githooks/` with `core.hooksPath`. Nothing else in the product
  gains that right. It never stages or commits.
- Gates come first from what the existing hooks already run (husky,
  lefthook, the pre-commit framework's local hooks, plain hooks), then from
  stack detection at the root (Angular, Node with its package manager, Rust,
  Python, Go). Mutating commands (formatters with `--write`, `--fix`,
  `lint-staged`, `pretty-quick`) stay in the hooks untouched.
- Hook files are edited textually, line by line: a line that became a gate
  is replaced in place by `sluice run --hook <hook> <gate>`; other lines,
  comments, file mode and line endings are kept. A line that cannot be
  matched safely is left alone and its snippet printed.
- The `.sluice.toml` is meant to be committed (team-wide gates). No
  `info/exclude` entry by default.
- `--dry-run` prints the gates, the files and a unified diff and writes
  nothing; the agent plugin shows it to the user before asking to run
  `init` ([[sluice/design/adr/0014-zero-config-agent-integration.md]]).
- Idempotent: lines already calling `sluice run` are recognised; a second
  run with `--force` changes no hook.

## Amended in 0.4.1

- A rewired line keeps working without Sluice: it is written as
  `if command -v sluice >/dev/null 2>&1; then sluice run --hook <hook> <gate>;
  else <original>; fi` (through `sh -c '...'` for a pre-commit `entry:`). The
  files `init` creates itself start with `command -v sluice ... || exit 0`.
  `init --force` recognises this form and upgrades the bare 0.4.0 lines to it,
  taking the fallback from the gate's `cmd` and `env`.
- Files are written atomically (temporary file in the same directory, fsync,
  mode, `rename`; the target of an in-repo symlink, never the link), because
  a hook may be executing while `init` rewrites it.
  See [[sluice/changes/init-hook-fallback.md]].

## Consequences

- `init` can surprise a user who expected only a config file; the summary and
  diff say exactly what changed, and git shows it before any commit.
- Detection covers common layouts at the root; monorepos get a commented
  list of nested projects to configure by hand.
