---
title: "Gates cookbook"
tags: [sluice, docs, alpha]
---

# Gates cookbook

Alpha plan task G ([[sluice/plan/alpha-plan.md]]).

## What changed

- New `docs/gates-cookbook.md`: complete `.sluice.toml` examples for Node
  (npm/pnpm/yarn, husky/lefthook), Python (ruff/mypy/pytest, pre-commit
  framework), Rust, Go, Docker builds (verdict gate with the base-image id in
  `key_inputs`, and a `cache = false` image build) and a multi-package
  monorepo. Sections: rules of thumb (weight is about the CPU threads used,
  `key_inputs`, `key_files`, when `cache = false`), per-worktree caches (xo
  and `node_modules/.cache` from the alpha evidence, `.mypy_cache`,
  `.pytest_cache`, `target/`, `.turbo`), what a cache hit does not give you
  (verdicts, not artifacts), and choosing hooks (fast in pre-commit, heavy in
  pre-push, agents verify first).
- Documentation only; no code, README or skill edits (the lead links them).

## Why

Alpha findings: only declared gates are controlled, a hit produces no build
artifacts, and tools with per-checkout state break when worktrees share a
`node_modules` ([[sluice/changes/alpha-readiness.md]]). Users need worked
configs and these warnings before 0.3.0.

## Constraints respected

Examples use only fields in `Gate`, `Defaults` and `QueueConfig`
(`crates/sluice-gate-core/src/config.rs`): cmd, weight, timeout, hooks,
key_inputs, key_files, env, cache, depends_on, `[defaults] tail_lines`,
`[queue] gates/max_train`. Future features (`sluice exec`, queue-only gates,
path-scoped gates) are mentioned in one sentence each as "planned for 0.3.0"
with no syntax.

## Verification

Each of the 7 TOML blocks was extracted into its own temporary git repository
as `.sluice.toml` and `sluice cache status` was run there (`sluice` from
`~/.cargo/bin`). All seven parsed (exit 0, every gate listed). Gates whose
`key_inputs` tool is not installed on the validation machine (ruff, mypy,
pytest, go, the docker base image) reported `ERROR (key input command
failed ...)`, which is the documented behaviour (the run would be uncached,
not failed); the `cache = false` gate reported `always runs`. The weights,
tool flags and hook timings quoted were not benchmarked here; the hit
latencies come from the alpha harness evidence.
