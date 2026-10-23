---
title: "Change: heavy classification without config (zero-config T1)"
tags: [sluice, change, guard, shims, heavy, package-scripts]
---

# Heavy classification without config (zero-config T1)

Plan: [[sluice/plan/zero-config-plan.md]] task T1. Design:
[[sluice/design/command-control.md]] sections 4.4, 4.4.1 and 5.

## Why

On an Angular project, `npx ng test --watch=false`, `npx ng build` and
`npm run axe:*` (Playwright plus `ng serve`) ran raw because the built-in list
knew neither the Angular CLI nor Playwright, and could not see what a
package script runs. Shims also sent a command that is exactly a gate command
to `sluice exec`, bypassing the gate cache.

## What changed

- `crates/sluice-gate-core/src/heavy.rs`
  - `DEFAULT_HEAVY_COMMANDS` gains `ng build|test|lint|e2e`, `nx run|run-many|
    affected|build|test|lint|e2e`, `turbo run|build|test|lint`,
    `playwright test`, `cypress run`, `react-scripts build|test`,
    `astro build`, `nuxt build`, `dotnet build|test`, `bazel build|test`,
    `ninja`, `cmake --build`. New shims: `astro bazel cmake cypress dotnet ng
    ninja nuxt nx playwright react-scripts turbo`.
  - `is_never_heavy` (`SERVER_COMMANDS`, `SHORT_WATCH_TOOLS`): dev servers and
    watch modes are never heavy. `HeavyMatcher::matches` applies it.
  - `HeavyMatcher::classify(canonical, dir, &dyn ScriptSource) -> Option<Match>`,
    `Match { entry, script }`, private `script_entry` (recursion, depth <= 4,
    stack for cycles). `matches` stays the words-only API.
- `crates/sluice-gate-core/src/pkgscripts.rs` (new): `ScriptSource` trait
  (`package_json`, `is_repo_root`), `FsScripts`, `NoScripts`, `Scripts::find`
  (nearest file, stops after the git root, <= 1 MiB, invalid means none),
  `pieces` (split on `&& || ; | &` and newlines, quotes honoured, drops
  assignments and `env`/`cross-env`). `lib.rs` exports the module.
- `crates/sluice-gate/src/guard/matcher.rs`: `World::scripts()` (default
  `NoScripts`), `plan_segment` uses `classify`, `same_command` factored out of
  `gate_edit`, new `exact_gate` for the shims. `guard/mod.rs`: `Fs` returns
  `FsScripts`.
- `crates/sluice-gate/src/shim.rs`: `route` takes a `World` and the cwd; new
  `Route::Gate { gate }` (exact gate command in the directory of the
  `.sluice.toml`, checked before the heavy check, after the `-` stdin check)
  runs `sluice run <gate> -v` in-process; heavy check uses `classify`.
- Docs: `command-control.md` 4.4, new 4.4.1, 5 step 2; CHANGELOG `Unreleased`.

## Tests

- core: `new_builtin_entries_match`, `classify_follows_package_scripts`,
  `shim_names_cover_the_default_list` (updated), `pkgscripts::tests::*`.
- guard unit: `angular_and_package_scripts_are_classified_without_config`
  (empty user config, from the repo directory and a subdirectory, nearest and
  invalid `package.json`); the injection and fuzz suites are unchanged.
- guard e2e: `rewrites_angular_and_package_scripts_with_an_empty_config`.
- shim unit: `package_scripts_and_gates_route`; shim e2e:
  `package_scripts_and_angular_commands_are_queued_without_config`,
  `a_shim_call_equal_to_a_gate_command_runs_sluice_run`.

## Known limits

- `-C`, `--dir`, `--prefix`, `--cwd` are dropped by canonicalisation; the
  script is read from the command's directory, not the target one.
- `npm run build` stays heavy by name even if the script runs `--watch`.

## Shim overhead (release binary, 200 calls, fake instant `npm`, cwd two
levels below the repository, `.sluice.toml` and `package.json` present)

| Call | per call |
|---|---|
| raw binary | 0.54 ms |
| shim, agent, `npm --version` (direct) | 2.35 ms |
| shim, agent, `npm run start` (reads `package.json`, not heavy) | 2.44 ms |
| shim, human session | 2.40 ms |

The overhead of a direct shimmed call is about 1.8 ms, far under the 15 ms
target (script: `sh` loop; no `hyperfine` on the machine).

## Verification

`sluice run fmt && sluice run clippy && sluice run test` from the worktree
root (see the commit on branch `worktree-agent-a86186db4e3bb74db`).
