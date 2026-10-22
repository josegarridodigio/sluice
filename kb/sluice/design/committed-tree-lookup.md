---
title: Lookup by committed tree
tags: [sluice, design, cache, phase-2]
---

# Lookup by committed tree (Phase 2)

Status: planned for Phase 2. Context: [[sluice/design/cache-key.md]] "Inside hooks".

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
- `sluice cache status` shows, for the current tree, markers whose
  `committed_tree` equals it with the note `passed with unstaged changes
  present` so a human can decide.
- No automatic hit on `committed_tree`. Revisit only if real usage shows
  many push-time misses caused by this case (measure in Phase 5).
