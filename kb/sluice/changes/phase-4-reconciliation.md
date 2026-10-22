---
title: "Phase 4 reconciliation report"
tags: [sluice, reconciliation, phase-4]
---

# Phase 4 reconciliation report

Scope: Phase 4 (PSI admission, `status` observability, frozen schema). Docs reviewed:
`design/psi-admission.md`, `design/scheduler.md` (admission and PSI sentences),
`design/protocol.md` (`status`), `design/config-format.md` (`[admission]`),
`changes/phase-4-psi.md`, `changes/phase-4-acceptance.md`, `plan/master-plan.md` (Phase 4),
`README.md`, `scripts/psi-pressure-demo.sh` header and `sluice status --help`. Code:
`crates/sluice-gate-daemon/src/{psi,server,scheduler,store}.rs` (`daemon:`),
`crates/sluice-gate-proto/src/lib.rs` and `tests/status_schema.rs` + fixture (`proto:`),
`crates/sluice-gate-core/src/config.rs` (`core:`), `crates/sluice-gate/src/{status,daemon_cmd}.rs`
(`bin:`), `crates/sluice-gate-daemon/tests/daemon.rs` (`test:`). Doc paths are relative to
`kb/sluice/` (except README and scripts). Findings come from reading the code; only
`sluice status --help` was executed. No file other than this report was written.

## 1. Documented but not implemented

| # | Statement | Doc file:line | What the code does instead | Code file:line |
|---|---|---|---|---|
| 1 | "metrics export (time saved, hit rate, wait times) for the user's existing HTML dashboard" is a Phase 4 deliverable; the README roadmap row says "metrics export" | plan/master-plan.md:153-155, README.md:195 | There is no exporter. The only output is `status`/`status --json` totals (runs, hits, `wait_ms_total`, `time_saved_ms`); no hit rate, no export file or endpoint, and no note says the item was dropped or deferred. | proto:lib.rs:497-520, bin:status.rs:111-150 |
| 2 | The reservation fires when the head is blocked by the budget "(or, from Phase 4, by PSI)" | design/scheduler.md:69-75 | While PSI blocks, `admit` returns before the loop, so the head never reserves, and `snapshot` computes `reserved` from the budget only. PSI blocking never produces `head-reserved`; it shows `psi` for every queued job. | daemon:scheduler.rs:180-184, 302-310, 322-323 |
| 3 | `status` reason precedence includes `psi` only "from Phase 4" and lists `budget`, then `repo-cap`, then `head-reserved` | design/protocol.md:117-121 | `psi` is first and overrides every other reason for every queued job (including jobs that would also be blocked by the budget or the cap) while blocking. | daemon:scheduler.rs:322-333 |
| 4 | "If a read fails (files missing or unreadable), admission falls back to budget-only for the rest of the daemon's life" | design/psi-admission.md:25-28 | Only when both `cpu` and `memory` are unreadable. If just one file is missing, PSI stays enabled, that value is `None` (shown `n/a`, counted as calm) and the other one still blocks. The integration test relies on this (`memory_some_avg10 == None`, PSI enabled). | daemon:psi.rs:59-72, test:daemon.rs:953 |
| 5 | Acceptance: "rewriting the file to `avg10=0.00` (two samples at a 50 ms poll) starts it" and "then starts after two calm samples" | changes/phase-4-acceptance.md:10, design/psi-admission.md:83-85 | The integration test only waits for `started`; it never asserts the two-sample rule or the 80 % band. Hysteresis is covered only by the `hysteresis` unit test with synthetic samples. | test:daemon.rs:956-958, daemon:psi.rs:293-320 |
| 6 | "Thresholds documented with measurements" and "a threshold of 60 % corresponds to a sustained oversubscription of roughly 2× on this machine" | changes/phase-4-acceptance.md:11, plan/master-plan.md:158-159 | Only CPU was measured, and only for 15 s: 40 loops reached 47.1 % while `avg10` was still rising, so 60 % was never observed. The 2× correspondence is an extrapolation (also the machine already had load about 10, so 40 loops is about 2.5× of 20 CPUs, 20 loops about 1.5×). The memory default (20.0) has no measurement and `psi-admission.md` itself contains no numbers. | scripts/psi-pressure-demo.sh:1-11, 25-37 |
| 7 | `psi_blocked_ms_total` is "0 when PSI is unavailable" | proto:lib.rs:518-520 | After PSI turns unavailable mid-run the value keeps what accrued before (the unit test asserts 1000 after the switch-off); it is 0 only if PSI never blocked. | daemon:psi.rs:117-136, 322-332 |

## 2. Implemented but not documented

| # | Behaviour | Code file:line | Suggested doc location |
|---|---|---|---|
| 1 | Blocking is strict: `avg10 > max` blocks, resume needs `avg10 < 0.8 * max` for both values on two consecutive samples; a sample in the 80-100 % band or over the threshold resets the count to 0. The thresholds are not validated: negative, zero, NaN or above 100 are accepted (0 blocks on any value above 0; NaN never blocks). | daemon:psi.rs:28-31, 139-168, core:config.rs:568-579 | psi-admission.md Rule; config-format.md:142 |
| 2 | A missing or malformed value (`None`) counts as calm for blocking and for resuming, per metric. A malformed file that still reads leaves PSI enabled; parsing accepts `some ... avg10=<f>` anywhere in the fields, rejects negative or non-finite numbers, and ignores `full` lines. | daemon:psi.rs:46-57, 139-140 | psi-admission.md Source (only "counts as calm" is stated) |
| 3 | A failed read is final: a single transient error (both files unreadable once) disables PSI until the daemon restarts, with one `warn`. With `psi_enabled = false` no sampler task runs and nothing is logged. | daemon:psi.rs:127-137, server.rs:344-346, 517-536 | psi-admission.md Source |
| 4 | Block and resume transitions are logged at `info` ("system pressure is high; new jobs wait", "...back to normal; admission resumes") with the sampled values. | daemon:psi.rs:147-151, 160-164 | psi-admission.md Observability |
| 5 | Starvation guard details: it is evaluated on every scheduling pass (a wake or the 1 s `ADMIT_TICK`), not only when the blocking state changes; "waited" is strict `>` and counts from the head's `queued_at`, so a job that waited on the budget is eligible the moment PSI starts blocking; only the head is considered, so a head that does not fit the budget or its cap admits nothing (no other job is tried); the "one per `psi_max_wait`" timer is `>=` since the last override, is recorded only on success and is not reset when blocking ends. The log line is "PSI starvation guard: starting the queue head despite system pressure" with `job` and `waited_ms`. | daemon:server.rs:65-66, 542-565, 566-579, psi.rs:199-212, scheduler.rs:211-223 | psi-admission.md Rule |
| 6 | `psi_blocked_ms_total` semantics: sampled, not continuous. Each sample adds the time since the previous sample if the state before it was blocking, so the onset interval is not counted, the resume interval (including the two calm samples) is, and the time since the last sample is missing (up to one interval). Memory only, reset on restart, overwrites the store's 0, and is shown only in `--json`, never in the text output. | daemon:psi.rs:117-123, 190-194, store.rs:704-705, server.rs:1132-1176, bin:status.rs:111-150 | psi-admission.md:69-73; protocol.md:112 |
| 7 | `status` reports queued reason `psi` for every queued job while blocking, but a running job is unaffected and a started-by-guard job simply runs; `budget.psi` when PSI is off or unavailable is `{enabled:false, cpu_some_avg10:null, memory_some_avg10:null, blocking:false}` (last values are cleared). While enabled, a single missing file shows `null` for that value. | daemon:psi.rs:132-136, 177-188, proto:lib.rs:424-433 | protocol.md:108 (shows only the enabled shape) |
| 8 | Text `pressure:` line formats: `pressure: cpu 12.3% mem 0.4% (ok)`, `(blocking admission)`, `(unavailable)`; values use one decimal and print `n/a` when `null`, so `cpu n/a mem n/a (unavailable)` when disabled and `cpu 12.3% mem n/a (ok)` with one file missing. The line sits between `budget:` and `running:`. | bin:status.rs:77-92, 111 | psi-admission.md:72-73; README.md:177-181 |
| 9 | `[admission]` is read once at daemon start: changing it needs `sluice daemon stop`. `config-format.md` says "everything else is live". `psi_poll_interval` and `psi_max_wait` must be > 0 and are rejected otherwise; `psi_enabled`, thresholds and poll interval cannot change at runtime. | core:config.rs:568-579, bin:daemon_cmd.rs:67, daemon:server.rs:317, 344 | config-format.md:138-146; scheduler.md:89-91 |
| 10 | The PSI sampler's first tick is immediate (at daemon start); the loop keeps its period with `MissedTickBehavior::Delay`; the scheduling loop is woken only when the blocking verdict changes, not on every sample. | daemon:server.rs:517-536 | psi-admission.md:33-34 |
| 11 | The schema test fixes the serialised shape of one populated report only. It does not cover `PsiInfo::default()` (nulls), `--json` with no daemon (`{"schema":1,"daemon":null}`), or older replies missing `psi_blocked_ms_total` (the `serde(default)` behaviour); `STATUS_SCHEMA_VERSION` is a constant pinned to 1. | proto:tests/status_schema.rs:62-95, proto:lib.rs:418-433, 518-520 | psi-admission.md:74-76; protocol.md:121-125 |
| 12 | `sluice status --help` says "Show the daemon, budget, running and queued jobs, and totals" and never mentions pressure; the README description of `status` (uptime, budget, jobs, reasons, totals) also omits the `pressure:` line and `psi` as a waiting reason. | bin:status.rs (clap about), README.md:177-181 | README Command surface |
| 13 | Dev note: the shared test config sets `psi_enabled: false` and `psi_root = <dir>/pressure`; the PSI tests share a process-wide log capture. | test:daemon.rs:105-111, 850-897 | changes/phase-4-psi.md:22 (only the first half is stated) |

## 3. Mismatches

| # | Topic | Doc says (file:line) | Code does (file:line) | Severity |
|---|---|---|---|---|
| 1 | README status banner | "pre-alpha, Phases 1-3 done ... Load-aware admission and packaging come next" (README.md:16-20) | Phase 4 is implemented; master-plan marks it done (plan/master-plan.md:25). | High |
| 2 | README roadmap row 4 | "4 / PSI-based admission, stable `--json` schema, metrics export / next" (README.md:195) | Implemented except metrics export (see 1.1); status should be "done" and the scope reworded. | High |
| 3 | README "How it works" | "Phase 4 adds admission by real pressure (PSI)" (README.md:55-56) | Future tense for something that exists; defaults and the `[admission]` table are not described anywhere in the README. | Medium |
| 4 | Reservation and PSI | "reservation fires when the head is blocked by the budget (or, from Phase 4, by PSI)" (design/scheduler.md:74) | PSI never triggers a reservation; the guard is separate (see 1.2). | Medium |
| 5 | Reason precedence | protocol.md:117-121 | `psi` first (see 1.3). | Medium |
| 6 | Read failure | "files missing or unreadable" (design/psi-admission.md:25-27) | Both files must fail (see 1.4). | Medium |
| 7 | Validation list | "`admission.psi_poll_interval > 0`" only (design/config-format.md:142) | `psi_max_wait > 0` is also validated (the comment at line 104 says so, the list does not); thresholds are not validated (core:config.rs:568-579). | Low |
| 8 | Live config | "everything else is live" (design/config-format.md:143-146) | `[admission]` is read once at start (bin:daemon_cmd.rs:67). | Medium |
| 9 | `psi_blocked_ms_total` meaning | "time the current daemon spent blocking" (design/psi-admission.md:70-71); proto comment "0 when PSI is unavailable" (proto:lib.rs:518-519) | Sampled accumulation, see 2.6 and 1.7. | Low |
| 10 | Starvation guard wording | "if the queue head has waited more than `psi_max_wait` while PSI blocks, the head is admitted anyway if it fits ... at most one job per `psi_max_wait`" (design/psi-admission.md:47-52) | Matches the rule, but the config comment says "once the queue head has waited this long while blocked only by PSI" (core:config.rs:390-391) and config-format.md:104 says "after this long blocked by PSI"; the code does not check that PSI is the only blocker (it needs the head to fit, otherwise it does nothing). | Low |
| 11 | Status and text on disabled PSI | "(unavailable) (also shown when PSI is disabled in the config)" (design/psi-admission.md:73-74) | Accurate, but protocol.md:108 and the README present `enabled:false` only implicitly; `psi.enabled` is false both for config-off and kernel-unavailable and cannot be told apart. | Low |
| 12 | Acceptance evidence strength | "Thresholds documented with measurements" (changes/phase-4-acceptance.md:11; plan/master-plan.md:158-159) | Command arguments (20 and 40 loops, 15 s) are not stated, CPU only, no memory data, and no value near the 60 % default (see 1.6). | Medium |
| 13 | Acceptance: two samples | changes/phase-4-acceptance.md:10 | Integration test does not assert it (see 1.5). | Low |
| 14 | Verification wording | "`cargo test --workspace` green (agent run); the lead re-ran the full suite at the Phase 3/4 close" (changes/phase-4-acceptance.md:14-15, phase-4-psi.md:26-27) | Not verifiable from the repository; no log or test count is recorded (Phase 2 notes list counts). | Low |
| 15 | Script header vs behaviour | "prints the `some` line ... every second" and "Exits 2 when the kernel has no PSI" (scripts/psi-pressure-demo.sh:7-11) | Accurate. Also prints a `baseline:` and an `after:` line (1 s after stopping) which the header does not mention; `loops` is not validated (a non-number aborts under `set -eu` with a shell error and no cleanup message). | Low |
| 16 | Plan checklist | "[ ] Reconciliation pass" (plan/master-plan.md:149) | This report; the box should be ticked once the resolution is recorded. | Low |

## Summary

The PSI mechanism itself matches the design: strict thresholds (60.0 / 20.0), resume under
80 % for two consecutive samples, missing values counted as calm, permanent fallback after a
failed read, a 1 s sampler that wakes the scheduling loop on a verdict change, a starvation
guard that admits only the fitting head once per `psi_max_wait` with an info log naming the
job, the `status` field names (`budget.psi.{enabled,cpu_some_avg10,memory_some_avg10,blocking}`,
`totals.psi_blocked_ms_total`), the three `pressure:` line states and the config defaults and
`> 0` validation. The important gaps are outside the state machine: the README still says
Phases 1-3 and lists Phase 4 as "next"; the promised "metrics export" deliverable does not
exist and is not recorded as dropped; `scheduler.md` claims PSI can trigger the head reservation
(it cannot); `protocol.md` leaves out that `psi` takes precedence over every other reason; the
"files missing or unreadable" rule is stricter in the code (both files); `[admission]` needs a
daemon restart while `config-format.md` says everything else is live; and the acceptance
measurements are CPU-only and never approach the 60 % default, so the "2x oversubscription"
claim is an extrapolation. Most other differences are undocumented detail (guard timing,
`psi_blocked_ms_total` sampling, `n/a` rendering, unvalidated thresholds).

Counts: Table 1 (documented but not implemented) = 7; Table 2 (implemented but not
documented) = 13; Table 3 (mismatches) = 16.

## Resolution (2026-10-22, lead)

| Items | Resolution |
|---|---|
| 1.1, 3.2 | Decision recorded in the plan, README and psi-admission.md: `sluice status --json` (schema 1) is the metrics export; no separate exporter. |
| 2.1 (validation), 3.7 | Code: `cpu_some_avg10_max` and `memory_some_avg10_max` validated as finite percentages in `[0, 100]` (test); validation list updated. |
| 1.7, 2.12, 3.15 | Code: proto comment, `status --help` and the script header corrected; the script validates `loops`. |
| 1.2–1.6, 2.2–2.11, 2.13, 3.1, 3.3–3.6, 3.8–3.14, 3.16 | Documented in psi-admission.md, scheduler.md, protocol.md, config-format.md, README, the acceptance note (commands stated, extrapolation labelled, memory not measured, test counts) and the change note. |
