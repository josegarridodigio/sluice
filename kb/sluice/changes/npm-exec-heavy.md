---
title: "Change: the npm exec family is heavy like npx (v0.4.1 F3)"
tags: [sluice, change, guard, shim, heavy]
---

# Change: `npm exec`, `npm x`, `pnpm dlx`, `yarn dlx` unwrap like `npx`

## What

`npm exec playwright test` (common inside package scripts) was not heavy:
`exec` is a package-manager builtin in `PM_BUILTINS`, so the canonical words
stayed `npm exec ...` and never matched `playwright test`.

`unwrap_launcher` in `crates/sluice-gate-core/src/heavy.rs` now unwraps, in the
existing bounded loop of 4 rounds:

- `npx|bunx [-y|--yes] [--] X...` (helper `exec_inner`),
- `npm exec|x [-y|--yes] [--] X...` (after the directory options skipped by
  `subcommand_index`),
- `pnpm|yarn exec|dlx X...`.

Any other option before `X` (`npm exec -p pkg X`, `npm exec --help`) stops the
unwrap, as for `npx` today; `npm install` is untouched.

## Files and symbols

- `crates/sluice-gate-core/src/heavy.rs`: `exec_inner` (new),
  `unwrap_launcher`, doc of `canonical_words`; unit tests in `equivalences`,
  `non_equivalences` and the new `npm_exec_family_is_heavy_like_npx`.
- `crates/sluice-gate/tests/guard.rs` (guard rewrites `npm exec playwright test
  --config=x` and `pnpm dlx vitest run`, leaves `npm exec --help` and `npm
  install`), `crates/sluice-gate/tests/shim.rs` (the shims queue the same).
- `docs/configuration.md` (Launchers paragraph), `CHANGELOG.md`.

## Why

First real-project rollout: package scripts using `npm exec playwright test`
ran outside the queue.

## Verification

`verify` through Sluice (fmt, clippy, test) on the committed branch.
