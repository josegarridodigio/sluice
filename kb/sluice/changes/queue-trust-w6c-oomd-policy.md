---
title: "Change: W6c, the gates' scope has its own oomd policy and the daemon is omitted"
tags: [sluice, change, queue-trust, memory, containment, systemd-oomd, w6]
---

# Change: W6c, oomd policy on the gates' scope (ADR-0019 piece 3)

Plan: W6c of [[sluice/plan/queue-trust-plan.md]]. Decision:
[[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]]. Analysis:
[[sluice/research/memory-gates-w6-analysis.md]] (A1, A2, A8, B1; tests T-W6-10, 13).
Measured shape: [[sluice/research/memory-gates-w6d-oomd-spike.md]]. Written without running
tests (the `cargo test` hook): `cargo fmt` and `cargo check --workspace --all-targets -j 4`
only; the full verification is the plan's final step.

## What changed

1. **`derive_gates_policy(agents) -> PressurePolicy`** (`sluice-gate-core/src/cgroup/detect.rs`):
   same limit as the agents' policy, `duration = max(1 s, ceil(agents / 2))` in whole seconds,
   never equal to or longer than the agents' unless that is 1 s.
2. **`TreeSettings.daemon_oom`** (`cgroup/tree.rs`): set in `from_config` from the same gate as
   `oom` (`oomd_policy_enabled`: oomd active, systemd >= 257, `memory_policy = "oomd"`), so the
   gates' policy exists exactly when the agents' does. `Tree::daemon_spec` builds the daemon
   scope's `ScopeSpec` with `oom: self.settings.daemon_oom`; `start_transient_scope_args` already
   renders the three `ManagedOOM*` properties.
3. **`user.oomd_omit` on `daemon/`**: `set_oomd_xattr`/`has_oomd_xattr` generalise the avoid
   helpers (`set_oomd_avoid`/`has_oomd_avoid` stay as thin wrappers, plus `set_oomd_omit`,
   `has_oomd_omit`, `OOMD_OMIT_XATTR`). `ensure_scope` takes `omit_first_leaf` and sets the
   attribute right after `make_dir(daemon/)`, before the daemon moves in; failures (including
   `EOPNOTSUPP`) are tolerated.
4. **Reuse of an existing daemon scope**: the agents' scope is not touched on reuse (the
   existing behaviour). The daemon scope can outlive its daemon (jobs keep it alive) and so come
   from a version without the policy: `ensure_daemon_scope` now notes whether the scope existed
   and, when it did and the policy is supported, runs `systemctl --user set-property --runtime`
   with `systemd::oom_assignments` (`set_scope_properties`, new in `systemd.rs`); best effort.
5. **Status**: `containment.gates_oom_policy {limit_percent, duration_secs}` (new
   `OomPolicyInfo`, optional, additive; schema 1; the status schema fixture test adds the field
   deliberately), `containment_text` prints `gates memory policy: systemd-oomd at 30% for 3s`;
   filled by `Containment::status`.
6. **Doctor** (`sluice-gate/src/doctor/containment.rs`): check `gates-memory-policy`
   (`GatesPolicyFacts`, `gates_policy`): "systemd-oomd acts on the gates' scope at L% for Ds,
   before the agents' scope (D's)" plus whether `daemon/` has `user.oomd_omit` (a warning when
   the leaf exists without it). Only when the agents' policy is on.

## Decisions

- **Limit = the agents' limit, not `max(agents, 40)`.** B1 offered a laxer gates' limit because
  the order is set by the duration, not the limit. Rejected: the measured evidence (T-W6-0) is
  that the gates' scope had stall and `pgscan` comparable to the agents'; the point of the policy
  is that it must fire whenever the agents' would, otherwise the agents' policy wins the race
  again, which is the incident. Over-eager kills of a legitimate suite are the price; admission
  (W6b) keeps the situation rare and the kill is explained (W6a).
- **Duration rounds up** (`div_ceil`): 5 s gives 3 s, 3 s gives 2 s, 2 s gives 1 s; whole
  seconds because systemd prints and oomd documents whole seconds, and `1 s` is the property's
  minimum.
- **Runtime alignment only for the daemon scope**, and only when the policy is wanted: nothing
  removes a stale policy when the user turns `memory_policy` off while a scope survives (it
  vanishes with the scope).
- The `omit` attribute is on `daemon/` only (a leaf; not recursive), never on `jobs/<id>`.

## Tests (none creates memory pressure)

- `detect::tests::gates_policy_has_the_same_limit_and_a_shorter_duration`,
  `gates_policy_follows_the_agents_override` (T-W6-10: never below 1 s, shorter than the agents'
  for every duration from 2 s to 120 s, equal only at 1 s).
- `tree::tests::the_daemon_scope_carries_the_gates_policy_only_when_supported` (property list of
  `start_transient_scope_args` for the daemon scope: present only when supported),
  `settings_follow_capabilities` (daemon policy follows the agents': absent on systemd 255,
  without the memory controller, without oomd, with `memory_policy = "none"`),
  `the_daemon_leaf_is_omitted_before_the_daemon_moves_in` (tempdir xattr; skipped with a message
  when the filesystem has no user xattrs).
- `systemd::tests::oom_assignments_for_an_existing_scope`.
- Doctor: `the_gates_policy_line_names_limit_durations_and_the_omit_mark`,
  `the_gates_policy_facts_read_the_daemon_leaf`.
- Proto: `the_gates_oom_policy_is_optional_and_printed`, the containment fixture test.
- Live T-W6-13 (`crates/sluice-gate/tests/containment_daemon.rs`,
  `the_gates_scope_has_a_policy_ahead_of_the_agents_and_the_daemon_is_omitted`): a real user
  manager, a test daemon with the usual `sluice-test-<pid>*` units stopped by `Drop`, no memory
  load; asserts `ManagedOOMMemoryPressure=kill`, the same limit and the half duration with
  `systemctl --user show` on the daemon scope when oomd can act (and that no policy exists
  otherwise), and `user.oomd_omit` on `daemon/`. Skips through `Stack::new` (needs a full
  containment host). Not part of T-W6-13: the `memory.oom.group` and adj assertions already live
  in W6a's tests; `oomctl` listing is covered by the `show` assertion.

## Docs

`kb/sluice/design/containment.md` (§1, §4, §7, §8), `docs/configuration.md`,
`docs/debugging.md`, `docs/compatibility.md`, ADR-0019 (piece 3 note).

## Open

Systemd 255 image, global OOM with `oom.group` and adj, swap/zram variants (T-W6-15/17-19) need
a disposable VM.
