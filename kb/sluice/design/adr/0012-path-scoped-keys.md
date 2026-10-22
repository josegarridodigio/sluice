---
title: "ADR-0012: Opt-in path-scoped cache keys"
tags: [sluice, adr, alpha, cache-key]
---

# ADR-0012: Opt-in path-scoped cache keys

Status: accepted (2026-10-22). Design: [[sluice/design/path-scoped-keys.md]].
Plan: [[sluice/plan/alpha-plan.md]] tasks D0, D1.

## Context

The cache key uses the whole working tree, so a documentation-only change
re-runs every gate (seen while dogfooding: each docs commit re-ran the full
suite).

## Decision

- Two optional gate fields, `paths` and `paths_ignore`, with git
  `:(glob)` pathspec semantics, root-relative; magic pathspecs, `!`, `..`
  and empty lists are rejected at parse time. Absent fields keep keys
  byte-identical to 0.2.
- The scoped hash is computed in process from the `ls-files -s -z` output
  the snapshot already produces (no extra git spawn), and always includes
  `.sluice.toml`. In the key material the tree field becomes
  `scope-v1:<sha256>`; `KEY_PREFIX` stays `sluice-cache-v1`.
- Markers, protocol and the post-run "gate modified the tree" check (root
  trees) do not change. Hooks, `verify` and the merge queue derive keys
  through one shared core function. On a scoped hit the run material uses
  the current root tree, not the marker's.
- Stale-pass mitigation: documentation, a `sluice doctor` heuristic for
  well-known manifests and lockfiles outside a gate's scope, and the scope
  shown in `cache status`.
- Hit-path budget: median under 35 ms on 5 000 files (measured in D1).

## Consequences

- A scoped gate can pass stale if it depends on files outside its scope;
  the feature is opt-in and documented as such.
- Binaries older than 0.3.0 reject a config using `paths` (strict parser):
  the README states the minimum version.

## Alternatives considered

- `git ls-tree` per gate (ignores wildcards, extra spawn) and a filtered
  index (slower): rejected after measurement in the design.
- Storing the scope hash in markers: not needed, and markers are frozen
  for old readers (ADR-0013).
