---
title: Lookup by committed tree
tags: [sluice, design, cache, phase-2]
---

# Lookup by committed tree (Phase 2)

Status: implemented in Phase 2 (evidence note only; no automatic hit). Context: [[sluice/design/cache-key.md]] "Inside hooks".

## Problem

Inside `pre-commit`, Sluice keys on the working-tree content (what the
gate saw). When the worktree had unstaged changes, the committed tree
differs from the working tree. Later, `pre-push` on a clean checkout of
that commit computes the committed tree and misses, although the gate
passed on a superset of the content.

Whether a pass on the working tree implies a pass on the committed subset
is not guaranteed in general (the unstaged change could be the fix that
made the tests pass). Sluice therefore keeps the committed tree only as
evidence and does **not** reuse the pass automatically.

## Decision for Phase 2

- Keep recording `committed_tree` in markers written from `--hook pre-commit`.
- `sluice cache status` reads it back: for a gate that misses on the
  current tree, it appends `; passed with unstaged changes present <age> ago
  (tree <12 hex>)` when a fresh marker of that gate has `committed_tree`
  equal to the current tree and a different `tree`
  (`crates/sluice-gate/src/cache.rs` `status`, `MarkerSummary.committed_tree`;
  test `cache_status_notes_a_pass_recorded_with_unstaged_changes`). It is a
  note only: the gate still runs.
- No automatic hit on `committed_tree`. Revisit only if real usage shows
  many push-time misses caused by this case (measure in Phase 5).

## Path-scoped gates (0.3.0)

Path scoping ([[sluice/design/path-scoped-keys.md]] section 7) closes
most of this gap without reusing `committed_tree`: a scoped gate's key
hashes only its scope, so when the unstaged changes lie outside it the
working-tree key at `pre-commit` equals the key of the committed checkout
and `pre-push` hits on its own (test
`tests/scoped.rs::pre_commit_with_unstaged_changes_outside_the_scope`).
`committed_tree` is still the root tree of the hook index, and the
`cache status` note still compares root trees, so it keeps appearing for
scoped gates whose unstaged changes were inside the scope.
