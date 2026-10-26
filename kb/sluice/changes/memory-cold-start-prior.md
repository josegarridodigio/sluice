---
title: "Change: data-driven first-run memory estimate"
tags: [sluice, change, memory, scheduler, admission, adr-0019]
---

# Change: cold-start prior for memory admission (0.6.1)

Decision: amendment of [[sluice/design/adr/0019-memory-safety-of-gate-jobs.md]] piece 1.
Rule: [[sluice/design/scheduler.md]] (memory rule). Written without running tests (the
`cargo test` hook): `cargo fmt` and `cargo check --workspace --all-targets -j 4` only.

## Why

Measured on a 12-thread, 30 GiB machine: a gate never run before is estimated at
`w / B x (MemTotal - reserve)`, 2.3 GiB for weight 1. Three first-run gates (`fmt`,
`lint`, `secrets`) that used about 20 MB each waited more than 5 minutes for memory
behind a test suite. Learned peaks on two machines: weight 1 gates 0.02-1.3 GiB,
weight 2 gates 0.2-2.2 GiB, weight 6-16 suites 4-13 GiB. The proxy is safe but far
too high for light gates.

## What changed

- Rule: a job with no learned peak of its own is estimated at `min(proxy, prior(w))`,
  `w` its effective weight and `prior(w)` the largest expected peak (same formula as
  learned peaks: sampled maximum x 1.2, killed x 1.25) among learned keys whose weight
  is `<= w`. No such key: the proxy (0.6.0 behaviour). The estimate stays `guessed`
  (`Expected.learned == false`), so over-admission still needs the gate's own peak, and
  `memory_expected_learned` stays false. No status/JSON change.
- `scheduler.rs`: `PeakPriors` (`BTreeMap<weight, largest peak>`), `prior_peak`,
  `MemoryFacts::estimate_primed` (`estimate` is the no-prior case), `Snapshot.priors`,
  `Scheduler::set_peak_priors`; used for queued jobs (`MemPass::new`), running jobs
  (`running_headroom_of`) and the `status` views. `plan` stays pure.
- `memory.rs`: `PeakCache` keeps the weight of each key (`set_weight`) and builds the
  table (`priors`); keys without a known weight are not used.
- `server.rs`: `close_job` reads the job's effective weight (`record.weight`) before it
  leaves the scheduler (`Closed.weight`); `announce_closed` records it with the sample
  and refreshes the priors; `load_peaks` loads the weights at start; `seed_peaks` seeds
  the priors.
- `store.rs`: `Store::gate_memory_weights` joins `gate_memory.job_id` with `jobs.weight`
  (newest run per key; a pruned job row leaves the key without weight).
- **No schema change**: no column added, `sched_schema` and the containment version
  untouched, no migration test needed.

## Tests

- `scheduler/tests.rs`: `the_prior_is_the_largest_peak_of_lighter_or_equal_weights`,
  `the_estimate_is_the_prior_capped_by_the_proxy_and_stays_a_guess`,
  `first_run_light_gates_start_beside_a_big_suite_with_a_light_prior` (the incident: a
  running learned 13 GiB job, about 1 GiB free, three weight-1 first runs start with a
  20 MiB prior and wait without it), `a_prior_never_over_admits`.
- `memory.rs`: `priors_are_the_largest_peak_per_weight_of_the_keys_with_a_known_weight`.
- `store.rs`: `gate_memory_weights_come_from_the_jobs_and_skip_pruned_ones`.
- No daemon-level test: the fake `proc` root of the meminfo harness gives no process
  samples, so a daemon cannot learn a peak there.

## Verification

`cargo fmt`, `cargo check --workspace --all-targets -j 4`; the full suite runs once at the
end of the task.
