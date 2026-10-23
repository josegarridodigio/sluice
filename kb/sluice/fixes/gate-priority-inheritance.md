---
title: "Fix: gate priority reaches every process from the first instruction (#1)"
tags: [sluice, fix, executor, nice, ionice, priority]
---

# Fix: gate priority reaches every process from the first instruction

## What

`daemon.nice` and the I/O class were applied with `renice -p <leader>` and
`ionice -p <leader>`, spawned asynchronously from `Running::wait`. Children
forked by the job before those helpers ran kept nice 0 (observed: 23 of 24
processes of one job at nice 0 under load).

Jobs now start through the standard wrappers, which set their own priority
and `exec` the next program, so every descendant inherits it:

```
[nice -n <N>] [ionice -c <class> [-n <level>]] sh -c <cmd>   (gates, exec)
[nice -n <N>] [ionice ...] <program> <args...>                (Launch::Argv)
```

`exec` keeps the PID and the process group, so the reaper registration and
`proc_start_ticks` are unaffected. `nice = 0` and no ionice add no wrapper;
the idle class (`-c 3`) takes no `-n`. The wrapper is not part of the gate
`cmd`: the log header, the cache key and the shown command are unchanged.

## Files and symbols

- `crates/sluice-gate-daemon/src/executor.rs`: `command()` builds the
  prefix and returns the halves still to apply; `priority_tools()` resolves
  `nice`/`ionice` once (`OnceLock`, `find_in_path`); `ionice_args()`;
  `Running` fields `fallback_nice`/`fallback_ionice` replace `nice`/`ionice`;
  `Running::wait` calls `apply_priority` only for a half whose tool is
  missing (debug log), no longer for every job.
- `crates/sluice-gate/tests/chaos.rs`: the slow-`ionice` shim regression now models a slow wrapper (the shim `exec`s the command after its delay), since the old window no longer exists.
- `kb/sluice/design/executor.md`: Priority and reaper paragraphs.
- `CHANGELOG.md`: Unreleased, Fixed.

## Why

`unsafe_code` is forbidden in the workspace, so `pre_exec` +
`setpriority` is not an option. The wrappers give the same guarantee
without it.

## Verification

New tests in `executor.rs`: `every_process_inherits_the_nice_value` (a
grandchild started at once and a background process, 50 runs in a row
while four busy threads load the CPU; skipped when `nice` is absent),
`grandchildren_inherit_the_io_class` (idle and best-effort; skipped when
`ionice` is absent), `zero_nice_and_no_ionice_add_no_wrapper`. The existing
executor, daemon and chaos tests cover exit codes, timeouts and kills.
Verified through Sluice (`fmt`, `clippy`, `test`).
