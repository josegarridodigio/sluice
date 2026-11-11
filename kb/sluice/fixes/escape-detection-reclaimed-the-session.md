---
title: "Fix: the escape detection moved the whole desktop session into a job"
tags: [sluice, fix, containment, telemetry, incident]
---

# Fix: the escape detection moved the whole desktop session into a job

Decision amended: [[sluice/design/adr/0028-gate-jobs-do-not-degrade-the-desktop.md]] (part A,
escaped processes).

## What happened (second machine, 2026-11-11, 0.12.0 daemon, code from 0.11.0)

A `ci-checks` gate job (pnpm, esbuild, a headless Chromium) ran at its kernel memory
ceiling (`memory.max` 1.2 GiB). In one telemetry tick the daemon logged 506 "processes of
the job that left its cgroup", starting with pid 1 (`systemd`) and pid 2 (`kthreadd`), and
moved 206 of them into the job's leaf: every process the user could move, among them
`systemd --user`, `gnome-shell`, Xwayland, the agents, the browser and the daemon itself.
Three seconds later the leaf hit its ceiling; with `memory.oom.group` the kernel killed the
whole leaf. `user@1000.service` died by signal, GDM showed the login screen. The journal
shows the user manager logging `Consumed` for `init.scope`, `sluice-agents.scope` and the
app scopes at the moment of the move (their cgroups emptied), then the memcg OOM kill list.

## Cause

`find_escapees` walks the descendants of the pids in the leaf's `cgroup.procs`. Pid 1 and 2
are only children of ppid 0, so the list held a `0`: the kernel prints 0 for a task whose
pid it cannot translate (a dying task under OOM pressure is the likely case). Nothing
filtered it, so the whole process tree became the job's.

## Fix

- `telemetry::leaf_procs` and `find_escapees` drop pids 0 to 2 (`is_system_root`).
- `find_escapees` never reports nor descends through the daemon and its ancestors
  (`protected_pids`, computed once per tick) nor a process in a `foreign_cgroup`: outside
  `/user.slice/`, the user manager's `init.scope`, `session.slice`.
- More than `MAX_ESCAPEES` (64) for one job in one tick is a detection error: logged at
  error level, nothing moved, recorded or killed (`server::telemetry_tick`).
- `telemetry::kill_leftovers` never signals a system root or the daemon, and kills no
  escapee from a list longer than `MAX_ESCAPEES`.

Files: `crates/sluice-gate-daemon/src/telemetry.rs`, `crates/sluice-gate-daemon/src/server.rs`.

## Verification

- Regression tests `the_walk_never_leaves_the_job` (a `0` and `1` in `cgroup.procs`, a
  poisoned procs set, known escapees in the session and `init.scope`) and
  `leftovers_never_kill_a_system_root_the_daemon_or_an_implausible_list`.
- `fmt`, `clippy`, `test` gates through Sluice `verify`.
- Mitigation on the affected machine until the fix is installed:
  `[containment] reclaim_escaped = false` in the user config, daemon restarted once idle.
