---
title: "Change: daemon containment, job leaves, interventions (task C2)"
tags: [sluice, change, containment, cgroups, daemon]
---

# Change: daemon containment (2026-10-24)

Task C2 of [[sluice/plan/containment-plan.md]]. Design:
[[sluice/design/containment.md]] (sections 1, 2 "daemon" rows, 5, 6, 7).
Builds on C1 ([[sluice/changes/containment-core.md]]) and coordinates with C3
([[sluice/changes/containment-session.md]]).

## What changed

- `crates/sluice-gate-daemon/src/containment.rs`: `ContainmentSetup` (built by
  the CLI) and `Containment::start`, run inside the detached daemon before the
  reaper: `detect::detect`, `TreeSettings::from_config`, derived oomd policy,
  `Tree::ensure_daemon_scope(own pid)` (daemon in `daemon/`), a holder child
  (`sluice daemon holder`, stdin pipe kept by the daemon, closed by the kernel
  when it exits) adopted by `Tree::ensure_agents_scope` into `holder/`. One
  retry after 300 ms (a previous daemon's scope may be stopping). Any error is
  logged once and the daemon continues as 0.4.1 (daemon scope failing: no
  containment at all; agents scope failing: daemon and jobs still contained).
  `job_prefix` creates `jobs/<id>` and returns `sluice cg-exec <leaf> --`;
  `release_job` removes it; `prune` (60 s grace); `collect_interventions`;
  `adopt_reaper`; `shutdown` (closes the holder pipe). With the daemon up, the
  hook's stopgap `sleep 86400` holder found in `holder/` is ended (exact
  command line match, that leaf only).
- `executor.rs`: `ExecSpec::wrapper` is prepended to the `nice`/`ionice` chain;
  `server.rs::launch` fills it and releases the leaf after the job (every exit
  path). PID, process group, reaper and `proc_start_ticks` stay valid because
  `cg-exec` execs.
- `server.rs`: `DaemonConfig::containment`, `Shared::containment`,
  `containment_loop` (every 10 s: intervention poll, every 6th tick prune),
  `recent_interventions` for the status reply.
- `interventions.rs`: `MemoryWatcher` (growth of `oom_kill`/`oom_group_kill`
  in `memory.events` of `s-*/{ui,tools}` and `jobs/*`; first poll is a baseline,
  leaves first seen later count from zero), `parse_oomd_journal`
  (`journalctl -o short-unix` lines of `systemd-oomd` naming a `sluice.slice`
  path; causes `oomd_memory_pressure`, `oomd_swap`), `InterventionRow`.
  `journalctl` is run only through `cgroup::systemd::journal_oomd` (C1's
  shell-out layer), for the user and the system journal, only when
  `systemd-oomd` is active.
- `migrate.rs`/`store.rs`: `containment_schema` version table (own table so the
  `sched_schema` released binaries check does not move) with migration 1, the
  `interventions` table (unique on time, leaf, cause);
  `record_intervention`, `recent_interventions`.
- `sluice-gate-proto`: `StatusReport::interventions: Option<Vec<Intervention>>`
  (`at`, `session`, `leaf`, `cause`, `detail`), skipped when absent; protocol 1
  and status schema 1 unchanged. C5 renders it.
- `sluice-gate-core::cgroup::tree`: `ensure_scope` places the leaf pid with
  `move_pid` (falls back to `AttachProcessesToUnit`), so a daemon started from a
  login scope can join an existing scope. `systemd::journal_oomd`.
- CLI: `sluice daemon holder` (hidden), `containment_setup` in `daemon_cmd.rs`.
  Test-only env: `SLUICE_TEST_CONTAINMENT_PREFIX` (shared with the session hook,
  must start with `sluice-test-`) and `SLUICE_TEST_MANAGER_RUNTIME_DIR` (the
  daemon's lock and socket live in `XDG_RUNTIME_DIR`, so tests keep it private
  and tell the tools where the user manager is).
- Docs: `kb/sluice/design/executor.md`, `docs/debugging.md`, CHANGELOG.

## Decisions

- Interventions go in a separate `containment_schema` table instead of bumping
  `sched_schema`: a 0.4.1 daemon would refuse a newer `sched_schema`, and many
  tests pin version 1.
- Holder = blocking read of a pipe, no polling, no `unsafe`.
- The daemon owns the agents scope long term; the hook keeps its stopgap for the
  no-daemon case and the daemon retires it.

## Verification

Unit: interventions parsing and watching over fixtures (`memory.events`,
journal lines, test layout paths), stopgap command line, store round trip and
migration. Integration (`crates/sluice-gate/tests/containment_daemon.rs`, real
user manager, skipped when unreachable, no load or memory pressure, units
`sluice-test-<pid>-<case>*`, drop guards, private HOME/runtime, `/tmp/sa-*`
socket): daemon and reaper in `daemon/`, holder in `holder/`, scope weights, a
gate and an `exec` job in `jobs/<id>` while running and the leaf gone after,
restart reuses the agents scope (same invocation id, session untouched),
adoption of a hook-created scope and retirement of its holder, fail open with
no user manager and with `enabled = false`. Final gates through Sluice.
