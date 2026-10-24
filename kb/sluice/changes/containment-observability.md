---
title: "Change: containment status, doctor and interventions (task C5)"
tags: [sluice, change, containment, status, doctor, interventions]
---

# Change: containment observability (2026-10-24)

Task C5 of [[sluice/plan/containment-plan.md]]. Design
[[sluice/design/containment.md]] sections 5, 6 and 8; decision
[[sluice/design/adr/0017-agent-resource-containment.md]]; builds on C1
([[sluice/changes/containment-core.md]]), C2
([[sluice/changes/containment-daemon.md]]), C3/C4
([[sluice/changes/containment-session.md]]) and the E2 finding in
[[sluice/changes/containment-e2e.md]].

## What changed

- **Status.** `sluice-gate-proto`: `StatusReport::containment`
  (`ContainmentInfo`: `level`, `detail`, `outside_jobs_cpu_percent`,
  `window_ms`) and `sessions` (`SessionUsage`: `id`, `cpu_seconds`,
  `memory_bytes`, `memory_peak_bytes`, `procs`), both optional and skipped when
  absent (schema 1 unchanged, `status_schema.rs` has an additive test and the
  fixture is untouched). `StatusReport::containment_text` renders the block
  shared by `sluice status` and the MCP `status` tool. Daemon
  (`containment.rs`): `Containment::start_reporting` (says why it is off),
  `sample` (every 10 s tick: CPU of the agents scope as a share of all CPUs,
  `window_percent`), `status` and `session_usages` (live `s-*` cgroups with
  `pids.current > 0`, read from cgroupfs). A daemon configured but not
  containing reports `off (<reason>)`; `enabled = false` reports
  `disabled by [containment] enabled = false`.
- **Doctor.** `crates/sluice-gate/src/doctor/containment.rs`: the
  `containment` group (JSON `group` field, additive on `Check`): summary,
  the S4 checks from `Capabilities::doctor_lines` with their exact messages,
  `cg-writable`, `controller-files` (presence of `cpu.weight`,
  `memory.current`, `pids.max`), `slice`, `memory-policy` (on with the derived
  thresholds and the system policy they come from, or off with the reason),
  `fallback` and `session` (shell and agent as `in ui`, `in tools` or outside,
  with the why). `detect::system_ancestor_policy` was split out of
  `pressure_policy` so doctor can show the system's values.
- **Explaining interventions.** `sluice-gate-core::cgroup::explain`: the daemon
  appends each new intervention to `interventions.jsonl` (state dir, trimmed
  to 200 lines); the MCP `verify` and `status` results and the guard's context
  note call `take_note`, which matches the caller's session (`SLUICE_SESSION_LEAF`,
  else the hook's `session_id`, else the caller's own cgroup) and appends one
  line per intervention not told yet, remembered in
  `interventions-seen/<session>`. Nothing older than six hours is told. The
  guard reads the file (it never contacts the daemon) and keeps its note on a
  single line.
- **Leak fix.** (a) The hook's stopgap holder is now `sluice daemon holder
  --lifetime 600` (hidden flag) instead of `sleep 86400`: it exits after its
  lifetime, when the Sluice scope it lives in is no longer `active`
  (`systemctl --user is-active`, every 5 s), or when no Sluice scope adopted
  it within 30 s. (b) After `ensure_agents_scope` the hook checks the scope is
  `active` and the holder is inside it; otherwise it kills the holder and fails
  open (E2 saw a holder survive in a `failed` scope). The daemon still retires
  the stopgap by command line (`is_stopgap_holder` also recognises the old
  `sleep 86400`). (c) `plugin/hooks/test-session-start.sh` ran the real binary
  with the developer's real runtime directory (so with the real user manager
  and, inside an agent session, possibly the real `sluice.slice`); it now
  passes an empty `XDG_RUNTIME_DIR`. (d) Test cleanup:
  `tests/common/cgtree.rs` `kill_leftovers` (SIGKILL, only for a layout named
  `sluice-test-*`, only below its own slice directory, zombies ignored) runs in
  the drop guards of `TestTree`, `containment.rs` and `containment_daemon.rs`
  and prints what it had to kill.
- **Race in `Tree::ensure_scope`** (found by a flaky `EBUSY` writing
  `cgroup.subtree_control` of a fresh agents scope): `StartTransientUnit`
  returns before the manager attaches the adopted process, which could pull a
  process the caller had already moved into `holder/` back into the scope's
  root. `ensure_scope` now waits (up to 3 s) until the process is listed in the
  new scope's `cgroup.procs` before moving it.
- Docs: `docs/cli.md`, `docs/debugging.md`, CHANGELOG.

## Decisions

- Interventions go through a small file, not a new protocol message: the guard
  must not contact the daemon and has a 500 ms budget.
- `outside jobs` is relative to all CPUs (comparable to the budget), over the
  last 10 s window; jobs live in the daemon scope, so every CPU second of the
  agents scope is outside Sluice jobs by construction.
- Holder liveness uses `systemctl`, the shell-out layer of C1, every 5 s; if
  `systemctl` is unusable the bounded lifetime still ends it.

## Verification

Unit: status text and JSON additive fields, daemon `window_percent` and
`session_usages` over fixture files, doctor builders over fabricated
`Facts` (the S4 messages, memory policy on and off, controller files, slice,
session), `explain` (once-only, per session, old events, log trimming),
guard note, stopgap command-line matching, holder scope parsing. Integration
(real user manager, skipped when unreachable, only `sleep`s): `status` and
`doctor` over a real session tree (`containment_daemon.rs`), off and
disabled status, MCP `verify` and `status` telling an intervention once
(fixture log), the hook creating a bounded stopgap and nothing left under the
test prefix after the tree is dropped, the holder ending at its lifetime. No
memory limit or pressure, no CPU load. Final run through Sluice (`fmt`,
`clippy`, `test`).
