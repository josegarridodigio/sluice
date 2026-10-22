---
title: "ADR-0009: Local merge queue runs inside the daemon and lands only on explicit approval"
tags: [sluice, adr, merge-queue, phase-6]
---

# ADR-0009: Local merge queue runs inside the daemon and lands only on explicit approval

Status: accepted (2026-10-22, user approval with the answers in section 14 of the design). Design: [[sluice/design/merge-queue.md]].
Reference: [[sluice/research/references/mergetrain.md]].

## Context

Several agents work in parallel worktrees of one repository and each produces a committed
branch. Landing them by hand means serial rebases and repeated test runs; letting agents push
means unverified combinations and pushes nobody approved. Sluice already has the pieces a
queue needs: one daemon per user with a fenced SQLite lease, a scheduler with a global budget,
coalescing by cache key, an executor with process-group control, and markers shared by all
worktrees. mergetrain shows the crash-safety mechanisms that make an unattended push
recoverable (write-ahead marker, pin ref, atomic push, endpoint-bound reconcile).

## Decision

- The queue runner is a task inside `sluice daemon`, fenced by the daemon's existing lease:
  every queue write is a compare-and-swap on the lease token and the expected state; no push
  starts while the lease is lost.
- Agents enqueue committed branches (`sluice queue enqueue`, MCP `queue_enqueue`); Sluice
  captures the SHA. The target is any branch: `<remote>/<branch>` lands by push, a local
  branch lands by a compare-and-swap ref update (never a branch checked out in a worktree).
  The runner merges the train (`--no-ff`) onto the fetched or local base in a disposable
  worktree under `<state>/merge/`, with hooks disabled for merge and ref operations.
- Gates run **through Sluice**: the runner derives each gate's cache key from the combined
  tree (snapshot asserted equal to the commit tree) and submits ordinary `run` requests, so
  markers, budget, repository caps and coalescing apply. A combined tree that already has
  markers becomes `ready` without running anything.
- Nothing is pushed without explicit approval: `sluice queue approve <train>` (TTY prompt or
  `--yes`), or MCP `queue_approve` only when the user config sets
  `[queue] allow_agent_approval = true` (default false). The approval binds a train hash
  (members' SHAs, base, landing SHA and tree, gate keys, endpoint); any drift voids it.
- Push protocol: pin ref `refs/sluice/pending/<train>`, then a fsynced write-ahead marker in
  the same transaction that moves the train to `pushing`, then
  `git push --atomic --force-with-lease=<target>:<base>` together with the audit ref
  `refs/sluice/landed/<sha>`. The repository's `pre-push` hook runs (the user chose not to
  bypass it; a Sluice hook answers from the markers the train produced, and a declining hook
  fails the train). Ambiguous outcomes park the train as `needs_reconcile`;
  `sluice queue reconcile` decides from the remote with a fixed table, refusing when the
  endpoint changed, the push process is alive or ancestry is unknown.
- Gate definitions and `[queue]` are read from the base commit, not from member branches.
- New crate `sluice-gate-queue` (async), wired into the daemon by the binary through a small
  `Extension` trait and a `DaemonHandle` (`submit_run`, lease fence, store transaction).
  Queue tables carry their own `queue_schema` version; the daemon schema stays 1.
- Protocol stays version 1: new message types and optional fields only, with
  `welcome.features = ["queue"]` for detection.
- v1 batching splits a failing multi-member train into single-job trains; bisection is an
  optional stretch goal.

## Consequences

- No new process to supervise; the queue inherits the daemon's single-instance guarantee,
  orphan handling and logs. A daemon restart dissolves unfinished trains and parks pushes
  for reconcile; it never silently re-pushes.
- Queue verification competes with agents' gates for the same budget, and benefits from
  their markers: a branch an agent already verified on the same tree costs nothing.
- The daemon now runs `git fetch`/`git push` with the user's credentials, taken from the
  approving client's environment allowlist; a remote that needs interactive auth fails
  definitively instead of hanging.
- Approval is only as strong as the boundary around `sluice queue approve`: an agent with
  shell access can call it. Mitigations are documented (deny rule, MCP approval off by
  default); forge branch protection remains the real enforcement.
- More surface in the daemon crate (the extension trait), but the dependency points from the
  queue to the daemon, never back.

## Alternatives considered

- **A separate queue daemon** (own lock, own lease, own process): cleaner isolation, but a
  second budget-unaware process would either run gates outside the scheduler or need its
  own client connection plus its own crash recovery and autostart logic.
- **Adopt mergetrain itself**: mature and well-tested, but Python, a separate SQLite state
  and its own gate execution; gates would bypass Sluice's cache and budget, which is the
  point of the project, and the user would supervise two tools.
- **No queue** (document a manual rebase + `sluice run` + push workflow): zero code, but
  leaves the serialisation, conflict reporting, approval and crash recovery to agents and
  humans, which is exactly where parallel agents fail.
