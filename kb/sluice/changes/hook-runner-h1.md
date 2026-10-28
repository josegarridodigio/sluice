---
title: "Hook runner H1: the entry point and faithful delegation"
tags: [sluice, changes, hooks, husky, lefthook, pre-commit]
---

# Hook runner H1: the entry point and faithful delegation

Wave H1 of [[sluice/plan/hook-runner-plan.md]] (ADR-0021). Design:
[[sluice/design/hook-runner.md]]. Status: implemented, uncommitted, `cargo check` clean;
the full verification (fmt, clippy, test) runs through Sluice at the end of the wave.

## What changed

- New module `crates/sluice-gate/src/hookentry/` (`mod.rs`, `activate.rs`, `delegate.rs`,
  `gitcfg.rs`, `script.rs`): `Layout`, `State`, `activate`, `deactivate`, `inspect`,
  `Inspection`, `decide`, `run`, `detect`/`Manager`, `ensure_last_include`, `render`,
  `ALL_HOOKS`, `restore_if_displaced`/`restore_within`.
- `crates/sluice-gate/src/main.rs`: `sluice hooks on|off|status [--json]` and the hidden
  `sluice hooks run [--common-dir DIR] <hook> -- <args>`; `sluice init --hooks rewire|entry`.
- `crates/sluice-gate/src/init/mod.rs`: `HooksMode`, `repository_hooks_path` (ignores
  Sluice's own value), `entry_notes`, activation in `write_changes`, `atomic` is `pub(crate)`.
- `crates/sluice-gate/src/doctor.rs`: `check_hooks` rewritten (`check_managed_hooks`,
  `configured_hooks_dir`, `hooks_call_sluice`); the "ignore this if a hook manager owns the
  hooks" warning is gone.
- `crates/sluice-gate/src/session_start.rs`: `hooks_entry` restores a displaced activation
  and adds one sentence to the context.
- Docs: `docs/hooks.md` (new), `docs/cli.md`, `CHANGELOG.md` (Unreleased).

## Why

A client repository's husky hook files could not be edited, so every commit re-ran lint and
the whole suite outside the queue and the cache. Owning the entry point locally needs no
repository change and survives husky's `core.hooksPath` rewrite on every install.

## Decisions taken inside the wave

- The scripts mirror the executable hooks of the underlying directory (lead's performance
  review): installing all 28 would cost a process start per `reference-transaction` /
  `post-index-change` / `post-checkout` even where the repository has no such hook, while
  installing none would stop hooks (git ignores `.git/hooks` once `core.hooksPath` is set).
  The set is kept in sync by `on`, by `hooks run` (`activate::sync`), by the session-start
  restore and by doctor (`unmirrored` -> fail, fixed by `sluice hooks on`).
- Lead's review fix: `hooks run` only adds scripts; `on` and the restore prune against the
  union of all worktrees (`mirrored_in_clone`, `git worktree list --porcelain`). Sluice's
  directory is shared by the worktrees while husky's `.husky/_` is relative and untracked, so
  a commit in a fresh linked worktree used to compute an empty set and delete every script,
  silently stopping the main worktree's hooks. Test:
  `a_linked_worktree_without_the_relative_hooks_dir_keeps_the_scripts`.
- The scripts prefer the activating binary's absolute path to the one in `PATH` (an older
  installed `sluice` would reject `hooks run`; seen while testing by hand).
- Delegation is "execute the file the previous hooks directory holds" for every manager.
  `lefthook run` / `pre-commit hook-impl` are not called when the manager's script is missing:
  that would start running hooks that did not run (H2).
- The previous value is recomputed at run time (last `core.hooksPath` that is not Sluice's);
  `hooks.json` is only the fallback when git cannot be asked and the source of the fallback
  in the scripts.
- `doctor`: `fail` for a manager (husky, lefthook, pre-commit) with non-calling hooks, or
  gates with hooks over non-calling hook files; `warn` when `.sluice.toml` has hook gates and
  there are no hook files at all (many existing sandbox tests have exactly that and expect
  exit 0).
- Lead's review fix: with lefthook or pre-commit the hook files only start the manager and
  `sluice init` wires the commands in the manager's configuration, so doctor now reads it too
  (`doctor.rs::manager_config_calls_sluice`: `lefthook*.yml|yaml|json|toml` and the
  `lefthook-local` variants, `.pre-commit-config.yaml`) and reports `ok` when it calls sluice.
  Without it the `fail` turned two `tests/init.rs` cases red (lefthook and pre-commit repos
  wired by `init`).

## Verification

`cargo fmt`, `cargo check --workspace --all-targets` clean. Manual runs with the built binary in
temporary repositories: activation, husky rewrite after activation, include before `[core]`,
off (config byte-identical), push stdin through git, missing-binary fallback, linked worktree.
New tests: `crates/sluice-gate/tests/hooks.rs` (activation order, husky rewrite, rebuilt
includes, idempotence, worktree, off, status, displacement, delegation per manager with fake
managers, exit codes and signals, stdin through `git push`, missing binary, doctor, session
start, init, real lefthook and pre-commit when on `PATH`), unit tests in `hookentry/`, and the
husky case of `tests/doctor.rs` now expects `fail`.

Full verification through Sluice (`verify`, gates fmt, clippy, test): all pass after the two
review fixes above (the first run failed clippy on three lints and `tests/init.rs` on the
doctor false failure). `tests/hooks.rs`: 29 passed. lefthook and pre-commit are not installed
on the development machine, so `real_lefthook_*` and `real_pre_commit_*` returned early; they
must run where the tools exist (CI or Matrix) before the release.
