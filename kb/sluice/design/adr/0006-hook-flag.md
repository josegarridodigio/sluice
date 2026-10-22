---
title: "ADR-0006: Hooks identify themselves with --hook"
tags: [sluice, adr, hooks]
---

# ADR-0006: Hooks identify themselves with `sluice run --hook <name>`

Status: accepted (2026-10-22), decided during Phase 1 implementation.

## Context

The agent policy (`agent_hook_policy`, G7) must apply only when a hook
invokes Sluice: an agent running `sluice run test` from its shell on
purpose should be allowed to run the gate (in Phase 1 that is the only way
an agent can verify a tree). Git does not export a reliable "hook name"
variable: `GIT_INDEX_FILE` and `GIT_DIR` appear in several hooks and also
outside hooks in some setups, and hook managers add their own layers.

## Decision

- `sluice run` takes `--hook <name>`. The generated snippets for Husky,
  lefthook, pre-commit/prek and plain hooks pass the hook name.
- The hook policy applies when `--hook` is present, or, as a fallback for
  hand-written hooks, when `GIT_DIR` or `GIT_INDEX_FILE` is set in the
  environment.
- `--hook pre-commit` additionally records the committed tree in the
  marker (`committed_tree`), using `index_tree()` on the hook's index.
- Without `--hook` and outside a hook environment, every caller waits and
  runs (`HookPolicy::Wait`).

## Consequences

- Hand-written hooks that omit `--hook` still get the policy through the
  environment fallback, but not the `committed_tree` record.
- The flag is part of the public CLI surface and is documented in
  `--help` and the README.
