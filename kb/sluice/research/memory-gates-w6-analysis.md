---
title: "W6 analysis: memory safety of gate jobs (design validation)"
tags: [sluice, research, containment, memory, systemd-oomd, scheduler, admission, queue-trust, w6]
---

# W6 analysis: memory safety of gate jobs

Read-only analysis for wave W6 of [[sluice/plan/queue-trust-plan.md]]. Nothing was run
(no cargo, no tests, no memory experiment, no daemon touched). Evidence is marked
**measured** (the incident figures given for the second machine, and the S5 live run on
the developer machine), **source** (code of this repository, or kernel/systemd source
read for this note), **documented** (man page or kernel doc), **inference** (reasoned,
not verified) or **open** (needs a test, section D). Paths are relative to `crates/`;
line numbers are those of the working tree on 2026-10-26 (another agent is editing the
queue, proto and init crates, so those lines may move).

Incident: second machine, Ubuntu 26.04, systemd 259, kernel 7.0, 12 threads, 30 GiB RAM,
8 GiB swap (full), five agents, Sluice 0.5.0. One `pnpm test` gate (`node --test`, one
worker per core minus one, 1.5 to 2.7 GiB each) peaked at 18 GiB in its job leaf, the
daemon scope at 24.5 GiB; two or three such gates were admitted together (weight
`auto`→6, budget 12, one "over"); memory PSI about 0 at admission; `oomctl` listed only
`sluice-agents.scope` (30 %, 5 s); oomd killed agents' `tools` leaves, then the global
OOM killer killed Chrome in agents' leaves (adj 300) and an unrelated user service; the
gate's workers have adj 200.

## 0. Verdict in one page

| # | Candidate piece | Verdict |
|---|---|---|
| 3 | Memory-aware admission | Sound and the only piece that prevents the incident instead of reacting to it. Needs corrections: measure anon+swap, not `memory.peak` alone (page cache inflates it); count only RAM; extend the head reservation to memory; unknown first runs use a proxy from the weight. |
| 2 | adj raise + `memory.oom.group=1` on job leaf | Sound for the **global** OOM killer (source confirmed). Does nothing for oomd (it does not read `oom_score_adj`). adj can only be raised, never lowered below the inherited value. `oom.group` on the leaf contradicts S5 §7 item 1 only superficially (S5 worried about session groups containing the agent). |
| 1 | oomd policy on `sluice-daemon.scope` | Sound in mechanism (S5 T0/T3), but **it may not trigger when it matters**: oomd triggers on the scope's own `full` pressure, and nothing measured says the gate scope reached 30 % in the incident (the agents scope reached 92.69 %). Keep it as a backstop, add a read-only measurement first (T-W6-0), and consider variant B (policy on `sluice.slice` + `avoid` on everything that is not a job leaf). Only for systemd ≥ 257. |
| 4 | Outcome reporting | Needed, and bigger than stated: the existing `MemoryWatcher` will **miss** nearly every gate kill (the leaf is removed when the job ends, the watcher polls every 10 s), and oomd kills never appear in `memory.events`. Classification must happen in `launch()` at job end. |
| 5 | Opt-in `gates_memory_max` | Sound as an opt-in with no default. Contradicts the W6 text of the plan (which says computed default); recommend the opt-in. |
| extra | Daemon-side guard that cancels its own newest job on real exhaustion | Not in the candidate list; the only protection when oomd is absent or systemd < 257. Optional, decision for the user (section B6). |

Order of value and testability: 3, 4 and 2 are deterministic and testable without memory
pressure; 1 and extra depend on behaviours only a bounded live test can confirm.

## A. Where each piece lives

### A1. Cgroup tree, daemon scope, xattrs

- Tree creation: `Tree::ensure_scope` (`sluice-gate-core/src/cgroup/tree.rs:505-556`), called by
  `ensure_daemon_scope` (`tree.rs:565-582`) with `first_leaf = "daemon"` and `nodes = ["jobs"]`.
  The daemon scope's `ScopeSpec` has `oom: None` (`tree.rs:573`); only
  `ensure_agents_scope` passes `self.settings.oom` (`tree.rs:599`).
- Scope properties: `ScopeSpec`/`OomProperties` (`cgroup/systemd.rs:225-251`),
  `start_transient_scope_args` (`systemd.rs:271-320`; the three `ManagedOOMMemoryPressure*`
  properties at `systemd.rs:292-310`; `Delegate=true` is always sent). The scope already
  carries `Delegate=yes`, so **systemd's `OOMPolicy` defaults to `continue`** (documented,
  local `systemd.service(5)`/`systemd.scope(5)`, systemd 259: "except for units where
  Delegate= is turned on, where it defaults to continue"). A kernel or oomd kill inside
  `jobs/<id>` therefore does not stop the scope (and the daemon). Nothing in the repository
  sets `OOMPolicy` (grep: none); do not set `kill` on the unit: it would put `memory.oom.group`
  on the scope itself and a single worker kill would take the daemon too.
- Settings: `TreeSettings` (`tree.rs:234-255`) and `from_config` (`tree.rs:263-289`). New fields:
  `daemon_oom: Option<OomProperties>`, `jobs_memory_max: Option<u64>`, `job_oom_group: bool`.
- xattrs: `OOMD_AVOID_XATTR` (`tree.rs:37`), `set_oomd_avoid` (`tree.rs:425-427`) and
  `has_oomd_avoid` (`tree.rs:431-434`); used only for `ui` leaves in `ensure_session`
  (`tree.rs:646`). Reuse for `daemon/`: generalise to `set_oomd_xattr(dir, "user.oomd_omit")`
  and call it in `ensure_scope` right after `make_dir(&leaf)` (`tree.rs:537`) when
  `first_leaf == "daemon"`, before the move at `tree.rs:541`. `daemon/` already contains the
  daemon and, via `Containment::adopt_reaper` (`daemon/src/containment.rs:286-290`), the reaper.
- Job leaf: `Tree::job_leaf` (`tree.rs:663-674`): add `write_if_present(&dir, "memory.oom.group",
  "1")` (tolerates a kernel without the file) and, with the opt-in, nothing per leaf (the
  ceiling is on `jobs/`).
- Ceiling: in `ensure_daemon_scope`, after `ensure_scope` returns, `write_if_present(jobs_dir,
  "memory.max" | "memory.high" | "memory.swap.max", ...)`. `jobs/` has `+memory` enabled for its
  children by `enable_controllers` (`tree.rs:342-357`, called at `tree.rs:553`), and the scope is
  delegated, so Sluice owns the file (single-writer rule, `design/containment.md` §1).
- Pruning: `Tree::prune` (`tree.rs:733-770`) removes empty `jobs/*` after `PRUNE_GRACE` 60 s
  (`daemon/src/containment.rs:37`); `release_job` (`containment.rs:315-322`) removes the leaf the
  moment the job ends.

### A2. Detection, oomd policy derivation

- Facts: `Facts.systemd_version`, `oomd_active` (`cgroup/detect.rs:49,61`; gathered at
  `detect.rs:639-657`). Gate: `Capabilities::oomd_policy_supported` (`detect.rs:240-247`,
  requires memory controller, `oomd_active`, `systemd_version >= OOMD_PER_UNIT_DURATION` = 257) and
  `oomd_policy_enabled` (`detect.rs:251`).
- Derivation: `DERIVED_LIMIT_CAP` 30 %, `DERIVED_LIMIT_FRACTION` 0.6, `DERIVED_DURATION_CAP` 5 s
  (`detect.rs:687-691`), `derive_pressure_policy` (`detect.rs:865-886`: limit `min(30, 0.6 × ancestor)`,
  duration `min(5 s, ancestor/4)`, floor 1 s), `system_ancestor_policy` (`detect.rs:889`),
  `pressure_policy` (`detect.rs:925`), caller `Containment::start_reporting`
  (`daemon/src/containment.rs:145-158`).
- New: `derive_gates_policy(agents: PressurePolicy) -> PressurePolicy` in `detect.rs`, same limit
  and `duration = max(1 s, agents.duration / 2)` (never below 1 s: documented minimum,
  `systemd.resource-control(5)` "at least 1s", added in 257). It is computed from the agents
  policy so a user override of `memory_pressure_duration` keeps the ordering.

### A3. `sluice cg-exec` and `oom_score_adj`

- `cg_exec::run` (`sluice-gate/src/cg_exec.rs:26-47`): `write_pid(leaf, own pid)` at line 31, then
  `Command::exec` at line 37. The adj write belongs between the two: read
  `/proc/self/oom_score_adj`, write `max(configured, current)` (a failure is only a debug log, like
  the move). `exec` keeps the value (documented, proc(5): inherited by children; `execve` does not
  reset it, source).
- The argv is built in `Containment::job_prefix` (`daemon/src/containment.rs:297-310`:
  `<exe> cg-exec <leaf> --`). Add `--oom-score-adj N` before `--`. The CLI parser for `cg-exec` is
  in `sluice-gate/src/main.rs` (not read in detail).
- Fallback without containment: `spec.wrapper` stays empty (`daemon/src/server.rs:1528` only fills it
  when `shared.containment` is `Some`), so no adj. adj works without cgroup v2; if wanted, let
  `cg-exec` accept an empty/`-` leaf and always prefix the wrapper (small change, section B7).
- The chain order is wrapper, then `nice`/`ionice` (`executor.rs:158-162` doc of `ExecSpec.wrapper`).

### A4. Interventions watcher

- `MemoryWatcher::poll` (`daemon/src/interventions.rs:68-134`) reads `oom_kill` and
  `oom_group_kill` of `memory.events` per leaf; `watch_targets` (`interventions.rs:139-171`) lists
  `s-*/*` and `jobs/*`; journal rows: `parse_oomd_journal` (`interventions.rs:209-244`), which already
  keeps paths containing `sluice.slice` and splits `jobs/<id>` as the leaf (`split_killed_path`,
  `interventions.rs:184-203`). Driver: `Containment::collect_interventions`
  (`daemon/src/containment.rs:336-348`), called by `containment_tick` (`server.rs:1139`), which runs
  every `CONTAINMENT_EVERY` = 10 s (`server.rs:1114`). Rows are stored by
  `Store::record_intervention` (`store.rs:811`), table `interventions` (`migrate.rs:36-41`).
- **Gap (source, contradicts the candidate's premise):** the watcher can only see a job leaf that
  still exists at a poll. `launch()` removes it at job end (`server.rs:1532-1534`), and the tick is
  every 10 s, so a gate killed by the kernel is recorded only if the job lasted past a poll *and*
  was killed before the previous one; in practice nearly never. oomd kills never touch
  `memory.events` (oomd sends SIGKILL / `cgroup.kill`; source-recalled, consistent with S5 T3 where
  only the journal showed the kill).
- Backstop to add: also read `memory.events` of `jobs/` and of the daemon scope (hierarchical:
  `oom_kill` of a descendant is counted in every ancestor, documented: "The number of processes
  belonging to this cgroup killed by any kind of OOM killer", kernel cgroup-v2 doc). Growth there
  without a matching job row means "a job died of OOM and we missed which".
- Note: a **global** OOM kill is also counted in the victim's leaf `memory.events`
  (`oom_kill`), because the counter is "any kind of OOM killer" (documented). So the leaf read at job
  end captures global kills too, not just in-cgroup ones; the comment at `interventions.rs:6-8`
  ("acted inside that cgroup") is incomplete.

### A5. Executor and outcome classification

- Process end: `Running::wait` (`executor.rs:~300-380`) yields `ExecStatus` (`executor.rs:~195-205`:
  `Exited`, `Signaled`, `Timeout`, `Killed`). `Killed` means **Sluice** cancelled it. A job killed by
  the OOM killer or oomd ends as `Signaled(9)` or, because `sh -c` is the leader, usually
  `Exited(137)` (inference: `sh` reports 128+9 for a killed child).
- Hook point: `launch` (`server.rs:1525-1536`), between `launch_placed` and `release_job`. Read there,
  while the leaf still exists: `memory.peak`, `memory.stat` (`anon`), `memory.swap.peak/current`,
  `memory.events` (`oom_kill`, `oom_group_kill`). Add `memory: Option<JobMemory>` to `Finished`
  (`executor.rs`, struct near line 210) or return it from `launch` next to `Finished`.
- Mapping: `classify` (`server.rs:1755-1795`, gates) and the inline `match` in `start_exec`
  (`server.rs:1655-1690`). New branch before them: status `Exited(n)`/`Signaled(_)` (never
  `Timeout`/`Killed`, those are Sluice's own signals) and (`oom_kill`/`oom_group_kill` delta > 0 **or**
  an oomd journal line naming `jobs/<id>` since the job started) gives `OutcomeStatus::Killed`
  with message `"<gate> was killed for lack of memory after 41.2s (job peak 18.1 GiB; 1.4 GiB
  available at the end)"` and `killed_by = "memory"`.
- `killed_by` plumbing today: `JobEntry.killed_by: Option<&'static str>` (`server.rs:295`) is copied
  into the outcome only when `Killed` (`close_job`, `server.rs:1365-1367`); change to
  `entry.killed_by.or(outcome.killed_by.take())` so the classifier's value survives. Stored in
  `jobs.killed_by` via `FinishMeta` (`server.rs:1397-1401`, `store.rs:926`). Wire doc of
  `Outcome.killed_by` lists the values (`proto/src/lib.rs:650-655`): add `memory` (additive).
- oomd detection at job end: if the leader ended with signal 9 or exit 137, no oom counter grew and
  the job was not cancelled by Sluice, run one bounded `journalctl` query (the existing
  `systemd::journal_oomd(env, since, user)`, `systemd.rs:456`) in `spawn_blocking`, at most one retry
  after 1 s, filtered on the job id. Only for SIGKILL-shaped endings, so no cost on normal runs.
- Interplay: `conclude` writes a miss record with reason `"killed"` for `Killed` (`server.rs:1920+`,
  `classify` returns `Some("killed")`); give it the reason `"killed for memory"`. Also record an
  `interventions` row from `launch` (cause `oom_kill` / `oomd_memory_pressure`, leaf `jobs/<id>`),
  deduplicated by the unique index `(at_ms, leaf, cause)` (`migrate.rs:40`), so the later journal read
  does not double count.
- Clients: MCP `verify` prints `KILLED after Xs (job ...) log=...` without the message
  (`sluice-gate-mcp/src/lib.rs:755`), `sluice run` maps to `Finish::Killed` (`sluice-gate/src/run.rs:635`),
  `sluice exec` prints "KILLED: the daemon stopped" for `Killed` with signo 0
  (`sluice-gate/src/exec.rs:625-631`), queue runner maps to `"killed"` (`sluice-gate-queue/src/runner.rs:1566`,
  and `prepare.rs:212`). All treat `Killed` as "the daemon stopped / cancelled". They must print
  `outcome.message` when `killed_by == "memory"`, otherwise an agent reads a memory kill as a
  transient cancel and retries into the same wall. (These files are being edited by another agent:
  coordinate.)

### A6. Scheduler admission

- Pure decision: `plan(&Snapshot)` (`daemon/src/scheduler.rs:512-633`); inputs `Snapshot`
  (`scheduler.rs:394-412`), `Pressure` (`scheduler.rs:343-356`), `QueuedSnap` (`scheduler.rs:359-379`),
  `RunningSnap` (`scheduler.rs:381-392`); weights `queued_weight` (`scheduler.rs:646-655`),
  `learned_weight` (679), `expected_ms` (699). Stateful `Scheduler::admit_with` (`scheduler.rs:967-1002`).
- Built in `server.rs::admit` (`server.rs:1215-1268`) from the PSI state and the utilisation sampler;
  called by `scheduling_loop` (`server.rs:1270-1296`, wake or every `ADMIT_TICK` 1 s, `server.rs:86`).
- Existing invariants (scheduler.md): budget `used + w ≤ B`; backfill; **reservation only for an overdue
  head blocked by the budget** (`scheduler.rs:~560`, `first_waiting && reserved.is_none() &&
  !fits_budget && overdue`); over-admission only when nothing started nominally, `cap > B`, `calm`,
  interval, idle CPUs `≥ ⌈w/3⌉`, `samples_after_last_start` (`scheduler.rs:595-632`, `635-644`); PSI blocking
  stops everything except the starvation guard (`scheduler.rs:540-554`); wait causes `WaitCause`
  (`scheduler.rs:78-130`) with five accumulators and a frozen wire `WaitReason`.
- New data: `Snapshot.memory: Option<MemoryFacts { available_bytes, total_bytes, reserve_bytes }>`
  (`None` = rule off, today's behaviour); `QueuedSnap.expected_peak: Option<u64>` and
  `QueuedSnap.weight_share` (proxy); `RunningSnap.headroom: u64`. The daemon fills them in `admit`
  from `/proc/meminfo` under `shared.config.proc_root` (a `DaemonConfig` field already faked by tests:
  `daemon/tests/daemon.rs:1479-1508`) and from the running jobs' leaves.
- Sampling: `sample_tick` (`server.rs:1042-1112`) already walks the running gates every second
  (`entry.usage.update`, `JobUsage`); add the per-job memory reading there
  (`memory.current`, `memory.stat anon`, `memory.swap.current` of the job leaf; for exec jobs too, they
  have a `stats_key`, `server.rs:1062`) into `JobUsage` and feed `RunningSnap.headroom` at the next pass.
  `Containment::tree().layout.jobs_dir().join(id)` gives the leaf.

### A7. Learning store (ADR-0015, ADR-0011 M2)

- Samples go to `gate_stats` (`migrate.rs:~60-80` `SCHED_1`; `store.rs:994-1031` insert, keep newest 20,
  `GATE_STATS_KEEP`; `store.rs:1035-1090` `gate_stats_recent`; `store.rs:1116` prune after 90 days).
  Only passing runs are samples (`learns_from`, `server.rs:1298-1300`); in memory,
  `StatsCache` (`server.rs:196-220`) keeps `(demand_milli, wall_ms)` of the last 10, loaded by
  `load_stats` (`server.rs:706-716`), refreshed in `announce_closed` (`server.rs:1431-1447`);
  `register_job` hands `demands_milli` and `expected_ms` to the scheduler (`server.rs:2635-2668`).
  The key is `(repo, gate, cmd_hash)`; exec jobs use `exec:<label>` (`server.rs:2545`).
- **Do not add a column to `gate_stats`.** That needs `sched_schema` 2, and an older binary opening the
  database fails hard with `NewerSchema` (`migrate.rs:1-18`, `store.rs:378`): the file's own header
  says additions go to a separate group. Use the containment group (additive table, no backup, old
  binaries ignore it: `store.rs:360-367`, `migrate.rs:28-47`): `CONTAINMENT_SCHEMA_VERSION` 2 with
  `gate_memory(repo, gate, cmd_hash, job_id, finished_at, outcome, peak_bytes, anon_peak_bytes,
  oom_killed INTEGER)`, newest 20 per key, pruned together with `gate_stats`.
- Learn on passing runs **and** on memory-killed runs. A killed run stopped early, so its peak is a
  lower bound, but dropping it (as `learns_from` does for failures) would leave the gate at "unknown"
  forever while it keeps dying. Failing runs that are not memory kills teach nothing (same reasoning as
  the weight, `scheduler.md` "Learned weights").
- Expected peak: `E = max(anon_peak of the last 5 usable samples)`; a memory-killed sample counts as
  `1.25 × its peak`. Needs 1 sample (a first pass is evidence enough for a memory ceiling; weights need 3
  because they are an average, a peak is a maximum). In-memory cache next to `StatsCache`; exposed as
  `Scheduler::set_learned_peak(key, bytes)` like `set_learned` (`scheduler.rs:855-866`).

### A8. Status, JSON, config, doctor

- Proto (additive, schema 1): `RunningJob` (`proto/src/lib.rs:~930-990`): `memory_bytes`,
  `memory_peak_bytes`, `memory_expected_bytes`; `QueuedJob`: `memory_expected_bytes`, and for the wait
  `wait_detail = "memory"` (existing field, `lib.rs:1021`); `Outcome`: `memory_peak_bytes`, `killed_by =
  "memory"`; `BudgetInfo`: `memory { available_bytes, reserve_bytes, headroom_bytes }`; `ContainmentInfo`
  (`lib.rs:745-760`): `gates_oom_policy`. The status schema is pinned by a fixture
  (`proto/tests/status_schema.rs`, per `psi-admission.md` "Observability"): update the fixture deliberately.
  Text rendering: `sluice-gate/src/status.rs` (`weight_text` `status.rs:208`, `containment_text`).
- Config: `[containment]` is `ContainmentConfig` (`sluice-gate-core/src/config/containment.rs:~28-100`,
  `deny_unknown_fields`), validated by `validate` (`containment.rs:~150-230`), size parser
  `parse_size_bytes` (`containment.rs:~95`). Proposed keys: `gates_memory_policy = "oomd" | "none"`
  (default follows `memory_policy`), `gates_memory_pressure_duration = "auto"`, `gates_oom_score_adj =
  "auto"` (800) or `0..=1000`, `gates_memory_max = ""`, `gates_memory_high = ""`. Admission keys belong to
  `[admission]` (`config.rs:~840-860`, validation `config.rs:~1126-1150`) because the rule also works
  without containment: `memory_aware = true`, `memory_reserve = "auto"` (10 % of `MemTotal`) or `"3G"`.
  Older binaries reject unknown keys, so the one-way compatibility entry and a fixture under
  `sluice-gate-core/tests/fixtures/unreleased/` (like `queue-prepare.toml`) are needed
  (`docs/compatibility.md`).
- Doctor: `sluice-gate/src/doctor/containment.rs`: `checks` (`:38-63`), `memory_policy` (`:204-268`, message
  "memory policy on: systemd-oomd acts on the agents scope at ..."). Add: gates policy line (limit, duration,
  "acts before the agents scope"), `daemon/` omit xattr readable (`has_oomd_omit`), oom_score_adj raising
  works (the doctor's own process can test its own file), `memory.oom.group` of every ancestor of
  `jobs/` (must be 0 above the leaves, see B2), swap total and free, ceiling when set, recent memory kills
  (the `interventions` table), and the learned peaks of the repo's gates.

## B. Soundness of each piece

### B1. oomd policy on `sluice-daemon.scope` (candidate 1)

Mechanism (documented/source, S5 §3 and §9): `ManagedOOMMemoryPressure=kill` can be set by the user manager
on its own scope (S5 T0, measured: accepted, listed by `oomctl`); candidates are only **leaf** descendants or
descendants with `memory.oom.group=1`; the monitored unit itself is never a candidate; so under the daemon
scope the candidates are the `jobs/<id>` leaves and `daemon/` (`jobs/` is not a leaf). Ranking: preference
class, then pgscan rate, then size (S5 §3). S5 T3 (measured, kernel 7.0): oomd killed the offending leaf of
an own scope and nothing outside.

- `daemon/` and `omit`: documented (`systemd.resource-control(5)`, local 259): for memory-pressure candidates
  the xattrs are respected "if the unit's cgroup owner, and the owner of the monitored ancestor cgroup are the
  same"; here the monitored ancestor is the daemon scope and `daemon/` are both owned by the user: respected.
  `omit` "will ignore this cgroup as a candidate"; `avoid` only reorders ("only select this cgroup if there
  are no other viable candidates"). For the daemon use `omit` (a dead daemon abandons running jobs; the
  reaper exists for exactly that, `daemon/src/reaper.rs`). **S5 T6 only measured that the xattr is accepted
  and read back** (S5 §9: "whether `user.oomd_avoid` changes the ranking is still to be isolated"): the
  ranking effect of `omit` is documented but not measured here (open, T-W6-5). Not recursive: set it on
  `daemon/` itself, which is a leaf.
- **Does it trigger when it matters? (the weak point, open).** oomd's trigger is the `full` avg10 of the
  *monitored cgroup*, above the limit for the duration, plus reclaim activity (S5 §3 "Trigger"). The incident
  gives 92.69 % for the agents scope; nothing in the incident data says what the daemon scope's `full` was.
  `full` needs *all* non-idle tasks of the scope stalled simultaneously. Twelve busy workers in direct
  reclaim plausibly reach it, but a gate that grows steadily while other cgroups (the agents) are the ones
  being reclaimed would not: the agents scope would cross 30 % and the daemon scope would not (inference).
  Then oomd kills the agents' leaves again, exactly the incident. This must be measured before relying on
  the policy (T-W6-0, read-only on the second machine: cumulative `memory.pressure` `total=` and
  `memory.stat pgscan*` of `sluice-daemon.scope` versus `sluice-agents.scope`; counters of deleted
  descendants are folded into the parent, source-recalled).
- Ordering with the agents' policy: both are monitored cgroups; oomd tries them in `SET_FOREACH` order and the
  first successful kill wins, one kill per iteration, 15 s post-action delay (S5 §3). A lower limit does not
  help (a throttled hog stalls 100 % of its own scope: S5 §5(a)); only a shorter duration orders them, and only
  systemd ≥ 257 has it (documented, "Added in version 257"). Hence `duration_gates = max(1 s,
  duration_agents / 2)`: with Ubuntu's derived 5 s agents duration, 2.5 s rounds to 3 s (use whole seconds
  `div_ceil`), the gates scope crosses first *if it is over its limit*. On systemd < 257 the policy is not
  created (same gate as `oomd_policy_supported`, `detect.rs:240`): both would use the global duration and the
  order would be unspecified.
- Risk, over-eager kills of a legitimate suite: S5 §5(a) failure mode 3 (Fedora #358). A 30 % limit on the gate
  scope kills a legitimate heavy test that thrashes by itself. Acceptable only with admission in front of it
  (piece 3 makes the situation rare) and with the kill explained (piece 4). Consider a higher limit for the gates
  than for the agents (for example `max(agents, 40 %)` with the shorter duration): the order is by duration,
  not by limit, so the limit is free to be less eager.
- No oomd (Arch, Debian default): the property is stored and inert (S5 §5(a)4); doctor says so.
- No swap: oomd documents that without swap the system livelocks faster and oomd may not react in time
  (S5 §3 "Swap"); protection by admission and adj matters more there. With swap *full* (the incident) anon
  pages cannot be reclaimed, so only file pages are scanned: pgscan is non-zero as long as page cache exists
  (inference); S5 T7 (measured with swap, no kill) is not this case.
- zram: swap exists but costs RAM; `MemAvailable` already reflects that; oomd treats it as swap (unverified).
  Admission using only `MemAvailable` is conservative with zram.
- cgroup v1/hybrid, no user manager, partial containment: `Containment::start` returns `None`
  (`containment.rs:121-123`): no scope, no policy, no leaf. The policy is simply absent; pieces 3 (with RSS
  sampling) and the guard remain.
- Variant B (to evaluate in T-W6-6): one policy on `sluice.slice` (the common parent; `ensure_slice`,
  `tree.rs:491-499`, `systemd::set_slice_properties`, `systemd.rs:354`) with `avoid` on every non-job leaf
  (`ui` already; add `tools`) and `omit` on `daemon/`/`holder/`. Slice units support the property (S5 §3 row
  "Units of a user manager"). The slice's pressure is the union, so it triggers when either side is stalled;
  the candidates are every leaf, and with `avoid` on all agent leaves the only non-avoided candidates are job
  leaves, which makes "the jobs are the preferred victims" true by construction (documented semantics of
  `avoid`). It replaces the duration race by a candidate filter. Costs: changes the agents' policy semantics
  (avoided leaves are chosen only when no other viable candidate exists, so with no job running an agent
  leaf is still killed, and an agents-only storm kills a running job first), and relies on `avoid`
  ranking, which is unmeasured (T6 gap).

### B2. adj raise and `memory.oom.group` (candidate 2)

- adj: documented (proc(5)): additive to badness, `-1000..1000`; a process can always raise its own, lowering
  needs `CAP_SYS_RESOURCE`. So `max(configured, inherited)`; the gate workers inherit the daemon's value
  (200 on the second machine, measured; the daemon was probably started under an agent shell or session that
  sets it, inference). Raising to 800 is allowed unprivileged; a configured value below the inherited one
  silently stays at the inherited one (log at debug, doctor says "effective adj").
- Effect (source-recalled, `mm/oom_kill.c`, `oom_badness`: points = rss + swap entries + page tables, plus
  `adj × totalpages / 1000`, `totalpages` = RAM + swap for the global OOM): at 30 + 8 GiB, adj 800 adds about
  30 GiB-equivalents, adj 500 about 19 GiB, Chrome's 300 about 11 GiB. With 500 a 2 GiB worker (21) beats a
  2 GiB renderer (13) but not a 9 GiB browser process; with 800 it wins unless the other process is far
  bigger. Default 800 rather than the candidate's 500. It only matters for the **global** OOM killer; **oomd
  does not read `oom_score_adj`** (S5 §3: ranking by preference xattr, pgscan, size), so piece 2 and piece 1
  cover disjoint killers.
- `memory.oom.group=1` and the global OOM: **confirmed from source** (`mm/memcontrol.c`,
  `mem_cgroup_get_oom_group(victim, oom_domain)`, read for this note: "oom_domain: memcg in case of memcg OOM,
  NULL in case of system-wide OOM", then it walks from the victim's cgroup up to the OOM domain (root for a
  system-wide OOM) and returns the **highest** ancestor with `oom_group` set). The kernel doc page
  (docs.kernel.org cgroup-v2) describes `memory.oom.group` only for cgroup-level OOM ("treated as an indivisible
  workload", all tasks killed together) and does not state the global case; the source does. Consequence: after a
  global OOM picks one worker, the whole `jobs/<id>` is killed, instead of the main `node` process surviving and
  reporting "worker crashed" as a test failure. Counters: `oom_kill` per process, `oom_group_kill` once
  (documented).
- **Trap:** "highest ancestor with oom.group set" means an `oom.group=1` anywhere above the job leaf widens the
  kill to that ancestor (the whole daemon scope, or worse). systemd sets `memory.oom.group` only through
  `OOMPolicy=kill` (documented, `systemd.service(5)`), and delegated units default to `continue`, but ancestors
  (`user@UID.service`, slices) may be configured by the distribution or the administrator. Doctor must read
  `memory.oom.group` of every ancestor up to the root and warn if any is `1` (read-only; T-W6-0 does it on the
  incident machine). Never set it on `jobs/` (it would also make `jobs/` an oomd candidate: non-leaf with
  `oom.group=1`, S5 §3).
- oomd and `oom.group` on a leaf: no change (a leaf is a candidate anyway; oomd kills the whole cgroup).
- Without swap/with swap: independent. cgroup v1: no `memory.oom.group` (kernel `cgroup_subsys_on_dfl` check
  in the source above); the adj piece still works.
- The daemon itself (`daemon/`) keeps the inherited adj and no `oom.group`: a global OOM that picks the daemon
  kills only it (the reaper then cleans up; existing behaviour).

### B3. Memory-aware admission (candidate 3)

Sound, with four corrections (details in section C):

1. **`memory.peak` includes page cache.** The kernel doc: "max memory usage recorded for the cgroup and its
   descendants". `memory.current` counts charged file pages, and nothing evicts them without pressure, so a job
   that reads many large files (build, tests over a big tree) has a peak far above its working set, while
   `MemAvailable` already counts reclaimable cache as available: comparing the two double counts. Learn the
   working set: `anon` of `memory.stat` plus `memory.swap.current`, sampled by `sample_tick` every second;
   record `memory.peak` as well (an upper bound, shown in status, not used for admission). Use
   `E = sampled (anon + swap) peak × 1.2`. The sampling can miss a
   sub-second spike; `max(E, current)` per running job and the reserve absorb it (inference).
2. **Count RAM only.** Swap is where the incident ended (full), and a job that fits only by swapping is slow and
   starves everything. Do not add `SwapFree`. With zram, `MemAvailable` already pays for the compressed pages.
3. **Headroom** of running jobs (the candidate's "still to grow") is essential: memory PSI is 0 at admission
   because the previous job is still ramping up (measured). `MemAvailable` is a lagging value; the headroom term
   makes the decision independent of the ramp.
4. **The head reservation must cover memory**, otherwise a big job starves behind a stream of small ones
   (section C).

### B4. Outcome reporting (candidate 4)

Sound, see A4/A5 for the corrections (watcher gap, oomd not in `memory.events`, classification at `launch`, clients
must print the message). One more point: a job whose leader exits 137 for a *different* reason (the gate's own
`kill -9`) is not classified as memory unless a counter or an oomd line confirms it: never guess from the exit
code alone. A coalesced job has one outcome for all subscribers (`scheduler.md` Coalescing), so all of them get
`killed_by = "memory"`; the queue train fails that job like any `killed` (`runner.rs:1566`) with the message in
its view.

### B5. Optional hard ceiling `gates_memory_max` (candidate 5)

- Mechanism measured in S5 T5: `MemoryMax=64M`, swap 0: kernel OOM kill inside the unit only, user@ pressure 0.
  With `memory.oom.group=1` on the leaf (piece 2) a ceiling hit kills the whole job cleanly.
- Without `memory.swap.max`, hitting `memory.max` first swaps (slow) then kills; `memory.high` throttles and
  creates the S2 incident's pressure without pgscan (S5 §1 item 1). So `gates_memory_high` is **not** offered by
  default advice; document `max` only, with `memory.swap.max` as a separate opt-in. Do not copy the W6 plan text
  (a ceiling "computed from `MemTotal`" by default): that is an absolute threshold on by default and a kill of a
  legitimate suite, against ADR-0017 point 4 and S5 §7 item 1; a hard limit's value also cannot know the
  agents' share. Opt-in, empty by default.
- The ceiling is on `jobs/` (all gates together), so a hit kills whichever task the in-cgroup OOM picks: with
  `oom.group` on leaves, the kernel picks the victim task by badness in the whole `jobs/` subtree and kills its
  whole leaf (source above, oom_domain = `jobs/`).

### B6. Extra: a Sluice-side guard (not in the candidate list)

Neither oomd (absent/old) nor admission (first run, a job that outgrows its history, agents' memory growth) is
enough everywhere. Sluice owns the gate processes, so it can cancel **its own** newest or largest running job
before the kernel picks a victim: poll `memory.pressure` (`full avg10`, a share of time, not an absolute size)
of the daemon scope and `MemAvailable` at the existing 1 s sampler; when the system is in real trouble for a few
consecutive seconds, cancel the job with the largest headroom-adjusted footprint (`ExecStatus::Killed`,
`killed_by = "memory"`, the same message path). Works without oomd, any systemd, v1 fallbacks that have `/proc`.
It is a new lethal action in product code, which S5 §7 item 2 deliberately left out of 0.5.0 and §5(c) says needs
"its own design and an opt-in". It never touches the agent's commands (only gate/exec jobs). Recommend as a second
step behind an opt-in `[admission] memory_guard = false`, decided by the user after the measurements.

### B7. Interactions and fallbacks, summary

| Situation | adj/oom.group | admission | oomd gates policy | outcome reporting |
|---|---|---|---|---|
| Full host, oomd, systemd ≥ 257 | yes | yes | yes (before agents) | counters + journal |
| oomd absent / stopped | yes | yes | inert (doctor says so) | counters only (kernel kills) |
| systemd 252-256 (Debian 12, Ubuntu 24.04) | yes | yes | not created (no per-unit duration) | counters + journal if oomd is there |
| No swap | yes | yes, matters more | oomd warns it may be too slow | same |
| zram | yes | yes (MemAvailable) | unverified | same |
| cgroup v1 / hybrid / partial / no manager | adj only if the wrapper is used without a leaf | yes, sampled RSS instead of cgroup files | none | no counters; only exit-code heuristics: do not report as memory |
| `[containment] enabled = false` | no (no wrapper) | yes (RSS) | none | none |

## C. The admission rule in detail

### C1. Inputs at an admission pass

- `MemAvailable` and `MemTotal` from `<proc_root>/meminfo` (daemon config already has `proc_root`; the file
  is cheap, read once per pass, at most 1/s). Container: also `min(…, memory.max - memory.current)` of the
  daemon's own cgroup chain when a limit exists (rare, inference).
- Per running job: current footprint `cur = anon + swap` of its leaf (sampled by `sample_tick`, last value),
  expected `E_r` (learned, else proxy). Jobs not in a leaf (no containment): sum of `VmRSS` of the group members
  (`sampler::group_members`, `sampler.rs:206`), which the sampler already enumerates.
- Per queued job: `E_j` learned (`StatsCache`-like, key `(repo, gate, cmd_hash)`), or the proxy.
- `reserve` = `memory_reserve`: `auto` is 10 % of `MemTotal` (a proportion, not an absolute number; 3 GiB on
  30 GiB): what the desktop, the agents and the page cache need to stay alive. It is deliberately not learned:
  the agents' memory is outside Sluice's control.

### C2. The rule

```
headroom_r  = max(0, E_r - cur_r)                    # running job r still to grow
H           = sum over running r (and over jobs started earlier in this same pass) of headroom_r
free        = MemAvailable - reserve - H
heavy_r     = E_r >= 5 % of MemTotal
alone       = no running job is heavy
fits_mem(j) = alone  ||  E_j <= free
```

- A job that is not `alone`-exempt must also satisfy the budget (`used + w <= B`) and the repo cap, exactly as
  today; `fits_mem` is an extra `&&` in the nominal pass (`scheduler.rs:~549-560`), in the over-admission
  candidate test (`scheduler.rs:~615-625`) and in the guard (`scheduler.rs:~540-548`). Over-admission also
  requires a **learned** `E_j` (never over-admit blind) and `E_j <= free` with no `alone` exemption (alone means
  `used = 0`, which is nominal anyway).
- Jobs admitted earlier in the same pass add their full `E_j` to `H` for the rest of the pass (like `held`
  already does for weights, `scheduler.rs:~528-533`); this prevents two big jobs starting in the same pass off one
  stale `MemAvailable` (the incident's "two or three admitted together").
- Unknown peak (first run, no history): `E_j = weight_share × (MemTotal - reserve)` where `weight_share =
  w / B` with the effective weight `w`. For `auto` (`w = ⌈B/2⌉`) half of the usable RAM, so a first `test` gate
  runs alone with respect to other first-run big gates and still beside light ones (a `lint` of weight 1 on 12
  CPUs predicts 2.3 GiB). It reuses the CPU weight as the only prior the user already expressed; it needs no
  new key. A configured `weight = 1` gate that actually uses 18 GiB is the case the guard and oomd (B6, B1)
  exist for. Mark learned-vs-guessed in `status`.
- Uncertain `MemAvailable` (file unreadable) or `memory_aware = false`: `fits_mem` is true (today's behaviour).

### C3. Starvation and deadlock

- No hold-and-wait: a job holds nothing while it waits; admission is all-or-nothing, so the memory rule cannot
  create a cycle among queued jobs. Trains of the merge queue submit ordinary runs (`merge-queue.md`,
  `scheduler.md` "Merge queue jobs"), so a train waits like any job and no train holds memory while waiting;
  `prepare`/vouch jobs of W3/W4 (other agent) are normal jobs too (verify when that code lands).
- Progress when memory is simply short because of the agents (not Sluice): the `alone` exemption lets one job
  run even if `E_j > free`. Bounded harm: that job is the preferred victim (adj, oomd gates policy).
- Head-of-line: today only an overdue head blocked by the **budget** reserves (`scheduler.rs:~565`). Extend to
  `(!fits_budget || !fits_mem) && overdue(head)`, so a big job cannot starve behind small ones; the ADR-0007
  bound (`head_reserve_after` + longest running job) survives because running jobs end (timeouts) and then `alone`
  becomes true. The existing property test `no_starvation_under_any_switch_combination` must be extended with the
  memory switch (D). A memory-blocked job that is not overdue still allows backfill of smaller jobs
  (`fits_mem` false only for jobs that do not fit).
- Wait cause: add `WaitCause::Memory` reported on the wire as `budget` with `wait_detail = "memory"` (as `Order`
  does, `scheduler.rs:113-120`), and accumulate into the budget accumulator (index 0) so the five
  `jobs.wait_*_ms` columns and the frozen `WaitReason` do not change; `done.wait_reason` may print `memory`
  (a free-form string, `proto/src/lib.rs:640-645`). PSI blocking still precedes everything.
- Coalesced/exec jobs: one job, one `E`; exec jobs have their own key (`exec:<label>`), so their peaks are
  learned too; `--weight` for exec sets the proxy.

### C4. Weight `auto` and over-admission

CPU budget and memory are independent dimensions: weight 6 + 6 fits the CPU budget of 12 and over-admission
adds a third. Memory adds a second constraint, it does not change weights (ADR-0015 stays intact; learned
weights keep lowering the ceiling). Over-admission remains CPU-driven but now needs a learned `E` and
`fits_mem`; the existing `calm` test (`memory some avg10 < 5 %`) stays as an extra condition.

### C5. Invariants that must not break (scheduler.md)

Budget `used + w ≤ B` nominally; FIFO when every switch is off (memory off ⇒ identical plans: add a differential
test); overdue jobs FIFO and outrank everything; PSI blocking stops all but the guard; `plan` stays a pure
function of `Snapshot`; over-admitted jobs are never preempted; `status` keeps four frozen wire reasons;
`jobs`/`gate_stats` schema unchanged (new data in the containment group).

## D. Verification plan

Safe in CI or on a developer machine (no memory pressure): everything pure or on fake files, in the style of the
existing tests: `scheduler/tests.rs` (pure `plan`), `daemon/tests/daemon.rs` with `proc_root`/`psi_root` fakes
(`daemon.rs:1479-1508`), `interventions.rs` tests on fixture directories (`interventions.rs:251+`),
`session_usages` on tempdir cgroup files (`containment.rs:519-545`), `tree.rs` tests, doctor tests with
`Facts`, `sluice-gate/tests/containment_daemon.rs` (real user manager, skipped otherwise, no memory load,
units `sluice-test-<pid>*` stopped by `Drop`).

| ID | Test | Level | Expected |
|---|---|---|---|
| T-W6-1 | `plan` with `MemoryFacts` unit tests: two jobs `E=18G`, `MemAvailable=22G`, reserve 3G: second waits; first admitted; `H` of jobs started in the same pass counted | pure | only one starts; cause `memory` |
| T-W6-2 | Properties: never `Σ E > free` unless `alone`; with memory off the plan equals today's (differential, all 16 switch combos); `no_starvation_under_any_switch_combination` extended: overdue memory-blocked head starts within `head_reserve_after + longest running` | pure/proptest | holds |
| T-W6-3 | Unknown peak: proxy `w/B × (total - reserve)`; learned peak replaces it after one pass; memory-killed sample counts ×1.25; `learns_from` stays pass-only for weights | pure + store | as stated |
| T-W6-4 | `gate_memory` migration: fresh db, db at version 1, db opened by an older fixture binary (`daemon/tests/compat.rs` fixtures): no `NewerSchema`, old binaries ignore the table | integration | opens everywhere |
| T-W6-5 | Fake `proc_root/meminfo` + fake job leaf files (`memory.current`, `memory.stat anon`, `memory.events`): daemon test where `meminfo` falls, second gate waits with `wait_detail=memory`, then runs when `meminfo` recovers; `alone` job runs with tiny `MemAvailable` | daemon integration (fake) | as stated |
| T-W6-6 | Outcome classification: `Finished` with `Exited(137)` + `oom_kill` delta 1 gives `Killed`/`killed_by=memory`, message has peak and available; `Exited(137)` with no evidence stays `Fail`; Sluice's own cancel stays `last-subscriber`; oomd journal fixture naming `jobs/<id>` gives `Killed` | unit (pure function over `Finished` + `MemoryReading` + journal text) | as stated |
| T-W6-7 | `MemoryWatcher` backstop: growth of `jobs/` and scope `memory.events` without a job row produces a row; row dedup with the one recorded at job end | unit on fixture dirs | as stated |
| T-W6-8 | `cg-exec` writes adj: run `sluice cg-exec <tmp leaf> --oom-score-adj 800 -- cat /proc/self/oom_score_adj` prints `800`; with a configured value below the inherited one it keeps the inherited; unwritable `/proc` path is fail-open | CLI integration (no cgroup needed) | as stated |
| T-W6-9 | `Tree::job_leaf` writes `memory.oom.group=1` on a tempdir leaf with the file present; absent file tolerated; `ensure_daemon_scope` writes `memory.max` on `jobs/` only when configured | unit on tempdir | as stated |
| T-W6-10 | Property list of `start_transient_scope_args` for the daemon scope: `ManagedOOMMemoryPressure kill`, limit, duration only when systemd ≥ 257 and oomd active; `derive_gates_policy` never below 1 s, always shorter than the agents' duration unless that is 1 s | unit (`systemd.rs`, `detect.rs`) | as stated |
| T-W6-11 | Config: new keys parse, validate, `deny_unknown_fields` message, fixture `unreleased/` + compat note | unit/compat | as stated |
| T-W6-12 | Doctor: ancestor with `memory.oom.group=1` warns; omit xattr missing warns; gates policy line | unit with `Facts` | as stated |
| T-W6-13 | Real user manager, no load (`containment_daemon.rs` style): daemon scope has the policy in `oomctl`; `daemon/` has `user.oomd_omit`; a gate leaf has `memory.oom.group=1`; `cg-exec` set the adj of the real job | live, disposable units | as stated |
| T-W6-0 | **Read-only on the incident machine** (no load, no change): `memory.pressure` (`some`/`full` totals), `memory.events`, `memory.stat` pgscan/pgsteal, `memory.oom.group` of `sluice-daemon.scope`, `sluice-agents.scope`, `sluice.slice`, `user@UID.service`; `oom_score_adj` of the daemon and a worker; `cat /proc/<daemon>/status | grep -i cap` | measurement | answers "did the daemon scope's `full` exceed 30 % while the agents' did" and "does any ancestor have `oom.group`" |
| T-W6-14 | Live bounded spike, S5 §9 style (own `sluice-w6-*` scopes, 8 s of pressure max, watchdog on `user@` `memory.pressure` avg10 ≥ 25 %, all stopped on exit, with the user's consent): daemon-scope policy (limit 5 %, 1 s) with two leaf allocators and a sleeping `ui`-like leaf in a sibling agents-like scope with its own policy | live | oomd kills the leaf of the gates-like scope first; the sibling survives; records which scope oomd acted on when both are over (`SET_FOREACH` vs duration) |
| T-W6-15 | Same, systemd 255 image (disposable VM) | VM | order unspecified: confirms the policy must stay off below 257 |
| T-W6-16 | `user.oomd_omit` on one leaf, `avoid` on another, both candidates under a monitored scope, tiny limits | live/VM | the omitted leaf is never killed; the avoided one only when no other candidate (closes the S5 T6 gap; also decides variant B) |
| T-W6-17 | Global OOM with `oom.group` and adj, **VM only** (4 GiB RAM, small swap): job leaf with `oom.group=1` and adj 800 vs a leaf with an adj-300 process | VM | the whole job leaf dies, the adj-300 process survives; `oom_kill` and `oom_group_kill` read from the leaf at job end |
| T-W6-18 | VM: variant B (policy on the slice, `avoid` on agent leaves) versus the daemon-scope policy, under the incident's shape (job steadily growing, agents idle) | VM | decides between B1 and variant B |
| T-W6-19 | VM: swap off / on / zram, memory admission end to end with a synthetic allocator gate (a few hundred MiB, ramp 20 s) | VM | second gate waits for the first one's headroom; nothing is killed; no starvation with five gates queued |

No test of this wave creates memory pressure on a developer machine; T-W6-14 and T-W6-17 to 19 need the user's
consent or a disposable VM.

## E. Contradictions with ADR-0017 / containment.md and how to amend

| Statement | Where | Contradiction | Amendment |
|---|---|---|---|
| "Memory: accounting only by default", "no `memory.oom.group` on any Sluice cgroup" | ADR-0017 point 4; `containment.md` §4; S5 §7 item 1 | Piece 2 sets `oom.group=1` on job leaves; piece 3 limits admission by memory; piece 1 adds a kill policy | "Memory: no limit by default. Gate jobs (Sluice's own children) are admitted by learned memory, are the preferred victim of the kernel (`oom_score_adj`) and die as a unit (`oom.group` on the job leaf). S5's objection to `oom.group` was about session cgroups that contain the agent (D4); a job leaf never does." |
| "No absolute threshold is on by default" | `containment.md` §4 | `memory_reserve` and the 5 % heavy threshold are thresholds; adj 800 is an absolute number | All are proportions of `MemTotal` or a priority, not a ceiling that kills a legitimate run; the only absolute ceiling (`gates_memory_max`) stays opt-in. State it in the ADR |
| "Never kill the agent" (D4) | ADR-0017 | Not contradicted: victims are job leaves; the gates policy makes the daemon scope act before the agents scope | Add: "the agents scope's policy is the second line; oomd acts on the gates scope first (shorter duration, systemd ≥ 257)" and say that if the gates scope is not over its limit the agents' leaves can still be killed (open, T-W6-0) |
| "No kill by Sluice in 0.5.0" | S5 §7 item 2 | Only if B6 is adopted | Keep it out of the default; opt-in with its own ADR |
| "Memory ceiling computed from `MemTotal`" | `queue-trust-plan.md` W6 first bullet | Differs from the candidate list (opt-in, no default) | Align the plan with the opt-in; a default hard ceiling is an absolute threshold and kills legitimate work |
| "`ui` avoid/omit honoured" assumed | ADR-0017 point 4, S5 §5(a) | Ranking effect never measured (S5 §9 note) | Measure (T-W6-16) before documenting `omit` on `daemon/` as protection; until then the documented semantics are the basis |
| "`MemoryWatcher` explains every intervention" | `containment.md` §5 | Misses gate kills (leaf removed at job end, 10 s poll) | Classify at job end (A5) and keep the watcher as backstop |

New ADR (next free number after 0018 of the queue-trust work, to be confirmed) amending ADR-0017 point 4 and
`containment.md` §4/§5/§7; update `docs/configuration.md`, `docs/merge-queue.md` (a memory-killed gate in a train),
`docs/debugging.md` (how to read `killed_by = memory`), `docs/compatibility.md` (new keys, one-way).

## F. Suggested order of work

1. T-W6-0 (read-only measurement on the incident machine) in parallel with everything else.
2. Piece 4 (classification at `launch`, outcome message, clients print it, interventions row) and piece 2
   (adj in `cg-exec`, `oom.group` on leaves, doctor ancestors check): deterministic, closes "the culprit was
   never the victim" for the kernel killer and makes failures explainable.
3. Piece 3 (learning in the containment table, `fits_mem`, reservation, status fields).
4. Piece 1 after T-W6-0/14/16 decide between the daemon-scope policy and variant B; piece 5 as opt-in;
   B6 only if the user wants a Sluice-side action.
