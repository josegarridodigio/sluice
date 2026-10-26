---
title: "Fix: session-start tests no longer depend on the machine's load"
tags: [sluice, fix, tests, ci, agent-plugin]
---

# Fix: session-start tests no longer depend on the machine's load

## Symptom

The Matrix workflow of the 0.6.0 release failed on `main` in the `alpine:latest` job
(`plugin_test_session_start_script_passes`,
`without_containment_the_hook_fails_open_and_adds_no_cgroup_line`: the hook printed
nothing, "not JSON (EOF ...)"), while the run on the `v0.6.0` tag, on the same commit,
passed; re-running only the failed job passed too. Three workflows (two Matrix runs and
the release build) ran at once.

## Cause

`sluice agent session-start` gives up silently after a 300 ms watchdog (by design: a
SessionStart hook must never delay or break the agent). With a debug build, musl in a
container and a loaded CI machine the hook can overrun it, and then prints nothing.
The tests check *what* the hook answers, so they failed for a reason they were not
about. The latency has its own test (`a_warm_run_takes_under_300_ms`).

## Change

- `crates/sluice-gate/src/session_start.rs`: test-only `SLUICE_TEST_SESSION_WATCHDOG_MS`
  (`TEST_WATCHDOG_ENV`) replaces the watchdog; the containment budget keeps the same
  margin below it (`watchdog`, `budget`). Without the variable nothing changes.
- `crates/sluice-gate/tests/session_start.rs`: `hook` sets it to 5 s; `hook_with` runs
  with exactly the given environment, used by `a_warm_run_takes_under_300_ms` so the
  latency is still checked against the real 300 ms; the plugin script test sets it.
- `plugin/hooks/test-session-start.sh`: passes the variable through its `env -i`
  end-to-end run when it is set.

## Verification

fmt, clippy and the whole suite through Sluice `verify`. Evidence of the flake: the
same commit passed on the tag run and on the re-run of the failed job.
