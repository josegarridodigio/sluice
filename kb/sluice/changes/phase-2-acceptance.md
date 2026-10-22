---
title: "Phase 2 acceptance evidence"
tags: [sluice, acceptance, phase-2]
---

# Phase 2 acceptance evidence (2026-10-22)

Criteria from [[sluice/plan/master-plan.md]] Phase 2, each proven by an
automated test. Daemon-level tests live in
`crates/sluice-gate-daemon/tests/daemon.rs` (in-process daemon on a
temporary socket, throwaway git repositories); CLI-level tests in
`crates/sluice-gate/tests/daemon_cli.rs` (the real `sluice` binary,
daemon autostarted inside a sandboxed `XDG_RUNTIME_DIR`, stopped on drop).

| Criterion | Evidence |
|---|---|
| 6 concurrent `sluice run test` on the same tree run the command once; all 6 get the result | CLI `six_concurrent_runs_on_one_tree_run_the_gate_once` (six threads, counter file = 1, all exit 0); daemon `concurrent_requests_for_one_key_run_once` (one `queued{coalesced:false}`, five `coalesced:true`, six `done{pass}`, one marker). |
| With different trees never more than the budget runs at once (weights honoured) | CLI `the_budget_limits_overlap` (budget 2, three gates, overlap ≤ 2 from timestamp files); daemon `budget_limits_concurrency`; scheduler unit tests for weights, backfill, head reservation and repo caps. |
| Killing a client or the daemon mid-run leaves no orphan process and no stale lock | CLI `killing_the_client_kills_the_gate_and_the_daemon_survives` (gate's `sleep` gone within 7 s, `status` still answers); daemon `last_subscriber_gone_cancel_kills_the_group`, `last_subscriber_gone_finish_writes_the_marker`, `orphans_are_reconciled_on_start` (a real orphaned process group from a previous instance is killed and its row marked `orphaned`), `second_daemon_is_refused` and CLI `a_second_daemon_refuses_to_start` (lock), `daemon_stop_without_and_with_running_jobs`, `stop_force_kills_running_jobs`, `stop_waits_for_running_jobs`. |
| A gate that exceeds its timeout is killed with its whole process group | daemon `timeout_kills_the_group`; executor unit test with a nested `sh -c 'sleep 30'` grandchild; Phase 1 CLI `timeout_kills_the_gate_and_exits_124` still green on the daemon path. |

Further behaviour proven:

| Behaviour | Test |
|---|---|
| Protocol handshake rejects another version | daemon `handshake_checks_the_protocol` |
| Marker appearing while queued yields `cached` without a run | daemon `marker_appearing_while_queued_is_cached` |
| `status`, `logs`, hit `event` | daemon `status_logs_and_events`; CLI `a_miss_starts_the_daemon_and_a_hit_only_sends_an_event` (totals.runs = 1, totals.hits = 1), `status_json_has_schema_one` |
| Unreachable daemon falls back to the in-process run | CLI `an_unreachable_daemon_falls_back_to_an_in_process_run` |
| Phase 1 acceptance suite unchanged on the daemon path | `crates/sluice-gate/tests/run.rs`, 19 tests |

Command and result:

```
cargo test --workspace
  sluice-gate-core: 89 passed
  sluice-gate-proto: 11 passed
  sluice-gate-daemon: 26 unit + 12 integration passed
  sluice-gate: 10 unit + 8 (cli) + 19 (run) + 8 (daemon_cli) passed
```

Hit-path latency after wiring the daemon: median 24.7 ms on 5 000 files
(load average about 10), see [[sluice/changes/hit-path-latency.md]].
