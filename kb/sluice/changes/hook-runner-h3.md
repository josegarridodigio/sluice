---
title: "Hook runner H3: hook units as gates"
tags: [sluice, changes, hooks, verify, gates, mcp]
---

# Hook runner H3: hook units as gates

Wave H3 of [[sluice/plan/hook-runner-plan.md]] (ADR-0021 decision 7). Design:
[[sluice/design/hook-runner.md]] (section "H3"). Status: implemented, uncommitted; `cargo check
--workspace --all-targets` and `cargo fmt` clean; the new `hook_gates` test file (9), the `gates` and
`session_start` unit tests and the existing `hook_units`, `hook_managers`, `hooks`, `mcp`, `doctor`,
`session_start`, `cli` and `run` test files pass in isolation. The full verification (fmt, clippy, test)
runs through Sluice.

## What changed

- `sluice-gate-core`: new `hookgate.rs` (`HookGate`, `HookExclusion`, `HookGates`, `HookGateSource`);
  `lookup.rs`: `resolve_repo_hook_gates`, `resolve_hook_gates`, `hook_config`; `select_unit` gains `run_env`
  (applied after `gate.env`); `ConfigNotFound` message mentions `sluice hooks status`.
- `sluice-gate`: `hookentry/gates.rs` (`derive`, `hook_of_name`, `share`, `GATE_HOOKS`);
  `hookentry/runner.rs` (`key_env`, `identity`, `gate_of` shared, `Prepared.key_env`); `hookentry/plan.rs`
  (`describe` with `name` and `shares`, `share_note`); `hookentry/mod.rs` (`hook_plans` takes a filter,
  status prints sharing); `gate_list.rs` and `main.rs` (`sluice gate list [--json]`, `hook_gate_source`);
  `run.rs::resolve` (falls back to the hook gates when `.sluice.toml` is missing); `doctor.rs`
  (`entry_point_active`, `derived_gates_check`); `session_start.rs` (`Place::Hooked`).
- `sluice-gate-mcp`: `serve(env, hook_gates)`, `Server.hook_gates`; `verify` resolves with the hook gates;
  `gate_list` returns `source`, `manager` per gate and `excluded`; instructions mention hook gates.
- Plugin: `skills/sluice/SKILL.md`, `reference.md`, `README.md`.
- Docs: `docs/hooks.md` ("Verify and gates without `.sluice.toml`"), `docs/cli.md` (`sluice gate`),
  `CHANGELOG.md` (Unreleased/Added).
- Tests: `crates/sluice-gate/tests/hook_gates.rs` (husky, lefthook with a fake binary, pre-commit with a fake
  Python, plain hooks with a configured gate, doctor, MCP) and unit tests in `gates.rs`, `session_start.rs`.

## Decisions and deviations

- Names stay `<manager>:<hook>#<index>` (design note: why not command-derived).
- **A key bug found by the cross test, fixed**: git prepends its exec path to the `PATH` of hooks, and the
  husky key hashes the `PATH` string, so a `verify` pass (no git around it) could never hit the commit's hook.
  `runner::key_env` removes that leading entry from the hashed `PATH` on both sides; the process keeps the
  full `PATH` (`select_unit(.., run_env)`). The cross tests go through a real `git commit` for this reason.
  H2a's key changes for husky units only when git's exec path is the first `PATH` entry (always under git): one
  miss after upgrading.
- `sluice gate list` did not exist; the brief named it, so it was added (`--json` included). `sluice run`
  keeps a required `<gate>` argument.
- With the entry point **off**, hook units are still gates (the brief allowed "or simply whose hooks Sluice
  can plan"): `verify` works, but nothing warms a hook that does not go through Sluice. Doctor's `repo-config`
  only turns `ok` for a missing `.sluice.toml` when the entry point is active.
- Units of lefthook and pre-commit hooks are gates only while their precondition holds at derivation time;
  `gate_list` is a snapshot ("what can be verified now"). The typical agent flow (edit, `git add`, `verify`,
  `git commit`) satisfies it; edit, `verify`, `git add` leaves pre-commit units excluded (unstaged changes) and
  lefthook ones excluded (nothing staged), with the reason.
- Default `verify` selection for hook gates is the unit set of `pre-commit` (else all, including `pre-push`).
  `pre-merge-commit` units are only picked when the repository has no `pre-commit` gate.
- Sharing with `.sluice.toml` is limited to plain-hook units (husky has a `PATH` addition; lefthook and
  pre-commit run `lefthook run ...` / `pre-commit run ...`, not a gate's command); each refusal has a
  visible reason in `hooks status`. Not done: sharing through a key without the name (it would also
  change every existing key).
- Not extended: `sluice cache status` and the MCP `status` cache section (still `.sluice.toml` only), the
  queue (reads the base commit's `.sluice.toml`; hook gates are not queue gates).
- `ConfigNotFound` text changed (`run 'sluice init' there; without one, gates come from hooks Sluice can run,
  see 'sluice hooks status'`); no test depended on it.

## Uncertainties

- The `PATH` the MCP server (or `sluice run`) sees can differ from the one in the user's shell that makes the
  commit (husky hashes it): then `verify` and the hook do not share an entry (a miss, not a wrong hit). Only the
  git exec path is normalised. A narrower key (only the `PATH` addition) would make the key independent of the
  caller's `PATH` at the price of exactness; left for a measured follow-up.
- `git commit -a` / `git commit <paths>` use a temporary index: lefthook and pre-commit units may see another
  index than `verify` did (a different tree: a miss).
- Derivation costs one `git config` read plus the adapters' probes (lefthook: `version`, `dump` per hook; the
  `sluice run <unit>` path plans only the unit's hook). `gate_list` plans up to three hooks.
- Real-tool tests (lefthook, pre-commit) are still pending in CI/Matrix as in H2b; the H3 tests use the fakes.

## How it was verified

`cargo fmt`; `cargo check --workspace --all-targets -j 4`; isolated runs: `--test hook_gates` (9), `--bin
sluice -- hookentry::gates` (3), `-- session_start` (4), `--test hook_units` (12), `hook_managers` (20),
`hooks` (29), `mcp` (19), `doctor` (5), `session_start` (16), `cli` (8), `run` (24). Full fmt, clippy and test
through Sluice (`verify`): all pass after the lead's fixes (clippy `type_complexity` in the MCP
`load_gates`, now `LoadedGates`; `single_match_else` in `gates.rs`; `assert_is_empty` in
`tests/hook_gates.rs`) and one test fix: `schema_tests::skipped_fields_are_never_required`
compares required field names across every output schema, and `ExcludedInfo::reason` (always
present) shares its name with fields skipped when empty elsewhere; the test now leaves that
definition out by name (`ALWAYS_PRESENT`), every other field is still checked.
