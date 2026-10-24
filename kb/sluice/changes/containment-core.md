---
title: "Change: containment core, cgroup module and config (task C1)"
tags: [sluice, change, containment, cgroups, systemd]
---

# Change: containment core (2026-10-24)

Task C1 of [[sluice/plan/containment-plan.md]]. Design:
[[sluice/design/containment.md]] (sections 1, 4, 6, 7). Decision:
[[sluice/design/adr/0017-agent-resource-containment.md]]. Evidence: the
S-wave notes `kb/sluice/research/containment-s1..s5*.md`. Nothing here is
wired into the daemon, the hook or the shims yet (C2 to C5).

## What changed

- `crates/sluice-gate-core/src/config/containment.rs`, `ContainmentConfig`
  (field of `UserConfig`, table `[containment]`): all fields optional with the
  design section 7 defaults; validation weights `1..=10000`, `sandbox_nice`
  `0..=19`, `pids_max >= 1`, `cpu_ceiling` empty or `1%..=100%`,
  `memory_pressure_limit` `"auto"` or `1%..=99%`, `memory_pressure_duration`
  `"auto"` or a duration above zero, `memory_high`/`memory_max` empty or a size
  (`parse_size_bytes`: K, M, G, T binary, optional `B`/`iB`),
  `memory_policy` `oomd|none`. Helpers `parse_percent`, `parse_size_bytes`,
  `pressure_limit_override`, `pressure_duration_override`,
  `cpu_ceiling_percent`, `memory_high_bytes`, `memory_max_bytes`. A config
  without the table loads unchanged; golden-file compat tests untouched.
- `crates/sluice-gate-core/src/cgroup/` (new module, no `unsafe`):
  - `mod.rs`: `CgroupError` (typed, never a panic), `sanitize_session_id`
    (`[A-Za-z0-9_-]{1,64}`), `session_dir_name` (`s-<id>`, `s-pid<pid>`).
  - `systemd.rs`: the only place that spawns `busctl`, `systemctl`, `oomctl`
    (explicit env, stdin null, 2 s or 5 s deadline, child killed on timeout).
    `start_transient_scope` (`StartTransientUnit` with `PIDs`, `Slice`,
    `Delegate`, `Description`, `CPUWeight`, `CPUQuotaPerSecUSec`, and
    `ManagedOOMMemoryPressure*` when allowed), `set_slice_properties`,
    `attach_processes` (`AttachProcessesToUnit`), `show`, `stop_units`,
    `list_units`, `user_manager_state`, `systemd_version`, `is_unit_active`.
  - `detect.rs`: `gather` (read-only facts: `statfs` magic through
    `nix::sys::statfs`, `statvfs` read-only flag, `/proc/self/cgroup`, user
    manager socket and `is-system-running`, `cgroup.controllers` of
    `user@UID.service`, `systemd-oomd`, system bus) and the pure `evaluate`
    into `Capabilities { outcome: Full | Partial{missing} | Off{reason}, .. }`
    with the S4 `doctor` lines (`doctor_lines`), the summary line
    (`containment: full | partial (memory, pids; cpu by nice) | off (...)`) and
    `leaf_controllers` (presence of `cpu.weight`, `memory.current`,
    `pids.max`). Memory pressure policy derivation (below).
  - `tree.rs`: `Layout` (unit names and directories; `with_prefix` for
    tests), `TreeSettings::from_config`, `Tree` with `ensure_slice`,
    `ensure_daemon_scope`, `ensure_agents_scope`, `ensure_session`,
    `job_leaf`, `move_pid`/`move_pids`, `prune`, `usage`, `stop_all`; free
    functions `write_pid`, `write_control`, `read_usage`, `set_oomd_avoid`,
    `has_oomd_avoid`, parsers of `cpu.stat` and `memory.events`.
- `crates/sluice-gate-core/Cargo.toml`: `rustix` (features `fs`, `process`;
  already in the lockfile through `tempfile`) for `setxattr`/`getxattr` and
  `getuid`, because `nix` has no xattr API and `unsafe` is forbidden.
- `crates/sluice-gate/src/cg_exec.rs` and the hidden `Command::CgExec` in
  `main.rs`: `sluice cg-exec <leaf> -- <argv>` writes its own pid to
  `<leaf>/cgroup.procs`, then `exec`s; the move failing is logged at debug and
  the command still runs; 127 not found, 126 not executable, 2 no command.
- Docs: `docs/configuration.md` (`[containment]`), `CHANGELOG.md`.

## Decisions taken while implementing

- **Memory pressure defaults (lead's design update).** `memory_pressure_limit`
  and `memory_pressure_duration` default to `"auto"`: the policy that watches
  the user manager is read from `systemctl show user@UID.service -p
  ManagedOOMMemoryPressureLimit -p ManagedOOMMemoryPressureDurationUSec` (raw
  fixed point scaled to `u32::MAX`, `0` or `[not set]` meaning unset), then the
  monitored ancestor and the defaults of `oomctl dump`, then `oomd.conf`, then
  the built-ins 60 % and 20 s; limit = `min(30 %, 0.6 x ancestor)`, duration =
  `min(5 s, ancestor / 4)`, never below 1 % and 1 s (`derive_pressure_policy`,
  unit tests: 50 %/20 s gives 30 %/5 s, 80 %/20 s gives 30 %/5 s, 40 %/8 s
  gives 24 %/2 s, nothing set gives 30 %/5 s). Measured here: `systemctl show`
  prints `0` and `[not set]` for `user@1000.service` while `oomctl dump` lists
  it at 50 % / 20 s (vendor drop-in), so the `oomctl` source is needed. An
  explicit value in the config overrides the derivation.
- **Slice weight by `systemctl --user set-property --runtime`, scopes by
  `busctl StartTransientUnit`.** On systemd 259, `StartTransientUnit` of a
  slice fails with "was already loaded or has a fragment file" as soon as the
  slice exists, even implicitly (`sluice.slice` is created as the implicit
  parent of any `sluice-*` unit), while `set-property --runtime` works on an
  implicit or not yet loaded slice and is idempotent. `--runtime` keeps the
  setting in `/run`, gone with the user manager. Both are documented API of
  systemd 252 and later (not run on 252 to 258 here: unverified).
- **Scopes adopt a process, so a scope needs a live pid** (an empty `PIDs`
  list fails with "failed to load"): the daemon passes its own pid, the agents
  scope a holder (C2). A scope ends with its last process.
- **Never `mkdir -p` inside the units' directories**: `make_dir` creates one
  level and requires the parent, so a missing scope is an error instead of a
  hand-built cgroup that systemd does not know.
- **`prune(min_age)`** takes a grace period (a fresh session may be about to
  receive its first process); `rmdir` on a populated cgroup fails, so nothing
  alive is touched. `holder/`, `daemon/` and the scopes are never pruned.
- **Login-scope without system bus** is `Partial` (gates and daemon are still
  placed), not `Off` as the S4 pseudo-code said; the matrix row says partial.
- **Detection spawns processes only after the cheap checks pass** (v2, writable,
  `systemctl` present, manager socket), to stay inside the hook budget.
- `ManagedOOMMemoryPressureDurationUSec` (`t`, microseconds) is the D-Bus
  name of the duration; sent only for systemd >= 257 (verified accepted and
  read back on 259).

## Files and symbols

`crates/sluice-gate-core/src/cgroup/{mod,detect,systemd,tree}.rs`,
`crates/sluice-gate-core/src/config.rs` (`UserConfig::containment`,
validation call), `crates/sluice-gate-core/src/config/containment.rs`,
`crates/sluice-gate-core/src/lib.rs`, `crates/sluice-gate/src/cg_exec.rs`,
`crates/sluice-gate/src/main.rs`, `crates/sluice-gate/tests/containment.rs`.

## Verification

Unit tests: config (defaults, full table, every validation error, size and
percent parsing), detection (S4 matrix over fabricated facts, `/proc/self/cgroup`
and `cgroup.controllers` fixtures, `systemctl --version` fixtures, placement
classification), derived pressure policy over fixture outputs (`show`, the real
`oomctl dump` of the development machine, `oomd.conf`), `busctl` argument
lists, session-id sanitising, layout paths, prune over a temporary directory,
usage parsing.

Integration (`crates/sluice-gate/tests/containment.rs`, skipped with a printed
reason when the host is not a full containment host, for example CI):
`tree_lifecycle_on_the_user_manager` builds `sluice-test-<pid>.slice` with both
scopes and a session, checks the directories against `ControlGroup`, the
`CPUWeight`/`Delegate`/`ManagedOOM*` properties through `systemctl --user show`,
weights and `pids.max` read back and changed, the `user.oomd_avoid` xattr,
direct move, `cg-exec` placement, `AttachProcessesToUnit` (accepted for a leaf,
refused for an inner node), `move_pids`, a job leaf, usage readings, pruning
while busy and after the processes end, and that
`systemctl --user list-units 'sluice-test-<pid>*'` is empty afterwards. Plus
the negative creation case, fail-open without a user manager
(`XDG_RUNTIME_DIR` pointing to an empty directory) and `cg-exec` fail-open
(missing and unwritable leaf, debug log, exit codes). No test sets
`MemoryHigh`/`MemoryMax` or creates CPU or memory load; only `sleep`s started
by the test are moved. Final gate run through Sluice (`fmt`, `clippy`, `test`).

Caveat: systemd keeps the implicit parent slices (`sluice.slice`,
`sluice-test.slice`) loaded and active after the test units stop; they hold no
process and `systemctl --user stop` removes them. They are not `sluice-test-*`
units and are shared by concurrent runs, so the tests leave them alone.
