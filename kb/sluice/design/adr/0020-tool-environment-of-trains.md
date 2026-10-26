---
title: "ADR-0020: Merge trains run their tools in the user's login environment, not the starter's"
tags: [sluice, adr, merge-queue, environment]
---

# ADR-0020: Merge trains run their tools in the user's login environment, not the starter's

Status: accepted (2026-10-26). Builds on [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]]
§1 (neutral daemon environment). Plan: [[sluice/plan/queue-env-plan.md]].

## Context

Seen on 2026-10-26 on the incident's second machine, right after the 0.6.0 rollout:
the daemon had been restarted from a non-interactive SSH session, so its `PATH`
lacked the user's Node toolchain (`nvm`, `pnpm`). Every train failed with
`key_input_failed: node --version: exit status 127`, while agents' gates passed
(a client's run carries the client's environment; a train runs in the daemon's).

ADR-0018 §1 removed per-invocation variables from the daemon's environment, but the
rest (above all `PATH`) is still whatever the starting process had: an agent session,
a terminal, SSH, cron, `sluice update` from a script. Trains, their `prepare` and
their `key_inputs` therefore work or not depending on who last started the daemon.

## Decision

1. At start the daemon resolves the user's **login environment**: it runs the user's
   login shell (`$SHELL`, else the passwd entry, else `/bin/sh`) as
   `<shell> -l -i -c '<print marker>; env -0'` with stdin `/dev/null`, a minimal
   environment (`HOME`, `USER`, `LOGNAME`, `SHELL`, `TERM=dumb`, `LANG`), its own
   process group and a deadline (default 10 s; the group is killed on expiry). Output
   before the marker (rc noise) is ignored. The result is passed through
   `env::daemon_env` (per-invocation and agent variables stripped, shim directory
   removed from `PATH`).
2. Train gates, the train's `prepare` and base probe, and the train's `key_inputs`
   run in that environment (the daemon's own variables underneath, the login
   environment on top). Nothing else changes: agents' and hooks' runs still carry
   their client's environment; the push still uses the approver's credentials
   allowlist.
3. Resolution failure (no shell, timeout, non-zero exit, no marker) falls back to
   today's behaviour (the daemon's environment) and is reported: daemon log at
   `warn`, `sluice status` (`tool env: login shell | daemon (reason)`),
   `sluice doctor`.
4. User config `[queue] tool_env = "login-shell" | "daemon"` (default `login-shell`)
   and optional `[queue] tool_path = "..."`: an explicit `PATH` for trains, which wins
   when set. Older binaries reject the new keys (one-way, documented).
5. The login environment is resolved once per daemon start (a restart picks up a new
   toolchain); `sluice doctor` shows which tools the repositories' trains would find
   (`key_inputs` commands' first word, resolved in that `PATH`).

## Why not the enqueuer's environment

The queue already stores each job's allowlisted environment (`PATH`, `HOME`, ...). Using
it for train gates would let whoever enqueues choose the binaries that judge the
branch: an agent could enqueue from a shell whose `PATH` starts with a directory holding
a fake `pnpm` that exits 0, and its train would pass. A branch must not choose its
judges (ADR-0009, ADR-0018 §6); the tools are part of the judges. The login environment
belongs to the user and is the same whoever starts the daemon.

## Consequences

- Trains find the same tools as the user's terminals and agents, whoever started the
  daemon; train keys match agents' keys more often (same `PATH` for `key_inputs`).
- Daemon start costs one shell start (typically 50–500 ms, bounded by the deadline),
  off the critical path of the socket (resolved before the queue runner starts).
- An rc file that blocks (waits for input, `exec`s another program) is cut by the
  deadline and falls back, with the reason shown.
- Tests run with `tool_env = "daemon"` or a fake shell; the resolution itself is
  tested with fake shells printing noise, timing out and failing.

## Alternatives considered

- **The enqueuer's `PATH`**: rejected above (lets an agent choose the judges' binaries).
- **The approver's `PATH`**: available only after the gates ran; trains verify before
  approval.
- **`systemctl --user show-environment`**: does not include rc-file toolchains (nvm,
  pnpm, pyenv set `PATH` in shell rc files).
- **Only a doctor warning**: explains the failure, does not prevent it.
- **Only `[queue] tool_path`**: works, but every user must configure it (against
  zero-config); kept as an override.
