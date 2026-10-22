---
title: Cache key and markers
tags: [sluice, design, cache]
---

# Cache key and markers (draft)

Status: draft for Phase-0 review. Reference analysis in
[[sluice/research/references/gate-cache.md]].

## Tree hash

Goal: a hash that changes when any tracked or untracked non-ignored file
changes, without ever touching the real index.

Procedure, ported from gate-cache's `_private_snapshot` (see
[[sluice/research/references/gate-cache.md]] section 2):

1. One `git rev-parse --show-toplevel --git-dir --git-common-dir --git-path index --git-path objects`
   call (all values in one spawn). `--git-path index` honours an inherited
   `GIT_INDEX_FILE`, which is what makes partial commits key on the content
   being committed.
2. Create a scratch dir under the runtime dir (`$XDG_RUNTIME_DIR/sluice/snap-*`,
   mode `0700`; stale `snap-*` dirs older than an hour are swept). Copy the
   index into it **preserving mtime** (git's racily-clean detection depends
   on it). Missing index (fresh repo) means an empty index.
3. Run `git -c core.splitIndex=false add -A` from the worktree root with:
   - `GIT_INDEX_FILE=<scratch>/index`
   - `GIT_OBJECT_DIRECTORY=<scratch>/objects` (fresh, empty)
   - `GIT_ALTERNATE_OBJECT_DIRECTORIES=<real objects>[:inherited alternates]`

   New blobs land in the throwaway object dir, existing ones resolve through
   the alternate, so the real index, `index.lock` and `.git/objects` are
   never touched. `core.splitIndex` is forced off because split index would
   write a shared-index file into the real git dir.
4. `git -c core.splitIndex=false write-tree` with the same env returns the tree hash.
5. Remove the scratch dir.

Read-only probes (`rev-parse`, `status`, `ls-files`) always pass
`--no-optional-locks` so a cache check never blocks on `index.lock` held by
another agent.

**Unknown tree policy** (also from gate-cache): unmerged index, skip-worktree
or assume-unchanged entries, dirty or uninitialised submodules, case-only
mismatches on `core.ignorecase`, or any git error make the tree *unknown*.
An unknown tree never fails the gate: Sluice runs the gate uncached (still
queued in Phase 2) and prints the reason. A cache error must never change
the gate's exit code.

Cost: three git spawns plus an index copy. For a large repo this is tens of
milliseconds, dominated by `git add -A` stat calls. Measure and record in
[[sluice/changes/hit-path-latency.md]]. gate-cache publishes no numbers; its
Python start-up plus about ten subprocesses is the baseline to beat.

### Inside hooks

Git exports variables to hook processes; Sluice reads them explicitly:

| Hook | Relevant env | Content to hash |
|---|---|---|
| `pre-commit` | `GIT_INDEX_FILE` points at the index being committed (or a temp index for `git commit -a` / partial commits) | Same procedure as outside hooks: copy the index git gave us, then `add -A` on top. The result is the **working tree** content, which is what the gate command actually sees. Hook managers that stash unstaged changes (pre-commit/prek, lefthook with `stage_fixed`) make it equal to the committed tree; Husky and plain hooks do not. The marker additionally records `committed_tree` (`write-tree` from the copied hook index without `add -A`, one extra spawn only inside `pre-commit`). In Phase 1 it is recorded but not used for lookups; a secondary lookup by committed tree (so that a `pre-push` after a commit made with unstaged changes can hit) is planned for Phase 2 and tracked in [[sluice/design/committed-tree-lookup.md]]. When the working tree was clean apart from the staged changes, the working-tree hash equals the committed tree and the normal lookup already hits at push time. Confirmed as the draft answer to Q12 in [[sluice/questions/open-questions.md]]. |
| `pre-push` | `GIT_DIR`, stdin lists `<local ref> <local sha> <remote ref> <remote sha>` | For each pushed commit range, the tree is `<local sha>^{tree}` only if the worktree is clean; otherwise fall back to the working-tree hash. Both are computed cheaply. |
| `commit-msg`, `post-*` | same as pre-commit | Not a target in v1. |

When `GIT_DIR` is set without `GIT_WORK_TREE` (plain hooks), git treats the
cwd as the worktree top; derive the real worktree (parent of `.git`, or the
parent of the `gitdir` file for linked worktrees) and inject
`GIT_WORK_TREE` **only into Sluice's own probes**.

The gate command itself receives the caller's environment with every
`GIT_*` variable listed by `git rev-parse --local-env-vars` removed
(`GIT_INDEX_FILE`, `GIT_DIR`, `GIT_WORK_TREE`, `GIT_PREFIX`, object-dir
vars, …). Otherwise tools that call git inside the gate operate on the
wrong index or worktree, a pitfall both gate-cache and mergetrain document.
Note the difference from gate-cache, which leaves the child env untouched:
Sluice runs gates from a daemon, so it must rebuild the env anyway.

## Key

```
key = sha256(
  "sluice-cache-v1" \0
  tree_hash          \0
  gate_name          \0
  normalised_cmd     \0
  sorted(env k=v)    \0
  for each key_input: cmd \0 stdout_sha256 \0
  for each key_file:  path \0 content_sha256_or_"absent" \0
)
```

Hex-encoded, 64 chars. The marker filename is the key.

What is deliberately **not** in the key:

- The repository path or worktree path (so worktrees share hits).
- The branch or commit id (trees matter, not history).
- The timeout, weight, hooks list (they do not affect the outcome).

## Markers

Location: `<git-common-dir>/sluice/markers/<key>.json`. All worktrees of a
repository share the common dir, so they share markers.

```json
{
  "format": 1,
  "key": "…",
  "tree": "…",
  "gate": "test",
  "cmd": "npm test -- --maxWorkers=2",
  "passed_at": "2026-10-22T13:05:11Z",
  "duration_ms": 32100,
  "exit_code": 0,
  "job_id": "01J…",
  "sluice_version": "0.1.0",
  "committed_tree": "…"   
}
```

Rules:

- Written atomically (temp file + rename) only by the process that ran the
  gate (daemon in Phase 2, the CLI in Phase 1).
- Only exit code 0 writes a marker.
- A run that exits 0 but **changed the tree** (tree hash recomputed after the
  run differs) does not write a marker: auto-fixers must be re-verified on
  the new tree.
- TTL: a marker older than `cache.ttl_days` is ignored and removed lazily
  (pruning on write only, mtime-based, errors swallowed).
- `sluice cache clear [--gate <name>]` removes markers for the current repo.
- `sluice cache status` lists markers for the current tree with age.
- **Miss records**: a failed run or a tree-changing pass writes
  `<key>.miss.json` with `exit_code`, `reason`, `job_id`, `log_path` and a
  timestamp. A later pass deletes it. `sluice cache status` and the MCP
  `status` tool report it, which is exactly what an agent asks ("why did
  this fail last time?"). Interrupted runs write nothing.
- The key derivation and the marker path are a **public contract**
  documented in the README, so shell snippets can check a marker without
  Sluice if they ever need to.
- The daemon writes markers with its job id and daemon instance id; a
  marker from an unknown daemon instance is still valid (markers are facts
  about trees, not about processes).

## Disabling

- `CI` set and `SLUICE_CACHE != 1`: no reads, no writes.
- `SLUICE_CACHE=0`: no reads, no writes.
- `SLUICE_DISABLE=1`: Sluice runs the gate directly and exits with its code.

## Known limitations (document prominently in the README)

- Inputs git does not see (tool versions, env vars, ignored files, services)
  must be declared via `key_inputs` / `key_files`, otherwise stale passes are
  possible.
- A cache hit is a local optimisation. CI stays authoritative.

Related: [[sluice/design/architecture.md]], [[sluice/design/config-format.md]].
