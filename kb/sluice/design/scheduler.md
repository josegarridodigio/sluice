---
title: Scheduler and coalescing
tags: [sluice, design, scheduler, phase-2]
---

# Scheduler and coalescing (Phase 2)

Status: design for Phase 2. Decision on fairness in
[[sluice/design/adr/0007-scheduler-fairness.md]]. Related:
[[sluice/design/protocol.md]], [[sluice/design/executor.md]],
[[sluice/design/psi-admission.md]] (Phase 4).

## Model

- **Budget** `B = daemon.budget` units (default `max(2, cpus - 1)`).
- A **job** is one execution of one gate on one tree: `(key, repo, gate,
  weight w, timeout, cmd, env, cwd)`. `w` is clamped to `1..=B`.
- A **request** is one client waiting for a job. A job has one or more
  subscribers; requests never run anything themselves.
- **Used** = Σ w of running jobs. A job is admissible when
  `used + w ≤ B`, the repository cap allows it (`repos.<path>.budget_share_max`,
  counted over running jobs of that repo), and PSI is not blocking (Phase 4).

## Coalescing (G3)

In-flight map: `key → JobId` for jobs that are queued or running and have a
non-null key. On a `run` request:

1. `key == null` → new job, never coalesced, never cached.
2. `key` in the in-flight map → attach the request to that job
   (`queued{coalesced:true}`), regardless of repo or worktree: markers are
   shared across worktrees, so the result is valid for both.
3. Otherwise → create the job, insert into the map, enqueue.

**Double-check before start:** when the scheduler is about to start a job
with a key, it calls `MarkerStore::lookup` once more. If a fresh marker
exists (a client that bypassed the daemon, or a daemon restart that lost
the in-flight map), the job completes immediately with `status:"cached"`
for every subscriber. The lookup costs a file read and avoids the only
remaining duplicate-run window.

After `done`, the key is removed from the map. Subscribers that attach
after that moment hit the marker on their own hit path instead.

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
- **Reservation:** once the head has waited longer than
  `head_reserve_after` (default 30 s), nothing behind it starts until it
  fits. Capacity drains and the heavy job runs. This bounds the head's
  extra wait to `head_reserve_after + longest running job`.
- Weights are not resized; a job needing more than `B` units is clamped to
  `B` at enqueue time (logged once).

Config (user config, `[scheduler]` table, Phase 2 addition to
[[sluice/design/config-format.md]]): `head_reserve_after = "30s"`.

## Per-repository cap

`repos."<toplevel>".budget_share_max = N` limits the units running for one
repository, so one busy project cannot take the whole budget from another.
A job blocked only by the cap reports `reason:"repo-cap"` in `status`.

## Subscribers and cancellation

- `last_subscriber_gone = cancel` (default): when the last subscriber
  disconnects, the job is killed if running or dropped if queued; outcome
  `killed` is recorded.
- `finish`: the job continues, the marker is written on a pass, nobody is
  notified. Useful when agents time out often and the result is still
  worth caching.
- Explicit `cancel` from the client is the same as disconnecting.

## Metrics (SQLite, [[sluice/design/executor.md]] for the schema)

Per job: queued_at, started_at, finished_at, wait_ms, run_ms, outcome,
exit_code, weight, subscribers_max, coalesced count, caller, session.
Derived for `status`: hits (from client `event`s), coalesced, time saved =
Σ run_ms of the job for each coalesced subscriber + Σ marker.duration_ms
for each reported hit.

## Properties to test (Phase 2 acceptance)

- Six concurrent requests for the same key → one process run (counter file = 1), six `done{pass}`.
- Different keys with weights 4 under `B = 8` → never more than two gates running at once (counter file with timestamps).
- A weight-6 job at the head under `B = 8` with a stream of weight-1 jobs behind it starts within `head_reserve_after + longest running job`.
- Marker appearing while queued → `cached` outcome, no process run.
- Last subscriber gone → process killed (pgrep shows nothing) under `cancel`; marker written under `finish`.
