---
title: "Change: doctor checks and documentation of the zero-config flow (task T6)"
tags: [sluice, change, doctor, docs, zero-config]
---

# Change: doctor and documentation (2026-10-23)

Task T6 of [[sluice/plan/zero-config-plan.md]]. Builds on
[[sluice/changes/session-start.md]], [[sluice/changes/heavy-classification.md]],
[[sluice/changes/automatic-weights.md]] and [[sluice/changes/init-detection.md]].

## What changed

- `crates/sluice-gate/src/doctor.rs`, four new checks (same `Check` struct, so
  text and `--json` carry them):
  - `shims` (`check_shims`): the shims of the current heavy list exist in the
    stable directory (`shim::default_dir`) with exactly the content
    `shim::script` would write for this binary; missing and stale ones are
    counted. First in `PATH` is judged for this shell (`path_index`). In an
    agent shell (`SLUICE_SHIMS`, else agent detection) a not-first or absent
    `PATH` entry, or no shims at all, is `warn`; in a human shell it is `ok`
    with a note that the agent plugin puts them in `PATH`. Missing or stale
    shims in a partly installed directory are `warn` for everyone.
  - `plugin` (`check_plugin`, `plugin_version`): `installed_plugins.json` under
    `$CLAUDE_CONFIG_DIR` or `~/.claude`, entry whose key starts with `sluice@`;
    `warn` on a different version, `ok` when equal, missing or unreadable.
  - `weights` (`check_weights`, `weight_check`): for each gate with a numeric
    `weight`, `Store::unbounded_learned` (database opened read-only, as
    `check_database` does) over 150 % of the ceiling is a `warn` naming the
    gates and suggesting a higher number or `weight = "auto"`. `"auto"` gates
    and gates without enough history are skipped.
  - `init`: in a git repository without `.sluice.toml`, `ok` with the hint
    `run 'sluice init --dry-run'`. A present but invalid file keeps the
    `repo-config` failure only.
- `crates/sluice-gate-mcp/src/text.rs`: `render_status` shows
  configured to effective weights (`weight auto→5`, `weight 10→3`) from
  `weight_cfg` and `weight_auto`, the same rule as the CLI's `weight_text`.
- Docs: `docs/agents.md` (Claude Code section reduced to binary plus agent
  plugin; session hook, consent flow for a missing or older binary, init
  proposal, limits from ADR-0014 including husky's `node_modules/.bin`;
  the "Claude Code users do not need shims" claim is gone; shims section kept
  for other agents), `docs/configuration.md` (heavy list, never-heavy rules,
  package-script resolution), `docs/cli.md` (hidden `sluice agent
  session-start`, doctor checks), `docs/debugging.md` (how to see the session
  hook's decision, doctor checks), `README.md` (three-step quick start,
  features, limitations). `CHANGELOG.md` `Unreleased` already had one heading
  per section; entries added, none removed.

## Files

`crates/sluice-gate/src/doctor.rs`, `crates/sluice-gate/tests/doctor.rs` (new),
`crates/sluice-gate-mcp/src/text.rs`, `docs/{agents,configuration,cli,debugging}.md`,
`README.md`, `CHANGELOG.md`.

## Why

The incident of the plan's section 1 showed that a user cannot tell from
`sluice status` that the shims are not first in `PATH`, that the plugin is a
version behind the binary, or that a declared weight is far too low. Doctor
now says so, and the docs describe the flow that the code implements.

## Notes

- `shim.rs` helpers (`path_position`, `shim_state`) are private and that file
  belongs to another task, so doctor compares the shim file with
  `shim::script` and resolves `PATH` itself; both use only the existing public
  API.
- The `weights` check reads the learned weight without the clamp, so it can
  see that a ceiling is too low (the clamped value never exceeds the ceiling).

## How it was verified

Unit tests in `doctor.rs` (`plugin_version_is_read_from_the_installed_list`,
`plugin_check_warns_on_a_different_version_and_never_fails`,
`weights_warn_only_when_the_ceiling_is_far_below_the_learned_weight`,
`shims_report_state_and_path_position_by_shell_kind`), integration tests in
`crates/sluice-gate/tests/doctor.rs` (JSON checks, plugin version warning,
`init` hint), and `status_text_shows_configured_and_effective_weights` in the
MCP crate. Full gates through Sluice: `fmt`, `clippy`, `test`.
