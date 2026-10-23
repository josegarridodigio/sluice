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
    }
}

fn r(id: &str, lane: &str, weight: u32) -> RunningSnap {
    RunningSnap {
        id: id.into(),
        repo: PathBuf::from("/repo"),
        lane: PathBuf::from(lane),
        weight,
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

/// ADR-0011 section 10: under any switch combination, a weight-B agent job
/// behind an endless stream of weight-1 human jobs in other lanes starts
/// within `head_reserve_after` plus the longest running job.
#[test]
fn no_starvation_under_any_switch_combination() {
    const RUN_MS: u64 = 1_000;
    const STEP: u64 = 100;
    for bits in 0u8..16 {
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
