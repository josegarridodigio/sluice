---
title: "Fix: the escape detection pulled back processes Sluice itself placed in its slice"
tags: [sluice, fix, containment, telemetry, tests]
---

# Fix: the escape detection pulled back processes Sluice itself placed in its slice

Decision amended: [[sluice/design/adr/0028-gate-jobs-do-not-degrade-the-desktop.md]]
(amendment of 2026-11-13).

## What happened (workstation, 2026-11-13, 0.12.1 daemon, `reclaim_escaped = true`)

Sluice's own `test` gate failed in 3 of 4 local runs through Sluice, always in
`crates/sluice-gate/tests/containment_daemon.rs`:

- `a_restart_reuses_the_agents_scope`: "the session is untouched" or "reused, not
  recreated".
- `status_and_doctor_show_a_real_session`: the shell and the agent were "outside the
  session".

The daemon log showed the running job's telemetry moving `sleep`, `sluice` and
`systemctl` processes back into the job (one to four per run). They came from the
cgroups the tests create under
`sluice.slice/sluice-test.slice/sluice-test-<pid>-<case>.slice/...`. GitHub CI runs no
outer daemon and never saw it.

## Cause

The tests are descendants of the job. They place processes in the agents scope and
session leaves of the test's own Sluice tree, which systemd nests under `sluice.slice`
because unit names nest on dashes. `find_escapees` only exempted `init.scope`,
`session.slice` and cgroups outside `/user.slice/`, so these looked like escapes and were
pulled back. That emptied the test's scopes, and systemd removed them.

## Fix

- `telemetry::find_escapees` takes the daemon's own slice (`sluice_slice`), computed in
  `server::cgroup_rel` from `layout.slice_dir()`.
- A process that Sluice placed (`telemetry::placed_by_sluice`) is never reported: one in
  the slice's `*-agents.scope` or in any `*.slice` below the slice.
- Still escapes, as before:
  - any other unit right in the slice; the integration test
    `an_escaping_process_is_moved_back_and_dies_with_the_job` starts its escapee there
    with `systemd-run --scope --slice=`;
  - Chromium's `app.slice` scopes.

A first version exempted the whole slice. It made the two escape integration tests
fail, because their `systemd-run` scope lives right in the test's slice. It was narrowed
before landing.

Files: `crates/sluice-gate-daemon/src/telemetry.rs`, `crates/sluice-gate-daemon/src/server.rs`.

## Verification

- Unit test `a_process_in_the_daemons_own_slice_is_not_an_escapee`. Not escapees: a
  nested test tree and a session leaf. Still escapees: a sibling name `sluice.slice2`,
  Chromium's scope, and a scope right in the slice. It also checks that without the slice the old
  behaviour returns every one of them.
- `fmt`, `clippy` and `test` gates through Sluice `verify`, with the 0.12.1 daemon
  (`reclaim_escaped = true`) running the suite.
- The installed daemon only gets the fix with the next release. Until then, its runs of
  the suite can still fail this way.
