---
title: "Change: automatic weights and learned exec weights (zero-config task T4)"
tags: [sluice, change, scheduler, config, exec, zero-config]
---

# Change: automatic weights (2026-10-23)

Task T4 of [[sluice/plan/zero-config-plan.md]]. Decision:
[[sluice/design/adr/0015-automatic-weights.md]]. Builds on learned weights
([[sluice/design/adr/0011-scheduler-throughput.md]], M2) and amends the
compatibility promise ([[sluice/design/adr/0013-compatibility-promise.md]]).

## What changed

- **Config** (`sluice-gate-core/src/config.rs`): new `Weight` enum
  (`Fixed(u32)` | `Auto`) with hand-written serde (integer or the string
  `"auto"`), `Weight::ceiling(budget)`, `Weight::is_auto()`, `Display`, and the
  free function `auto_ceiling(budget) = ceil(budget / 2)` (also used by
  `AgentConfig::exec_weight_or`). `Gate::weight` is now a `Weight`; omitted is
  `Fixed(1)`; `0` is still rejected by the validation with the key path
  (`gates.<name>.weight must be >= 1`); other strings fail with
  `invalid weight "x": use an integer >= 1 or "auto"`.
- **Protocol** (`sluice-gate-proto`): `GateSpec` gains the optional
  `weight_auto: bool` (omitted when false). `GateSpec.weight` keeps carrying the
  integer ceiling the client computed from its own budget, so an older daemon
  sees a sane declared weight and ignores the flag; a newer daemon receiving an
  older client's spec sees `false`. `RunningJob` and `QueuedJob` gain optional
  `weight_effective` and `weight_auto` (status schema stays 1; the fixture is
  unchanged because the fields are omitted when absent).
- **Clients**: `sluice run` (`run.rs`), the MCP server and the queue runner build
  `GateSpec` with `gate.weight.ceiling(budget)` and `weight_auto`; the in-process
  fallback acquires `ceiling(budget)` slots. `gate_list` keeps its frozen
  output schema (`weight` integer = the ceiling) and its text shows `auto`.
- **Daemon** (`server.rs`, `scheduler.rs`): `register` resolves
  `weight_auto` to `auto_ceiling` of the daemon's own budget; the scheduler
  needs no new rule because the ceiling goes through the existing clamp
  (`clamp_learned`, floor `ceil(ceiling / 4)`). `JobSpec`, `RunningView` and
  `QueuedView` carry `weight_auto` for `status`.
- **Exec learning**: `register_exec` gives exec jobs the stats key
  `(repository root or canonical cwd, "exec:<label>", "")`, so the existing
  sampler, `gate_stats` rows, 10-run window, 3-run warm-up, passing-runs-only
  rule and pruning apply unchanged. The ceiling is whatever the client resolved
  (`--weight`, else `[agent] exec_weight`, else `ceil(budget / 2)`).
- **Status**: `sluice status` prints `weight auto→5` / `weight 10→3`
  (`weight_text` in `sluice-gate/src/status.rs`); the JSON carries
  `weight_effective` (= `weight`) and `weight_auto` (only when true).
- **Doctor input**: `Store::unbounded_learned(repo, gate)` returns the P75
  demand (rounded half up) of the newest 10 passing runs of a gate, without the
  clamp; `None` before 3 samples. T6 builds the `weights` check on it.
- **Docs**: `docs/configuration.md` (`weight`, `scheduler.learned_weights`,
  `agent.exec_weight`), `docs/compatibility.md` (a file with `"auto"` needs
  0.4.0; item 2 of the promise names new accepted values of an existing key as
  additive), `kb/sluice/design/scheduler-throughput.md` (new section),
  `kb/sluice/design/config-format.md`, CHANGELOG.

## Compatibility

- Every existing config fixture loads to identical values (`weight` still
  serialises as the same integer in `expected.json` comparisons).
- A `.sluice.toml` with `"auto"` is rejected by 0.3.0 and older (strict
  parsing); stated in the docs. The new fixture is
  `crates/sluice-gate-core/tests/fixtures/unreleased/auto-weight.toml`, outside
  the checksummed per-release recordings.
- Protocol 1 and status schema 1 unchanged; only optional fields were added.

## Tests

- `config.rs`: `weight_accepts_auto_and_integers_only`,
  `weight_ceiling_resolves_auto_from_the_budget` (plus the existing
  `weight_must_be_positive`).
- `sluice-gate-core/tests/compat.rs`: `auto_weight_fixture_parses`.
- `sluice-gate-proto/tests/compat.rs`: `gate_spec_weight_auto_is_additive`;
  `status_schema.rs`: `weight_effective_and_auto_are_additive_optional_fields`.
- `scheduler/tests.rs`: auto on a budget of 20 is admitted at 10 and drops to
  the measured 4 (floor 3); a declared 8 never exceeds 8; exec jobs of one
  label measured at about 1 CPU are admitted at 3 (the floor of a ceiling of
  10) from the fourth run, other labels keep their own history.
- `store.rs`: `unbounded_learned_weight_is_the_p75_demand_without_a_clamp`.
- `status.rs`: `weights_show_configured_and_effective`.

## Verification

`sluice run fmt`, `sluice run clippy`, `sluice run test` from the worktree
root, once at the end (see the commit).
