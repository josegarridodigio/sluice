---
title: "Hook runner H2a: units for husky and plain hooks, through the daemon, with caching"
tags: [sluice, changes, hooks, husky, units, cache]
---

# Hook runner H2a: units for husky and plain hooks

Wave H2a of [[sluice/plan/hook-runner-plan.md]] (ADR-0021 decisions 3, 4, 5, 6, 8). Design:
[[sluice/design/hook-runner.md]] (section "H2a"). Status: implemented, uncommitted; `cargo
check`, `cargo fmt` clean; the new test file and unit tests pass when run in isolation; the
full verification (fmt, clippy, test) runs through Sluice at the end.

## What changed

- New in `crates/sluice-gate/src/hookentry/`: `units.rs` (`Unit`, `Kind`, `Program`, `Plan`),
  `grammar.rs` (`parse`, `Script`, `Cmd`), `classify.rs` (`classify`), `plan.rs` (`plan`,
  `Input`, `describe`, husky and plain adapters), `runner.rs` (`try_units`, `opt_out`,
  `DELEGATE_VAR`, `prepare`, `execute`, `gate_of`).
- `hookentry/delegate.rs`: `run` calls `runner::try_units` before the H1 `exec`; `Manager::slug`.
  `hookentry/mod.rs`: `hook_plans`, `print_plans`, `describe` (status gains `hooks`).
  `hookentry/activate.rs`: `mirrored` and `resolve` are `pub(super)`.
- `crates/sluice-gate/src/run.rs`: `run_traced` split into `run_traced` + `run_resolved(ctx,
  args, mode, trace, enforce_policy)`; `record_invocation`, `caller_label`; new
  `run_unit(res, hook) -> u8`. `sluice run` behaves as before (`enforce_policy = true`).
- `crates/sluice-gate-core`: `RepoConfig::implicit`, `lookup::resolve_repo_or_implicit`,
  `lookup::build_context` (shared with `resolve_repo`), `RepoContext::select_unit(name, gate,
  keep_git_env)`.
- Tests: `crates/sluice-gate/tests/hook_units.rs` (12 tests; the stdin one is `a_hook_with_stdin_runs_as_units_only_when_every_unit_is_a_check`), unit tests in `grammar.rs`,
  `classify.rs`, `plan.rs`, `runner.rs`.
- Docs: `docs/hooks.md` (Units), `docs/cli.md`, `CHANGELOG.md` (Unreleased/Added), design note.

## Why

A husky hook whose file cannot be edited re-ran lint and the whole suite outside the queue and
the cache on every commit. H1 gave Sluice the entry point; H2a makes the hooks Sluice
understands completely go through the daemon and the cache, and keeps delegating the rest.

## Decisions and deviations (none from the ADR's decision text)

- **Reused instead of rebuilt**: `sluice run`'s pipeline (lookup, daemon `run` request, in-process
  fallback, report, `client.log`) through `run_unit`, with synthetic gates. No second queue or
  cache. Only the hook policy differs (a unit never gets `fail-fast`).
- **Mutating units are not reordered.** The ADR says mutating steps run first; the lead's brief
  for this wave overrides that: file order is kept, mutating units always run uncached, check
  units are cached (the key is computed right before each unit, so it already reflects earlier
  mutating units). To be reflected in the ADR's follow-ups if the lead agrees.
- **Exit codes.** A unit's code is the hook's code. Sluice never emits 3 from this path, so 3 is
  never ambiguous; the printed `FAIL exit=3` names the unit. `sluice run`'s fail-fast 3 and a
  delegated pre-commit's 3 are unchanged. Sluice-internal errors after a unit started use the
  same codes as `sluice run` (1, 124, 143).
- **Check units run without git's hook variables** (like gates), mutating ones keep them. This
  differs from a delegated run for checks (which would see `GIT_DIR`, `GIT_INDEX_FILE`); needed
  for a sound key and benign for tests that spawn git.
- **stdin** (lead review): units run as daemon jobs without the hook's stdin, so a hook that
  receives stdin runs as units only when every unit is a `Check` (whitelisted tools do not read
  stdin), independent of the unit count; otherwise the whole hook is delegated with the command
  named in the reason. `pre-push` with `npm test` stays accelerated.
- **`audit`** (lead review): `npm|pnpm|yarn audit` is `Mutating` (always runs, uncached): its
  result depends on the network advisory database, not on the tree.
- **Flake fix** (lead review): `a_missing_sluice_binary_still_runs_the_original_hook` copies the
  binary with `fs::copy`; a concurrent test thread's `fork` can briefly inherit the write
  descriptor, making exec fail with ETXTBSY. The test now retries the spawn on
  `ExecutableFileBusy` (the repo had no earlier handling to reuse).
- **husky sourcing lines**: v4-v8 bootstrap lines are ignored as the brief says; v8's
  `husky.sh` re-runs the hook with `sh -e` (modelled as errexit) and honours `HUSKY=0`,
  `~/.huskyrc` and `init.sh` (all delegate). v8 was not measured against its source.
- **Gate sharing**: not done. `KeyMaterial` contains the gate name, so a unit never matches a
  `.sluice.toml` gate; H3 needs a name-independent key to share entries.
- **Toolchain inputs**: only the manager's `PATH` string. `node_modules` is untracked and not in
  the tree snapshot, so upgrading a tool without touching tracked files does not invalidate a
  cached check (same as gates without `key_inputs`). `key_files`/`key_inputs` derived from the
  hook are a possible follow-up.
- **No timeout** on units (a hook has none; the gate default of 30 min would be a new failure).
  Weight is `auto` (a ceiling that learned weights lower).
- **Doctor** unchanged (no new check).
- Not tested: a daemon dying in the middle of a hook (the in-process fallback is `sluice run`'s
  existing, tested path).

## How it was verified

`cargo fmt`; `cargo check --workspace --all-targets -j 4`. Isolated runs: `cargo test -p
sluice-gate --test hook_units` (12 pass), `--bin sluice hookentry` (22 pass), `--test hooks` (the
H1 delegation tests; one run hit a `Text file busy` race in
`a_missing_sluice_binary_still_runs_the_original_hook`, fixed by a retry, see above). Full fmt, clippy
and test through Sluice (`verify`): all pass after the lead fixed three clippy lints
(`needless_borrow` in `classify.rs`, `match_same_arms` and `needless_continue` in
`grammar.rs`). The lead's review also asked for the stdin rule (units only when every unit is a
check), `audit` as uncached and the flake fix, and amended ADR-0021 decision 5 (file order).
