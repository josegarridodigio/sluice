---
title: "Plan: Sluice runs a repository's existing git hooks (ADR-0021)"
tags: [sluice, plan, hooks]
---

# Plan: Sluice runs a repository's existing git hooks

Decision: [[sluice/design/adr/0021-sluice-runs-the-repository-hooks.md]]. Research:
[[sluice/research/hook-managers-synthesis.md]]. Approved by the user on 2026-10-28.
Sub-agents run on Sonnet. Waves run one after another; each ends with `cargo check`
clean and its new tests written; the full verification runs through Sluice at the end of
each wave (fmt, clippy, test), then Matrix on the branch before any release.

## H1 — Entry point and faithful delegation (no behaviour change)

- Per-clone hooks directory under the clone's git directory, one small script per hook
  git supports, each calling `sluice hooks run <hook> -- "$@"` (stdin forwarded).
- Activation: a Sluice-managed config file included by the last `[include]` of the clone's
  `.git/config` sets `core.hooksPath`; the previous effective value is recorded so
  `sluice hooks off` restores it. Idempotent; never writes inside the repository's
  tracked files.
- Manager detection at run time: husky (`core.hooksPath` that would be in effect without
  Sluice, `.husky/`), lefthook (`lefthook.yml`, `.lefthook.yml`, `lefthook-local.yml`
  variants, its hook scripts), pre-commit (`.pre-commit-config.yaml`, its hook stub),
  plain hooks (the previous `core.hooksPath` or `.git/hooks`), none.
- Full delegation for every manager, byte-for-byte arguments and stdin, exit code
  propagated: `.husky/_/<hook>`; lefthook's installed script or `lefthook run <hook>` with
  its binary discovery; `pre-commit hook-impl --config=... --hook-type=<hook> -- <args>`;
  plain: the previous hook file. If anything fails before delegating, delegate anyway.
- `sluice hooks on|off|status`; `sluice init` offers it; doctor reports a clone whose hooks
  bypass Sluice as a failure and the activation's state; the plugin session start
  restores a displaced activation (husky's rewrite does not displace it, but `hooks off`
  or a manual edit can).
- Tests: delegation fidelity per manager with fake managers (scripts recording argv,
  stdin, cwd, env) and, where the real tool exists on the machine, an e2e with it (husky
  via a fixture of its generated `_` directory, lefthook/pre-commit skipped when absent);
  include-order survives a simulated husky rewrite; off restores the previous value.

## H2 — Units and caching

- husky/plain line grammar (simple commands, `&&` lists, comments; anything else →
  delegate the hook); classification of index-modifying steps (`lint-staged`,
  `pretty-quick`, `git add`, known formatters with write flags) vs checks.
- lefthook: named jobs through `lefthook run <hook> --command <name> --no-auto-install
  --skip-lfs` (`--no-stage-fixed` only if Sluice stages); unnamed jobs, setups,
  `parallel`/`piped` semantics that matter → delegate; effective config via `lefthook dump
  --format json`.
- pre-commit: hook ids through `pre-commit run <id> --hook-stage <stage>`; ambiguous ids,
  remote-ref dependent runs → delegate; config + resolved revs in the key.
- Execution in the user's worktree, sequentially, mutating steps first and uncached,
  checks through the daemon with keys from the index tree + unit + effective config +
  toolchain inputs.
- Exit code mapping (pre-commit's 3 vs Sluice's refusal).

## H3 — Verify and gates without `.sluice.toml`

- `verify` and `gate_list` in a repository without `.sluice.toml` use the hook units as
  gates (same identities, shared cache); with a `.sluice.toml`, matching gates share cache.

## H4 — Docs, skill, doctor messages, compatibility, release

- docs (hooks page with the compatibility table: what is accelerated, what is delegated),
  skill/reference/MCP instructions, CHANGELOG, compatibility notes, ADR follow-ups.

## Progress

| Wave | Status |
|---|---|
| H1 | done (verified through Sluice: fmt, clippy, test) |
| H2 | H2a (husky and plain units, caching, status) done, verified through Sluice; H2b (lefthook, pre-commit adapters) pending |
| H3 | pending |
| H4 | pending |
