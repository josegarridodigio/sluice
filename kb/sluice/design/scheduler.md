---
title: Scheduler and coalescing
tags: [sluice, design, scheduler, phase-2]
---

# Scheduler and coalescing (Phase 2)

Status: implemented in Phase 2 (`sluice-gate-daemon::scheduler`). Decision on fairness in
[[sluice/design/adr/0007-scheduler-fairness.md]]. Related:
[[sluice/design/protocol.md]], [[sluice/design/executor.md]],
[[sluice/design/psi-admission.md]] (Phase 4).

## Model

- **Budget** `B = daemon.budget` units (default `max(2, cpus - 1)`).
- A **job** is one execution of one gate on one tree: `(key, repo, gate,
  weight w, timeout, cmd, env, cwd)`. `w` is clamped to `1..=min(B, cap)`
  where `cap` is the repository's `budget_share_max` if any; a clamp is
  logged once per job. `repo` is the repository identity (the main
  worktree path, derived from the git common dir, no symlink
  normalisation), so every worktree of a project shares its cap and is
  listed under the same path in `status`; the job row in `sluice.db`
  keeps the worktree path. The clamp warning fires only when a weight is
  reduced.
- A **request** is one client waiting for a job. A job has one or more
  subscribers; requests never run anything themselves.
- **Used** = Σ w of running jobs. A job is admissible when
  `used + w ≤ B`, the repository cap allows it (`repos.<path>.budget_share_max`,
  counted over running jobs of that repo), and PSI is not blocking
  ([[sluice/design/psi-admission.md]]; the starvation guard may admit the
  head alone through `admit_head`).

## Coalescing (G3)

In-flight lookup by key over the queued and running jobs (a linear scan;
the sets are small). On a `run` request:

1. `key == null` → new job, never coalesced, never cached.
2. `key` already queued or running → attach the request to that job
   (`queued{coalesced:true}`, plus an immediate `started` when the job is
   already running), regardless of worktree: markers are shared across
   worktrees, so the result is valid for both.
3. Otherwise → create the job and enqueue it.

**Double-check before start:** when the scheduler is about to start a job
with a key, it calls `MarkerStore::lookup` once more. If a fresh marker
exists (a client that bypassed the daemon, or a daemon restart that lost
the in-flight map), the job completes immediately with `status:"cached"`
for every subscriber. The lookup costs a file read and avoids the only
remaining duplicate-run window.

After `done` the job is gone from both sets. Requests that arrive after
that moment hit the marker on their own hit path instead.

## Queue order and admission

Queue = FIFO by `queued_at`. Admission loop, run whenever the state
changes (job finished, request arrived, PSI sample changed, timer):

```
for job in queue (oldest first):
    if head_reserved and job is not head: break
    if fits(job): start(job); continue
    if job is head and waited(job) > scheduler.head_reserve_after: head_reserved = true
```

- **Backfill:** while the head does not fit, lighter jobs behind it may
  start (no idle capacity while a heavy suite waits for a wide slot).
- **Reservation:** once the head has waited `head_reserve_after` or longer
  (default 30 s; `0s` means strict FIFO with no backfill), nothing behind
  it starts until it fits. Capacity drains and the heavy job runs. This
  bounds the head's extra wait to `head_reserve_after + longest running
  job`. The reservation fires only when the head is blocked by the budget;
  a head blocked only by its repository cap does not stop other
  repositories, and while PSI blocks nothing is admitted and no
  reservation is evaluated (the PSI starvation guard is separate, see
  [[sluice/design/psi-admission.md]]).
- Weights are clamped at enqueue time (see Model); there is no dynamic
  resizing.

Config (user config, `[scheduler]` table, Phase 2 addition to
[[sluice/design/config-format.md]]): `head_reserve_after = "30s"`.

## Per-repository cap

`repos."<main worktree path>".budget_share_max = N` limits the units
running for one repository (all its worktrees together), so one busy
project cannot take the whole budget from another. Keys are compared as
exact paths (no `~` or symlink normalisation); a key that matches no
repository is ignored. A job blocked only by the cap reports
`reason:"repo-cap"` in `status`. The daemon reads the user config once at
start: cap, budget, nice, retention and scheduler changes need
`sluice daemon stop`; the client re-reads the file on every run.

## Subscribers and cancellation

- `last_subscriber_gone = cancel` (default): when the last subscriber
  disconnects, the job is killed if running or dropped if queued; outcome
  `killed` is recorded.
- `finish`: the job continues, the marker is written on a pass, nobody is
  notified. Useful when agents time out often and the result is still
  worth caching.
- Explicit `cancel` from the client is the same as disconnecting.

## Metrics (SQLite, [[sluice/design/executor.md]] for the schema)

Per job: queued_at, started_at, finished_at (wait and run times are
derived from them), outcome, exit_code, weight, subscribers_max, coalesced
count, caller, session. Derived for `status`: hits (from client `event`s),
coalesced, time saved = Σ run time of the job for each coalesced
subscriber of a passed job + Σ marker.duration_ms for each reported hit.

## Properties to test (Phase 2 acceptance)

- Six concurrent requests for the same key → one process run (counter file = 1), six `done{pass}`.
- Different keys with weights 4 under `B = 8` → never more than two gates running at once (counter file with timestamps).
- A weight-6 job at the head under `B = 8` with a stream of weight-1 jobs behind it starts within `head_reserve_after + longest running job`.
- Marker appearing while queued → `cached` outcome, no process run.
- Last subscriber gone → process killed (pgrep shows nothing) under `cancel`; marker written under `finish`.
