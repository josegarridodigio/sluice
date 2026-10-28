//! Tests of ADR-0023: the shadow time of a reserved head, the two rules that
//! let a later job start before it, the switch, and replays of the evidence of
//! `kb/sluice/research/queue-scheduling-analysis.md`.

use super::*;

const SECOND: u64 = 1_000;

fn on(budget: u32) -> Policy {
    Policy {
        reservation: HeadReservation::Backfill,
        ..policy(budget)
    }
}

fn holder(weight: u32, end: u64, memory: u64) -> Holder<'static> {
    Holder {
        repo: Path::new("/repo"),
        weight,
        end,
        memory: memory * GIB,
        backfilled: false,
    }
}

fn head(weight: u32, peak_gib: Option<u64>) -> Head<'static> {
    Head {
        repo: Path::new("/repo"),
        weight,
        repo_cap: None,
        peak: peak_gib.map(|g| g * GIB),
    }
}

fn machine(budget: u32, free_gib: u64) -> Machine {
    Machine {
        budget,
        free: free_gib * GIB,
    }
}

/// A running job started at `started_at` that is expected to take `expected`.
fn run_for(id: &str, weight: u32, started_at: u64, expected: u64) -> RunningSnap {
    RunningSnap {
        started_at,
        expected_ms: Some(expected),
        ..r(id, &format!("/{id}"), weight)
    }
}

/// A queued job with a learned peak (GiB) and an optional learned duration.
fn want(id: &str, weight: u32, gib: u64, queued_at: u64, expected: Option<u64>) -> QueuedSnap {
    QueuedSnap {
        expected_ms: expected,
        ..peaked(id, weight, gib, queued_at)
    }
}

fn backfills(p: &Plan) -> Vec<(&str, BackfillRule)> {
    p.backfills
        .iter()
        .map(|(id, r)| (id.as_str(), *r))
        .collect()
}

// ---------------------------------------------------------------- shadow

#[test]
fn a_running_job_ends_at_start_plus_expected() {
    assert_eq!(predicted_end(10_000, 0, Some(60_000)), 60_000);
    // No history: the default guess feeds the prediction of a running job.
    assert_eq!(predicted_end(10_000, 0, None), DEFAULT_EXPECTED_MS);
    assert_eq!(predicted_end(0, 0, Some(5_000)), 5_000);
}

#[test]
fn a_job_past_its_expectation_gets_the_pessimistic_remainder() {
    // max(expected - elapsed, 0.1 x expected + 2 s): 1 s + 2 s from now.
    assert_eq!(predicted_end(15_000, 0, Some(10_000)), 18_000);
    // Exactly at its expectation it is already overrunning.
    assert_eq!(predicted_end(10_000, 0, Some(10_000)), 13_000);
    // Long jobs: 10 % of the expectation.
    assert_eq!(predicted_end(700_000, 0, Some(600_000)), 700_000 + 62_000);
}

#[test]
fn holders_are_released_in_order_of_predicted_end() {
    // Budget 6, three holders of 2 units; the head needs 4: two must go, the
    // two that end first, whatever the order they are listed in.
    let holders = [
        holder(2, 30_000, 0),
        holder(2, 10_000, 0),
        holder(2, 20_000, 0),
    ];
    let at = shadow(5_000, &holders, &head(4, None), &machine(6, 0));
    assert_eq!(at.at, 20_000);
    assert_eq!(at.released, [false, true, true]);
}

#[test]
fn a_head_that_already_fits_starts_now() {
    let at = shadow(
        5_000,
        &[holder(2, 30_000, 0)],
        &head(4, None),
        &machine(6, 0),
    );
    assert_eq!(at.at, 5_000);
    assert_eq!(at.released, [false]);
    let empty = shadow(5_000, &[], &head(4, None), &machine(6, 0));
    assert_eq!(empty.at, 5_000);
}

#[test]
fn a_budget_bound_head_waits_for_enough_units() {
    // 8 units, holders of 5 and 1 and 2; the head needs 6: after the 1 and the
    // 2 ends only the 5 holds (5 + 6 > 8), so it must wait for that one too.
    let holders = [
        holder(5, 100_000, 0),
        holder(1, 40_000, 0),
        holder(2, 50_000, 0),
    ];
    let at = shadow(0, &holders, &head(6, None), &machine(8, 0));
    assert_eq!(at.at, 100_000);
    assert_eq!(at.released, [true, true, true]);
    // The head needs only 3: 5 + 3 <= 8 once the 2 and the 1 are gone.
    let at = shadow(0, &holders, &head(3, None), &machine(8, 0));
    assert_eq!(at.at, 50_000);
    assert_eq!(at.released, [false, true, true]);
}

#[test]
fn a_memory_bound_head_counts_what_each_holder_hands_back() {
    // Plenty of budget; 5 GiB free, the head's peak is 10 GiB.
    let holders = [
        holder(1, 100_000, 10),
        holder(1, 50_000, 4),
        holder(1, 70_000, 2),
    ];
    // 5 + 4 + 2 = 11 >= 10 once the two that end first are gone.
    let at = shadow(0, &holders, &head(1, Some(10)), &machine(12, 5));
    assert_eq!(at.at, 70_000);
    assert_eq!(at.released, [false, true, true]);
    // 18 GiB needs the 10 GiB holder too.
    let at = shadow(0, &holders, &head(1, Some(18)), &machine(12, 5));
    assert_eq!(at.at, 100_000);
}

#[test]
fn a_head_too_big_for_the_machine_starts_when_it_is_empty() {
    // The progress exemption: nothing running, nothing to fit.
    let holders = [holder(1, 100_000, 4), holder(1, 50_000, 4)];
    let at = shadow(0, &holders, &head(1, Some(100)), &machine(12, 5));
    assert_eq!(at.at, 100_000);
    assert_eq!(at.released, [true, true]);
}

#[test]
fn a_capped_repository_delays_the_head_until_its_own_jobs_end() {
    let mut other = holder(1, 10_000, 0);
    other.repo = Path::new("/other");
    let holders = [holder(2, 40_000, 0), other];
    let capped = Head {
        repo_cap: Some(3),
        ..head(2, None)
    };
    // Budget is no problem; the cap of 3 is: 2 + 2 > 3 until the first ends.
    let at = shadow(0, &holders, &capped, &machine(12, 0));
    assert_eq!(at.at, 40_000);
}

#[test]
fn an_overrunning_job_pushes_the_shadow_by_the_pessimistic_remainder() {
    let now = 100_000;
    let late = predicted_end(now, 0, Some(60_000));
    let fresh = predicted_end(now, now, Some(60_000));
    let holders = [holder(4, late, 0), holder(4, fresh, 0)];
    let at = shadow(now, &holders, &head(4, None), &machine(8, 0));
    // The late one ends first (0.1 x 60 s + 2 s from now), then the head fits.
    assert_eq!(at.at, now + 8_000);
    assert_eq!(at.released, [true, false]);
}

// ---------------------------------------------------------------- room

#[test]
fn the_room_counts_survivors_the_head_and_everything_backfilled() {
    let finished = holder(2, 5_000, 4);
    let mut backfilled = holder(1, 20_000, 3);
    backfilled.backfilled = true;
    let survivor = holder(3, 90_000, 5);
    let holders = [finished, backfilled, survivor];
    let m = machine(12, 2);
    let h = head(4, Some(8));
    let at = shadow(0, &holders, &h, &m);
    // The head fits once the finished and the backfilled job are gone.
    assert_eq!(at.at, 20_000);
    assert_eq!(at.released, [true, true, false]);
    let counted = room(&holders, &at, &h, &m);
    // 12 - (3 survivor + 1 backfilled) - 4 head.
    assert_eq!(counted.weight, 4);
    // Free then: 2 + 4 (the finished job); the backfilled job's 3 GiB stay
    // taken, so the head's 8 GiB no longer fit: the room is closed.
    assert_eq!(counted.memory, Some(0));
    assert!(!counted.open);
    // The same job not flagged hands its memory back: 9 GiB, 1 GiB to spare.
    let mut plain = holders;
    plain[1].backfilled = false;
    let counted = room(&plain, &at, &h, &m);
    assert_eq!(counted.weight, 5);
    assert_eq!(counted.memory, Some(GIB));
    assert!(counted.open);
}

#[test]
fn a_room_admits_while_units_and_memory_last_and_takes_them() {
    let mut room = Room {
        weight: 3,
        memory: Some(5 * GIB),
        open: true,
        capped: None,
    };
    let repo = Path::new("/repo");
    assert!(room.admits(repo, 2, Some(4 * GIB)));
    assert!(!room.admits(repo, 4, Some(GIB)), "units");
    assert!(!room.admits(repo, 1, Some(6 * GIB)), "memory");
    room.take(2, Some(4 * GIB));
    assert!(
        !room.admits(repo, 2, Some(GIB)),
        "the first one took the units"
    );
    assert!(room.admits(repo, 1, Some(GIB)));
    room.take(1, Some(GIB));
    assert!(!room.admits(repo, 1, Some(0)));
}

#[test]
fn a_closed_room_or_a_capped_repository_admits_nothing() {
    let closed = Room {
        weight: 9,
        memory: Some(9 * GIB),
        open: false,
        capped: None,
    };
    assert!(!closed.admits(Path::new("/repo"), 1, Some(0)));
    let capped = Room {
        open: true,
        capped: Some(Path::new("/repo")),
        ..closed
    };
    assert!(!capped.admits(Path::new("/repo"), 1, Some(0)));
    assert!(capped.admits(Path::new("/other"), 1, Some(0)));
}

#[test]
fn the_duration_rule_uses_the_larger_of_the_two_margins() {
    // 1.5 x 60 s = 90 s beats 60 s + 10 s; 1 s + 10 s beats 1.5 s.
    assert!(ends_in_time(0, Some(60_000), 90_000));
    assert!(!ends_in_time(0, Some(60_000), 89_999));
    assert!(ends_in_time(5_000, Some(SECOND), 16_000));
    assert!(!ends_in_time(5_000, Some(SECOND), 15_999));
    // No learned duration, no bound: the default guess is never used.
    assert!(!ends_in_time(0, None, u64::MAX));
}

// ---------------------------------------------------------------- the pass

/// The evidence of section 2.1: a weight-6 suite runs, another waits for memory
/// as the overdue head, and a linter that runs in 105 ms waits behind it.
fn suite_blocks_a_head(backfill: bool, lint: QueuedSnap) -> Plan {
    let p = Policy {
        reservation: HeadReservation::backfill(backfill),
        ..policy(12)
    };
    let run = [RunningSnap {
        started_at: 0,
        expected_ms: Some(600_000),
        ..running("suite", 6, 14, 18)
    }];
    let blocked = want("head", 6, 18, 0, Some(600_000));
    let queued = [blocked, lint];
    // 16 GiB available, 3 reserved, 4 still to grow: 9 GiB free.
    let s = snapshot(100_000, &p, Pressure::default(), &run, &queued);
    plan(&with_memory(s, facts(16, 30, 3)))
}

fn linter() -> QueuedSnap {
    QueuedSnap {
        expected_peak: Some(GIB / 5),
        ..want("lint", 1, 0, 50_000, Some(105))
    }
}

#[test]
fn a_small_linter_starts_behind_a_memory_blocked_suite() {
    let strict = suite_blocks_a_head(false, linter());
    assert_eq!(started(&strict), Vec::new());
    assert_eq!(
        waits_of(&strict),
        [
            ("head", WaitCause::Memory, Some("memory")),
            ("lint", WaitCause::Reserved, None)
        ]
    );
    assert_eq!(strict.shadow, Some(600_000), "the ETA shows either way");

    let open = suite_blocks_a_head(true, linter());
    assert_eq!(open.reserved.as_deref(), Some("head"));
    assert_eq!(open.shadow, Some(600_000));
    assert_eq!(started(&open), [("lint", Admission::Nominal)]);
    assert_eq!(backfills(&open), [("lint", BackfillRule::ExtraResources)]);
    assert_eq!(
        waits_of(&open),
        [("head", WaitCause::Memory, Some("memory"))]
    );
}

#[test]
fn a_job_that_does_not_fit_now_never_backfills() {
    // 12 GiB does not fit the 9 free, whatever the room says.
    let big = QueuedSnap {
        expected_ms: Some(SECOND),
        ..peaked("big", 1, 12, 50_000)
    };
    let decided = suite_blocks_a_head(true, big);
    assert_eq!(started(&decided), Vec::new());
    assert_eq!(
        waits_of(&decided)[1],
        ("big", WaitCause::Memory, Some("memory"))
    );
}

/// A head of 22 GiB leaves 5 GiB of room beside it (27 usable at its shadow
/// time), while 9 GiB are free now: a job of 8 GiB fits now but not beside it.
fn big_head_pass(candidate: QueuedSnap, suite_ends: u64) -> Plan {
    let p = on(12);
    let run = [RunningSnap {
        started_at: 0,
        expected_ms: Some(suite_ends),
        ..running("suite", 6, 14, 18)
    }];
    let queued = [want("head", 6, 22, 0, Some(600_000)), candidate];
    let s = snapshot(100_000, &p, Pressure::default(), &run, &queued);
    plan(&with_memory(s, facts(16, 30, 3)))
}

#[test]
fn extra_resources_refuse_a_job_that_would_take_the_heads_memory() {
    let no_duration = want("job", 1, 8, 50_000, None);
    let decided = big_head_pass(no_duration, 600_000);
    assert_eq!(started(&decided), Vec::new());
    assert_eq!(waits_of(&decided)[1], ("job", WaitCause::Reserved, None));
}

#[test]
fn the_duration_rule_accepts_a_job_that_ends_before_the_shadow() {
    // Expected 1 s: 11 s of margin, the suite ends in 500 s.
    let quick = want("job", 1, 8, 50_000, Some(SECOND));
    let decided = big_head_pass(quick, 600_000);
    assert_eq!(started(&decided), [("job", Admission::Nominal)]);
    assert_eq!(backfills(&decided), [("job", BackfillRule::EndsInTime)]);
}

#[test]
fn the_duration_rule_refuses_a_job_that_may_not_end_in_time() {
    // The suite ends 5 s from now: 1 s + 10 s of margin does not fit.
    let quick = want("job", 1, 8, 50_000, Some(SECOND));
    let decided = big_head_pass(quick, 105_000);
    assert_eq!(decided.shadow, Some(105_000));
    assert_eq!(started(&decided), Vec::new());
    // A suite past its expectation: 0.1 x 60 s + 2 s from now = 8 s.
    let quick = want("job", 1, 8, 50_000, Some(SECOND));
    let decided = big_head_pass(quick, 60_000);
    assert_eq!(decided.shadow, Some(108_000));
    assert_eq!(started(&decided), Vec::new());
}

#[test]
fn a_guessed_candidate_backfills_on_its_estimate_and_never_by_duration() {
    // No learned peak and no duration: guessed at the weight share, 2 x
    // 27 / 12 = 4.5 GiB, which fits the 5 GiB of room.
    let first_run = QueuedSnap {
        expected_peak: None,
        ..want("job", 2, 0, 50_000, None)
    };
    let decided = big_head_pass(first_run, 600_000);
    assert_eq!(decided.expected[1].basis, Basis::Guessed);
    assert_eq!(backfills(&decided), [("job", BackfillRule::ExtraResources)]);
    // Weight 3 guesses 6.75 GiB: beyond the room, and no duration to use.
    let heavier = QueuedSnap {
        expected_peak: None,
        ..want("job", 3, 0, 50_000, None)
    };
    assert_eq!(started(&big_head_pass(heavier, 600_000)), Vec::new());
}

#[test]
fn a_probe_never_backfills() {
    // The candidate does not fit its 13.5 GiB guess (9 free) but a quarter of
    // it would: behind a reserved head it waits for memory.
    let p = on(12);
    let run = [RunningSnap {
        started_at: 0,
        expected_ms: Some(600_000),
        ..running("suite", 6, 14, 18)
    }];
    let first = QueuedSnap {
        expected_ms: Some(SECOND),
        learn_key: Some(lk("pytest")),
        ..q("first", "/first", 6, 50_000)
    };
    let queued = [want("head", 6, 18, 0, Some(600_000)), first.clone()];
    let at = 100_000;
    let s = snapshot(at, &p, Pressure::default(), &run, &queued);
    let held = plan(&with_memory(s, facts(16, 30, 3)));
    assert_eq!(started(&held), Vec::new());
    assert_eq!(held.probes, Vec::<String>::new());
    assert_eq!(
        waits_of(&held)[1],
        ("first", WaitCause::Memory, Some("memory"))
    );
    // With no overdue head the same job probes, as before.
    let fresh = [
        QueuedSnap {
            queued_at: at,
            ..queued[0].clone()
        },
        first,
    ];
    let s = snapshot(at, &p, Pressure::default(), &run, &fresh);
    let free = plan(&with_memory(s, facts(16, 30, 3)));
    assert_eq!(free.reserved, None);
    assert_eq!(free.probes, ["first"]);
}

#[test]
fn a_running_probe_hands_back_a_quarter_and_never_takes_room() {
    // The shadow counts what a running probe holds (a quarter of its
    // estimate), the same figure admission holds back for it.
    let p = on(12);
    let mut probe = running("probe", 3, 0, 8);
    probe.probe = true;
    probe.expected_peak = None;
    probe.learn_key = Some(lk("pytest"));
    let s = snapshot(100_000, &p, Pressure::default(), &[], &[]);
    let s = with_memory(s, facts(20, 30, 3));
    let pass = Pass::new(Snapshot {
        running: std::slice::from_ref(&probe),
        ..s
    });
    // Guessed at 3 / 12 of 27 GiB = 6.75 GiB: a quarter is 1.6875 GiB.
    assert_eq!(pass.holders[0].memory, 27 * GIB / 4 / 4);
}

// ---------------------------------------------------------------- the pass: counting

/// Budget 14: a weight-6 job ends in 69 s, a weight-2 job in 9 s; the head of
/// weight 9 waits (8 + 9 > 14) and its shadow time is the end of the first.
fn budget_pass(run: &[RunningSnap], queued: &[QueuedSnap], backfill: bool) -> Plan {
    let p = Policy {
        reservation: HeadReservation::backfill(backfill),
        ..policy(14)
    };
    plan(&snapshot(31_000, &p, Pressure::default(), run, queued))
}

fn budget_run() -> Vec<RunningSnap> {
    vec![run_for("a", 6, 0, 100_000), run_for("b", 2, 0, 40_000)]
}

fn budget_queue(units: &[u32]) -> Vec<QueuedSnap> {
    let mut queued = vec![q("head", "/head", 9, 0)];
    for (n, &w) in units.iter().enumerate() {
        let at = 1_000 + u64::try_from(n).unwrap();
        queued.push(q(&format!("c{n}"), &format!("/c{n}"), w, at));
    }
    queued
}

#[test]
fn every_job_already_backfilled_in_the_pass_takes_room() {
    // Room: 14 - 9 = 5 units; three jobs of 2 fit now (8 + 2 + 2 + 2 = 14) but
    // only two fit the room.
    let decided = budget_pass(&budget_run(), &budget_queue(&[2, 2, 2]), true);
    assert_eq!(decided.shadow, Some(100_000));
    assert_eq!(
        backfills(&decided),
        [
            ("c0", BackfillRule::ExtraResources),
            ("c1", BackfillRule::ExtraResources)
        ]
    );
    assert_eq!(waits_of(&decided)[1], ("c2", WaitCause::Reserved, None));
}

#[test]
fn a_job_backfilled_by_an_earlier_pass_still_takes_room() {
    // Same machine, plus a running job that started behind the head: it ends
    // before the shadow, but counts for the whole interval.
    let mut run = budget_run();
    run.push(RunningSnap {
        backfilled: true,
        ..run_for("old", 2, 30_000, 1_000)
    });
    let queued = budget_queue(&[4]);
    // Room: 14 - (2 old) - 9 = 3 < 4.
    let decided = budget_pass(&run, &queued, true);
    assert_eq!(started(&decided), Vec::new());
    // The same job, not backfilled, ends and gives its units back: room 5.
    run[2].backfilled = false;
    let decided = budget_pass(&run, &queued, true);
    assert_eq!(backfills(&decided), [("c0", BackfillRule::ExtraResources)]);
}

#[test]
fn a_job_that_needs_the_heads_units_waits_for_it() {
    // Weight 6 fits now (8 + 6 = 14) but would sit on the head's units.
    let decided = budget_pass(&budget_run(), &budget_queue(&[6]), true);
    assert_eq!(started(&decided), Vec::new());
}

#[test]
fn the_switch_restores_the_strict_reservation() {
    let strict = budget_pass(&budget_run(), &budget_queue(&[1, 1]), false);
    assert_eq!(started(&strict), Vec::new());
    assert_eq!(strict.backfills, Vec::new());
    assert_eq!(strict.reserved.as_deref(), Some("head"));
    assert_eq!(
        waits_of(&strict)[1..],
        [
            ("c0", WaitCause::Reserved, None),
            ("c1", WaitCause::Reserved, None)
        ]
    );
}

#[test]
fn nothing_backfills_while_pressure_blocks() {
    let p = on(14);
    let run = budget_run();
    let queued = budget_queue(&[1]);
    let blocking = Pressure {
        blocking: true,
        ..Pressure::default()
    };
    let decided = plan(&snapshot(31_000, &p, blocking, &run, &queued));
    assert_eq!(started(&decided), Vec::new());
    assert_eq!(decided.backfills, Vec::new());
    assert_eq!(decided.shadow, None);
    assert!(
        decided.waits.iter().all(|w| w.cause == WaitCause::Psi),
        "{:?}",
        decided.waits
    );
}

#[test]
fn a_head_that_is_not_overdue_reserves_nothing_and_backfill_is_plain() {
    let p = on(14);
    let run = budget_run();
    let queued = budget_queue(&[1]);
    let decided = plan(&snapshot(10_000, &p, Pressure::default(), &run, &queued));
    assert_eq!(decided.reserved, None);
    assert_eq!(decided.shadow, None);
    assert_eq!(started(&decided), [("c0", Admission::Nominal)]);
    assert_eq!(decided.backfills, Vec::new(), "no reservation, no backfill");
}

#[test]
fn a_backfill_is_never_an_over_admission() {
    let p = Policy { cap: 20, ..on(14) };
    let run = budget_run();
    let queued = budget_queue(&[2]);
    let decided = plan(&snapshot(31_000, &p, calm(8.0), &run, &queued));
    assert_eq!(started(&decided), [("c0", Admission::Nominal)]);
}

#[test]
fn a_candidate_of_a_capped_repository_of_the_head_never_takes_room() {
    let mut caps = BTreeMap::new();
    caps.insert(PathBuf::from("/repo"), 12);
    let p = Policy {
        repo_caps: caps,
        ..on(14)
    };
    let run = budget_run();
    let queued = budget_queue(&[1]);
    let decided = plan(&snapshot(31_000, &p, Pressure::default(), &run, &queued));
    assert_eq!(started(&decided), Vec::new());
}

// ---------------------------------------------------------------- the scheduler

fn timed(id: &str, weight: u32, queued_at: u64, expected: Option<u64>) -> JobSpec {
    JobSpec {
        expected_ms: expected,
        ..spec(id, None, "/r", weight, queued_at)
    }
}

fn backfilling(budget: u32) -> Scheduler {
    Scheduler::with_policy(Policy {
        reservation: HeadReservation::Backfill,
        ..policy(budget)
    })
}

#[test]
fn the_scheduler_logs_counts_and_measures_a_backfill() {
    let mut s = backfilling(14);
    s.enqueue(timed("a", 6, 0, Some(100_000)));
    s.enqueue(timed("b", 2, 0, Some(40_000)));
    assert_eq!(s.admit(0, false), ["a", "b"]);
    s.enqueue(timed("head", 9, 1_000, None));
    s.enqueue(timed("c", 2, 2_000, None));
    assert_eq!(s.admit(31_000, false), ["c"]);

    let (_, queued) = s.snapshot(31_000, false);
    assert_eq!(queued[0].id, "head");
    assert_eq!(queued[0].shadow_at, Some(100_000), "the ETA of the head");
    assert!(queued.iter().skip(1).all(|v| v.shadow_at.is_none()));

    let notes = s.take_backfill_notes();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].rule, BackfillRule::ExtraResources);
    let line = backfill_line(&notes[0]);
    assert!(
        line.contains("job c (r:test) starts before the reserved head job head (r:test)")
            && line.contains("(extra resources)")
            && line.contains("expected unknown")
            && line.contains("in 69.0s"),
        "{line}"
    );
    assert_eq!(s.take_backfill_notes(), Vec::new(), "once");

    // The head starts 5 s after its first shadow time.
    for id in ["a", "b", "c"] {
        assert!(s.finish(id).is_some());
    }
    assert_eq!(s.admit(105_000, false), ["head"]);
    let stats = s.backfill_stats();
    assert_eq!(stats.backfilled, 1);
    assert_eq!(stats.head_delay_ms, 5_000);
}

#[test]
fn a_head_that_starts_early_adds_no_delay() {
    let mut s = backfilling(14);
    s.enqueue(timed("a", 6, 0, Some(100_000)));
    s.enqueue(timed("b", 2, 0, Some(40_000)));
    s.admit(0, false);
    s.enqueue(timed("head", 9, 1_000, None));
    s.admit(31_000, false);
    s.finish("a");
    s.finish("b");
    assert_eq!(s.admit(50_000, false), ["head"]);
    assert_eq!(s.backfill_stats().head_delay_ms, 0);
    assert_eq!(s.backfill_stats().backfilled, 0);
}

#[test]
fn a_backfilled_job_is_counted_by_the_next_pass() {
    let mut s = backfilling(14);
    s.enqueue(timed("a", 6, 0, Some(100_000)));
    s.enqueue(timed("b", 2, 0, Some(40_000)));
    s.admit(0, false);
    s.enqueue(timed("head", 9, 1_000, None));
    s.enqueue(timed("c", 3, 2_000, None));
    assert_eq!(s.admit(31_000, false), ["c"]);
    // Room was 5 units: c took 3, so a second job of 3 waits in the next pass.
    s.enqueue(timed("d", 3, 32_000, None));
    assert_eq!(s.admit(33_000, false), Vec::<String>::new());
    // A job of 2 still fits the 2 units left.
    s.enqueue(timed("e", 2, 33_000, None));
    assert_eq!(s.admit(34_000, false), ["e"]);
}

// ---------------------------------------------------------------- a replay

/// One job of the replay: arrival, class and the real run time.
struct Arrival {
    id: String,
    at: u64,
    class: &'static str,
    /// The gate has history: a learned duration and peak.
    known: bool,
    weight: u32,
    gib: u64,
    expected: u64,
    actual: u64,
}

fn arrivals(rng: &mut Lcg) -> Vec<Arrival> {
    let mut out = Vec::new();
    for n in 0..70 {
        let at = rng.below(900) * SECOND;
        let (class, weight, gib, expected) = match n {
            n if n % 14 == 0 => ("heavy", 6, 18, 120 * SECOND),
            n if n % 5 == 0 => ("mid", 2, 3, (20 + rng.below(40)) * SECOND),
            _ => ("small", 1, 1, (1 + rng.below(12)) * SECOND),
        };
        // Real run times: half to two times the expectation.
        let actual = expected / 2 + rng.below(expected * 3 / 2 + 1);
        out.push(Arrival {
            id: format!("n{n}"),
            at,
            class,
            // A third of the light jobs are first runs.
            known: class == "heavy" || n % 3 != 1,
            weight,
            gib,
            expected,
            actual,
        });
    }
    out.sort_by_key(|a| a.at);
    out
}

fn replay_spec(a: &Arrival) -> JobSpec {
    JobSpec {
        expected_ms: a.known.then_some(a.expected),
        learn_key: a.known.then(|| lk(a.class)),
        ..spec(&a.id, None, "/r", a.weight, a.at)
    }
}

/// Runs the arrivals through a scheduler (12 units, 30 GiB with 4 GiB of
/// everything else) one second at a time; returns when each heavy job started.
fn replay(backfill: bool, jobs: &[Arrival]) -> (BTreeMap<String, u64>, u64) {
    let mut s = Scheduler::with_policy(Policy {
        reservation: HeadReservation::backfill(backfill),
        ..policy(12)
    });
    for (class, gib) in [("heavy", 18), ("mid", 3), ("small", 1)] {
        s.set_learned_peak(lk(class), Some(gib * GIB));
    }
    let heavy = jobs.iter().filter(|a| a.class == "heavy").count();
    let mut running: Vec<(u64, &Arrival)> = Vec::new();
    let mut starts = BTreeMap::new();
    let mut now = 0;
    while now <= 4_000 * SECOND && starts.len() < heavy {
        for a in jobs.iter().filter(|a| a.at == now) {
            s.enqueue(replay_spec(a));
        }
        running.retain(|(end, a)| {
            let over = *end <= now;
            if over {
                s.finish(&a.id);
            }
            !over
        });
        let used: u64 = running.iter().map(|(_, a)| a.gib).sum();
        let currents = running
            .iter()
            .map(|(_, a)| (a.id.clone(), a.gib * GIB))
            .collect();
        s.set_memory(Some(facts(30 - 4 - used, 30, 3)), currents);
        for id in s.admit(now, false) {
            let a = jobs.iter().find(|a| a.id == id).unwrap();
            running.push((now + a.actual, a));
            if a.class == "heavy" {
                starts.insert(id, now);
            }
        }
        now += SECOND;
    }
    (starts, s.backfill_stats().backfilled)
}

/// ADR-0023, replay: whatever the arrivals, no heavy job starts later with
/// backfilling than with the strict reservation (a tolerance for the real
/// overrun of a job that backfilled by duration and the one-second tick).
#[test]
fn backfilling_never_delays_a_heavy_job_in_the_replays() {
    let mut rng = Lcg(2026);
    let mut backfilled = 0;
    for case in 0..12 {
        let jobs = arrivals(&mut rng);
        let (strict, none) = replay(false, &jobs);
        let (open, count) = replay(true, &jobs);
        assert_eq!(none, 0, "the switch off backfills nothing");
        backfilled += count;
        assert_eq!(open.len(), strict.len(), "case {case}: every heavy starts");
        for (id, at) in &open {
            assert!(
                *at <= strict[id] + 15 * SECOND,
                "case {case}: {id} started at {at} instead of {}",
                strict[id]
            );
        }
    }
    assert!(
        backfilled > 20,
        "the replays exercise backfilling: {backfilled}"
    );
}
