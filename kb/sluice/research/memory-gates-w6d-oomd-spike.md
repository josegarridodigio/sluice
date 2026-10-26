---
title: "W6d spike: systemd-oomd ordering and omit/avoid, measured on two machines"
tags: [sluice, research, containment, memory, systemd-oomd, spike, w6]
---

# W6d spike: systemd-oomd ordering and omit/avoid

Decides the open item of [[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]]
piece 3 (tests T-W6-14 and T-W6-16 of [[sluice/research/memory-gates-w6-analysis.md]]).
Run on 2026-10-26 with the user's consent, on the developer machine and on the second
machine of the incident, following the S5 §9 rules
([[sluice/research/containment-s5-memory-oomd.md]]).

## Method

A Python script (kept in the session scratchpad; outline below) creates its own
transient scopes `sluice-w6-<pid>-<name>.scope` in the user manager, each with
`Delegate=yes`, `ManagedOOMMemoryPressure=kill`, limit 5 % and a per-unit duration;
inside, leaves with `memory.high = 64M` and `memory.swap.max = 0`; in each leaf a
Python allocator asking for 200 MiB, so it is throttled in its leaf (pressure inside
the unit only, never a global shortage). xattrs set with `os.setxattr`. Each test holds
at most 8 s; a watchdog stops it when the user manager's `full` memory pressure
(avg10) reaches 25 %; every unit is stopped at the end and the script waits 16 s
(oomd's 15 s post-action delay) between tests. Victims read from the allocators'
exit and from `journalctl -u systemd-oomd`.

On the second machine the script must run inside the user manager
(`systemd-run --user --scope`): started from an SSH login session its children live in
`session-N.scope`, and moving them into a user-manager cgroup is refused (EACCES: the
common ancestor `user-UID.slice` is root-owned). Sluice's own placement never does this
(gate jobs start inside the daemon scope through `cg-exec`).

Machines: both Ubuntu 26.04, systemd 259, kernel 7.0, about 30 GiB RAM, 8 GiB swap.
The developer machine (20 threads) had another agent's test suite and two agent sessions
running; oomd monitored only `user@UID.service` (50 %, 20 s). The second machine
(12 threads) was idle (no gate running); oomd monitored only `sluice-agents.scope`
(30 %, 5 s).

## Results

| Test | Setup | Developer machine | Second machine |
|---|---|---|---|
| T-W6-14 | scope "gates" (1 s) and scope "agents" (3 s), both 5 %, one throttled allocator each | oomd killed `gates/job`; `agents` untouched; user@ full peak 12.9 % | same victim; user@ full peak 29.8 % (watchdog after the kill) |
| T-W6-16a | one scope: leaves `daemon` (`user.oomd_omit`), `job`, `ui` (`user.oomd_avoid`), all throttled | killed `job` | killed `job` (watchdog after the kill) |
| T-W6-16b | leaves `daemon` (omit) and `ui` (avoid) only | killed `ui` | killed `ui` (watchdog after the kill) |
| T-W6-16c | leaf `daemon` (omit) only | nothing killed | nothing killed |

No oomd or kernel kill outside the test units on either machine (journal checked for
the window; units and cgroups gone afterwards).

## Conclusions

1. **Duration orders monitored units** (systemd 259): with two monitored scopes over
   their limit, oomd acted on the shorter-duration one first; within the 15 s
   post-action delay the other was left alone. The ADR's rule (gates' duration =
   max(1 s, agents' / 2)) gives the order the design needs.
2. **`user.oomd_omit` excludes a leaf** (T-W6-16b/c): never chosen, even when it is the
   only leaf under pressure (then nothing is killed). **`user.oomd_avoid` only
   reorders**: the avoided leaf is killed when no other candidate exists. This closes
   the S5 T6 gap (the ranking effect had only been inferred).
3. **Piece 3 ships as designed** (policy on `sluice-daemon.scope`, shorter duration,
   `omit` on `daemon/`); variant B (one policy on `sluice.slice` with `avoid` on agent
   leaves) is not needed: avoid would not protect agents absolutely anyway (16b).
4. **On an idle machine a contained throttle shows up as user-manager pressure.** With
   nothing else running, the throttled allocators were the only non-idle tasks of
   `user@UID.service`, so its `full` pressure reached about 30 % within seconds (S5 §2
   explains why). A vendor policy on `user@UID.service` (50 %, 20 s) would only act
   after 20 s; the gates' policy acted within 1–2 s. Admission (piece 1) keeps gates
   from reaching this state; the policy is the backstop.
5. Not covered here (VM only): systemd < 257 (no per-unit duration), machines without
   swap or with zram, and the global OOM killer with `oom.group` + `oom_score_adj`
   (T-W6-15, 17–19).
