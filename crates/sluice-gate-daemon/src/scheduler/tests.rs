//! Unit tests of the admission rules. The ADR-0007 tests run with every
//! ADR-0011 switch off; the rest cover one rule each over pure snapshots.

use super::*;

const RESERVE: u64 = 30_000;

fn key(c: char) -> CacheKey {
    CacheKey::try_from(c.to_string().repeat(64)).unwrap()
}

fn spec(id: &str, key: Option<CacheKey>, repo: &str, weight: u32, queued_at: u64) -> JobSpec {
    JobSpec {
        id: id.into(),
        key,
        repo: PathBuf::from(repo),
        lane: PathBuf::from(repo),
        gate: "test".into(),
        weight,
        queued_at,
        ..JobSpec::default()
    }
}

fn sched(budget: u32) -> Scheduler {
    Scheduler::new(budget, RESERVE, BTreeMap::new())
}

// ---------------------------------------------------------------- ADR-0007

#[test]
fn same_key_coalesces() {
    let mut s = sched(8);
    assert_eq!(
        s.enqueue(spec("j0", Some(key('a')), "/r", 1, 0)),
        Enqueued::Created { position: 1 }
    );
    for i in 1..6 {
        assert_eq!(
            s.enqueue(spec(&format!("j{i}"), Some(key('a')), "/r", 1, 0)),
            Enqueued::Coalesced {
                job_id: "j0".into(),
                position: Some(1)
            }
        );
    }
    assert_eq!(s.subscribers("j0"), Some(6));
    assert_eq!(s.subscribers_max("j0"), Some(6));
    assert_eq!(s.find_by_key(&key('a')), Some("j0"));
    assert!(!s.is_queued("j1"));
}

#[test]
fn budget_limits_concurrency() {
    let mut s = sched(8);
    for (i, c) in ['a', 'b', 'c', 'd'].into_iter().enumerate() {
        s.enqueue(spec(&format!("j{i}"), Some(key(c)), "/r", 4, 0));
    }
    assert_eq!(s.admit(0, false), vec!["j0", "j1"]);
    assert_eq!(s.used(), 8);
    assert_eq!(s.admit(1, false), Vec::<String>::new());
    assert!(s.finish("j0").is_some());
    assert_eq!(s.used(), 4);
    assert_eq!(s.admit(2, false), vec!["j2"]);
    assert_eq!(s.used(), 8);
    assert!(s.is_queued("j3"));
}

#[test]
fn backfill_then_reservation() {
    let mut s = sched(8);
    s.enqueue(spec("r", None, "/r", 4, 0));
    assert_eq!(s.admit(0, false), vec!["r"]);
    s.enqueue(spec("head", None, "/r", 6, 0));
    s.enqueue(spec("l1", None, "/r", 1, 500));
    s.enqueue(spec("l2", None, "/r", 1, 600));
    assert_eq!(s.admit(1_000, false), vec!["l1", "l2"]);
    assert_eq!(s.used(), 6);

    s.enqueue(spec("l3", None, "/r", 1, 31_000));
    assert_eq!(s.admit(31_000, false), Vec::<String>::new());
    let (_, queued) = s.snapshot(31_000, false);
    assert_eq!(queued[0].id, "head");
    assert_eq!(queued[0].reason, WaitReason::Budget);
    assert_eq!(queued[1].id, "l3");
    assert_eq!(queued[1].reason, WaitReason::HeadReserved);

    for id in ["r", "l1", "l2"] {
        assert!(s.finish(id).is_some());
    }
    assert_eq!(s.admit(32_000, false), vec!["head", "l3"]);
    assert_eq!(s.used(), 7);
}

#[test]
fn repo_cap_blocks_only_that_repo() {
    let caps = BTreeMap::from([(PathBuf::from("/a"), 2)]);
    let mut s = Scheduler::new(8, RESERVE, caps);
    s.enqueue(spec("a1", None, "/a", 1, 0));
    s.enqueue(spec("a2", None, "/a", 1, 0));
    s.enqueue(spec("a3", None, "/a", 1, 0));
    s.enqueue(spec("b1", None, "/b", 1, 0));
    assert_eq!(s.admit(10, false), vec!["a1", "a2", "b1"]);
    let (_, queued) = s.snapshot(10, false);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].id, "a3");
    assert_eq!(queued[0].reason, WaitReason::RepoCap);
}

#[test]
fn weight_is_clamped_to_repo_cap() {
    let caps = BTreeMap::from([(PathBuf::from("/a"), 2)]);
    let mut s = Scheduler::new(8, RESERVE, caps);
    s.enqueue(spec("heavy", None, "/a", 5, 0));
    assert_eq!(s.spec("heavy").unwrap().weight, 2);
    assert_eq!(s.admit(0, false), vec!["heavy"]);
    assert_eq!(s.used(), 2);
}

#[test]
fn cap_blocked_head_does_not_reserve() {
    let caps = BTreeMap::from([(PathBuf::from("/a"), 2)]);
    let mut s = Scheduler::new(8, RESERVE, caps);
    s.enqueue(spec("a1", None, "/a", 2, 0));
    assert_eq!(s.admit(0, false), vec!["a1"]);
    s.enqueue(spec("a2", None, "/a", 2, 0));
    s.enqueue(spec("b1", None, "/b", 1, 40_000));
    // The head (a2) has waited past head_reserve_after but is blocked only
    // by its repository cap: b1 still starts.
    assert_eq!(s.admit(40_000, false), vec!["b1"]);
    let (_, queued) = s.snapshot(40_000, false);
    assert_eq!(queued[0].id, "a2");
    assert_eq!(queued[0].reason, WaitReason::RepoCap);
    s.enqueue(spec("b2", None, "/b", 1, 40_001));
    let (_, queued) = s.snapshot(40_001, false);
    assert_eq!(queued[1].reason, WaitReason::Budget);
}

#[test]
fn psi_blocks_everything() {
    let mut s = sched(8);
    s.enqueue(spec("j0", None, "/r", 1, 0));
    s.enqueue(spec("j1", None, "/r", 1, 0));
    assert_eq!(s.admit(0, true), Vec::<String>::new());
    let (running, queued) = s.snapshot(0, true);
    assert_eq!(running, Vec::new());
    assert!(queued.iter().all(|q| q.reason == WaitReason::Psi));
    assert_eq!(s.admit(1, false), vec!["j0", "j1"]);
}

#[test]
fn admit_head_only_when_it_fits() {
    let mut s = sched(4);
    assert_eq!(s.head_queued_at(), None);
    assert_eq!(s.admit_head(0), None);
    s.enqueue(spec("big", None, "/r", 3, 5));
    s.enqueue(spec("small", None, "/r", 1, 6));
    s.enqueue(spec("next", None, "/r", 3, 7));
    assert_eq!(s.head_queued_at(), Some(5));
    assert_eq!(s.admit_head(10).as_deref(), Some("big"));
    assert!(s.is_running("big") && s.is_queued("small"));
    assert_eq!(s.admit_head(11).as_deref(), Some("small"));
    assert_eq!(s.admit_head(12), None, "next does not fit the budget");
    assert_eq!(s.head_queued_at(), Some(7));
}

#[test]
fn weight_is_clamped() {
    let mut s = sched(8);
    s.enqueue(spec("big", None, "/r", 50, 0));
    s.enqueue(spec("small", None, "/r", 1, 0));
    s.enqueue(spec("zero", None, "/r", 0, 0));
    assert_eq!(s.spec("big").unwrap().weight, 8);
    assert_eq!(s.spec("zero").unwrap().weight, 1);
    assert_eq!(s.admit(0, false), vec!["big"]);
    assert_eq!(s.used(), 8);
    assert_eq!(Scheduler::new(0, RESERVE, BTreeMap::new()).budget(), 1);
}

#[test]
fn unknown_ids_and_unsubscribe() {
    let mut s = sched(8);
    assert!(s.remove_queued("nope").is_none());
    assert!(s.finish("nope").is_none());
    assert!(s.subscribe("nope").is_none());
    assert!(s.unsubscribe("nope").is_none());
    s.enqueue(spec("j0", None, "/r", 1, 0));
    assert!(s.finish("j0").is_none(), "queued, not running");
    assert_eq!(s.subscribe("j0"), Some(2));
    assert_eq!(s.unsubscribe("j0"), Some(1));
    assert_eq!(s.unsubscribe("j0"), Some(0));
    assert_eq!(s.subscribers_max("j0"), Some(2));
    assert_eq!(s.remove_queued("j0").map(|j| j.id), Some("j0".into()));
    assert!(!s.is_queued("j0"));
}

#[test]
fn coalesce_onto_running_and_null_key() {
    let mut s = sched(8);
    s.enqueue(spec("j0", Some(key('a')), "/r", 1, 0));
    s.admit(5, false);
    assert!(s.is_running("j0"));
    assert_eq!(
        s.enqueue(spec("j1", Some(key('a')), "/other", 1, 6)),
        Enqueued::Coalesced {
            job_id: "j0".into(),
            position: None
        }
    );
    assert_eq!(s.subscribers("j0"), Some(2));
    assert_eq!(
        s.enqueue(spec("n1", None, "/r", 8, 0)),
        Enqueued::Created { position: 1 }
    );
    assert_eq!(
        s.enqueue(spec("n2", None, "/r", 8, 0)),
        Enqueued::Created { position: 2 }
    );
    assert!(s.finish("j0").is_some());
    assert!(s.find_by_key(&key('a')).is_none());
}

#[test]
fn snapshot_order_and_positions() {
    let mut s = sched(2);
    for (i, id) in ["x", "y", "z", "w"].into_iter().enumerate() {
        s.enqueue(spec(id, None, "/r", 1, i as u64));
    }
    assert_eq!(s.admit(10, false), vec!["x", "y"]);
    let (running, queued) = s.snapshot(10, false);
    assert_eq!(
        running.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["x", "y"]
    );
    assert!(running.iter().all(|r| r.started_at == 10));
    let ids: Vec<&str> = queued.iter().map(|q| q.id.as_str()).collect();
    assert_eq!(ids, ["z", "w"]);
    assert_eq!(s.position("z"), Some(1));
    assert_eq!(s.position("w"), Some(2));
    assert_eq!(s.position("x"), None);
    assert!(queued.iter().all(|q| q.reason == WaitReason::Budget));
}

// ---------------------------------------------------------------- helpers

fn policy(budget: u32) -> Policy {
    Policy::fifo(budget, RESERVE, BTreeMap::new())
}

fn q(id: &str, lane: &str, weight: u32, queued_at: u64) -> QueuedSnap {
    QueuedSnap {
        id: id.into(),
        repo: PathBuf::from("/repo"),
        lane: PathBuf::from(lane),
        weight_cfg: weight,
        learn_key: None,
        class: Class::Agent,
        queued_at,
        expected_ms: None,
        expected_peak: None,
    }
}

fn r(id: &str, lane: &str, weight: u32) -> RunningSnap {
    RunningSnap {
        id: id.into(),
        repo: PathBuf::from("/repo"),
        lane: PathBuf::from(lane),
        weight,
        current: 0,
        expected_peak: None,
    }
}

fn calm(idle: f64) -> Pressure {
    Pressure {
        calm: true,
        idle_cpus: Some(idle),
        ..Pressure::default()
    }
}

fn snapshot<'a>(
    now: u64,
    policy: &'a Policy,
    pressure: Pressure,
    running: &'a [RunningSnap],
    queued: &'a [QueuedSnap],
) -> Snapshot<'a> {
    Snapshot {
        now,
        policy,
        pressure,
        last_overcommit_at: None,
        last_start_at: None,
        running,
        queued,
        learned: &NO_LEARNED,
        memory: None,
    }
}

static NO_LEARNED: Learned = BTreeMap::new();

/// A learned-statistics key of repository `/r`.
fn lk(gate: &str) -> LearnKey {
    (PathBuf::from("/r"), gate.into(), "c0ffee".into())
}

fn started(p: &Plan) -> Vec<(&str, Admission)> {
    p.start.iter().map(|(id, a)| (id.as_str(), *a)).collect()
}

fn ranked<'a>(s: &Snapshot<'a>) -> Vec<&'a str> {
    rank(s)
        .into_iter()
        .map(|i| s.queued[i].id.as_str())
        .collect()
}

// ---------------------------------------------------------------- M2

#[test]
fn learned_weight_warm_up_floor_and_ceiling() {
    // Fewer than three samples: the configured weight.
    assert_eq!(effective_weight(8, &[]), 8);
    assert_eq!(effective_weight(8, &[6_100, 6_400]), 8);
    // Three samples: P75 rounded half up.
    assert_eq!(effective_weight(8, &[6_100, 6_400, 5_900]), 6);
    assert_eq!(
        effective_weight(8, &[6_500, 6_500, 6_500]),
        7,
        "6.5 rounds up"
    );
    // Never above the configured weight.
    assert_eq!(effective_weight(8, &[12_000, 11_000, 15_000]), 8);
    // Never below ceil(w / 4) (and never below 1).
    assert_eq!(effective_weight(8, &[100, 0, 50]), 2);
    assert_eq!(effective_weight(9, &[100, 0, 50]), 3);
    assert_eq!(effective_weight(1, &[0, 0, 0]), 1);
    assert_eq!(effective_weight(0, &[0, 0, 0]), 1);
    // Only the last ten samples count.
    let mut old = vec![8_000; 30];
    old.extend([3_000; 10]);
    assert_eq!(effective_weight(8, &old), 3);
}

#[test]
fn learned_weights_admit_more_suites() {
    // Weight 8 in budget 20 runs two suites; the learned 6 runs three.
    let mut fixed = Scheduler::new(20, RESERVE, BTreeMap::new());
    let mut learned = Scheduler::with_policy(Policy {
        learned_weights: true,
        ..policy(20)
    });
    for s in [&mut fixed, &mut learned] {
        for i in 0..5 {
            let mut j = spec(&format!("t{i}"), None, "/r", 8, i);
            j.learn_key = Some(lk("t"));
            j.demands_milli = vec![6_100, 6_400, 6_000];
            s.enqueue(j);
        }
    }
    assert_eq!(fixed.admit(10, false).len(), 2);
    assert_eq!(learned.admit(10, false).len(), 3);
    assert_eq!(learned.spec("t0").unwrap().weight, 6);
    assert_eq!(learned.spec("t0").unwrap().weight_cfg, 8);
    // Learned weights off: the samples are ignored.
    assert_eq!(fixed.spec("t0").unwrap().weight, 8);
}

#[test]
fn expected_time_is_the_median_of_the_last_ten() {
    assert_eq!(expected_ms(&[]), None);
    assert_eq!(expected_ms(&[5]), Some(5));
    assert_eq!(expected_ms(&[1, 9, 3]), Some(3));
    assert_eq!(expected_ms(&[1, 3, 5, 9]), Some(4));
    let mut v = vec![1_000_000; 5];
    v.extend([10; 10]);
    assert_eq!(expected_ms(&v), Some(10));
}

// ---------------------------------------------------------------- M3

fn three_running_suites() -> Vec<RunningSnap> {
    vec![r("a", "/a", 6), r("b", "/b", 6), r("c", "/c", 6)]
}

fn over_policy() -> Policy {
    Policy {
        cap: Policy::cap_for(20, 1.5),
        ..policy(20)
    }
}

#[test]
fn cap_is_floor_of_overcommit_times_budget() {
    assert_eq!(Policy::cap_for(20, 1.5), 30);
    assert_eq!(Policy::cap_for(6, 1.5), 9);
    assert_eq!(Policy::cap_for(5, 1.5), 7);
    assert_eq!(Policy::cap_for(8, 1.0), 8);
    assert_eq!(Policy::cap_for(8, 0.5), 8, "never below the budget");
}

#[test]
fn over_admission_needs_calm_idle_cpus_and_the_interval() {
    let p = over_policy();
    let running = three_running_suites();
    let queued = [q("d", "/d", 6, 0)];
    // Calm, two idle CPUs (ceil(6/3) = 2): a fourth suite starts over budget.
    let s = snapshot(1_000, &p, calm(2.0), &running, &queued);
    assert_eq!(started(&plan(&s)), [("d", Admission::Over)]);
    // Not enough idle CPUs.
    let s = snapshot(1_000, &p, calm(1.9), &running, &queued);
    assert_eq!(plan(&s).start, Vec::new());
    // Idle CPUs unknown (no utilisation sample yet).
    let s = snapshot(
        1_000,
        &p,
        Pressure {
            calm: true,
            ..Pressure::default()
        },
        &running,
        &queued,
    );
    assert_eq!(plan(&s).start, Vec::new());
    // Not calm (PSI above the low marks, unavailable or just resumed).
    let s = snapshot(1_000, &p, Pressure::default(), &running, &queued);
    let pl = plan(&s);
    assert_eq!(pl.start, Vec::new());
    assert_eq!(pl.waits[0].cause, WaitCause::Budget);
    // One over-admission per interval.
    let mut s = snapshot(6_000, &p, calm(8.0), &running, &queued);
    s.last_overcommit_at = Some(1_500);
    assert!(plan(&s).start.is_empty(), "4.5 s since the last one");
    s.last_overcommit_at = Some(1_000);
    assert_eq!(started(&plan(&s)), [("d", Admission::Over)]);
    // Over-admission switched off (cap = budget).
    let off = policy(20);
    let s = snapshot(1_000, &off, calm(8.0), &running, &queued);
    assert_eq!(plan(&s).start, Vec::new());
}

#[test]
fn over_admission_never_exceeds_the_cap_nor_runs_while_blocking() {
    let p = over_policy();
    let mut running = three_running_suites();
    running.push(r("d", "/d", 6));
    // 24 + 6 = 30 = cap: allowed; 30 + 6 > cap: refused.
    let queued = [q("e", "/e", 6, 0)];
    let s = snapshot(1_000, &p, calm(8.0), &running, &queued);
    assert_eq!(started(&plan(&s)), [("e", Admission::Over)]);
    running.push(r("e", "/e", 6));
    let queued = [q("f", "/f", 6, 0), q("g", "/g", 1, 1)];
    let s = snapshot(1_000, &p, calm(8.0), &running, &queued);
    assert!(plan(&s).start.is_empty(), "30 + 6 > 30 and 30 + 1 > 30");
    // While PSI blocks nothing is over-admitted, even calm-looking.
    let running = three_running_suites();
    let queued = [q("d", "/d", 6, 0)];
    let s = snapshot(
        1_000,
        &p,
        Pressure {
            blocking: true,
            calm: true,
            idle_cpus: Some(8.0),
            ..Pressure::default()
        },
        &running,
        &queued,
    );
    let pl = plan(&s);
    assert_eq!(pl.start, Vec::new());
    assert_eq!(pl.waits[0].cause, WaitCause::Psi);
}

#[test]
fn the_guard_never_over_admits() {
    let p = over_policy();
    let running = three_running_suites();
    let queued = [q("d", "/d", 6, 0)];
    let s = snapshot(
        400_000,
        &p,
        Pressure {
            blocking: true,
            guard_due: true,
            calm: true,
            idle_cpus: Some(8.0),
            sampled_at: None,
        },
        &running,
        &queued,
    );
    assert!(
        plan(&s).start.is_empty(),
        "18 + 6 > 20: the guard stays nominal"
    );
    let queued = [q("d", "/d", 2, 0)];
    let s = snapshot(
        400_000,
        &p,
        Pressure {
            blocking: true,
            guard_due: true,
            ..Pressure::default()
        },
        &running,
        &queued,
    );
    assert_eq!(started(&plan(&s)), [("d", Admission::Guard)]);
}

#[test]
fn under_reservation_only_the_head_over_admits() {
    let p = over_policy();
    let running = three_running_suites();
    // The head waited past head_reserve_after: it holds the reservation, the
    // light job behind it may neither backfill nor be over-admitted.
    let queued = [q("head", "/h", 6, 0), q("light", "/l", 1, 100)];
    let s = snapshot(40_000, &p, calm(1.0), &running, &queued);
    let pl = plan(&s);
    assert_eq!(pl.reserved.as_deref(), Some("head"));
    assert!(pl.start.is_empty(), "1 idle CPU < ceil(6/3)");
    assert_eq!(pl.waits[1].cause, WaitCause::Reserved);
    let s = snapshot(40_000, &p, calm(2.0), &running, &queued);
    assert_eq!(started(&plan(&s)), [("head", Admission::Over)]);
}

#[test]
fn the_first_ranked_job_under_the_cap_is_over_admitted() {
    let p = over_policy();
    let mut running = three_running_suites();
    running.push(r("d", "/d", 6));
    // 24 used: a 7 does not fit the cap (31 > 30), the 2 behind it does.
    let queued = [q("big", "/x", 7, 0), q("small", "/y", 2, 10)];
    let s = snapshot(1_000, &p, calm(1.0), &running, &queued);
    assert_eq!(started(&plan(&s)), [("small", Admission::Over)]);
}

#[test]
fn laptop_never_runs_two_heavy_suites_together() {
    // budget 6, cap 9: two weight-6 suites (12) never run together.
    let p = Policy {
        cap: Policy::cap_for(6, 1.5),
        ..policy(6)
    };
    let running = [r("a", "/a", 6)];
    let queued = [q("b", "/b", 6, 0), q("lint", "/c", 1, 10)];
    let s = snapshot(1_000, &p, calm(5.0), &running, &queued);
    assert_eq!(started(&plan(&s)), [("lint", Admission::Over)]);
    let s = snapshot(100_000, &p, calm(5.0), &running, &queued);
    let pl = plan(&s);
    assert_eq!(pl.reserved.as_deref(), Some("b"));
    assert!(
        pl.start.is_empty(),
        "the reserved head does not fit the cap"
    );
}

#[test]
fn scheduler_counts_over_admissions_and_drains_back() {
    let mut s = Scheduler::with_policy(Policy {
        cap: Policy::cap_for(4, 1.5),
        ..policy(4)
    });
    for i in 0..4 {
        s.enqueue(spec(&format!("j{i}"), None, "/r", 2, i));
    }
    let first = s.admit_with(10, calm(4.0));
    assert_eq!(first.len(), 2, "nominal: 2 + 2");
    // Over-admission needs a sample one tick after the latest start.
    let fresh = |idle: f64, at: u64| Pressure {
        sampled_at: Some(at),
        ..calm(idle)
    };
    let over = s.admit_with(1_100, fresh(4.0, 1_010));
    assert_eq!(over, vec![("j2".into(), Admission::Over)]);
    assert_eq!(s.used(), 6);
    assert_eq!(s.over_admissions(), 1);
    // used > budget: nothing nominal; the cap holds.
    assert_eq!(s.admit_with(10_000, fresh(4.0, 9_000)), Vec::new());
    assert!(s.finish("j0").is_some());
    // 4 used = budget: still no room nominally; over-admission again.
    assert_eq!(
        s.admit_with(20_000, fresh(4.0, 19_000)),
        vec![("j3".into(), Admission::Over)]
    );
    assert_eq!(
        s.record("j3", 20_000).unwrap().admission,
        Some(Admission::Over)
    );
}

// ---------------------------------------------------------------- M5–M7

#[test]
fn overdue_agent_outranks_fresh_human() {
    let p = Policy {
        human_first: true,
        fairness: true,
        order: Order::Sjf,
        ..policy(4)
    };
    let mut human = q("human", "/h", 1, 39_000);
    human.class = Class::Human;
    let agent = q("agent", "/a", 1, 0);
    let queued = [human, agent];
    let s = snapshot(40_000, &p, Pressure::default(), &[], &queued);
    assert_eq!(ranked(&s), ["agent", "human"], "the agent waited 40 s");
    let s = snapshot(20_000, &p, Pressure::default(), &[], &queued);
    assert_eq!(
        ranked(&s),
        ["human", "agent"],
        "nobody overdue: humans first"
    );
    let off = policy(4);
    let s = snapshot(20_000, &off, Pressure::default(), &[], &queued);
    assert_eq!(ranked(&s), ["agent", "human"], "human_first off: FIFO");
}

#[test]
fn human_first_reports_why_an_agent_lost_its_room() {
    let p = Policy {
        human_first: true,
        ..policy(2)
    };
    let mut human = q("human", "/h", 2, 500);
    human.class = Class::Human;
    let queued = [q("agent", "/a", 2, 0), human];
    let s = snapshot(1_000, &p, Pressure::default(), &[], &queued);
    let pl = plan(&s);
    assert_eq!(started(&pl), [("human", Admission::Nominal)]);
    assert_eq!(pl.waits[0].cause, WaitCause::Order);
    assert_eq!(pl.waits[0].detail, Some("human-first"));
    assert_eq!(
        pl.waits[0].cause.reason(),
        WaitReason::Budget,
        "frozen values"
    );
}

#[test]
fn lanes_interleave() {
    let p = Policy {
        fairness: true,
        ..policy(1)
    };
    // Lane /a queued three jobs first, then /b and /c one each.
    let queued = [
        q("a1", "/a", 1, 0),
        q("a2", "/a", 1, 1),
        q("a3", "/a", 1, 2),
        q("b1", "/b", 1, 3),
        q("c1", "/c", 1, 4),
    ];
    let s = snapshot(1_000, &p, Pressure::default(), &[], &queued);
    assert_eq!(ranked(&s), ["a1", "b1", "c1", "a2", "a3"]);
    // A lane with a job running goes behind lanes with nothing running.
    let running = [r("b0", "/b", 1)];
    let s = snapshot(1_000, &p, Pressure::default(), &running, &queued);
    assert_eq!(ranked(&s), ["a1", "c1", "a2", "b1", "a3"]);
    let pl = plan(&snapshot(1_000, &p, Pressure::default(), &[], &queued));
    assert_eq!(started(&pl), [("a1", Admission::Nominal)]);
    assert_eq!(pl.waits[0].id, "b1");
    // Fairness off: arrival order.
    let off = policy(1);
    let s = snapshot(1_000, &off, Pressure::default(), &[], &queued);
    assert_eq!(ranked(&s), ["a1", "a2", "a3", "b1", "c1"]);
}

#[test]
fn lane_detail_when_a_lane_takes_the_room() {
    let p = Policy {
        fairness: true,
        ..policy(2)
    };
    let queued = [
        q("a1", "/a", 1, 0),
        q("a2", "/a", 2, 1),
        q("b1", "/b", 1, 2),
    ];
    let pl = plan(&snapshot(1_000, &p, Pressure::default(), &[], &queued));
    assert_eq!(
        started(&pl),
        [("a1", Admission::Nominal), ("b1", Admission::Nominal)]
    );
    assert_eq!(pl.waits[0].id, "a2");
    assert_eq!(pl.waits[0].cause, WaitCause::Order);
    assert_eq!(pl.waits[0].detail, Some("lane"));
}

#[test]
fn sjf_orders_by_response_ratio_and_ages() {
    let p = Policy {
        order: Order::Sjf,
        ..policy(1)
    };
    let mut long = q("long", "/a", 1, 0);
    long.expected_ms = Some(20_000);
    let mut short = q("short", "/b", 1, 5_000);
    short.expected_ms = Some(2_000);
    let queued = [long, short];
    // At 6 s: long (6+20)/20 = 1.3, short (1+2)/2 = 1.5: short first.
    let s = snapshot(6_000, &p, Pressure::default(), &[], &queued);
    assert_eq!(ranked(&s), ["short", "long"]);
    // Ageing crossover: right after the short job arrives the long one,
    // which already waited, has the higher ratio; at 5.1 s long is
    // (5.1+20)/20 = 1.255 and short (0.1+2)/2 = 1.05. The ratios cross at
    // t = 10 * 5 / 9 ≈ 5.56 s.
    let s = snapshot(5_100, &p, Pressure::default(), &[], &queued);
    assert_eq!(ranked(&s), ["long", "short"]);
    let s = snapshot(5_600, &p, Pressure::default(), &[], &queued);
    assert_eq!(ranked(&s), ["short", "long"]);
    // A job without history counts as 60 s.
    let unknown = q("unknown", "/c", 1, 5_000);
    let queued2 = [queued[0].clone(), unknown];
    let s = snapshot(6_000, &p, Pressure::default(), &[], &queued2);
    assert_eq!(ranked(&s), ["long", "unknown"]);
    // Off by default: FIFO.
    let off = policy(1);
    let s = snapshot(6_000, &off, Pressure::default(), &[], &queued);
    assert_eq!(ranked(&s), ["long", "short"]);
}

#[test]
fn sjf_crossover_between_two_fresh_jobs() {
    let p = Policy {
        order: Order::Sjf,
        ..policy(1)
    };
    let mut long = q("long", "/a", 1, 0);
    long.expected_ms = Some(20_000);
    let mut short = q("short", "/b", 1, 0);
    short.expected_ms = Some(2_000);
    let queued = [long, short];
    // Same arrival: the short job always has the higher ratio.
    for now in [1, 1_000, 10_000, 29_999] {
        let s = snapshot(now, &p, Pressure::default(), &[], &queued);
        assert_eq!(ranked(&s), ["short", "long"], "at {now}");
    }
    // Both overdue at 30 s: strict FIFO (arrival, then id).
    let s = snapshot(30_000, &p, Pressure::default(), &[], &queued);
    assert_eq!(ranked(&s), ["long", "short"]);
}

/// ADR-0011 section 10: under any switch combination (the four scheduling
/// switches and, since ADR-0019, memory-aware admission), a weight-B agent job
/// behind an endless stream of weight-1 human jobs in other lanes starts
/// within `head_reserve_after` plus the longest running job.
#[test]
fn no_starvation_under_any_switch_combination() {
    const RUN_MS: u64 = 1_000;
    const STEP: u64 = 100;
    for bits in 0u8..32 {
        let p = Policy {
            fairness: bits & 1 != 0,
            human_first: bits & 2 != 0,
            order: if bits & 4 != 0 {
                Order::Sjf
            } else {
                Order::Fifo
            },
            cap: if bits & 8 != 0 { 6 } else { 4 },
            ..policy(4)
        };
        let mut s = Scheduler::with_policy(p);
        if bits & 16 != 0 {
            // The guessed peaks: the big job wants all the usable memory,
            // each human job a quarter of it.
            s.set_memory(Some(facts(32, 32, 3)), BTreeMap::new());
        }
        let mut big = spec("big", None, "/r", 4, 0);
        big.lane = PathBuf::from("/agent");
        big.expected_ms = Some(600_000);
        s.enqueue(big);
        let mut ends: Vec<(u64, String)> = Vec::new();
        let mut started_at = None;
        let mut n = 0;
        let mut now = 0;
        while now <= 60_000 {
            ends.retain(|(end, id)| {
                if *end <= now {
                    s.finish(id);
                    false
                } else {
                    true
                }
            });
            for lane in ["/h1", "/h2"] {
                let mut h = spec(&format!("h{n}"), None, "/r", 1, now);
                h.lane = PathBuf::from(lane);
                h.class = Class::Human;
                h.expected_ms = Some(RUN_MS);
                s.enqueue(h);
                n += 1;
            }
            for (id, _) in s.admit_with(now, calm(4.0)) {
                if id == "big" {
                    started_at = Some(now);
                }
                ends.push((now + RUN_MS, id));
            }
            if started_at.is_some() {
                break;
            }
            now += STEP;
        }
        let at = started_at.unwrap_or_else(|| panic!("switches {bits:04b}: big never started"));
        assert!(
            at <= RESERVE + RUN_MS + STEP,
            "switches {bits:04b}: big started at {at}"
        );
    }
}

#[test]
fn wait_accumulators_follow_the_previous_cause() {
    let mut s = sched(1);
    s.enqueue(spec("a", None, "/r", 1, 0));
    assert_eq!(s.admit(0, false), vec!["a"]);
    s.enqueue(spec("b", None, "/r", 1, 0));
    assert_eq!(s.admit(0, false), Vec::<String>::new());
    assert_eq!(s.snapshot(0, false).1[0].cause, WaitCause::Budget);
    assert!(s.admit(1_000, true).is_empty(), "PSI blocks from 1 s");
    assert_eq!(s.admit(3_000, false), Vec::<String>::new());
    let rec = s.record("b", 3_500).unwrap();
    // 0..1 s budget (cause of the first pass), 1..3 s psi, 3..3.5 s budget.
    assert_eq!(rec.waits[WaitCause::Budget.index()], 1_500);
    assert_eq!(rec.waits[WaitCause::Psi.index()], 2_000);
    assert_eq!(rec.dominant(), Some(WaitCause::Psi));
    assert_eq!(rec.admission, None);
    assert!(s.finish("a").is_some());
    assert_eq!(s.admit(4_000, false), vec!["b"]);
    let rec = s.record("b", 9_000).unwrap();
    assert_eq!(rec.admitted_at, Some(4_000));
    assert_eq!(rec.admission, Some(Admission::Nominal));
    assert_eq!(
        rec.waits.iter().sum::<u64>(),
        4_000,
        "no wait after the start"
    );
    assert_eq!(s.record("a", 0), None);
}

#[test]
fn snapshot_views_carry_rank_lane_and_weights() {
    let mut s = Scheduler::with_policy(Policy {
        fairness: true,
        learned_weights: true,
        ..policy(2)
    });
    let mut a = spec("a", None, "/r", 2, 0);
    a.learn_key = Some(lk("a"));
    a.demands_milli = vec![1_000, 1_000, 1_000];
    a.lane = PathBuf::from("/wt1");
    s.enqueue(a);
    let mut b = spec("b", None, "/r", 2, 1);
    b.lane = PathBuf::from("/wt1");
    b.expected_ms = Some(7);
    s.enqueue(b);
    let mut c = spec("c", None, "/r", 2, 2);
    c.lane = PathBuf::from("/wt2");
    s.enqueue(c);
    assert_eq!(s.admit(10, false), vec!["a"]);
    let (running, queued) = s.snapshot(10, false);
    assert_eq!(running[0].weight, 1);
    assert_eq!(running[0].weight_cfg, 2);
    assert_eq!(running[0].admission, Admission::Nominal);
    assert_eq!(
        queued
            .iter()
            .map(|v| (v.id.as_str(), v.rank))
            .collect::<Vec<_>>(),
        [("c", 1), ("b", 2)]
    );
    assert_eq!(queued[1].lane, PathBuf::from("/wt1"));
    assert_eq!(queued[1].expected_ms, Some(7));
    assert_eq!(s.position("b"), Some(2));
}

// ------------------------------------------- weights resolved at admission

fn learning(budget: u32) -> Scheduler {
    Scheduler::with_policy(Policy {
        learned_weights: true,
        ..policy(budget)
    })
}

fn gate_job(id: &str, weight: u32, queued_at: u64) -> JobSpec {
    let mut j = spec(id, None, "/r", weight, queued_at);
    j.learn_key = Some(lk("test"));
    j
}

#[test]
fn job_queued_before_learning_is_admitted_with_the_learned_weight() {
    let mut s = learning(6);
    for i in 0..3 {
        s.enqueue(gate_job(&format!("t{i}"), 4, i));
    }
    assert_eq!(s.spec("t1").unwrap().weight, 4, "nothing learned yet");
    assert_eq!(s.admit(10, false), vec!["t0"]);
    // The first runs teach a demand of about 3 CPUs.
    s.set_learned(lk("test"), &[2_900, 3_200, 3_000]);
    let (_, queued) = s.snapshot(20, false);
    assert!(
        queued.iter().all(|v| v.weight == 3 && v.weight_cfg == 4),
        "status shows the current effective weight"
    );
    assert!(s.finish("t0").is_some());
    // Two tests of 3 fit the budget of 6 where the configured 4 fit one.
    assert_eq!(s.admit(30, false), vec!["t1", "t2"]);
    assert_eq!(s.used(), 6);
    let rec = s.record("t1", 30).unwrap();
    assert_eq!((rec.weight, rec.weight_cfg), (3, 4));
}

#[test]
fn configured_weight_admits_one_where_learned_admits_two() {
    let mut fixed = learning(6);
    let mut learned = learning(6);
    learned.set_learned(lk("test"), &[2_900, 3_200, 3_000]);
    for s in [&mut fixed, &mut learned] {
        s.enqueue(gate_job("t0", 4, 0));
        s.enqueue(gate_job("t1", 4, 1));
    }
    assert_eq!(fixed.admit(10, false), vec!["t0"]);
    assert_eq!(learned.admit(10, false), vec!["t0", "t1"]);
}

#[test]
fn running_jobs_keep_their_admitted_weight_when_stats_change() {
    let mut s = learning(8);
    s.enqueue(gate_job("t0", 4, 0));
    s.enqueue(gate_job("t1", 4, 1));
    assert_eq!(s.admit(10, false), vec!["t0", "t1"]);
    assert_eq!(s.used(), 8);
    s.set_learned(lk("test"), &[1_000, 1_000, 1_000]);
    assert_eq!(
        s.used(),
        8,
        "running jobs hold what they were admitted with"
    );
    let (running, _) = s.snapshot(20, false);
    assert!(running.iter().all(|v| v.weight == 4));
    s.enqueue(gate_job("t2", 4, 20));
    assert_eq!(s.admit(30, false), Vec::<String>::new(), "no room left");
    assert_eq!(s.finish("t0").unwrap().weight, 4);
    assert_eq!(s.used(), 4);
    assert_eq!(s.admit(40, false), vec!["t2"]);
    assert_eq!(s.used(), 5, "4 still held by t1, plus the learned 1");
    assert_eq!(s.finish("t1").unwrap().weight, 4);
    assert_eq!(s.finish("t2").unwrap().weight, 1);
    assert_eq!(s.used(), 0);
}

#[test]
fn resolved_weights_respect_the_bounds() {
    let mut s = learning(8);
    s.enqueue(gate_job("hi", 4, 0));
    let mut low = gate_job("low", 8, 1);
    low.learn_key = Some(lk("lint"));
    s.enqueue(low);
    let mut exec = spec("exec", None, "/r", 3, 2);
    exec.demands_milli = vec![500, 500, 500];
    s.enqueue(exec);
    s.set_learned(lk("test"), &[12_000, 11_000, 15_000]);
    s.set_learned(lk("lint"), &[100, 0, 50]);
    let weights = |s: &Scheduler| {
        let (_, queued) = s.snapshot(5, false);
        queued
            .iter()
            .map(|v| (v.id.clone(), v.weight))
            .collect::<BTreeMap<_, _>>()
    };
    let w = weights(&s);
    assert_eq!(w["hi"], 4, "never above the configured weight");
    assert_eq!(w["low"], 2, "never below ceil(w_cfg / 4)");
    assert_eq!(w["exec"], 3, "jobs without a key never learn");
    // Too few samples again: back to the configured weight.
    s.set_learned(lk("lint"), &[100]);
    assert_eq!(weights(&s)["low"], 8);
}

#[test]
fn plan_resolves_weights_from_the_snapshot() {
    let p = Policy {
        learned_weights: true,
        ..policy(6)
    };
    let mut a = q("a", "/wt", 4, 0);
    a.learn_key = Some(lk("test"));
    let mut b = q("b", "/wt", 4, 1);
    b.learn_key = Some(lk("test"));
    let queued = [a, b];
    let learned: Learned = [(lk("test"), 3)].into_iter().collect();
    let s = Snapshot {
        learned: &learned,
        ..snapshot(10, &p, Pressure::default(), &[], &queued)
    };
    let decided = plan(&s);
    assert_eq!(decided.weights, [3, 3]);
    assert_eq!(started(&decided).len(), 2);
    // Without a learned weight the configured 4 starts only one.
    let decided = plan(&snapshot(10, &p, Pressure::default(), &[], &queued));
    assert_eq!(decided.weights, [4, 4]);
    assert_eq!(started(&decided).len(), 1);
}

// ------------------------------------------- alpha review: fresh samples

#[test]
fn over_admission_waits_for_a_sample_taken_after_the_latest_start() {
    let p = over_policy();
    let mut s = Scheduler::with_policy(p);
    // Three suites of 6 fill 18 of 20; the fourth does not fit nominally.
    for i in 0..4 {
        s.enqueue(spec(&format!("s{i}"), None, &format!("/r{i}"), 6, i));
    }
    let calm_at = |sampled_at: u64| Pressure {
        calm: true,
        idle_cpus: Some(8.0),
        sampled_at: Some(sampled_at),
        ..Pressure::default()
    };
    let started = s.admit_with(10_000, calm_at(9_900));
    assert_eq!(started.len(), 3, "three nominal starts at 10 s");
    // The newest sample predates the starts: its idle CPUs are stale.
    assert_eq!(s.admit_with(10_500, calm_at(9_900)), Vec::new());
    // A sample less than one tick after the start still overlaps it.
    assert_eq!(s.admit_with(10_800, calm_at(10_700)), Vec::new());
    // No sample at all is never fresh after a start.
    let no_sample = Pressure {
        sampled_at: None,
        ..calm_at(0)
    };
    assert_eq!(s.admit_with(11_500, no_sample), Vec::new());
    // One full tick after the start: the over-admission may happen.
    assert_eq!(
        s.admit_with(11_500, calm_at(11_000)),
        [("s3".to_owned(), Admission::Over)]
    );
}

// ---------------------------------------------------------------- ADR-0015

/// An `auto` gate job as the daemon registers it: the ceiling is
/// `ceil(budget / 2)` and `weight_auto` marks it for `status`.
fn auto_job(id: &str, budget: u32, queued_at: u64) -> JobSpec {
    let mut j = gate_job(
        id,
        sluice_gate_core::config::auto_ceiling(budget),
        queued_at,
    );
    j.weight_auto = true;
    j
}

#[test]
fn auto_is_admitted_at_half_the_budget_and_drops_to_the_measured_demand() {
    let mut s = learning(20);
    s.enqueue(auto_job("t0", 20, 0));
    assert_eq!(s.spec("t0").unwrap().weight, 10, "nothing learned yet");
    assert_eq!(s.admit(10, false), vec!["t0"]);
    assert_eq!(s.used(), 10);
    let (running, _) = s.snapshot(11, false);
    assert!(running[0].weight_auto, "status can show `auto`");
    assert!(s.finish("t0").is_some());
    // Three runs measured about 4 CPUs: the next one holds 4, not 10.
    s.set_learned(lk("test"), &[3_900, 4_100, 4_000]);
    s.enqueue(auto_job("t1", 20, 20));
    let (_, queued) = s.snapshot(21, false);
    assert_eq!((queued[0].weight, queued[0].weight_cfg), (4, 10));
    assert!(queued[0].weight_auto);
    // Never below a quarter of the ceiling: ceil(10 / 4) = 3.
    s.set_learned(lk("test"), &[200, 300, 100]);
    let (_, queued) = s.snapshot(22, false);
    assert_eq!(queued[0].weight, 3);
}

#[test]
fn a_declared_weight_is_a_ceiling_that_learning_never_raises() {
    let mut s = learning(20);
    s.enqueue(gate_job("t0", 8, 0));
    s.set_learned(lk("test"), &[15_000, 14_000, 16_000]);
    let (_, queued) = s.snapshot(1, false);
    assert_eq!(queued[0].weight, 8, "declared 8 never exceeds 8");
    assert!(!queued[0].weight_auto);
    s.set_learned(lk("test"), &[5_000, 5_200, 4_900]);
    let (_, queued) = s.snapshot(2, false);
    assert_eq!((queued[0].weight, queued[0].weight_cfg), (5, 8));
}

#[test]
fn exec_jobs_learn_from_their_label_and_repository() {
    let mut s = learning(20);
    let exec_key = |label: &str| (PathBuf::from("/r"), format!("exec:{label}"), String::new());
    let exec_job = |id: &str, label: &str, at: u64| {
        let mut j = spec(id, None, "/r", 10, at);
        j.gate = format!("exec:{label}");
        j.learn_key = Some(exec_key(label));
        j
    };
    s.enqueue(exec_job("e0", "ng-lint", 0));
    assert_eq!(
        s.spec("e0").unwrap().weight,
        10,
        "first runs hold the ceiling"
    );
    assert_eq!(s.admit(10, false), vec!["e0"]);
    assert!(s.finish("e0").is_some());
    // Three runs at about one CPU: the fourth is admitted at the floor, 3.
    s.set_learned(exec_key("ng-lint"), &[1_000, 1_100, 900]);
    s.enqueue(exec_job("e1", "ng-lint", 20));
    s.enqueue(exec_job("other", "ng-build", 21));
    let (_, queued) = s.snapshot(22, false);
    let weight = |id: &str| queued.iter().find(|v| v.id == id).unwrap().weight;
    assert!(weight("e1") <= 3, "learned: {}", weight("e1"));
    assert_eq!(weight("other"), 10, "another label keeps its own history");
}

// ------------------------------------------------ ADR-0019: memory admission

const GIB: u64 = 1 << 30;

/// Memory facts in GiB: available, total, reserve.
fn facts(available: u64, total: u64, reserve: u64) -> MemoryFacts {
    MemoryFacts {
        available_bytes: available * GIB,
        total_bytes: total * GIB,
        reserve_bytes: reserve * GIB,
    }
}

/// A queued job whose peak was learned (in GiB).
fn peaked(id: &str, weight: u32, gib: u64, queued_at: u64) -> QueuedSnap {
    QueuedSnap {
        expected_peak: Some(gib * GIB),
        ..q(id, &format!("/{id}"), weight, queued_at)
    }
}

/// A running job holding `current` GiB and expected to reach `peak` GiB.
fn running(id: &str, weight: u32, current: u64, peak: u64) -> RunningSnap {
    RunningSnap {
        current: current * GIB,
        expected_peak: Some(peak * GIB),
        ..r(id, &format!("/{id}"), weight)
    }
}

fn with_memory(s: Snapshot<'_>, memory: MemoryFacts) -> Snapshot<'_> {
    Snapshot {
        memory: Some(memory),
        ..s
    }
}

fn waits_of(p: &Plan) -> Vec<(&str, WaitCause, Option<&'static str>)> {
    p.waits
        .iter()
        .map(|w| (w.id.as_str(), w.cause, w.detail))
        .collect()
}

/// T-W6-1: the incident. Two gates that peak at 18 GiB on a 30 GiB machine
/// with 22 GiB available and a 3 GiB reserve: only the first starts, the
/// second waits for memory although the budget (12) has room for both.
#[test]
fn two_eighteen_gib_gates_never_start_together() {
    let p = policy(12);
    let queued = [peaked("a", 6, 18, 0), peaked("b", 6, 18, 1)];
    let s = with_memory(
        snapshot(10, &p, Pressure::default(), &[], &queued),
        facts(22, 30, 3),
    );
    let decided = plan(&s);
    assert_eq!(started(&decided), [("a", Admission::Nominal)]);
    assert_eq!(
        waits_of(&decided),
        [("b", WaitCause::Memory, Some("memory"))]
    );
    assert_eq!(decided.expected.len(), 2);
    assert!(
        decided
            .expected
            .iter()
            .all(|e| e.learned && e.bytes == 18 * GIB)
    );
    // The same snapshot without the rule starts both: the incident.
    let off = plan(&snapshot(10, &p, Pressure::default(), &[], &queued));
    assert_eq!(started(&off).len(), 2);
    assert_eq!(off.expected, Vec::new(), "no estimate with the rule off");
}

#[test]
fn a_running_gate_counts_its_headroom_not_what_it_already_holds() {
    let p = policy(12);
    let queued = [peaked("b", 6, 18, 1)];
    // `a` already holds 2 of its expected 18 GiB: 16 GiB are still to grow.
    // 22 - 3 - 16 = 3 GiB free: b waits.
    let run = [running("a", 6, 2, 18)];
    let s = with_memory(
        snapshot(10, &p, Pressure::default(), &run, &queued),
        facts(22, 30, 3),
    );
    let decided = plan(&s);
    assert_eq!(started(&decided), Vec::new());
    assert_eq!(waits_of(&decided)[0].1, WaitCause::Memory);
    assert_eq!(running_headroom(&s), Some(16 * GIB));
    // Once `a` holds its 18 GiB MemAvailable has already paid for it: with
    // 40 GiB available (and nothing left to grow) b fits.
    let run = [running("a", 6, 18, 18)];
    let s = with_memory(
        snapshot(10, &p, Pressure::default(), &run, &queued),
        facts(40, 30, 3),
    );
    assert_eq!(started(&plan(&s)), [("b", Admission::Nominal)]);
    assert_eq!(running_headroom(&s), Some(0));
    // A job that outgrew its expectation has no negative headroom.
    let run = [running("a", 6, 20, 18)];
    assert_eq!(
        running_headroom(&with_memory(
            snapshot(10, &p, Pressure::default(), &run, &queued),
            facts(40, 30, 3)
        )),
        Some(0)
    );
}

#[test]
fn jobs_started_earlier_in_the_pass_count_in_full() {
    let p = policy(12);
    // One heavy job runs (4 of 4 GiB held): 22 - 3 = 19 GiB free. Three 6 GiB
    // jobs fit (18 GiB); the fourth does not.
    let run = [running("r", 1, 4, 4)];
    let queued: Vec<QueuedSnap> = (0..4).map(|n| peaked(&format!("q{n}"), 1, 6, n)).collect();
    let s = with_memory(
        snapshot(10, &p, Pressure::default(), &run, &queued),
        facts(22, 30, 3),
    );
    let decided = plan(&s);
    assert_eq!(
        started(&decided)
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        ["q0", "q1", "q2"]
    );
    assert_eq!(
        waits_of(&decided),
        [("q3", WaitCause::Memory, Some("memory"))]
    );
}

#[test]
fn a_gate_alone_runs_even_when_it_does_not_fit() {
    let p = policy(12);
    // 1 GiB available on a 30 GiB machine, a gate that peaks at 18.
    let queued = [peaked("big", 6, 18, 0), peaked("small", 1, 1, 1)];
    let s = with_memory(
        snapshot(10, &p, Pressure::default(), &[], &queued),
        facts(1, 30, 3),
    );
    let decided = plan(&s);
    // Nothing runs: progress is always possible, `big` starts. Something
    // started in this pass now, so `small` (not alone any more) waits.
    assert_eq!(started(&decided), [("big", Admission::Nominal)]);
    assert_eq!(
        waits_of(&decided),
        [("small", WaitCause::Memory, Some("memory"))]
    );
    // Any running job, however light (1 GiB of 30), withdraws the exemption:
    // `big` waits until the machine is idle.
    let run = [running("light", 1, 1, 1)];
    let s = with_memory(
        snapshot(10, &p, Pressure::default(), &run, &queued[..1]),
        facts(1, 30, 3),
    );
    let decided = plan(&s);
    assert_eq!(started(&decided), Vec::new());
    assert_eq!(
        waits_of(&decided),
        [("big", WaitCause::Memory, Some("memory"))]
    );
}

#[test]
fn an_overdue_head_blocked_only_by_memory_reserves() {
    let p = policy(8);
    let run = [running("r", 2, 10, 18)];
    // Budget has room for everything; free = 16 - 3 - 8 = 5 GiB.
    let mut head = peaked("head", 2, 18, 0);
    head.lane = PathBuf::from("/head");
    let small = peaked("small", 1, 1, 1);
    let queued = [head, small];
    let fresh = with_memory(
        snapshot(10, &p, Pressure::default(), &run, &queued),
        facts(16, 30, 3),
    );
    let decided = plan(&fresh);
    assert_eq!(decided.reserved, None, "not overdue: smaller jobs backfill");
    assert_eq!(started(&decided), [("small", Admission::Nominal)]);
    // Overdue: the head holds the reservation and nothing behind it starts.
    let overdue = with_memory(
        snapshot(RESERVE, &p, Pressure::default(), &run, &queued),
        facts(16, 30, 3),
    );
    let decided = plan(&overdue);
    assert_eq!(decided.reserved.as_deref(), Some("head"));
    assert_eq!(started(&decided), Vec::new());
    assert_eq!(
        waits_of(&decided),
        [
            ("head", WaitCause::Memory, Some("memory")),
            ("small", WaitCause::Reserved, None)
        ]
    );
}

#[test]
fn a_memory_wait_reports_budget_with_the_memory_detail() {
    assert_eq!(WaitCause::Memory.reason(), WaitReason::Budget);
    assert_eq!(WaitCause::Memory.index(), WaitCause::Budget.index());
    assert_eq!(WaitCause::Memory.as_str(), "memory");
    let mut s = Scheduler::with_policy(policy(12));
    s.set_memory(Some(facts(22, 30, 3)), BTreeMap::new());
    s.set_learned_peak(lk("test"), Some(18 * GIB));
    for id in ["a", "b"] {
        let mut j = gate_job(id, 6, 0);
        j.lane = PathBuf::from(format!("/{id}"));
        s.enqueue(j);
    }
    assert_eq!(s.admit(0, false), vec!["a"]);
    assert_eq!(s.admit(1_000, false), Vec::<String>::new());
    let (running, queued) = s.snapshot(1_000, false);
    assert_eq!(queued[0].reason, WaitReason::Budget);
    assert_eq!(queued[0].cause, WaitCause::Memory);
    assert_eq!(queued[0].detail, Some("memory"));
    assert_eq!(queued[0].memory_expected.map(|e| e.bytes), Some(18 * GIB));
    assert_eq!(running[0].memory_current, Some(0));
    assert!(running[0].memory_expected.is_some_and(|e| e.learned));
    // The wait lands in the budget accumulator, the frozen column.
    s.admit(3_000, false);
    let rec = s.record("b", 3_000).unwrap();
    assert_eq!(rec.waits[WaitCause::Budget.index()], 3_000);
    assert_eq!(s.memory_headroom(), Some(18 * GIB));
    // MemAvailable recovers while `a` runs (it holds its 18 GiB): b starts.
    let held = BTreeMap::from([("a".to_owned(), 18 * GIB)]);
    s.set_memory(Some(facts(40, 64, 3)), held);
    assert_eq!(s.memory_headroom(), Some(0));
    assert_eq!(s.admit(4_000, false), vec!["b"]);
}

/// PSI blocking still comes first: a blocked pass reports `psi`, and the
/// starvation guard does not start a head that does not fit memory.
#[test]
fn psi_blocking_comes_before_memory() {
    let p = policy(12);
    let run = [running("r", 1, 4, 18)];
    let queued = [peaked("a", 6, 18, 0)];
    let blocking = Pressure {
        blocking: true,
        guard_due: true,
        ..Pressure::default()
    };
    let s = with_memory(snapshot(10, &p, blocking, &run, &queued), facts(22, 30, 3));
    let decided = plan(&s);
    assert_eq!(started(&decided), Vec::new(), "the guard respects memory");
    assert_eq!(waits_of(&decided), [("a", WaitCause::Psi, None)]);
    // Memory available: the guard starts it as before.
    let s = with_memory(snapshot(10, &p, blocking, &run, &queued), facts(60, 30, 3));
    assert_eq!(started(&plan(&s)), [("a", Admission::Guard)]);
}

#[test]
fn over_admission_needs_a_learned_peak_that_fits() {
    let p = over_policy();
    // Three suites of 1 GiB each, fully grown.
    let run = [
        running("a", 6, 1, 1),
        running("b", 6, 1, 1),
        running("c", 6, 1, 1),
    ];
    let pressure = calm(8.0);
    // A fourth suite does not fit the budget nominally; plenty of memory.
    let learned = |gib| {
        let mut j = q("s3", "/r3", 6, 3);
        j.expected_peak = Some(gib * GIB);
        j
    };
    let guessed = q("s3", "/r3", 6, 3);
    let go = |queued: &[QueuedSnap], mem: MemoryFacts| {
        let s = with_memory(snapshot(10_000, &p, pressure, &run, queued), mem);
        plan(&s).start
    };
    let roomy = facts(200, 300, 3);
    assert_eq!(
        go(&[learned(2)], roomy),
        [("s3".to_owned(), Admission::Over)],
        "learned and fits"
    );
    assert_eq!(go(&[guessed], roomy), Vec::new(), "never over-admit blind");
    assert_eq!(
        go(&[learned(500)], roomy),
        Vec::new(),
        "learned but it does not fit"
    );
    // The rule off: today's behaviour, a guess over-admits.
    let queued = [q("s3", "/r3", 6, 3)];
    let s = snapshot(10_000, &p, pressure, &run, &queued);
    assert_eq!(plan(&s).start, [("s3".to_owned(), Admission::Over)]);
}

#[test]
fn an_unknown_peak_is_guessed_from_the_weight_and_a_learned_one_replaces_it() {
    let f = facts(22, 30, 3);
    // `auto` on a budget of 12 is weight 6: half of the 27 GiB usable.
    let guess = f.estimate(None, 6, 12);
    assert_eq!(guess.bytes, 27 * GIB / 2);
    assert!(!guess.learned);
    // A weight of 1 predicts a twelfth of it, never above the budget share.
    assert_eq!(f.estimate(None, 1, 12).bytes, 27 * GIB / 12);
    assert_eq!(f.estimate(None, 99, 12).bytes, 27 * GIB);
    // One learned sample is enough and replaces the guess.
    let learned = f.estimate(Some(2 * GIB), 6, 12);
    assert_eq!((learned.bytes, learned.learned), (2 * GIB, true));
    // Through the scheduler: the key learns, the next pass uses it.
    let mut s = Scheduler::with_policy(policy(12));
    s.set_memory(Some(f), BTreeMap::new());
    s.enqueue(gate_job("t0", 6, 0));
    let expected = |s: &Scheduler| s.snapshot(1, false).1[0].memory_expected.unwrap();
    assert!(!expected(&s).learned);
    assert_eq!(expected(&s).bytes, 27 * GIB / 2);
    s.set_learned_peak(lk("test"), Some(3 * GIB));
    assert!(expected(&s).learned);
    assert_eq!(expected(&s).bytes, 3 * GIB);
    s.set_learned_peak(lk("test"), None);
    assert!(!expected(&s).learned);
}

// A tiny deterministic generator for the property tests.
struct Lcg(u64);

impl Lcg {
    /// A number in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }

    /// A weight in `1..=max`.
    fn weight(&mut self, max: u32) -> u32 {
        u32::try_from(1 + self.below(u64::from(max))).unwrap()
    }
}

/// A random snapshot's parts: running jobs, queued jobs and memory facts.
fn random_case(rng: &mut Lcg) -> (Vec<RunningSnap>, Vec<QueuedSnap>, MemoryFacts) {
    let running = (0..rng.below(4))
        .map(|n| {
            let peak = 1 + rng.below(20);
            let mut j = running(&format!("r{n}"), rng.weight(4), rng.below(peak + 1), peak);
            if rng.below(3) == 0 {
                j.expected_peak = None;
            }
            j
        })
        .collect();
    let queued = (0..=rng.below(8))
        .map(|n| {
            let mut j = q(
                &format!("q{n}"),
                &format!("/l{}", rng.below(3)),
                rng.weight(4),
                rng.below(40_000),
            );
            if rng.below(3) != 0 {
                j.expected_peak = Some((1 + rng.below(25)) * GIB);
            }
            j
        })
        .collect();
    let total = 8 + rng.below(56);
    let memory = MemoryFacts {
        available_bytes: rng.below(total + 1) * GIB,
        total_bytes: total * GIB,
        reserve_bytes: rng.below(total / 4 + 1) * GIB,
    };
    (running, queued, memory)
}

fn switch_policy(bits: u8) -> Policy {
    Policy {
        fairness: bits & 1 != 0,
        human_first: bits & 2 != 0,
        order: if bits & 4 != 0 {
            Order::Sjf
        } else {
            Order::Fifo
        },
        cap: if bits & 8 != 0 { 9 } else { 6 },
        ..policy(6)
    }
}

/// T-W6-2: whatever the snapshot, the jobs a pass starts never add up to more
/// than what is free, unless the job started with nothing else running and
/// nothing started earlier in the pass (the progress exemption); an
/// over-admission also needs a learned peak.
#[test]
fn started_jobs_never_exceed_free_memory_unless_alone() {
    let mut rng = Lcg(7);
    for bits in 0u8..16 {
        let p = switch_policy(bits);
        for _ in 0..300 {
            let (run, queued, memory) = random_case(&mut rng);
            let s = with_memory(snapshot(40_000, &p, calm(8.0), &run, &queued), memory);
            let decided = plan(&s);
            let expected: BTreeMap<&str, Expected> = decided
                .order
                .iter()
                .map(String::as_str)
                .zip(decided.expected.iter().copied())
                .collect();
            let mut held = running_memory_of(&s, &memory);
            let mut busy = !run.is_empty();
            for (id, admission) in &decided.start {
                let e = expected[id.as_str()];
                let free = memory
                    .available_bytes
                    .saturating_sub(memory.reserve_bytes)
                    .saturating_sub(held);
                match admission {
                    Admission::Nominal => assert!(
                        !busy || e.bytes <= free,
                        "switches {bits:04b}: {id} needs {} of {free}",
                        e.bytes
                    ),
                    Admission::Over => assert!(
                        e.learned && e.bytes <= free,
                        "switches {bits:04b}: over-admitted {id} blind or too big"
                    ),
                    Admission::Guard => unreachable!("not blocking"),
                }
                held += e.bytes;
                busy = true;
            }
        }
    }
}

/// The running side of the oracle, written independently of the pass.
fn running_memory_of(s: &Snapshot<'_>, memory: &MemoryFacts) -> u64 {
    s.running
        .iter()
        .map(|r| {
            let e = memory
                .estimate(r.expected_peak, r.weight, s.policy.budget)
                .bytes;
            e.saturating_sub(r.current)
        })
        .sum()
}

/// T-W6-2 (differential): with the rule off every plan is today's plan, under
/// all 16 switch combinations, whatever the memory figures in the snapshot say;
/// and with more memory than anything can use the rule changes nothing but the
/// reported estimates.
#[test]
fn memory_off_or_plentiful_plans_like_cpu_only_admission() {
    let mut rng = Lcg(42);
    for bits in 0u8..16 {
        let p = switch_policy(bits);
        for n in 0..300 {
            let (run, queued, memory) = random_case(&mut rng);
            // The same jobs as CPU-only admission knows them.
            let plain_run: Vec<RunningSnap> = run
                .iter()
                .map(|r| RunningSnap {
                    current: 0,
                    expected_peak: None,
                    ..r.clone()
                })
                .collect();
            let plain_queued: Vec<QueuedSnap> = queued
                .iter()
                .map(|j| QueuedSnap {
                    expected_peak: None,
                    ..j.clone()
                })
                .collect();
            let pressure = if n % 2 == 0 {
                calm(8.0)
            } else {
                Pressure::default()
            };
            let today = plan(&snapshot(40_000, &p, pressure, &plain_run, &plain_queued));
            let off = plan(&snapshot(40_000, &p, pressure, &run, &queued));
            assert_eq!(off, today, "switches {bits:04b}, case {n}: rule off");
            let plenty = MemoryFacts {
                available_bytes: u64::MAX / 4,
                ..memory
            };
            // Over-admission never starts a job blind, so every queued job
            // has a learned peak here.
            let known: Vec<QueuedSnap> = queued
                .iter()
                .map(|j| QueuedSnap {
                    expected_peak: j.expected_peak.or(Some(GIB)),
                    ..j.clone()
                })
                .collect();
            let on = plan(&with_memory(
                snapshot(40_000, &p, pressure, &run, &known),
                plenty,
            ));
            assert_eq!(
                Plan {
                    expected: Vec::new(),
                    ..on
                },
                today,
                "switches {bits:04b}, case {n}: plenty of memory"
            );
        }
    }
}

/// A job that fits the budget but never the memory of the jobs running beside
/// it starts within `head_reserve_after` plus the longest running job, under
/// every switch combination: the reservation covers memory-blocked heads.
#[test]
fn an_overdue_head_blocked_only_by_memory_starts_within_the_bound() {
    const RUN_MS: u64 = 1_000;
    const STEP: u64 = 100;
    // 28 GiB fits an idle machine; 40 GiB never fits and starts only by the
    // progress exemption, once the reservation has drained the machine.
    for (bits, peak_gib) in (0u8..16).flat_map(|b| [(b, 28), (b, 40)]) {
        let p = Policy {
            cap: if bits & 8 != 0 { 12 } else { 8 },
            fairness: bits & 1 != 0,
            human_first: bits & 2 != 0,
            order: if bits & 4 != 0 {
                Order::Sjf
            } else {
                Order::Fifo
            },
            ..policy(8)
        };
        let mut s = Scheduler::with_policy(p);
        s.set_memory(Some(facts(32, 32, 3)), BTreeMap::new());
        // Weight 1 on a budget of 8, but 28 or 40 GiB learned: with any small job
        // running (2 GiB each, 29 GiB usable) it never fits.
        let mut big = spec("big", None, "/r", 1, 0);
        big.lane = PathBuf::from("/agent");
        big.learn_key = Some(lk("big"));
        big.expected_ms = Some(600_000);
        s.set_learned_peak(lk("big"), Some(peak_gib * GIB));
        s.enqueue(big);
        let mut ends: Vec<(u64, String)> = Vec::new();
        let (mut n, mut now, mut started_at) = (0, 0, None);
        while now <= 60_000 && started_at.is_none() {
            ends.retain(|(end, id)| {
                if *end <= now {
                    s.finish(id);
                    false
                } else {
                    true
                }
            });
            for lane in ["/h1", "/h2", "/h3"] {
                let mut h = spec(&format!("h{n}"), None, "/r", 1, now);
                h.lane = PathBuf::from(lane);
                h.class = Class::Human;
                h.expected_ms = Some(RUN_MS);
                h.learn_key = Some(lk("small"));
                s.enqueue(h);
                n += 1;
            }
            s.set_learned_peak(lk("small"), Some(2 * GIB));
            for (id, _) in s.admit_with(now, calm(4.0)) {
                if id == "big" {
                    started_at = Some(now);
                }
                ends.push((now + RUN_MS, id));
            }
            now += STEP;
        }
        let at = started_at
            .unwrap_or_else(|| panic!("switches {bits:04b}, {peak_gib} GiB: big never started"));
        assert!(
            at <= RESERVE + RUN_MS + 2 * STEP,
            "switches {bits:04b}, {peak_gib} GiB: big started at {at}"
        );
    }
}
