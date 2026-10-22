---
title: Cache key and markers
tags: [sluice, design, cache]
---

# Cache key and markers (draft)

Status: implemented in Phase 1 (`sluice-gate-core::tree`, `key`, `marker`),
see [[sluice/changes/phase-1-cache-only.md]]. Reference analysis in
[[sluice/research/references/gate-cache.md]].

## Tree hash

Goal: a hash that changes when any tracked or untracked non-ignored file
changes, without ever touching the real index.

Procedure, ported from gate-cache's `_private_snapshot` (see
[[sluice/research/references/gate-cache.md]] section 2):

1. One `git rev-parse --path-format=absolute --show-toplevel --git-dir --git-common-dir --git-path index --git-path objects`
   call (all values in one spawn; needs git ≥ 2.31). `--git-path index`
   honours an inherited `GIT_INDEX_FILE`, which is what makes partial
   commits key on the content being committed.
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
4. `git ls-files -s -z` on the private index: any gitlink (mode `160000`)
   makes the tree unknown (submodules are not supported in v1).
5. `git -c core.splitIndex=false write-tree` with the same env returns the tree hash.
6. Remove the scratch dir (also on error).

Before step 2 a single read-only `git ls-files -v -z` (with
`--no-optional-locks`, against the index git handed us) rejects unmerged
entries, assume-unchanged (lowercase tag) and skip-worktree (`S`) entries
as unknown. Five git spawns in total on the hit path; the measured cost is
in [[sluice/changes/hit-path-latency.md]]. The reasons printed are
`unmerged index: <path>`, `skip-worktree or assume-unchanged entry: <path>`,
`submodules are not supported yet: <path>`, `could not stage working copy: …`
and `could not write tree: …`.

Read-only probes (`rev-parse`, `ls-files`, `config --get core.worktree`)
always pass `--no-optional-locks` so a cache check never blocks on
`index.lock` held by another agent. `git rev-parse --local-env-vars` is
spawned only when a gate actually runs (or has `key_inputs`), to build the
sanitised gate environment.

**Unknown tree policy** (also from gate-cache): unmerged index, skip-worktree
or assume-unchanged entries, any gitlink in the index, or any git error make
the tree *unknown*. (Case-only mismatches on `core.ignorecase` are not
checked: Linux only, case-sensitive filesystems assumed.)
An unknown tree never fails the gate: Sluice runs the gate uncached (still
queued in Phase 2) and prints the reason. The same holds for every cache
error (`cannot snapshot the tree`, `cannot derive the cache key`,
`cannot read the cache`): a notice on stderr, then an uncached run. A
cache error must never change the gate's exit code. Because an uncached
run can never be cached, the agent `fail-fast` policy does not apply in
these cases either (nor under `CI` / `SLUICE_CACHE=0` / `cache = false`):
the gate simply runs.

Cost: five git spawns plus an index copy; `git add -A` stat calls dominate
on large repositories. Measured in [[sluice/changes/hit-path-latency.md]]
(median 29 ms on 5 000 files). gate-cache publishes no numbers; its Python
start-up plus about ten subprocesses is the baseline to beat.

### Inside hooks

Git exports variables to hook processes; Sluice reads them explicitly:

| Hook | Relevant env | Content to hash |
|---|---|---|
| `pre-commit` | `GIT_INDEX_FILE` points at the index being committed (or a temp index for `git commit -a` / partial commits) | Same procedure as outside hooks: copy the index git gave us, then `add -A` on top. The result is the **working tree** content, which is what the gate command actually sees. Hook managers that stash unstaged changes (pre-commit/prek, lefthook with `stage_fixed`) make it equal to the committed tree; Husky and plain hooks do not. The marker additionally records `committed_tree` (`write-tree` from the copied hook index without `add -A`, one extra spawn only inside `pre-commit`). It is recorded but never used for an automatic hit: the decision in [[sluice/design/committed-tree-lookup.md]] is to keep it as evidence only (it is stored in the marker; nothing reads it back). When the working tree was clean apart from the staged changes, the working-tree hash equals the committed tree and the normal lookup already hits at push time. Confirmed as the draft answer to Q12 in [[sluice/questions/open-questions.md]]. |
| `pre-push` | `GIT_DIR` (stdin lists the pushed refs; Sluice does not read it) | Working-tree hash, like everywhere else. On a clean checkout that equals the pushed commit's tree, so a pass recorded at commit time hits. |
| other hooks | whatever git exports | Same working-tree hash; the hook policy applies to any `--hook` value (or whenever `GIT_DIR`/`GIT_INDEX_FILE` is present). Only `pre-commit` triggers the `committed_tree` record. |

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

**Path-scoped gates** (`paths` / `paths_ignore`, Sluice >= 0.3.0,
[[sluice/design/path-scoped-keys.md]], ADR-0012): the `tree_hash` field
becomes `scope-v1:<scope hash>`; everything else is unchanged, and so is
`"sluice-cache-v1"`. The scope hash is computed in process (no extra git
spawn) from the `ls-files -s -z` output of the private index after
`add -A` (step 4 above):

```
scope = sha256(
  "sluice-scope-v1"                                      \0
  for each paths pattern (config order):  "p:" pattern  \0
  for each paths_ignore pattern:          "i:" pattern  \0
                                                         \0
  ".sluice.toml " mode " " oid   or   ".sluice.toml absent" \0
  for each selected entry other than .sluice.toml, in path byte order:
    mode " " oid "\t" path                               \0
)
```

`mode` is the octal mode and `oid` the hex blob id as git prints them;
paths are raw bytes. Unscoped gates keep the root tree id, so their keys
are byte-identical to 0.2 and every existing marker still hits (golden
tests `lookup::tests::unscoped_key_is_byte_identical_to_v0_2` and
`tests/scoped.rs::markers_written_by_0_2_still_hit`). The `:` cannot occur
in a tree id, so scoped and unscoped fields never collide. Implementation:
`sluice-gate-core::scope` (`PathScope`, `ScopeHash`), `key::KeyTree`,
`lookup::key_tree` and `lookup::derive_key_at` (the one derivation shared
by `sluice run`, `cache status`, MCP `verify`/`status` and the merge
queue). Markers do not change: `tree` stays the root working tree the gate
passed on, and the post-run "the gate modified the tree" check compares
root trees.

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
  "job_id": "1761138311123-4242-0a1b2c",
  "sluice_version": "0.1.0",
  "committed_tree": "…"   
}
```

Rules:

- Written atomically (temp file + rename) only by the process that ran the
  gate (daemon in Phase 2, the CLI in Phase 1). A marker whose `format`
  differs from `CACHE_FORMAT_VERSION`, whose JSON is corrupt, or whose inner
  `key` does not match its file name is deleted on read and treated as a miss.
- Only exit code 0 writes a marker.
- A run that exits 0 but **changed the tree** (tree hash recomputed after the
  run differs) does not write a marker: auto-fixers must be re-verified on
  the new tree.
- TTL: a marker older than `cache.ttl_days` (by file mtime or `passed_at`,
  whichever is older) is deleted when it is read and treated as a miss;
  `prune` (called after every marker write) additionally removes markers
  older than the TTL by mtime. Miss records never expire on their own.
- `sluice cache clear [--gate <name>]` removes markers **and miss records**
  of the current repository (optionally one gate) and prints the count.
- `sluice cache status [--all]` prints the markers directory, the current
  tree, and one line per configured gate: `HIT (passed <age> ago, job <id>)`,
  `MISS (last run <age> ago: <reason>, exit <code>)`, `MISS`,
  `always runs (cache = false)` or `ERROR (<why>)`; a path-scoped gate's
  line gets ` [scoped: <n> of <total> files, <12 hex of the scope hash>]`
  (MCP `status`: `scope: {files, total, hash}` per gate). It runs the gates'
  `key_inputs` to derive their keys. `--all` appends every stored marker
  (gate, age, tree and key prefixes).
- **Miss records**: a failed run, a timeout or a tree-changing pass writes
  `<key>.miss.json`:

  ```json
  {"format": 1, "key": "…", "gate": "test", "exit_code": 1,
   "reason": "gate failed", "job_id": "…", "log_path": "/…/<job>.log",
   "recorded_at": "2026-10-22T13:05:11Z"}
  ```

  `reason` is `gate failed`, `timed out` or `the gate modified the tree`
  (that one with `exit_code: 0`); `exit_code` is `null` when the gate died
  from a signal. Miss records are written only when caching was active and
  the tree known. A later pass of the same key deletes the record. `sluice cache status` and the MCP
  `status` tool report it, which is exactly what an agent asks ("why did
  this fail last time?"). Interrupted runs write nothing.
- The key derivation (byte layout above), the marker path
  `<common-dir>/sluice/markers/<key>.json` and the marker JSON are a
  **public contract**: this document is referenced from the README so
  shell snippets can check a marker without Sluice if they ever need to.
  Markers are pretty-printed JSON with a trailing newline, written via
  `.tmp-<pid>-<n>` files, `fsync` and rename; the directory is `0700`.
  A marker with unknown fields is treated as corrupt (strict schema).
- The daemon writes markers with its job id and daemon instance id; a
  marker from an unknown daemon instance is still valid (markers are facts
  about trees, not about processes).

## Disabling

- `CI` set and `SLUICE_CACHE != 1`: no reads, no writes.
- `SLUICE_CACHE=0`: no reads, no writes.
- `SLUICE_DISABLE=1`: Sluice still needs the repository and `.sluice.toml`
  to know the gate command, but reads no user config, no marker, takes no
  slot and writes no log: it runs `sh -c '<cmd> "$@"'` with inherited stdio
  and the sanitised environment, and exits with the gate's code (143 after
  a signal).
- Truthiness: `1`, `true`, `yes`, `on` (case-insensitive) are true; `0`,
  `false`, `no`, `off` are false; `CI` counts as set unless it is false.

## Known limitations (document prominently in the README)

- Inputs git does not see (tool versions, env vars, ignored files, services)
  must be declared via `key_inputs` / `key_files`, otherwise stale passes are
  possible.
- A cache hit is a local optimisation. CI stays authoritative.

- A path-scoped gate that reads files outside its scope can pass stale;
  scoping is opt-in, and `sluice doctor` (`scoped-gates`) warns about
  well-known manifests and lockfiles left outside a scope.

The merge queue derives the keys of its gates with this same code (`lookup::key_tree` and `lookup::derive_key_at`) from the snapshot of the clean merge worktree at the tree `T` of the combined commit, so a pass on that exact tree, or for a scoped gate on any tree with the same scope, hit or written by any client counts ([[sluice/design/merge-queue.md]] section 5).

Related: [[sluice/design/architecture.md]], [[sluice/design/config-format.md]].
