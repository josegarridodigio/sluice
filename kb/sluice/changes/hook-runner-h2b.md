---
title: "Hook runner H2b: lefthook and pre-commit units"
tags: [sluice, changes, hooks, lefthook, pre-commit, units, cache]
---

# Hook runner H2b: lefthook and pre-commit units

Wave H2b of [[sluice/plan/hook-runner-plan.md]] (ADR-0021 decisions 4, 6, 8). Design:
[[sluice/design/hook-runner.md]] (section "H2b"). Research:
[[sluice/research/hook-managers-lefthook.md]], [[sluice/research/hook-managers-pre-commit.md]].
Status: implemented, uncommitted; `cargo check --workspace --all-targets`, `cargo fmt` clean;
`hook_managers` (20), `hook_units` (12), `hooks` (29) and the `hookentry` unit tests (33) pass in
isolation; the real-tool tests skip on this machine (the tools are not installed here); full
verification (fmt, clippy, test) runs through Sluice.

## What changed

- New in `crates/sluice-gate/src/hookentry/`: `lefthook.rs` (`plan`), `precommit.rs` (`plan`),
  `tooling.rs` (`find_on_path`, `capture`, `parse_version`, `sh_line`, `sha256_hex`), `precond.rs`
  (`check`, `Precondition::describe`).
- `units.rs`: `Unit.git_env`, `Unit.key_inputs`, `Unit.key_files`, `Program.keep_going`,
  `Program.precondition`, `Precondition`.
- `plan.rs`: dispatches `Manager::Lefthook` and `Manager::PreCommit` to the adapters; `describe`
  adds `requires`. `mod.rs`: `print_plans` prints the run-time condition.
- `runner.rs`: precondition check in `try_units`, `execute_all` (keep-going semantics), `gate_of`
  uses `key_inputs`/`key_files`, `run_one` uses `unit.git_env`.
- Dependency: `serde_norway` 0.9 (YAML for `.pre-commit-config.yaml`; the workspace had none;
  `serde_yaml` is archived).
- Tests: `crates/sluice-gate/tests/hook_managers.rs` (fake `lefthook`, fake pre-commit Python, two
  real-tool tests), unit tests in `lefthook.rs`, `precommit.rs`, `precond.rs`, `tooling.rs`.
- CI: `scripts/ci/install-hook-managers.sh` (lefthook 2.2.2 release binary, pre-commit 4.7.0 in a
  venv), called from `ci.yml`, `matrix.yml` (git-minimum, aarch64) and `scripts/ci/container-test.sh`
  (distro packages for python3/venv added) so the real-tool tests run there.
- Docs: `docs/hooks.md` (two sections and a compatibility table), `docs/cli.md`, `CHANGELOG.md`
  (Unreleased/Added).

## Decisions and deviations

- **No new ADR; none of ADR-0021's text is contradicted.** The ADR says unnamed jobs, setups,
  ambiguous ids and ref-dependent stages delegate; this wave adds more delegations (below).
- **Skip is indistinguishable from failure in lefthook** (`--command` on a skipped job exits 1,
  "no job matching"). Instead of guessing, a hook is run as units only when no selected job can be
  skipped (see the design note). Consequence: the dominant real shape (`glob` + `{staged_files}` +
  `stage_fixed`) delegates; `run: npm test` style commands run as units. A later wave can lift this
  with a reliable skip signal (for example an output marker) or by evaluating the filters with
  Sluice's own list and `--file`.
- **lefthook `pre-push` always delegates** (a command with no push files is skipped; the push-file
  list depends on remote-tracking refs).
- **lefthook `stage_fixed` delegates**, although the brief allowed it as a mutating unit: lefthook
  stages after all jobs and its partial-staging guard runs per invocation.
- **lefthook git-lfs hooks delegate** (`post-checkout`, `post-commit`, `post-merge`): `--skip-lfs`
  (required by the brief) would drop the LFS hook that lefthook runs.
- **Git hook arguments are not passed to `lefthook run`**: with no `{n}` template (templates
  delegate) a job cannot see them, and flag order after positionals was not verified.
- **Version gate**: lefthook 2.x from 2.2.2 only (flags read from v2.2.2). Older or newer majors
  delegate.
- **Binary discovery** is a subset of the hook script's: `$LEFTHOOK_BIN`, `lefthook` on `PATH`. The
  script probes `lefthook -h` before using a `PATH` entry; Sluice only checks the file is executable.
- **Every lefthook and pre-commit unit keeps git's variables**, checks included (both managers ask
  git for the staged files; `git commit -a` uses a temporary index). This differs from H2a checks,
  which run without them. The cache key is unaffected (private tree snapshot).
- **keep-going semantics** (new): lefthook and pre-commit run every unit after a failure (lefthook
  sequential mode; pre-commit without `fail_fast`) and the hook exits 1; any other code (pre-commit's
  3 or 130, a signal) ends the hook with that code. Sluice itself never emits 3 on this path: the
  `fail-fast` refusal is not applied (`enforce_policy = false`, H2a), confirmed by a test with
  `CLAUDECODE=1`.
- **pre-commit stash decision**: per-id runs only while no tracked file differs from the index
  (`git status --porcelain -uno`, `Y != ' '` means unstaged change, intent-to-add and unmerged
  included); otherwise delegate. Never stash from Sluice.
- **pre-commit remote hooks need `stages` in the config** to be placed; otherwise delegate. Real
  configs mixing remote fixers with local checks therefore mostly delegate (68 of 72 sampled configs
  contain remote repos, per the research). Reading manifests from pre-commit's store is the obvious
  follow-up.
- **pre-commit caching requires `always_run: true`** (a skipped hook exits 0).
- **`serde_norway` reads YAML 1.2-ish scalars**; PyYAML reads 1.1 (`yes`, `on`). Booleans and
  strings that are not plain make the config unreadable, hence a delegation; merge keys delegate.
- **Config path** comes from the stub's `--config=` (relative to the root), as `hook-impl` does.
  Without an installed stub the hook is not run at all (nothing ran before).
- **`minimum_pre_commit_version`** above 4.7.0 delegates (the version read).
- **Status**: units show `requires` (the run-time condition). The condition is evaluated only when a
  hook runs, not in `status`.

## Uncertainties

- Real-tool tests were written from the research notes and the sources in `refs/`, not run: lefthook
  2.2.2 and pre-commit 4.7.0 are not installed on the developer machine. Points to watch on the first CI
  run: the exact JSON of `lefthook dump` (assumed: top-level hook objects with `parallel`,
  `commands`, `jobs`), `lefthook version` output shape, flags after the hook name (the integration
  tests of lefthook use them), and `pre-commit` config path handling.
- `lefthook run` in a pty-less daemon job prints its own formatting; the daemon logs hold it.
- `git status --porcelain` with `status.showUntrackedFiles`/submodule settings: `-uno` is explicit;
  a dirty submodule shows as ` M` and delegates the pre-commit hook (conservative).

## How it was verified

`cargo fmt`; `cargo check --workspace --all-targets -j 4`; isolated runs: `cargo test -p sluice-gate
--test hook_managers` (20 pass, 2 of them real-tool tests that return early without the tools),
`--test hook_units` (12), `--test hooks` (29), `--bin sluice hookentry` (33). The full fmt, clippy and
test run through Sluice (`verify`): all pass after the lead's review fixes (clippy
`manual_let_else` and `assigning_clones` in `lefthook.rs`; `struct_excessive_bools` in
`precommit.rs`, where `always_run`, `pass_filenames` and `log_file` became one
`unconditional` field, both booleans still validated). The lead also pinned the SHA-256 of
the lefthook release binaries in `scripts/ci/install-hook-managers.sh` (taken from the
release's checksum file and checked against a download).

The real-tool tests have not run yet: the tools are not on the development machine, and the
Sluice guard (correctly) refuses a raw `cargo test` with a modified `PATH`. They run in CI and
in Matrix on the branch, which must pass before any release.
