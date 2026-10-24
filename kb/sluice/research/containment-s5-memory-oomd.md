---
title: "Containment spike S5: memory policy around systemd-oomd"
tags: [sluice, research, containment, systemd-oomd, memory, psi, spike]
---

# Containment spike S5: memory policy around systemd-oomd

Spike S5 of [[sluice/plan/containment-plan.md]], feeding
[[sluice/design/adr/0017-agent-resource-containment.md]] point 4. Trigger:
[[sluice/research/containment-s2-incident.md]]. Sibling:
[[sluice/research/containment-s4-fallbacks.md]].

Method: documentation and read-only inspection only. No memory experiment, no
unit with memory settings, no setting changed. Evidence is marked **measured**
(read-only commands here: Ubuntu 26.04.1, systemd 259.5, kernel 7.0.0-38,
31 GB RAM, 8 GB swap file), **source** (systemd and Linux source read from the
primary repositories), **documented** (man page or web page, URL given) or
**unverified** (inference, or needs the disposable VM). Section 8 lists what is
still open.

## 1. Summary

1. **Why the desktop died, refined by the source.** `systemd-oomd` ranks the
   candidates of a pressure kill by pgscan rate first and memory usage second.
   The incident's victim had a pgscan rate of zero. A cgroup throttled by
   `MemoryHigh` with no swap sleeps in `mem_cgroup_handle_over_high()` between
   `psi_memstall_enter()` and `psi_memstall_leave()`: it creates memory stall
   without scanning pages. The real cause of the pressure was therefore
   invisible to the ranking, which degraded to "largest leaf cgroup", and the
   largest leaf was `session.slice/dbus.service` (10.3 GB). This is an
   inference that fits every journal line; the test that proves it (T1) is
   open.
2. **Only descendants of the monitored cgroup can be killed**, and only leaf
   cgroups or cgroups with `memory.oom.group=1`. A cgroup monitored with
   `ManagedOOMMemoryPressure=kill` therefore confines the kill to its own
   subtree. It is the only mechanism that, by construction, keeps the action
   on agents and off the desktop.
3. **A user manager can opt its own units in**, without root: it reports units
   with `ManagedOOMMemoryPressure=kill` (and limit, duration) to `systemd-oomd`
   over varlink and `oomd` accepts them when the cgroup is owned by the
   sender's uid. Scope, service and slice units support it.
4. **`ManagedOOMPreference=avoid|omit` is honoured for user-owned cgroups only
   under a user-owned monitored ancestor** (for example `user@1000.service`),
   is not recursive, has no "prefer to kill" value, and is ignored when the
   monitored cgroup is root-owned (`-.slice`) and for swap kills.
5. **`MemoryLow` cannot protect the desktop from the user manager**: protection
   needs the same allocation on all ancestors, and `user.slice` and `-.slice`
   are root-owned.
6. **Defaults differ by distribution**: Ubuntu and Fedora ship the 50 %
   `user@.service` policy; Arch ships `systemd-oomd` without configuration and
   Debian packages it separately. Without `systemd-oomd`, a memory limit is
   kernel throttling only.
7. **Recommendation** (section 7): 0.5.0 ships memory accounting and no memory
   limit by default, a Sluice-side pressure policy, a read-only `doctor` check,
   and an opt-in `protect` tier (`ManagedOOMMemoryPressure=kill` on
   `sluice-agents.scope`) that stays experimental until the VM tests pass.

## 2. The incident, read against the source

Read-only journal of the incident (`journalctl -u systemd-oomd`, measured):

```
Considered 70 cgroups for killing, top candidates were:
  Path: /user.slice/user-1000.slice/user@1000.service/session.slice/dbus.service
    Pressure: Avg10: 0.00, Avg60: 0.00, Avg300: 0.00, Total: 2s
    Current Memory Usage: 10.3G
    Pgscan: 26526455
    Last Pgscan: 26526455
Killed .../session.slice/dbus.service due to memory pressure for
  .../user@1000.service being 77.76% > 50.00% for > 20s with reclaim activity
```

- `Pgscan == Last Pgscan` means `oomd_pgscan_rate()` is 0 for the victim. The
  candidates are sorted by `compare_pgscan_rate_and_memory_usage` (source,
  `src/oom/oomd-util.h`): preference class, then pgscan rate descending, then
  `current_memory_usage` descending. The dump prints candidates up to the one
  killed, so the victim was first: nothing in the subtree had a higher pgscan
  rate, and `dbus.service` was the largest.
- The trigger metric is the `full` line of `memory.pressure`
  (`read_resource_pressure(p, PRESSURE_TYPE_FULL, ...)`, source,
  `oomd-util.c`): "the share of time in which all non-idle tasks are stalled on
  a given resource simultaneously"
  ([kernel PSI doc](https://docs.kernel.org/accounting/psi.html)). A desktop is
  mostly asleep, so the only non-idle task in the whole `user@1000.service`
  subtree was the throttled test process, and its stall became the ancestor's
  stall (77.76 %). The ancestor's pressure says nothing about who causes it.
- The throttle path (Linux `mm/memcontrol.c`, `mem_cgroup_handle_over_high`,
  source): "Reclaim didn't manage to push usage below the limit, slow this
  allocating task down." followed by `psi_memstall_enter(&pflags);
  schedule_timeout_killable(penalty_jiffies); psi_memstall_leave(&pflags);`.
  A sleep inside memstall is stall without page scanning. The kernel documents
  the other side: "Going over the high limit never invokes the OOM killer"
  ([cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)).
- Current state here (measured): `oomctl` shows one monitored cgroup,
  `/user.slice/user-1000.slice/user@1000.service`, limit 50 %, duration 20 s,
  from `/usr/lib/systemd/system/user@.service.d/10-oomd-user-service-defaults.conf`
  (`ManagedOOMMemoryPressure=kill`, `ManagedOOMMemoryPressureLimit=50%`) and
  `/usr/lib/systemd/oomd.conf.d/10-oomd-defaults.conf`
  (`DefaultMemoryPressureDurationSec=20s`). No swap-monitored cgroups
  (`Swap Monitored CGroups:` empty). `app.slice`, `session.slice` and
  `background.slice` report `ManagedOOMMemoryPressure=auto`,
  `ManagedOOMPreference=none`, `MemoryHigh=infinity`; `user@1000.service` has
  `MemoryLow=0`. The cgroup2 mount has `memory_recursiveprot`. Both
  `user@1000.service` and `dbus.service` are owned by uid 1000.

## 3. How systemd-oomd decides (primary sources)

| Question | Answer | Source |
|---|---|---|
| Who is monitored | Cgroups of units with `ManagedOOMMemoryPressure=kill` or `ManagedOOMSwap=kill`. An `auto` unit is not monitored but "if an ancestor cgroup has one of these properties set to kill, a unit with auto can still be a candidate" | `systemd.resource-control(5)`, local man 259 |
| Who can be killed | "only descendant cgroups are eligible candidates for killing; the unit with its property set to kill is not a candidate (unless one of its ancestors set their property to kill). Also only leaf cgroups and cgroups with memory.oom.group set to 1 are eligible candidates" | `systemd-oomd.service(8)`; `recursively_get_cgroup_context()`, `src/oom/oomd-manager.c` |
| Ranking of a pressure kill | preference, then pgscan delta between two candidate refreshes, then memory usage. Cgroups with pgscan 0 and usage 0 are skipped. Selection uses the triggering cgroup's path as prefix | source `oomd-util.h`, `oomd_select_by_pgscan_rate()` |
| Trigger | `full` avg10 above the limit for the whole duration, and reclaim activity (pgscan increase) inside the monitored cgroup within the last 30 s; polled every 1 s | source `oomd_pressure_above()`, `monitor_memory_pressure_contexts_handler()`, `RECLAIM_DURATION_USEC` |
| After a kill | 15 s post-action delay (`POST_ACTION_DELAY_USEC`); at most one kill per iteration ("return as soon as we kill one") | source |
| Several monitored cgroups over limit | They are tried one after another in `SET_FOREACH` order and the first successful kill wins; the victim is always under the target that triggered it | source |
| Limit and duration per unit | `ManagedOOMMemoryPressureLimit=` (0 % means the `oomd.conf` default). `ManagedOOMMemoryPressureDurationSec=` ("at least 1s", "Added in version 257"); before that only `DefaultMemoryPressureDurationSec=` (30 s default; 20 s on Ubuntu) | `systemd.resource-control(5)`, `oomd.conf(5)` |
| Units of a user manager | The user manager sends its units' settings over varlink (`MANAGER_IS_USER`, `src/core/varlink.c`); `oomd` checks `cg_path_get_owner_uid(path) == sender uid` and answers EPERM otherwise. `can_set_managed_oom` is true for scope, service and slice | source `varlink.c`, `oomd-manager.c`, `scope.c`, `service.c`, `slice.c` |
| `ManagedOOMPreference` | Sets xattr `user.oomd_avoid` or `user.oomd_omit` on the cgroup (comment in `cgroup.c`: "The 'user.*' xattrs can be set from a user manager"). For memory-pressure candidates it is respected "if the unit's cgroup is owned by root, or if the unit's cgroup owner, and the owner of the monitored ancestor cgroup are the same"; for swap candidates only if root-owned; "not applied recursively"; needs xattr support | `systemd.resource-control(5)`; `oomd_fetch_cgroup_oom_preference()`; `cgroup_oomd_xattr_apply()` |
| Swap | "It is highly recommended for the system to have swap enabled ... Without swap, the system enters a livelocked state much more quickly and may prevent systemd-oomd from responding in a reasonable amount of time." Swap actions are ignored without swap. Swap kill needs both memory and swap over `SwapUsedLimit=` (90 %) | `systemd-oomd.service(8)`, `oomd.conf(5)` |
| Placement advice | "if you intend to enable monitoring and actions on user.slice, user-$UID.slice, or their ancestor cgroups, it is highly recommended that your programs be managed by the systemd user manager"; "something like user@$UID.service may prefer a much lower value like 40%" | `systemd-oomd.service(8)` |

Granularity in Sluice's tree: a session sub-cgroup holds two leaves (`ui`,
`tools`) and is not a leaf. Without `memory.oom.group=1` `oomd` kills one
leaf, chosen by pgscan rate then size: normally the hog's `tools`, but nothing
stops it choosing a `ui` leaf. With `memory.oom.group=1` the whole session
(agent included) becomes one candidate, which breaks D4 ("never kill the
agent").

## 4. Non-English and community sources

- Fedora, origin of the 50 % user policy
  ([Changes/EnableSystemdOomd](https://www.fedoraproject.org/wiki/Changes/EnableSystemdOomd)):
  "For memory pressure configuration, this will be ManagedOOMMemoryPressure=kill
  and ManagedOOMMemoryPressureLimit=50% on user@.service." Field experience
  ([fedora-workstation #358, "systemd-oomd seems overly aggressive"](https://www.pagure.io/fedora-workstation/issue/358)):
  kills of Evolution, Firefox, "a whole desktop session" and `dnf` with memory
  left; swap kill dropped in F37 ("The swap-based policy was indeed too
  aggressive since the Fedora's default swap configuration was small"); the
  limit raised from 50 % to 80 % keeping the short window (rawhide 2023-05-11,
  systemd-253.4-1.fc38); a 5-minute window rejected ("We should still aim to
  kill something quickly when the system is truly out of control, before the
  system locks up"); freezing instead of killing rejected ("freezer doesn't
  actually free memory"); `omit`/`avoid` for critical services discussed and
  not set by anyone. Lesson: vendor limits are a moving target and disliked, so
  a design must not depend on a specific vendor value.
- Ubuntu 22.04 cycle: swap kill disabled by default
  ([Phoronix](https://www.phoronix.com/news/Ubuntu-Drops-Swap-Kill)); the user
  session policy is 50 % and 20 s
  ([Phoronix](https://www.phoronix.com/news/Ubuntu-22.04-Systemd-OOMD)); the
  per-launch workaround for apps is `systemd-run --user -p
  ManagedOOMPreference=avoid firefox`
  ([ubuntu-devel, "systemd-oomd issues on desktop", June 2022](https://lists.ubuntu.com/archives/ubuntu-devel/2022-June/042159.html),
  found through a French-language query; the thread is in English).
- Upstream change that made `ManagedOOMPreference` usable by desktops: Canonical's
  [systemd#23764](https://github.com/systemd/systemd/pull/23764), reported by
  [Phoronix, 2 Sep 2022](https://www.phoronix.com/news/OOMD-Preference-All-Cgroups):
  "ManagedOOMPreference was only honored for cgroups owned by root"; goal "allow
  desktop environments to have some control over which applications are
  targeted (or avoided)". The shipped rule is the narrower same-owner rule
  quoted in section 3.
- Arch: `systemd-oomd` is in the `systemd` package with no default
  configuration; the maintainer declined non-upstream configs
  ([FS#71546](https://bugs.archlinux.org/task/71546)). Debian publishes the man
  pages under a separate `systemd-oomd` package
  ([manpages.debian.org](https://manpages.debian.org/trixie/systemd-oomd/systemd-oomd.8.en.html));
  whether it is installed or enabled by default is unverified (inference: not
  installed by default).
- Searches in Russian, Chinese and Spanish returned only translations or
  mirrors of the man pages (for example Arch's German/Arabic man mirrors and
  the `oomd.conf(5)` pages) and generic blogs such as a third-party
  [StackHarbor tuning guide](https://stackharbor.com/en/knowledge-base/systemd-oomd-memory-pressure-tuning/)
  (not authoritative). They added no fact beyond the English man pages and
  the source; the Russian query surfaced the same warning about monitoring user
  slices and the 40 % advice for `user@`. No report of the exact failure
  (throttle-induced pressure attributed to the user manager) was found.

## 5. The four options

Common assumption: containment (ADR-0017) puts agents in `sluice-agents.scope`
under `user@1000.service`, with `ui`/`tools` leaves per session. That alone
shrinks the blast radius of any `oomd` kill from "10 GB leaf with the session
bus" to "one leaf of one session" (source: leaf granularity).

### (a) `ManagedOOMMemoryPressure=kill` on `sluice-agents.scope`, lower limit and shorter duration

- Action: `oomd` monitors the scope's own `full` pressure; above the limit for
  the duration, with reclaim activity inside the scope, the candidates are only
  the scope's descendant leaves. The desktop is excluded by construction.
- Who can set it: the user, through the user manager. The duration property
  needs systemd 257 or later; before that the global 20 s (Ubuntu) or 30 s
  applies.
- Ordering against the 50 % policy of `user@UID.service`: a throttled hog
  stalls 100 % of its own scope's non-idle time, so the scope and the
  ancestor cross their limits within the same second, whatever the numbers. A
  lower limit therefore does not make the scope act first; only a shorter
  duration does. With systemd older than 257 both durations are 20 s and the
  winner is the `SET_FOREACH` order (source), which is not guaranteed. This is
  the main weakness of (a) on Ubuntu 24.04 (systemd 255) and Debian 12 (252).
- Failure modes:
  1. No reclaim activity inside the scope (pure sleep-throttle without a scan):
     the 30 s reclaim check skips the action, and the user-manager policy is
     left to kill the biggest leaf (the incident). Open: T3.
  2. Wrong leaf: ranking is by pgscan then size, not by "who caused it", so a
     `ui` leaf or another session's leaf may be chosen. Mitigation: set
     `user.oomd_avoid` on every `ui` leaf, an xattr Sluice writes itself on
     directories it created (user-owned, under a user-owned monitored ancestor,
     honoured per the man page). It needs `user.*` xattr support on cgroupfs;
     `cgroup.c` notes that older kernels had none, minimum version unverified.
     Open: T6.
  3. Over-eager kills: Fedora #358 shows a short window kills legitimate heavy
     work. Opt-in only.
  4. No `systemd-oomd` (Arch, Debian by default): the setting is stored and
     inert. No harm, no protection.
  5. Swap kill (`ManagedOOMSwap=kill` on an ancestor): ranked by swap usage,
     and user xattrs are ignored for swap candidates, so a user cannot steer it.
     Not active here (no swap-monitored cgroups).

### (b) `ManagedOOMPreference=avoid|omit` on desktop units

- User-settable: yes for user-manager units under a user-owned monitored
  ancestor (true for `user@1000.service`); no when `oomd` monitors a
  root-owned cgroup such as `-.slice`; never for swap candidates.
- Reach: one cgroup, not recursive. The desktop is many transient scopes and
  services (GNOME starts apps as separate scopes), so "protect the desktop"
  means protecting each launch, which is why Ubuntu's workaround is
  per-launch. `avoid` only reorders (avoided cgroups are killed last, not never);
  `omit` can leave `oomd` with no candidate so nothing relieves the pressure.
- Cost: Sluice would edit or override units it does not own (`dbus.service`,
  the shell, browser scopes): invasive, racy against desktop updates and
  against "no configuration by each developer".
- Verdict: not a default. Acceptable only as the inverse (`avoid` on Sluice's
  own `ui` leaves) or as a documented optional user drop-in for
  `dbus.service`.

### (c) No memory limit; per-session accounting and a Sluice-side policy

- Mechanism: no `MemoryHigh`, `MemoryMax` or `memory.oom.group`. Sluice reads
  `memory.current`, `memory.peak` and `memory.pressure` per session and of
  `user@UID.service`; the scheduler, which already uses PSI for admission,
  holds heavy jobs when pressure is high; `status` and `doctor` name the
  heaviest session; at most a warning to the agent.
- Why it is safe: Sluice creates no throttle and therefore no pressure. If
  memory really runs out, a runaway is reclaiming, so its pgscan rate is
  non-zero and the existing `oomd` ranking puts its small leaf ahead of the
  desktop. Unverified (T2) but consistent with the ranking source.
- Failure modes: Sluice's loop is polling and cannot stop a fast allocation
  spike; escapees are limited only by the kernel OOM killer and `oomd`; without
  swap and without `oomd` a runaway can still freeze the machine, as today
  (not worse); any Sluice-initiated kill is a new lethal action in product code
  and needs its own design and an opt-in.
- Works on every distribution, with or without `oomd`.

### (d) `MemoryHigh` only together with swap; `MemoryLow` for the desktop

- `MemoryHigh` with swap: the kernel code differs. "If reclaim is making
  forward progress but we're still over memory.high, we want to encourage that
  rather than doing allocator throttling" (source). With swap the hog's anon
  pages are reclaimed, which scans pages and raises its pgscan rate, so `oomd`
  ranks it correctly; the sleep-throttle (the incident) only happens when
  reclaim cannot push usage under the limit: no swap, `MemorySwapMax=0`, swap
  full. Rule if ever enabled: `SwapTotal > 0`, swap not nearly full, no
  `MemorySwapMax=0`. Zram behaviour is unverified. Cost: heavy swap I/O (a slow
  machine instead of a dead one); ancestors' pressure still rises meanwhile.
- `MemoryLow`: "For a protection to be effective, it is generally required to
  set a corresponding allocation on all ancestors" (`systemd.resource-control(5)`).
  The ancestors are `user@1000.service` (here `MemoryLow=0`), `user-1000.slice`,
  `user.slice`, `-.slice`; the last three are root-owned. With
  `memory_recursiveprot` mounted (measured) protection flows down, but a root
  allocation on `user.slice` or `user@.service` is still what makes it mean
  anything, so only an administrator drop-in can do it. It also protects the
  desktop's pages from reclaim, not from an `oomd` kill, and is best effort
  ("won't be reclaimed unless there is no reclaimable memory available in
  unprotected cgroups").

### Comparison

| Option | Keeps `oomd` kill off the desktop | Root or admin needed | Needs systemd-oomd | Safe without swap | Risk to agent work | Zero config |
|---|---|---|---|---|---|---|
| (a) kill policy on `sluice-agents.scope` | Yes by subtree confinement, if it triggers first and sees reclaim activity | No | Yes (inert otherwise) | Unknown (T3) | Medium: kills a leaf at 20-40 % | Yes |
| (b) `ManagedOOMPreference` on desktop | Partly: per cgroup, reorders only | No (same owner); root for root-monitored | Yes | Yes | Shifts to other apps | No: edits desktop units |
| (c) accounting and Sluice policy | Indirectly: creates no pressure; real pressure kills the hog by pgscan | No | No | Yes | Low: no kill by default | Yes |
| (d) `MemoryHigh` with swap | No alone; fixes attribution | `MemoryLow` needs admin | Helps | No (the incident) | Low | Partly: swap detection |

## 6. Interaction facts worth keeping

- Pressure is hierarchical, guilt is not: a stalled cgroup raises the `full`
  pressure of every ancestor when everything else idles, while `oomd` assigns
  guilt by pgscan, not by PSI.
- Sleep-throttle (`memory.high` with failing reclaim) yields PSI without
  pgscan. A hard `memory.max` yields an in-cgroup kernel OOM kill instead,
  which acts inside the cgroup only (`memory.oom.group` decides process or
  group) and is the other way to make the action fall on the agent; it
  contradicts D4 and stays an unverified opt-in (T5).
- Version gates: per-unit duration 257; `ManagedOOMPreference` 248; the D-Bus
  `Killed` signal 252 (from a search summary, unverified); the exact version in
  which user managers report to `oomd` was not established. Ubuntu 22.04 has
  systemd 249, Ubuntu 24.04 has 255, Debian 12 has 252 (from memory of the
  distribution tables, unverified here): none has the per-unit duration.
- Current systemd source has an `OOMRules=` ruleset mechanism
  (`ruleset_execute_action`), absent from the 259 man page: not a basis for a
  design.

## 7. Recommendation for 0.5.0

Default, nothing that can throttle or kill:

1. No `MemoryHigh`, no `MemoryMax`, no `memory.oom.group` on any Sluice cgroup.
   Read `memory.current` and `memory.peak` per session for `sluice status`;
   `pids.max` stays. ADR-0017 point 4 becomes: "memory: accounting only;
   throttling is opt-in and conditional on swap".
2. Sluice-side policy (option c): the daemon watches `memory.pressure` of
   `user@UID.service` and `memory.current` per session; admission holds heavy
   jobs under high pressure (existing behaviour); `status` and `doctor` name the
   heaviest session. No kill by Sluice in 0.5.0.
3. `sluice doctor`, read-only: whether `systemd-oomd` is active, the monitored
   cgroups and limits (`oomctl`), swap total, and a warning "a memory limit
   without swap can make oomd kill an unrelated cgroup" when the user's own
   configuration sets one. Doctor changes nothing.
4. Opt-in `[containment] memory_policy`, off by default and documented as
   experimental until the VM tests pass:
   - `protect`: `ManagedOOMMemoryPressure=kill`, limit 30 %, duration 10 s
     (duration only when systemd >= 257) on `sluice-agents.scope`, and
     `user.oomd_avoid` on every `ui` leaf. Requires `oomd` active and
     `SwapTotal > 0`; otherwise doctor says why it is off.
   - `throttle`: adds a per-session `MemoryHigh`, only with swap present,
     and not on zram-only until tested.
5. Sluice never touches desktop units (b) or `MemoryLow` (d). The team rollout
   guide documents optional administrator drop-ins (`user@.service.d/` with
   `ManagedOOMMemoryPressureLimit=`, and `MemoryLow` if a team wants it).
6. No product test sets memory limits; memory tests live only in the VM matrix
   below.

Rationale: the only provably safe change is the one that creates no pressure;
containment already makes any kill small; the kill-confining mechanism (a) is
sound on paper but depends on two behaviours (reclaim activity during
sleep-throttle, duration ordering) that only a VM can confirm.

## 8. Open items: disposable-VM test list (not run)

Environment: throwaway VM, Ubuntu 26.04 (systemd 259), repeat on a systemd 255
image and on Debian 13 if possible; 4 GB RAM, with and without a 2 GB swap
file; a real `user@UID.service`; `systemd-oomd` active with vendor defaults; a
sleeping 1 GB process in `app.slice` standing for "the desktop"; observe with
`oomctl`, `journalctl -u systemd-oomd`, `memory.pressure`, `memory.stat`.

| # | Test | Expected outcome |
|---|---|---|
| T1 | Baseline: transient scope with `MemoryHigh=200M`, `MemorySwapMax=0`, process asks 600 MB; vendor policy only | Incident reproduced: the 1 GB sleeper is killed, the hog is not. The hog's `pgscan*` in `memory.stat` stays flat, confirming the pgscan-rate-0 explanation |
| T2 | No `MemoryHigh`; hog asks for more than free RAM, swap present | `oomd` or the kernel kills the hog's leaf (high pgscan rate); sleeper survives. Confirms option (c) |
| T3 | Option (a): `sluice-agents.scope` with kill, 30 %, 10 s (systemd 257+); hog in a `tools` leaf with `MemoryHigh=200M`, `MemorySwapMax=0` | `oomd` marks a leaf under the scope, not the sleeper. If "No reclaim activity" appears or the sleeper dies, (a) fails without swap and `protect` must require swap |
| T4 | T3 on systemd 255 (global 20 s for both policies) | Records which target acts when both are over limit; expected not guaranteed. Decides whether `protect` is allowed below 257 |
| T5 | Hard `MemoryMax` on a session cgroup, `memory.oom.group` 0 and 1, hog allocating | In-cgroup OOM kill of the hog (or group), no ancestor-pressure kill. Decides whether a hard limit is a safe opt-in |
| T6 | As the unprivileged user, `setfattr -n user.oomd_avoid -v 1` on a user-created leaf under the scope; pressure kill with two leaves | The xattr is accepted; the avoided leaf ranks last. Records the minimum kernel |
| T7 | `MemoryHigh=200M` with swap (file, then zram), hog asks 600 MB | Hog swapped, pgscan rate high; if `oomd` acts it picks the hog; machine slow but alive; sleeper untouched. Records the slowdown |
| T8 | `systemd-oomd` absent or stopped, T1 and T3 | No `oomd` kills; hog throttled; VM stays responsive; `doctor` reports `oomd` inactive |
| T9 | Daemon policy (c): admission held above a pressure threshold; hog started outside the queue | Admission pauses; `status` names the session; nothing killed |
| T10 | Two agent processes, one hog: shared leaf (as today) vs own scope; `oomd` pressure kill | With containment only the hog's leaf dies; without it the shared leaf dies. Quantifies the benefit of containment |
| T11 | Session with `ui` and `tools`, `memory.oom.group` 0 then 1, under (a) | 0: only the chosen leaf dies; 1: both die. Decides leaf vs group granularity |
| T12 | `ManagedOOMSwap=kill` on the root slice, swap exhausted, user xattr on a leaf | Victim is the biggest swap user and the user xattr is ignored (root-only rule). Confirms (b) cannot steer swap kills |

Other open items:

- Debian's and Arch's current defaults for `systemd-oomd` (installed, enabled):
  check on current images.
- Whether Fedora and Ubuntu still ship 50 %: the 80 % value is the Fedora change
  in #358 and a CentOS Stream commit seen only through a search summary.
- The minimum kernel for `user.*` xattrs on cgroupfs and the version in which
  user managers report to `oomd`.
- Session sizes for any future `memory_high` default come from the CPU rerun of
  S2, not from this spike.
- Ubuntu 22.04 (systemd 249): keep memory containment off there.
