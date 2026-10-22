---
title: "ADR-0002: Shell out to the git binary"
tags: [sluice, adr, git]
---

# ADR-0002: Shell out to the git binary

Status: accepted (2026-10-22), pre-agreed in the bootstrap brief.

## Context

The tree hash must match what git itself would compute for the working
tree, including `.gitignore` semantics, `core.excludesFile`, sparse
checkouts, submodules, worktrees and `GIT_*` hook variables. Libraries
(`git2`/libgit2, `gix`) re-implement these rules with occasional
differences, and they add build complexity (libgit2 C dependency) or a
large dependency tree.

## Decision

All git operations go through the `git` binary in `PATH`, wrapped in one
module in `sluice-core` (`git.rs`) with:

- an explicit environment builder that strips inherited `GIT_*` variables
  unless the caller passes them on purpose,
- typed helpers (`rev_parse`, `write_tree_from_index_copy`, `is_clean`,
  `common_dir`), each covered by tests on throwaway repositories,
- a hard requirement check at startup with a clear error message if `git`
  is missing or older than the minimum supported version (to be decided when
  the first feature needing a specific version appears; candidate: 2.30).

## Consequences

- Each git call costs a process spawn (about 2–5 ms). The hit path needs
  `rev-parse` + index copy + `add -A` + `write-tree`: three spawns. The
  < 30 ms target must be measured and may require merging calls
  (`rev-parse` can return several values at once).
- `git` becomes the only hard runtime dependency, which is acceptable: hooks
  already imply git.

## Alternatives considered

- `gix` (gitoxide): pure Rust, fast, but ignore/index semantics are still
  maturing and the API surface is large.
- `git2`: libgit2 bindings; known divergences in ignore handling and a C
  build dependency.
