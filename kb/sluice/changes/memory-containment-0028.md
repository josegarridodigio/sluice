---
title: "Change: escaped processes moved back, swap ceiling, desktop guard (ADR-0028 part B)"
tags: [sluice, change, daemon, memory, containment, cgroup, swap, chromium]
---

# Change: escaped processes moved back, swap ceiling, desktop guard (ADR-0028 part B)

Why: Playwright's Chromium moves its browser process out of the job's cgroup, swap was
unbounded, and nothing reacted to the desktop stalling
([[sluice/research/desktop-hang-2026-11-05.md]],
[[sluice/research/chromium-scope-escape.md]],
[[sluice/research/cgroup-escape-and-swap.md]]). Decision:
[[sluice/design/adr/0028-gate-jobs-do-not-degrade-the-desktop.md]] decisions 2 to 5.

## What changed

- **Escapes.** `telemetry::reclaim` writes an escapee's pid back into the job leaf's
  `cgroup.procs` the tick it is seen (`JobTelemetry::reclaimed`, notes `Reclaimed` /
  `ReclaimFailed`, log "moved a process of the job back into its cgroup").
  `telemetry::kill_leftovers` (called from `server::kill_leftovers` in `launch`, before
  the leaf is released) writes `cgroup.kill` on a non-empty leaf and kills every known
  escapee still alive through `pidfd_open` + start-time check + `pidfd_send_signal`
  (`rustix` added to the daemon crate, `process` feature). Config
  `[containment] reclaim_escaped` (default `true`).
- **Swap ceiling.** `ceiling::swap_ceilings` (pure), `SwapJob`, `Writer::spawn_for(file)`
  (the writer is now generic over the leaf file); `server::swap_ceiling_tick` writes
  `memory.swap.max` per leaf from `memory::read_swap` (`SwapTotal`/`SwapFree`). Config
  `[admission] swap_ceiling` (default `true`) and `swap_floor` (`"auto"`, a quarter of
  `SwapTotal`; `AdmissionConfig::swap_floor_bytes`).
- **Desktop guard.** `server::desktop_degraded` (`SessionWatch::degraded`); `admit` blocks
  while it holds (the starvation guard still applies, settling does not change);
  `ceiling_tick` uses `max(memory_floor, memory_reserve)` as the floor meanwhile. Config
  `[admission] desktop_guard` (default `true`).
- Docs: `docs/configuration.md`, `docs/compatibility.md` (one-way keys, fixture
  `admission-desktop-swap.toml`), CHANGELOG, ADR-0026 and ADR-0027 amendment notes.

## How it was verified

- Unit tests: `ceiling::tests::swap_ceilings_share_the_free_swap_above_the_floor`,
  `telemetry::tests::leftovers_are_killed_only_when_the_pid_is_still_the_same_process`
  (a reused pid is never signalled), `telemetry::tests::job_telemetry_keeps_peaks_and_notes_each_event_once`,
  `config::tests::the_swap_ceiling_and_the_desktop_guard_are_on_by_default`,
  `tests/compat.rs::admission_desktop_swap_fixture_parses`.
- Against the real systemd user manager, no load and no memory pressure, every unit under
  the test prefix (`crates/sluice-gate/tests/containment_daemon.rs`): a gate starts a
  marked `sleep` through `setsid systemd-run --user --scope` (the same
  `StartTransientUnit`-with-own-pid mechanism Chromium uses, in a process group of its own
  as Playwright's browsers are). With `reclaim_escaped` on, the daemon logs the escape,
  moves it back and the process is gone once the job ends; with it off, the escape is
  logged and the process keeps running outside the job (the test's guard stops it). The
  first version of the test matched `sleep <marker>` and missed `/usr/bin/sleep <marker>`
  (systemd-run runs the absolute path), so it passed vacuously; it now compares the argv.
- A bounded probe (a headless Chromium for 6-8 s in a transient scope, about 200 MB)
  confirmed the escape on the workstation before the design.
- The full gates through Sluice (`verify`).
