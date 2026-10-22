---
title: "Change: Phase 2, daemon with scheduler and coalescing"
tags: [sluice, change, phase-2]
---

# Change: Phase 2, daemon with scheduler and coalescing (2026-10-22)

Implements [[sluice/plan/master-plan.md]] Phase 2 from
[[sluice/design/protocol.md]], [[sluice/design/scheduler.md]]
(+ [[sluice/design/adr/0007-scheduler-fairness.md]]) and
[[sluice/design/executor.md]].

## What changed

### `sluice-gate-proto`

`ClientMessage` / `ServerMessage` (tagged by `type`), `RunRequest`,
`GateSpec`, `RepoInfo`, `Caller`, `ClientEvent`, `Outcome`,
`OutcomeStatus`, `StatusReport` and its parts, `ErrorCode`, `WaitReason`;
`encode` / `decode_client` / `decode_server`; `PROTOCOL_VERSION = 1`,
`STATUS_SCHEMA_VERSION = 1`, `MAX_LINE_BYTES = 1 MiB`. Forward compatible:
no `deny_unknown_fields`, defaults on optional fields.

### `sluice-gate-daemon`

| Module | Symbols | Notes |
|---|---|---|
| `store.rs` | `Store`, `JobRow`, `JobState`, `SCHEMA_VERSION` | SQLite (WAL, busy timeout 2 s), schema from the executor design, fenced lease (`acquire_lease`, `heartbeat`, `release_lease`), fenced job transitions (`mark_started`, `mark_done` check `daemon_instance`), `orphans`, `mark_orphaned`, hit `events`, `totals`, `prune_logs`, `open_read_only`. Busy maps to a retryable `Error::Busy`. |
| `scheduler.rs` | `Scheduler`, `JobSpec`, `Enqueued`, `RunningView`, `QueuedView` | pure, clock passed by the caller; coalescing by key onto queued or running jobs; FIFO with backfill and head-of-line reservation after `head_reserve_after`; per-repo caps; `WaitReason` per queued job. |
| `executor.rs` | `ExecSpec`, `IoPriority`, `IoClass`, `Running`, `RunningHandle`, `Finished`, `ExecStatus`, `ProcessInfo`, `spawn`, `terminate_group`, `process_matches`, `proc_start_ticks`, `tail_lines` | `sh -c` in a new process group, `renice`/`ionice` binaries for priorities, interleaved log with the Phase 1 framing, timeout and cancel through SIGTERM → 5 s → SIGKILL on the group, pid-reuse guard via `/proc/<pid>/stat` start ticks. |
| `server.rs` | `DaemonConfig`, `run`, `connect`, re-export `LastSubscriberGone` | `flock` on `daemon.lock`, lease with 15 s heartbeats, orphan reconciliation at start, socket `0600`, handshake, per-job broadcast channels, scheduling loop with the marker double-check, marker/miss writing after runs, `status`, `logs`, `event`, `stop`, daily log pruning. |

### `sluice-gate` (CLI)

| File | What |
|---|---|
| `client.rs` | blocking std client: handshake, `run` streaming, `status`, `logs`, `stop`, best-effort `hit` event (20 ms), `ensure_daemon` with the start lock, detached spawn and a 5 s socket wait. |
| `daemon_cmd.rs` | `sluice daemon [--foreground]` (builds `DaemonConfig` from the user config, detaches with `nix::unistd::daemon` before creating the tokio runtime, redirects stdio to `daemon.log`, `SLUICE_LOG` filter) and `sluice daemon stop [--force]`. |
| `status.rs` | `sluice status [--json]`: text summary or the `StatusReport` JSON; `no daemon running` when absent. |
| `run.rs` | on a miss with policy `wait`: `RunRequest` to the daemon, notices `queued (position N, U of B units busy)` / `running (job …)`, Phase 1 output on `done`; falls back to the in-process run with the `flock` semaphore when the daemon is unavailable (`running in-process` notice). Hits send the hit event. `"$@"` is appended to the command only when extra arguments exist. |
| `logs.rs` | asks the daemon first, scans the directory otherwise. |
| core `config.rs` | `[scheduler] head_reserve_after` (default 30 s). |

## Verification

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace     # core 89, proto 11, daemon 26 unit + 12 integration, CLI 10 unit + 8 + 19 + 8 integration
scripts/bench-hit-path.sh 5000 20   # hit median 24.7 ms (load ~10); the hit event costs about 1 ms
```

Acceptance evidence: [[sluice/changes/phase-2-acceptance.md]].

Reconciliation at phase close: [[sluice/changes/phase-2-reconciliation.md]]
(56 findings; the code items were fixed the same day, see its Resolution
section, the rest documented).

## Known limitations (documented, intentional)

- PSI admission is Phase 4; the daemon passes `psi_blocking = false`.
- `queued` carries no budget numbers; the CLI makes one `status` call to
  print `U of B units busy`.
- A request that coalesces onto a job being cancelled because its last
  subscriber just left receives `killed` (rare race, documented).
- `sluice logs` without a job id shows the daemon's newest job, which may be
  older than a log written by an in-process fallback run.
- The command sent to the daemon is trimmed, not whitespace-normalised
  (multi-line commands must keep their newlines); the key uses the
  normalised form.
