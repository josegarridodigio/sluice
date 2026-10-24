---
title: "Change: containment from the session hook and the shims (tasks C3 and C4)"
tags: [sluice, change, containment, cgroups, plugin, hooks, shims]
---

# Change: session containment (2026-10-24)

Tasks C3 and C4 of [[sluice/plan/containment-plan.md]]. Design
[[sluice/design/containment.md]] sections 2, 3 and 6; decision
[[sluice/design/adr/0017-agent-resource-containment.md]]; builds on
[[sluice/changes/containment-core.md]] (C1) and the spikes S1, S3, S4.

## What changed

- `crates/sluice-gate/src/session_contain.rs` (new): `contain_session`
  (detection by C1, agents scope ensured, session node `s-<sanitised id>` with
  `ui`/`tools`, agent plus current descendants moved into `ui` by
  `Tree::move_pids`: direct write, `AttachProcessesToUnit` on EACCES;
  `user.oomd_avoid` is set by `ensure_session`), `agent_pid` (`CLAUDE_PID`,
  else the first ancestor whose comm is `claude`, else none), `with_descendants`
  (one pass over `/proc`), `tools_line`, `renice_line`, `place_self` (C4).
- `crates/sluice-gate/src/session_start.rs`: containment runs in a worker thread
  that gets what is left of a 230 ms budget (the watchdog stays 300 ms), so a
  slow first run still prints the context; a late worker is abandoned (every
  step is idempotent) and logged as `contained=timeout`. The client log's
  `decision` gains ` contained=full|partial|off|timeout`. `ensure_line` is the
  generic form of the idempotent env-file append.
- `crates/sluice-gate/src/shim.rs`: before a heavy tool (every route except
  "not a heavy command" and "inside a Sluice job") the shim calls `place_self`.
- Docs: design section 3 (line and rules), `docs/agents.md`, `docs/debugging.md`,
  `plugin/skills/sluice/reference.md`, CHANGELOG.

## The env-file line (final)

```sh
export SLUICE_SESSION_LEAF='<tools dir>'; if [ -w '<tools dir>/cgroup.procs' ]; then echo $$ > '<tools dir>/cgroup.procs'; else renice -n 19 $$ >/dev/null 2>&1; fi
```

Appended once (second and later hooks find an equal line). Variant for a host
where CPU cannot be weighted but cgroups work: `...; fi; renice -n 19 $$
>/dev/null 2>&1`. When containment is unavailable: no cgroup line; only
`renice -n 19 $$ >/dev/null 2>&1` for cgroup v1/hybrid, no systemd, no
controller delegated (matrix of S4: nice is the only CPU lever), nothing for an
unreachable user manager, a read-only cgroupfs or `enabled = false`.

## Decisions

- **Agents scope.** The daemon (C2) owns it, but the hook cannot wait for a
  daemon inside 300 ms. If `<slice>/<agents scope>/cgroup.procs` is missing the
  hook creates the scope with C1's `ensure_agents_scope` adopting a
  `sleep 86400` holder it starts; `ensure_*` are idempotent and the daemon
  later finds the scope there and adds its own holder. The holder dies after a
  day; the scope ends with its last process and the next session recreates it.
  Revisit when C2 lands (the daemon could replace the `sleep`).
- **`SLUICE_SESSION_LEAF`** is exported by the same line so the shims know the
  leaf without a lookup; `place_self` refuses paths outside `/sys/fs/cgroup`
  or with `..`, and never reports an error.
- **Test-only prefix.** `SLUICE_TEST_CONTAINMENT_PREFIX` (must start with
  `sluice-test-`) selects `Layout::with_prefix`, so tests never touch
  `sluice.slice`; anything else is ignored.
- **Label.** `full` needs capabilities `Full` and the agent process moved;
  anything else usable is `partial`.
- Session without id and without an agent pid: nothing is created.

## Verification

Unit tests in `session_contain.rs` (line rendering with awkward paths, stat
parsing, nice matrix, descendants, test prefix). Integration
(`tests/session_start.rs`, `tests/shim.rs`, helper `tests/common/cgtree.rs`; a
`sluice-test-<pid>` slice, only `sleep`s the test starts, no load, no memory
limits, skipped when the user manager is unreachable): a fake agent and its
child land in `ui`, the line appears once after two runs, sourcing it moves a
shell into `tools` silently, a read-only leaf gives nice 19 silently, the first
and a warm run stay under 300 ms, detection off and `enabled = false` fail
open with no cgroup line, no systemctl yields only the renice line; a heavy
fake tool run through the shim ends up in the leaf, not inside a job, and bad
leaves never fail the command. Final run through Sluice gates `fmt`, `clippy`,
`test`.
