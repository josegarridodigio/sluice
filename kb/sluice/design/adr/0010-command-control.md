---
title: "ADR-0010: Command control beyond hook gates (sluice exec, queue-only gates, guard rewrite, shell shims)"
tags: [sluice, adr, alpha, command-control]
---

# ADR-0010: Command control beyond hook gates

Status: accepted (2026-10-22). Design: [[sluice/design/command-control.md]].
Plan: [[sluice/plan/alpha-plan.md]] tasks A0, A1, A2, H5.

## Context

Sluice controlled only the commands declared as gates, and only when they
were reached through a git hook or the `verify` tool. Agents also run heavy
commands directly through their shell (`docker build`, `pnpm run build`,
`make`, test runners with other arguments), which bypass the budget and
can overload the machine. A cache hit gives a verdict, not build
artifacts, so builds whose outputs matter cannot simply be answered from
the cache.

## Decision

- `sluice exec [--weight N] [--label L] [--coalesce-key K] -- <cmd...>`
  runs any command through the daemon under the budget, without cache:
  a new `exec` client message advertised by the welcome feature `"exec"`
  (protocol stays 1; old daemons make the client run in-process). Output
  is spooled per stream and replayed byte-exact to the caller's stdout and
  stderr; stdin is `/dev/null`, no TTY; the exit code is propagated
  (signal deaths as 128+N); SIGINT/SIGTERM are forwarded to the job's
  process group; the caller's environment and cwd are used unchanged.
  Default weight `ceil(budget / 2)` until learned weights (ADR-0011) apply;
  coalescing only with an explicit key matched with the exact command and
  directory.
- Queue-only gates: new optional gate field `mode = "queue-only"`; `cache`
  stays a bool (frozen format, ADR-0013). They always run, never write a
  marker or miss record, coalesce simultaneous identical requests, and the
  agent fail-fast policy does not apply to them.
- The plugin guard moves its logic into `sluice guard` (Rust); `guard.sh`
  stays a thin wrapper that falls back to the current shell deny logic
  when the binary lacks the command. A rewrite only inserts a fixed prefix
  (`sluice run <gate> -v -- ` or `sluice exec --label … -- `) at the start
  of a segment of the untouched original string; pipelines, `&`, heredocs,
  redirections from stdin, command substitution, subshells, `sh -c`,
  wrappers and commands starting with `sluice` are never rewritten.
  Verified hook contract (Claude Code docs, 2026-10-22): `updatedInput`
  applies with `allow` and `ask`, replaces the whole input, and permission
  rules apply to the rewritten command. Therefore the guard emits `allow`
  only where it cannot widen permissions (bypass mode or an original that
  is already allowed), `ask` in default/acceptEdits modes, and no rewrite
  when a deny or ask rule matches the original; it always adds a one-line
  `additionalContext` note.
- Shell shims (`sluice shim install|uninstall|status`) give the same
  control to agents that only have a shell: POSIX wrappers calling
  `sluice shim exec <name>`, active only in agent sessions or with
  `SLUICE_SHIMS=1`. Every process Sluice runs carries `SLUICE_JOB=<id>`, so
  nested `exec`, `run` and shims pass through (no double wrapping, no job
  waiting for budget it already holds).

## Consequences

- Heavy commands of agents share the budget even when nobody declared
  them; builds keep producing their artifacts.
- The guard needs the agent client's settings to keep permission semantics
  intact; its matching of allow rules is conservative and may decline a
  rewrite it could have done.
- `sluice exec` adds a spool file per job, pruned with job logs.

## Alternatives considered

- Deny-only guard (current): safe but forces the agent to retry through a
  tool, and does nothing for non-gate commands.
- Re-quoting the original command inside `sluice exec -- sh -c '<cmd>'`:
  rejected, quoting agent-provided text is an injection risk.
- Caching build outputs: out of scope for alpha (verdicts only).
