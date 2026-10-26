---
title: "Plan: merge trains run their tools in the user's login environment (0.6.1)"
tags: [sluice, plan, merge-queue, environment]
---

# Plan: merge trains run their tools in the user's login environment (0.6.1)

Decision: [[sluice/design/adr/0020-tool-environment-of-trains.md]]. Branch
`fix/train-tool-env`. Approved by the user on 2026-10-26.

## Tasks

- Core: `sluice_gate_core::env::login_env(env, shell, deadline)`: runs the login
  shell with a marker, parses `env -0` after the marker, deadline with process-group
  kill, minimal input environment; result through `daemon_env`. Pure parser tested on
  noisy output; resolution tested with fake shells (noise, timeout, failure, no marker).
- Config: user `[queue] tool_env = "login-shell" | "daemon"` (default `login-shell`),
  `[queue] tool_path` (optional). Validation, docs, unreleased fixture, compatibility.
- Queue: `Core` gets the train tool environment (resolved once at extension start,
  before the runner) used by train gates (`RunRequest.child_env`), `prepare` and the
  base probe, and `derive_key` (train `key_inputs`). Agents' runs and the push are
  unchanged. Test harness: `tool_env = "daemon"` or a fake shell.
- Status/doctor: `tool env: login shell | daemon (reason)` in `sluice status` and its
  JSON (optional field); doctor resolves the `key_inputs` tools of known repositories
  in that `PATH` and warns about missing ones.
- Docs: `docs/merge-queue.md`, `docs/configuration.md`, `docs/how-it-works.md`,
  `kb/sluice/design/merge-queue.md` (§5 environment), the skill's reference if agents
  need to know (`key_input_failed` with exit 127 = a tool missing in the trains' PATH).
- Tests, full verification through Sluice, change note, CHANGELOG [Unreleased].
- Live check on the second machine: restart its daemon from a non-interactive SSH
  shell (the failing case) and verify a train finds `node`/`pnpm`.

## Progress

| Task | Status |
|---|---|
| Implementation | done (see [[sluice/changes/queue-env-login-shell.md]]) |
| Live check | pending |
