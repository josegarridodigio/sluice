---
title: "Change: key_env, installed-state key files from init, doctor key-env check (ADR-0029)"
tags: [sluice, change, cache-key, init, doctor]
---

# Change: key_env, installed-state key files from init, doctor key-env check (ADR-0029)

Why: a user asked whether sharing a run is safe when the tree does not describe the whole
test environment; variables of the caller's environment and the installed dependencies
were not in the key. Decision:
[[sluice/design/adr/0029-environment-inputs-of-the-cache-key.md]].

## What changed

- `sluice-gate-core/src/config.rs`: `Gate::key_env` (validated: no empty name, no `=`),
  template comment. `key.rs`: `KeyMaterial::key_env`, `collect_env`, layout
  `env:NAME \0 (=value | unset) \0` after the key files. `lookup.rs::derive_key_at` reads
  the values from the same child environment `key_inputs` use.
- `sluice-gate/src/hookentry/runner.rs`: zero-config units carry an empty `key_env`
  (their keys do not change).
- `sluice-gate/src/init`: `GatePlan::{key_env, key_files}`, rendered by `render.rs`;
  `detect::environment_inputs` (variables per toolchain, Node installed-state file per
  package manager) applied in `plan` to every gate not carried from an existing config.
- `sluice-gate/src/doctor.rs`: `check_key_env` / `key_env_warning` (`key-env` warning);
  `BUILD_INPUTS` extended with toolchain pins, package manager settings, lint settings and
  test runner configuration.
- `kb/sluice/design/cache-key.md` (public contract), `docs/configuration.md`,
  `docs/compatibility.md` (fixture `gate-key-env.toml`), README limits, CHANGELOG.

## How it was verified

- Golden: `key::tests::key_env_extends_the_layout_and_tells_unset_from_empty` (exact bytes,
  unset vs empty vs `"unset"`, empty list = old layout) and the release fixtures of
  `tests/compat.rs::old_keys_are_derived_identically` with `key_env: Vec::new()`.
- `key::tests::collect_env_reads_each_name_once_in_order`,
  `tests/compat.rs::gate_key_env_fixture_parses`,
  `init::detect::tests::environment_inputs_follow_the_toolchain_and_the_package_manager`,
  `doctor::key_env_tests::a_toolchain_without_its_environment_is_warned_about`.
- The full gates through Sluice (`verify`).
