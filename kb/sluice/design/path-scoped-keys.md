---
title: Path-scoped cache keys
tags: [sluice, design, cache, alpha, d0]
---

# Path-scoped cache keys (D0)

Status: implemented in D1 (2026-10-23; ADR-0012). Implementation notes and
the deviations from this text: [[sluice/changes/path-scoped-keys.md]]. Builds on [[sluice/design/cache-key.md]],
[[sluice/design/committed-tree-lookup.md]], [[sluice/design/config-format.md]]
and [[sluice/design/merge-queue.md]] section 5. Latency baseline:
[[sluice/changes/hit-path-latency.md]].

## 1. Problem

Today every gate keys on the root tree of the working copy, so a README edit
invalidates `cargo test`. Monorepos and doc-heavy repositories rerun gates
whose inputs did not change. Path scoping lets a gate declare which part of
the tree it depends on; the key then hashes only that part.

## 2. Decisions (summary)

| # | Decision |
|---|---|
| 1 | Two new optional gate fields, `paths` and `paths_ignore` (lists of strings). Both absent = unscoped gate, key bytes identical to v0.2. `version = 1` stays. |
| 2 | Pattern semantics = git `:(glob)` pathspec semantics (as with `git --glob-pathspecs`), root-relative. No magic pathspecs (`:(exclude)`, `:!`, `:/`, `:(icase)`, `:(attr)`, `:(top)`, `:(literal)`): rejected at parse. Exclusion is `paths_ignore`. |
| 3 | The filtered hash is computed **in process** from the `ls-files -s -z` output the snapshot already produces (spawn 4, the gitlink check). Zero extra git spawns. |
| 4 | The scoped set always contains `.sluice.toml` (its index entry, or the literal `absent`), regardless of `paths_ignore`. `key_inputs`, `key_files`, `env`, gate name and normalised command stay in the key exactly as today. |
| 5 | In the key, the tree field becomes `scope-v1:<sha256 hex>` for scoped gates. `KEY_PREFIX` stays `sluice-cache-v1`, `CACHE_FORMAT_VERSION` stays 1. |
| 6 | Markers do not change. `tree` keeps meaning "root working tree the gate passed on". The scope hash is not stored (derivable). No protocol change. |
| 7 | The post-run "the gate modified the tree" check keeps comparing **root** trees. |
| 8 | The queue derives scoped keys from the same snapshot entries of the merge worktree through one shared core function. |
| 9 | Mitigation of stale passes: documentation plus a `sluice doctor` heuristic (well-known manifests and lockfiles outside the scope), and scope details in `cache status`. No warning "paths without key_files" (noisy proxy). |

## 3. Configuration

```toml
[gates.test]
cmd = "cargo test --workspace"
weight = 8
hooks = ["pre-push"]
paths = ["crates/**", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml"]
paths_ignore = ["crates/**/*.md"]

[gates.docs]
cmd = "mdbook build docs"
paths = ["docs"]                 # literal: the docs/ directory, recursively

[gates.lint-web]
cmd = "pnpm -C web lint"
paths_ignore = ["crates", "docs/**/*.png"]   # everything except these
```

Field reference (rows to add to config-format.md):

| Field | Type | Default | Notes |
|---|---|---|---|
| `gates.<name>.paths` | list of patterns | unset | When set, the cache key hashes only matching entries of the tree (plus `.sluice.toml`). Must not be empty. |
| `gates.<name>.paths_ignore` | list of patterns | `[]` | Entries matching any of these are removed from the scope. Alone, it scopes "everything except". |

Matching (fixed; verified against git 2.53 with `git --glob-pathspecs ls-files`):

- Patterns are relative to the repository root, whatever the cwd.
- A pattern **without** `*`, `?` or `[` is a literal: it matches the path
  itself and, as a directory, everything under it (`docs` and `docs/` both
  match `docs/a/b.md`; `doc` does not match `docs/x`).
- A pattern **with** wildcards matches the full path only, with
  `WM_PATHNAME` wildmatch rules: `*` and `?` never cross `/`; `[...]` is a
  class; `**/` matches zero or more leading directories; `/**` at the end
  matches everything inside; `a/**/b` matches zero or more middle
  directories. A wildcard pattern does **not** match a directory
  recursively: `src/1*` matches no file under `src/12/`; write `src/1*/**`.
- Matching is byte-wise and case-sensitive (Linux only).
- Selected = (`paths` unset or any `paths` pattern matches) and no
  `paths_ignore` pattern matches; then `.sluice.toml` is added.

Validation (errors at parse, message `invalid configuration in <path>:
gates.<name>.paths[<i>]: <reason>`):

- `paths = []` → `must not be empty (remove the field to use the whole tree)`.
- Empty pattern; leading `/`; leading `./`; any `..` component; leading `:`
  (`magic pathspecs are not supported; use paths_ignore`); leading `!`
  (`negation is not supported; use paths_ignore`); a backslash (no escapes
  in v1); NUL or control characters; more than 64 patterns per list.
- `paths`/`paths_ignore` with `cache = false` is accepted and has no effect
  (`doctor` notes it).

Old binaries reject the new fields (strict parsing: unknown field
`paths`). This is the accepted cost of the frozen-but-additive policy, as
for `cache`; the README states "requires Sluice ≥ 0.3.0".

## 4. Computing the scoped hash

### Options considered

Measured on a 5 000-file repository (git 2.53, load average 15 on 20 CPUs):

| Option | Extra spawns | Measured cost | Verdict |
|---|---|---|---|
| `git ls-tree -r <tree> -- <specs>` | 1 | 4.4 ms (`-r`, no filter) | **Rejected**: `ls-tree` ignores wildcards (`src/*.rs` matches nothing; verified), so it cannot implement the syntax. |
| `git write-tree --prefix=<dir>` | 1 per directory | ~1.5 ms each | Rejected: directories only, no globs, no exclusions; cost grows with patterns. |
| Filtered private index (`read-tree` + `update-index`) + `write-tree` | 2+ | > 5 ms | Rejected: slowest, more scratch I/O. |
| `git --glob-pathspecs ls-files -s -z -- <specs> :(exclude)…` on the private index | 1 per distinct scope | 2.4–3.2 ms | Fallback only (see below). |
| **In-process filter over spawn-4 output** | 0 | ~0.3 ms (output 323 KB; SHA-256 of all of it 0.2 ms) | **Chosen.** |

The snapshot already runs `ls-files -s -z` on the private index after
`add -A` to reject gitlinks; that output is exactly the staged content
(`<mode> SP <oid> SP <stage> TAB <path> NUL`, stage always 0 because
unmerged indexes are rejected earlier). Filtering and hashing it in process
adds no spawn and makes the key independent of the installed git's pathspec
code. Fidelity to git is enforced by a differential test (section 9). If
D1 finds a divergence it cannot fix, it switches to the fallback spawn and
re-measures; the budget still holds (29 + 3 ≈ 32 ms).

### Algorithm

```
entries = parse(ls_files_s_z_output)          # (mode, oid, path) per record
assert entries sorted by path bytes           # index order; sort if not (cheap)
h = sha256()
field("sluice-scope-v1")
for p in paths (config order):        field("p:" + p)
for p in paths_ignore (config order): field("i:" + p)
field("")                                       # separator
cfg = entry for ".sluice.toml" or None
field(".sluice.toml " + (cfg ? mode + " " + oid : "absent"))
for e in entries where selected(e.path) and e.path != ".sluice.toml":
    field(e.mode + " " + e.oid + "\t" + e.path)   # path as raw bytes
scope_hash = hex(h)                              # 64 chars
```

`field(x)` = bytes of `x` followed by `\0`, as in `key::derive`. Patterns
are hashed as written (any edit to them is a config change anyway, which
the `.sluice.toml` entry already captures; hashing them too keeps the hash
self-describing). The oid width follows the repository's object format
(SHA-1 or SHA-256); it is hashed as text.

### Determinism

- **Order**: index order (byte order of full paths), identical for the hook,
  `verify`, `cache status` and the queue because all of them parse the same
  `ls-files -s` of a private index. D1 asserts the order and sorts if
  violated, so the hash never depends on it.
- **Modes**: the octal mode is hashed, so `chmod +x` on an in-scope file
  misses. `100644`, `100755`, `120000` are the possible values.
- **Symlinks** (`120000`): the oid is the blob of the link target string; the
  target's content is not followed. A link inside the scope pointing outside
  it does not invalidate on target changes: put the target in `paths`.
- **Submodules**: a gitlink anywhere still makes the whole tree unknown
  (unchanged policy, even outside the scope). See open question Q2.
- **Renames across the boundary**: paths are hashed, so moving a file into
  or out of the scope changes the selected set and misses; renames entirely
  outside the scope hit; renames inside it miss.
- **Untracked, non-ignored files** are in the private index after `add -A`
  and therefore in scope exactly like tracked ones. Ignored files are not;
  that is what `key_files` is for.
- **Empty scope**: the hash covers only the header and `.sluice.toml`; the
  gate hits until the config changes. `cache status` shows `0 files` and
  `doctor` warns.
- **Config changes**: any byte change of `.sluice.toml` (any gate, any
  table) changes its blob oid and misses every scoped gate, as it already
  does for unscoped ones through the root tree. If `.sluice.toml` is
  gitignored, `absent` is hashed and the key still captures `cmd`, `env`,
  `key_inputs`, `key_files` and the patterns.

## 5. Key and code layout

`KeyMaterial.tree: TreeHash` becomes `KeyMaterial.tree: KeyTree`:

```rust
pub enum KeyTree { Root(TreeHash), Scoped(ScopeHash) }
// key field: Root -> the 40/64-hex tree id (unchanged bytes)
//            Scoped -> "scope-v1:" + 64 hex
```

The `:` makes collision with any tree id impossible, and `Root` keeps the
v0.2 byte layout, so **every existing marker of an unscoped gate still
hits** (golden test). New code (names indicative):

- `tree.rs`: `snapshot_entries(...) -> Result<SnapshotFull>` returning
  `Known { root: TreeHash, entries: StagedEntries }` (raw spawn-4 bytes plus
  record offsets, parsed lazily); `snapshot()` becomes a wrapper so other
  callers do not change.
- `scope.rs` (new, pure): `PathScope::from_gate(&Gate) -> Option<PathScope>`,
  `matches(&[u8]) -> bool` (wildmatch with `WM_PATHNAME` + literal prefix
  rule), `hash(&StagedEntries) -> ScopeHash`, `count(&StagedEntries)`.
- `config.rs`: the two fields (`Option<Vec<String>>`, `Vec<String>`),
  validation of section 3.
- `lookup.rs`: one function used by **every** front-end and the queue:
  `key_tree(gate, &SnapshotFull) -> KeyTree` and
  `derive_key_at(gate, name, key_tree, extra, root_dir, child_env)`;
  `derive_key` and the runner's private `derive_key` both call it (the
  runner's copy is deleted).
- `Lookup::Hit` carries the current root tree next to the marker
  (`Hit { marker, tree }`); `run_material` uses that tree, never
  `marker.tree`, which on a scoped hit is the tree of the earlier pass.

## 6. Markers, protocol and compatibility

- **Marker JSON unchanged.** `tree` = root working tree the gate passed on
  (needed by the post-run check and the committed-tree evidence). The
  scope hash is not written: it is a pure function of that tree and the
  config committed in it. No new field means no `deny_unknown_fields`
  risk in either direction.
- **New binary, old markers**: unscoped keys are byte-identical, so they
  hit. A gate that gains `paths` misses once (different key), then hits.
- **Old binary, new markers**: a scoped marker is an ordinary format-1
  marker under a key the old binary never derives (it cannot even parse a
  config with `paths`); `list`, `prune`, `clear` treat it like any marker.
- **Protocol**: unchanged (protocol 1). The client sends the scoped key and
  the root tree; an old daemon writes a correct marker from them, and the
  daemon never derives keys itself.
- **Post-run check**: root tree before vs after (decision 7). A scoped gate
  that rewrites an out-of-scope tracked file still records a miss (`the
  gate modified the tree`): conservative, same as today. See Q3.

## 7. Hooks, `cache status`, queue

**pre-commit.** The key is derived from the working-tree snapshot as today.
`committed_tree` stays the root tree of the hook index (unchanged extra
spawn). Scoping helps exactly the case the committed-tree design left open:
if the unstaged changes lie outside the gate's scope, the working-tree
scope hash equals the committed one and `pre-push` hits on its own. The
`cache status` note ("passed with unstaged changes present") keeps
comparing root trees and still applies to scoped gates whose unstaged
changes were inside the scope.

**`cache status` / MCP `status`.** One snapshot, then per gate a scope hash
from the same entries (no extra spawn per gate). Text: the line gets
` [scoped: <n> of <total> files, <12 hex>]`; JSON (MCP `status`, `status
--json`) gets optional fields `scope: {files, total, hash}`. `--all` is
unchanged (markers carry no scope).

**Queue (merge-queue.md section 5, step 5).** The runner already takes
`tree::snapshot` of the clean merge worktree and fails with `tree_mismatch`
if it differs from `T`. It switches to `snapshot_entries`, checks
`root == T`, and derives each gate's key with the shared
`lookup::key_tree` + `derive_key_at` (base config's gate, `key_inputs`
run and `key_files` read in the merge worktree, no extra args). Because the
entries come from a private index of a clean checkout of `T`, an agent's
`verify` on a clean worktree at `T` derives the same key and the markers
interoperate. If a member branch edits `.sluice.toml`, the base config's
patterns and `T`'s config blob both enter the key; the agent's key uses
`T`'s patterns, so the keys can differ and the queue runs the gate:
conservative and identical to how a `cmd` change behaves today.

## 8. Failure modes and mitigation

| Failure | Effect | Mitigation |
|---|---|---|
| Gate reads files outside its scope (root manifest, lockfile, shared config, fixtures, generated code, `build.rs` inputs) | **Stale pass** | Opt-in only; README and cookbook section "Choosing paths" with per-ecosystem lists; `doctor` heuristic below; recommend adding such files to `paths` (tracked) or `key_files` (ignored). |
| Symlink in scope pointing outside | Stale pass on target edits | Documented; `doctor` lists in-scope symlinks whose target is outside the scope. |
| Pattern typo (`src/1*` expecting recursion), empty match | Too small scope → stale pass | `cache status` shows the file count; `doctor` warns `gate '<g>': paths[<i>] matches no file`. |
| Toolchain/env not in tree | Same as today | `key_inputs`. Unchanged. |
| Old binary in another worktree | `invalid configuration` | Documented minimum version. |

`doctor` check `scoped-gates` (warning, never a failure): for each scoped
gate, on the current snapshot, list tracked files that exist at the root or
in an ancestor directory of a matched path and are well-known build inputs
(`Cargo.toml`, `Cargo.lock`, `rust-toolchain*`, `package.json`,
`package-lock.json`, `pnpm-lock.yaml`, `pnpm-workspace.yaml`, `yarn.lock`,
`tsconfig*.json`, `go.mod`, `go.sum`, `go.work`, `pyproject.toml`,
`uv.lock`, `poetry.lock`, `requirements*.txt`, `setup.cfg`, `Makefile`,
`Dockerfile`, `.nvmrc`, `.tool-versions`) but are not selected and not in
`key_files`. Message: `gate 'test' is scoped by paths but Cargo.lock is
outside its scope; edits to it will not invalidate the cache`.

A blanket "paths without key_files" warning is rejected: most correct scoped
gates need no `key_files`; it would train users to ignore warnings.

## 9. Test plan for D1

Unit (`scope.rs`, pure):

1. Literal: `docs` and `docs/` match `docs/a.md`, `docs/x/y.md`; not
   `docs2/a.md`, not `doc`.
2. Wildcards: `src/*.rs` matches `src/x.rs` not `src/a/y.rs`; `**/*.rs`
   matches `top.rs` and `src/a/y.rs`; `src/**` everything under `src`;
   `a/**/b` with zero and two middle dirs; `?` and `[0-1]`.
3. Wildcard does not recurse: `src/1*` matches nothing under `src/12/`.
4. `paths_ignore` alone, `paths` + `paths_ignore`, ignore never removes
   `.sluice.toml`.
5. Differential: a generated corpus (≥ 500 paths, unicode, spaces, dots,
   deep nesting) × ≥ 60 patterns; selection equals
   `git --glob-pathspecs ls-files -z -- <paths> :(exclude)<ignore>…`
   for every pattern set.
6. Hash: order independence (shuffled input sorts to same hash), mode
   change, oid change, path change, pattern change all change the hash;
   `absent` config differs from present.
7. Golden vectors: fixed entries + scope → fixed hex; fixed unscoped
   material → the v0.2 key bytes (proves old markers still hit).

Config: each validation error of section 3; both fields absent parse to
unscoped; `cache = false` with `paths` accepted.

Integration (`sluice-gate` CLI, real repos in tempdirs):

8. Doc-only change hits a `paths = ["src/**"]` gate.
9. A change under `src/` misses; reverting it hits again.
10. Untracked non-ignored new file in scope misses; out of scope hits.
11. `.sluice.toml` change (another gate's `cmd`, a comment) misses the
    scoped gate.
12. `key_inputs` output change and `key_files` content change miss.
13. `paths_ignore`: edit an ignored in-scope file hits.
14. Rename `docs/a.rs` → `src/a.rs` misses; `src/a.rs` → `docs/a.rs`
    misses; rename within `docs/` hits.
15. `chmod +x` in scope misses; symlink retarget in scope misses.
16. Old markers: write a marker with a v0.2-built key fixture for an
    unscoped gate; the new binary hits it.
17. Marker of a scoped pass: `tree` equals the root tree at pass time, file
    parses with the v0.2 `Marker` struct (strict schema).
18. Scoped hit after an out-of-scope edit: the daemon request (or
    `verify` result) reports the current root tree, not `marker.tree`.
19. pre-commit with unstaged changes outside scope: `pre-push` on the
    committed checkout hits; with unstaged changes inside scope it misses
    and `cache status` shows the evidence note.
20. Gate that modifies an out-of-scope tracked file: miss record `the gate
    modified the tree`.
21. Gitlink outside the scope: tree unknown, gate runs uncached.
22. `cache status` text and MCP `status` JSON show `scope.files/total`.
23. `doctor` warns for `Cargo.lock` outside the scope and for a 0-file
    scope; no warning when it is in `key_files`.

Queue (`sluice-gate-queue`):

24. Single-member train fast-forwarded to an agent's verified tree: scoped
    gate hits, no execution.
25. Two-member train where the second member only touches out-of-scope
    files after the first passed on its own combined tree: hit.
26. Agent key and runner key equal for the same `T` (direct unit test of
    the shared function with both call paths).

## 10. Benchmark D1 must run

Extend `scripts/bench-hit-path.sh` (same synthetic repo, 5 000 files in
`src/<n>/`) with a second gate:

```toml
[gates.scoped]
cmd = "true"
paths = ["src/1*/**", ".sluice.toml"]
paths_ignore = ["src/12/**"]
```

Procedure: populate both gates; then time `sluice run noop` and `sluice
run scoped` **interleaved** (ABAB, 40 iterations each) so machine load hits
both equally; then edit a file outside the scope (`src/3/f300.txt`) and
time 40 scoped hits again (the case that motivates the feature). If
`SLUICE_BASELINE=<v0.2 binary>` is set, interleave it as well for the
unscoped gate. Print min/median/p90/max, load average, git version; also
run once with 20 000 files (informational). Record the results in
`kb/sluice/changes/hit-path-latency.md`.

Acceptance: scoped median < 35 ms at 5 000 files; scoped minus unscoped
median ≤ 2 ms; unscoped median within noise (≤ 1 ms) of the baseline
binary. Estimate: +0.3–1 ms (parse 5 000 records, wildmatch, SHA-256 of
< 330 KB, no spawn), i.e. about 30 ms against the 29.1 ms baseline.

## 11. Open questions (with recommended answers)

- **Q1. Store the scope hash in markers?** Recommended: no for alpha (keeps
  the frozen marker schema readable by v0.2); revisit as an optional
  `scope` field only if debugging needs it.
- **Q2. Tolerate gitlinks outside a scoped gate's paths?** Recommended: no
  in alpha (one policy, submodules unsupported); cheap to add later because
  entries are already filtered in process.
- **Q3. Post-run check on the scope hash instead of the root tree?**
  Recommended: no; it would need the patterns in the protocol (optional
  field) and only helps when something edits out-of-scope files during a
  run. Revisit with measurement of "modified the tree" misses.
- **Q4. Allow `\` escapes and `:(exclude)` for familiarity?** Recommended:
  no; `paths_ignore` covers exclusion and the matcher stays small.
- **Q5. Repository-level default scope (`[defaults] paths_ignore`)?**
  Recommended: not in alpha; per-gate only, so the risk stays visible at
  the gate that takes it.
