---
title: "Change: Phase 1, cache only (sluice run without a daemon)"
tags: [sluice, change, phase-1]
---

# Change: Phase 1, cache only (2026-10-22)

Implements [[sluice/plan/master-plan.md]] Phase 1 on top of
[[sluice/design/cache-key.md]], [[sluice/design/config-format.md]] and
[[sluice/design/architecture.md]] sections 4, 4b, 8 and 9. Decision added:
[[sluice/design/adr/0006-hook-flag.md]].

## What changed

### `sluice-gate-core` (library, synchronous)

| Module | Symbols | Notes |
|---|---|---|
| `config.rs` | `RepoConfig`, `Gate`, `Defaults`, `HookPolicy`, `UserConfig` and its sections, `starter_repo_config()` | strict parsing (`deny_unknown_fields`), humantime durations, validation rules of the config format. `human_hook_policy` rejects `fail-fast`. |
| `mode.rs` | `CacheMode`, `cache_mode()`, `is_truthy()` | `SLUICE_DISABLE` > `SLUICE_CACHE` > `CI`. |
| `git.rs` | `RepoPaths`, `discover()`, `probe_env()`, `child_env()`, `run()`, `version()`, `head_tree()`, `MIN_GIT_VERSION` | one `rev-parse --path-format=absolute` spawn for discovery; an older git (< 2.31) is reported as `git <v> is too old`; derives `GIT_WORK_TREE` from `GIT_DIR` in hooks for Sluice's own probes only; `child_env` strips `git rev-parse --local-env-vars` and `CLAUDE*`. `version()` feeds `--explain`; `head_tree()` is library API for later phases. |
| `tree.rs` | `TreeHash`, `Snapshot`, `scratch_root()`, `snapshot()`, `index_tree()` | private index copy (mtime preserved) + private object dir with the real objects as alternates; `core.splitIndex=false`; pre-check with one `ls-files -v -z` (unmerged, assume-unchanged, skip-worktree); gitlinks after `add -A` make the tree unknown; any git failure is `Unknown{reason}`, never an error. |
| `key.rs` | `CacheKey`, `KeyMaterial`, `derive()`, `normalize_cmd()`, `collect_inputs()` | exact byte layout of the design doc, golden-vector test; `Gate::normalized_cmd` delegates to `normalize_cmd`; a test pins `KEY_PREFIX` to `CACHE_FORMAT_VERSION`. |
| `marker.rs` | `MarkerStore`, `Marker`, `MissRecord`, `MarkerSummary`, `Lookup` | `<key>.json` and `<key>.miss.json` under `<common-dir>/sluice/markers`, atomic writes, TTL by mtime and `passed_at`, corrupt or foreign-format files are deleted on read. |
| `agent.rs` | `Caller`, `detect()` | ADR-0004 rules. |
| `semaphore.rs` | `Semaphore`, `SlotGuard` | `flock` on `slot-<n>.lock`, non-blocking tries with backoff. |
| `error.rs` | `Error`, `Result` | shared error type. |

### `sluice-gate` (binary `sluice`)

| File | What |
|---|---|
| `run.rs` | `sluice run <gate> [--hook <name>] [-v] [--explain] [-- extra]`: cache mode → resolve (user config skipped under `SLUICE_DISABLE`) → explain or bypass → snapshot → key → marker lookup → hit (one line, exit 0) or policy (only inside hooks and only when the cache missed; uncached runs are never refused) → weighted slots → run with log capture → marker or miss record. Timeout kills the whole process tree found in `/proc` (SIGTERM, 5 s, SIGKILL). The gate environment is computed lazily (one spawn saved on hits). `defaults.tail_lines` overrides `cache.tail_lines`; the failure tail skips the log framing lines. |
| `cache.rs` | `cache status [--all]` (per-gate HIT/MISS for the current tree, last miss reason) and `cache clear [--gate]`. |
| `logs.rs` | `logs [job-id] [-n N]` from `$XDG_STATE_HOME/sluice/logs`. |
| `dirs.rs` | state and logs directories, job ids `<unix-ms>-<pid>-<hex>`. |
| `snippets.rs`, `main.rs` | hook snippets now call `sluice run --hook <hook> <gate>`; `init` writes `starter_repo_config()`; `hook print` reads gates per hook from the parsed config. |

Exit codes of `sluice run`: 0 pass or hit; the gate's own code on failure;
3 refused by `fail-fast`; 124 timeout; 1 Sluice error; 143 killed by signal;
2 usage; 70 not implemented (`mcp`, `daemon`, `status`).

Reconciliation at phase close: [[sluice/changes/phase-1-reconciliation.md]]
(61 findings, all resolved in code or docs on 2026-10-22).

### Phase 1 semantics that differ from the final design (documented, intentional)

- Concurrency guard is the `flock` semaphore with `daemon.budget` slots; a
  gate takes `weight` slots all-or-nothing (no deadlock, no fairness
  guarantee). The daemon replaces it in Phase 2.
- No coalescing yet: two concurrent misses on the same tree both run.
- `committed_tree` is recorded in markers written from `--hook pre-commit`
  but not used for lookups ([[sluice/design/committed-tree-lookup.md]]).
- The gate shares the client's process group (Ctrl-C works for humans);
  timeout kills descendants through `/proc`.

## Why

Phase 1 is independently useful: hooks stop re-running suites on trees that
already passed, from any worktree, and agents in hooks fail fast instead of
hanging. Everything else (daemon, MCP) builds on the same core.

## Verification

```
cargo fmt --all --check                                  # ok
cargo clippy --workspace --all-targets -- -D warnings    # clean (pedantic on)
cargo test --workspace                                   # 85 core + 8 unit + 8 CLI + 18 acceptance, all green
scripts/bench-hit-path.sh 5000 40                        # see hit-path-latency
```

Acceptance evidence: [[sluice/changes/phase-1-acceptance.md]].
Latency: [[sluice/changes/hit-path-latency.md]].

## Follow-ups

- Resolve the `git` binary once per process (each spawn currently probes
  every `PATH` entry; measured cost is well under a millisecond).
- Parse the index file natively to drop the `ls-files -v` pre-check spawn
  (candidate for Phase 2 polishing if latency on huge repos matters).
- Secondary lookup by `committed_tree` (Phase 2).
