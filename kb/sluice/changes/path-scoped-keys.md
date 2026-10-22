---
title: "Change: path-scoped cache keys (alpha task D1)"
tags: [sluice, change, cache, cache-key, alpha, d1]
---

# Change: path-scoped cache keys (2026-10-23)

Task D1 of [[sluice/plan/alpha-plan.md]]. Design:
[[sluice/design/path-scoped-keys.md]]; decision: ADR-0012
([[sluice/design/adr/0012-path-scoped-keys.md]]).

## What changed

- Two optional gate fields, `paths` and `paths_ignore` (git `:(glob)`
  pathspec semantics, root-relative). When either is set, the tree field of
  the cache key becomes `scope-v1:<sha256>` over the selected staged
  entries plus `.sluice.toml`; unscoped keys are byte-identical to 0.2.
- The scope hash is computed in process from the `ls-files -s -z` output
  the snapshot already reads for the gitlink check: zero extra git spawns.
- Every front-end (`sluice run`, `run --explain`, `cache status`, MCP
  `verify` and `status`) and the merge queue derive keys through one shared
  core function; the queue's private `derive_key` copy now delegates to it.
- `Lookup::Hit` carries the current root tree next to the marker
  (`Hit { marker, tree }`); `run_material` and the client log use it, never
  `marker.tree` (on a scoped hit that is the tree of the earlier pass).
- `cache status` appends ` [scoped: <n> of <total> files, <12 hex>]` to a
  scoped gate's line; MCP `status` adds `scope: {files, total, hash}` per
  scoped gate; `run --explain` prints a `scope:` line.
- `sluice doctor` gains the `scoped-gates` check (warning only): well-known
  manifests and lockfiles at the root or in an ancestor directory of a
  selected path that are outside the scope and not in `key_files`, `paths`
  patterns that match no file, an empty scope, in-scope symlinks pointing
  outside the scope, and `paths` with `cache = false`.
- `scripts/bench-hit-path.sh` times an unscoped and a scoped gate
  interleaved, optionally against a baseline binary, and the scoped gate
  after an out-of-scope edit (results in
  [[sluice/changes/hit-path-latency.md]]).
- Markers, the protocol, `KEY_PREFIX` and `CACHE_FORMAT_VERSION` are
  unchanged; the post-run "the gate modified the tree" check still compares
  root trees.

## Files and symbols

- `crates/sluice-gate-core/src/scope.rs` (new): `PathScope` (`from_gate`,
  `new`, `matches`, `count`, `hash`, `unmatched_paths`), `ScopeHash`,
  `ScopeCount`, `SCOPE_DOMAIN`, `SCOPE_KEY_PREFIX`, `wildmatch` (a port of
  git's `wildmatch.c` with `WM_PATHNAME`) and `pattern_matches` (git's
  `match_pathspec_item`: literal prefix rule, then wildmatch after the
  literal part).
- `crates/sluice-gate-core/src/tree.rs`: `StagedEntries`, `StagedEntry`,
  `SnapshotFull`, `snapshot_entries`; `snapshot` is now a wrapper.
- `crates/sluice-gate-core/src/key.rs`: `KeyTree { Root, Scoped }`,
  `KeyMaterial.tree: KeyTree`.
- `crates/sluice-gate-core/src/config.rs`: `Gate.paths`,
  `Gate.paths_ignore`, `invalid_path_pattern`, `check_path_patterns`,
  `MAX_PATH_PATTERNS`.
- `crates/sluice-gate-core/src/lookup.rs`: `key_tree`, `derive_key_at`,
  `derive_key(r, KeyTree, extra)`, `ScopeSummary` (+ `status_suffix`),
  `scope_summary`, `Lookup::Hit { marker, tree }`, `run_material`.
- `crates/sluice-gate-queue/src/runner.rs`: step 5 uses
  `snapshot_entries`; `derive_key` delegates to `lookup::derive_key_at`.
- `crates/sluice-gate/src/{run.rs,cache.rs,doctor.rs}`,
  `crates/sluice-gate-mcp/src/lib.rs` (`GateCache.scope`, `cache_state`,
  `prepare`).
- Docs: `kb/sluice/design/{cache-key,config-format,committed-tree-lookup,
  merge-queue,mcp-tools,path-scoped-keys}.md`, `docs/gates-cookbook.md`
  (monorepo and Rust examples, "Choosing paths"), `README.md`.

## Deviations from the design text

- **Validation is slightly stricter**: besides the listed errors, a `.`
  component and an empty component (`a//b`) are rejected (git would
  normalise them, so the key would depend on spelling); one trailing `/` is
  allowed. A list longer than 64 reads `gates.<g>.paths: at most 64
  patterns are allowed`.
- **Differential oracle**: the design's
  `git --glob-pathspecs ls-files -- <paths> :(exclude)<ignore>` is not a
  valid oracle for combined sets. git strips the common literal prefix of
  the positive pathspecs from every item, exclusions included (git 2.53:
  `crates/a :(exclude)**/x/**` excludes nothing; `crates/sluice-gate
  :(exclude)**/src/**` excludes everything). The test asks git for the
  `paths` set and the `paths_ignore` set separately and subtracts, which
  checks every pattern against git while keeping the documented per-pattern
  semantics. All 80 single patterns (as `paths` and as `paths_ignore`) and
  83 combined sets match git on a 567-path corpus.
- `key_tree(gate, root, entries)` takes the root tree and the entries
  rather than a `&SnapshotFull` (an unknown snapshot has no key).
- `derive_key_at` takes the gate environment lazily (a closure), so the
  `git rev-parse --local-env-vars` spawn still happens only for gates with
  `key_inputs` or `key_files`.
- `sluice status --json` (the daemon report) is not per gate, so only MCP
  `status` and `cache status` show the scope.
- Open questions Q1–Q5 answered as recommended: no scope hash in markers,
  gitlinks anywhere keep the tree unknown, post-run check on root trees, no
  escapes or `:(exclude)`, no repository-level default scope.

## Verification

- Unscoped keys unchanged: `lookup::tests::unscoped_key_is_byte_identical_to_v0_2`
  pins a key computed with the 0.2 code before the change, and
  `tests/scoped.rs::markers_written_by_0_2_still_hit` hand-writes a marker
  with a key and tree printed by the 0.2 binary's `run --explain` and hits it.
- Tests added (33): `scope.rs` 8 (literal, wildcards, no recursion,
  ignore and config rule, count, hash sensitivity and order independence,
  golden vector, differential against git), `config.rs` 2, `key.rs` 1,
  `tree.rs` 1, `lookup.rs` 2 (case 18, golden), queue 3 (cases 24, 25 and
  26), CLI `tests/scoped.rs` 15 (cases 8–23), MCP 1 (case 22). Cases 1–7
  of the design's plan are the `scope.rs` tests.
- Benchmark: [[sluice/changes/hit-path-latency.md]] "Path-scoped keys":
  scoped median 23.0 / 26.4 ms at 5 000 files (target < 35 ms), scoped
  minus unscoped +0.8 / +1.0 ms, unscoped vs the 0.2.0 binary +1.1 / 0.0 ms
  (load 3.0–3.6).
- Dogfooding: `sluice run fmt`, `sluice run clippy`, `sluice run test` from
  the worktree root: `fmt PASS 0.4s`, `clippy PASS`, `test PASS 49.3s`
  (load average 6.8 during the test run).
