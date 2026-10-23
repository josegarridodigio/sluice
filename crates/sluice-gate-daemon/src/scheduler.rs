//! Pure admission logic: budget, backfill, head-of-line reservation, per-repo
//! caps, request coalescing, and the ADR-0011 policies (learned weights,
//! PSI-driven over-admission, lane fairness, humans first, shortest expected
//! job first). See `kb/sluice/design/scheduler.md`, ADR-0007 and ADR-0011.
//!
//! No I/O, no clocks, no async: callers pass `now` (unix milliseconds) and the
//! current pressure, so every decision is deterministic and testable. The
//! decision itself is [`plan`], a pure function over a [`Snapshot`];
//! [`Scheduler`] only owns the queue state and applies the plan.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sluice_gate_core::key::CacheKey;
use sluice_gate_proto::WaitReason;

/// Expected run time of a job without history (HRRN), milliseconds.
pub const DEFAULT_EXPECTED_MS: u64 = 60_000;
/// Samples needed before a learned weight is used.
pub const LEARN_AFTER: usize = 3;
/// Samples (most recent) a learned weight and an expected time look at.
pub const LEARN_WINDOW: usize = 10;

/// Key of a gate's learned statistics: repository identity, gate name and
/// command hash. Jobs without a key (`exec`) never learn.
pub type LearnKey = (PathBuf, String, String);

/// Learned weights by statistics key: the rounded P75 demand of the last
/// [`LEARN_WINDOW`] runs ([`learned_weight`]), before the per-job clamp.
pub type Learned = BTreeMap<LearnKey, u32>;

/// Priority class of a job (ADR-0011 M6): a human always waits for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// Requested by a person (`caller = human`); ranks first with `human_first`.
    Human,
    /// Requested by an agent (or an unknown caller).
    #[default]
    Agent,
}

/// Tie-break among jobs of equal priority (ADR-0011 M7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Order {
    /// Arrival order.
    #[default]
    Fifo,
    /// Highest response ratio next: `(waited + expected) / expected`.
    Sjf,
}

/// How a job was admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Within the budget.
    Nominal,
    /// Above the budget, up to the over-admission cap (ADR-0011 M3).
    Over,
    /// By the PSI starvation guard, within the budget.
    Guard,
}

impl Admission {
    /// The spelling used in `status`, `done` and the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Over => "over",
            Self::Guard => "guard",
        }
    }
}

/// Why a queued job did not start in an admission pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitCause {
    /// Not enough budget units free.
    Budget,
    /// PSI blocks admission.
    Psi,
    /// The repository cap is reached.
    RepoCap,
    /// The head of the queue holds a reservation.
    Reserved,
    /// It would have fitted, but higher-ranked jobs took the room first.
    Order,
}

impl WaitCause {
    /// Every cause, in accumulator order.
    pub const ALL: [Self; 5] = [
        Self::Budget,
        Self::Psi,
        Self::RepoCap,
        Self::Reserved,
        Self::Order,
    ];

    /// Index of the cause's accumulator.
    pub fn index(self) -> usize {
        match self {
            Self::Budget => 0,
            Self::Psi => 1,
            Self::RepoCap => 2,
            Self::Reserved => 3,
            Self::Order => 4,
        }
    }

    /// The frozen wire reason (an outranked job reports `budget`).
    pub fn reason(self) -> WaitReason {
        match self {
            Self::Budget | Self::Order => WaitReason::Budget,
            Self::Psi => WaitReason::Psi,
            Self::RepoCap => WaitReason::RepoCap,
            Self::Reserved => WaitReason::HeadReserved,
        }
    }

    /// The spelling used in `done.wait_reason` and the timeline.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::Psi => "psi",
            Self::RepoCap => "repo-cap",
            Self::Reserved => "head-reserved",
            Self::Order => "order",
        }
    }
}

/// What the scheduler needs to know about one job.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JobSpec {
    /// Unique job id.
    pub id: String,
    /// Cache key; `None` means the job is never coalesced.
    pub key: Option<CacheKey>,
    /// Repository identity the job runs in (used for the per-repo cap).
    pub repo: PathBuf,
    /// Fairness lane: the worktree path of the request (ADR-0011 M5).
    pub lane: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Budget units the job holds. On input the configured weight; after
    /// [`Scheduler::enqueue`] the effective one (learned, clamped). While
    /// queued it is recomputed at every admission pass from the current
    /// learned weights; once running it is the weight it was admitted with.
    pub weight: u32,
    /// Configured weight after the budget and repository-cap clamp (set by
    /// [`Scheduler::enqueue`]).
    pub weight_cfg: u32,
    /// Priority class.
    pub class: Class,
    /// Time the job was queued, unix milliseconds.
    pub queued_at: u64,
    /// Expected run time from history ([`expected_ms`]).
    pub expected_ms: Option<u64>,
    /// Learned-statistics key; `None` for jobs that never learn (`exec`).
    pub learn_key: Option<LearnKey>,
    /// Measured demands of earlier runs of `learn_key` (milli-CPUs, oldest
    /// first) known at enqueue; they refresh the scheduler's learned weight
    /// of that key.
    pub demands_milli: Vec<u32>,
}

/// Result of [`Scheduler::enqueue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enqueued {
    /// A new job was created at this 1-based queue position.
    Created {
        /// 1-based queue position.
        position: usize,
    },
    /// The request was attached to an existing job (queued: `Some(position)`;
    /// running: `None`).
    Coalesced {
        /// Id of the existing job.
        job_id: String,
        /// 1-based queue position, `None` when the job is already running.
        position: Option<usize>,
    },
}

/// A running job as shown by `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningView {
    /// Job id.
    pub id: String,
    /// Repository identity.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Budget units held (effective weight).
    pub weight: u32,
    /// Configured weight after the clamp.
    pub weight_cfg: u32,
    /// Start time, unix milliseconds.
    pub started_at: u64,
    /// Queue time, unix milliseconds.
    pub queued_at: u64,
    /// Current subscribers.
    pub subscribers: usize,
    /// How it was admitted.
    pub admission: Admission,
}

/// A queued job as shown by `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedView {
    /// Job id.
    pub id: String,
    /// Repository identity.
    pub repo: PathBuf,
    /// Fairness lane.
    pub lane: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Budget units needed (effective weight from the current learned
    /// statistics).
    pub weight: u32,
    /// Configured weight after the clamp.
    pub weight_cfg: u32,
    /// Queue time, unix milliseconds.
    pub queued_at: u64,
    /// Current subscribers.
    pub subscribers: usize,
    /// Why the job is not running yet (frozen wire values).
    pub reason: WaitReason,
    /// The detailed cause.
    pub cause: WaitCause,
    /// `outranked`, `lane` or `human-first` for [`WaitCause::Order`].
    pub detail: Option<&'static str>,
    /// 1-based position in the priority order.
    pub rank: usize,
    /// Expected run time from history.
    pub expected_ms: Option<u64>,
}

/// Scheduling facts of one job for the timeline (`jobs` columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// Configured weight after the clamp.
    pub weight_cfg: u32,
    /// Effective weight.
    pub weight: u32,
    /// Admission time, when admitted.
    pub admitted_at: Option<u64>,
    /// How it was admitted.
    pub admission: Option<Admission>,
    /// Wait accumulators, milliseconds, indexed by [`WaitCause::index`].
    pub waits: [u64; 5],
    /// Expected run time used for ordering.
    pub expected_ms: Option<u64>,
}

impl JobRecord {
    /// The cause with the largest accumulator; `None` when it never waited.
    pub fn dominant(&self) -> Option<WaitCause> {
        WaitCause::ALL
            .into_iter()
            .filter(|c| self.waits[c.index()] > 0)
            .max_by_key(|c| (self.waits[c.index()], std::cmp::Reverse(c.index())))
    }
}

// ---------------------------------------------------------------------------
// Pure decision: snapshot -> plan
// ---------------------------------------------------------------------------

/// The admission policy (every ADR-0011 switch).
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Budget units (at least 1).
    pub budget: u32,
    /// Over-admission cap (`>= budget`; equal disables over-admission).
    pub cap: u32,
    /// Head reservation and ageing bound, milliseconds.
    pub head_reserve_after_ms: u64,
    /// At most one over-admission per this many milliseconds.
    pub overcommit_interval_ms: u64,
    /// Tie-break among equal priorities.
    pub order: Order,
    /// Interleave lanes (worktrees).
    pub fairness: bool,
    /// Humans before agents.
    pub human_first: bool,
    /// Use learned weights (resolved at every admission pass).
    pub learned_weights: bool,
    /// Per-repository caps.
    pub repo_caps: BTreeMap<PathBuf, u32>,
}

impl Policy {
    /// ADR-0007 behaviour: FIFO with backfill and reservation, every
    /// ADR-0011 switch off.
    pub fn fifo(
        budget: u32,
        head_reserve_after_ms: u64,
        repo_caps: BTreeMap<PathBuf, u32>,
    ) -> Self {
        let budget = budget.max(1);
        Self {
            budget,
            cap: budget,
            head_reserve_after_ms,
            overcommit_interval_ms: 5_000,
            order: Order::Fifo,
            fairness: false,
            human_first: false,
            learned_weights: false,
            repo_caps,
        }
    }

    /// `floor(overcommit * budget)`, never below the budget.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded: overcommit is validated to 1.0..=4.0"
    )]
    pub fn cap_for(budget: u32, overcommit: f64) -> u32 {
        let cap = (f64::from(budget) * overcommit.clamp(1.0, 4.0)).floor();
        (cap as u32).max(budget)
    }
}

/// Pressure facts of one admission pass.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pressure {
    /// PSI blocks admission (hysteresis state).
    pub blocking: bool,
    /// The PSI starvation guard may start the first ranked job.
    pub guard_due: bool,
    /// ADR-0011 section 5 conditions 1–3 hold (PSI available with both
    /// values, not blocking, calm, not just resumed).
    pub calm: bool,
    /// Mean idle CPUs over the last two utilisation samples.
    pub idle_cpus: Option<f64>,
}

/// A queued job in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedSnap {
    /// Job id.
    pub id: String,
    /// Repository identity.
    pub repo: PathBuf,
    /// Fairness lane.
    pub lane: PathBuf,
    /// Configured weight after the budget and repository-cap clamp; the
    /// effective weight is resolved from it by [`queued_weight`].
    pub weight_cfg: u32,
    /// Learned-statistics key; `None` never learns.
    pub learn_key: Option<LearnKey>,
    /// Priority class.
    pub class: Class,
    /// Queue time.
    pub queued_at: u64,
    /// Expected run time.
    pub expected_ms: Option<u64>,
}

/// A running job in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningSnap {
    /// Job id.
    pub id: String,
    /// Repository identity.
    pub repo: PathBuf,
    /// Fairness lane.
    pub lane: PathBuf,
    /// Units held.
    pub weight: u32,
}

/// Everything one admission decision depends on.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<'a> {
    /// Unix milliseconds.
    pub now: u64,
    /// The policy.
    pub policy: &'a Policy,
    /// Pressure facts.
    pub pressure: Pressure,
    /// Time of the last over-admission.
    pub last_overcommit_at: Option<u64>,
    /// Running jobs.
    pub running: &'a [RunningSnap],
    /// Queued jobs (any order).
    pub queued: &'a [QueuedSnap],
    /// Current learned weights by key.
    pub learned: &'a Learned,
}

/// One queued job left waiting by a [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    /// Job id.
    pub id: String,
    /// Why it waits.
    pub cause: WaitCause,
    /// Detail of [`WaitCause::Order`].
    pub detail: Option<&'static str>,
}

/// The decision of one admission pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Jobs to start, in rank order, with how each is admitted.
    pub start: Vec<(String, Admission)>,
    /// The overdue head holding a reservation, if any.
    pub reserved: Option<String>,
    /// Jobs left waiting, in rank order.
    pub waits: Vec<Wait>,
    /// Every queued job id in rank order (before the starts).
    pub order: Vec<String>,
    /// Effective weight of each job of `order` in this pass, parallel to it
    /// (the weight a started job is admitted with).
    pub weights: Vec<u32>,
}

fn overdue(s: &Snapshot<'_>, j: &QueuedSnap) -> bool {
    s.now.saturating_sub(j.queued_at) >= s.policy.head_reserve_after_ms
}

/// Highest response ratio next: `(waited + expected) / expected`.
#[allow(clippy::cast_precision_loss, reason = "ratios of millisecond counts")]
fn hrrn(now: u64, j: &QueuedSnap) -> f64 {
    let expected = j.expected_ms.unwrap_or(DEFAULT_EXPECTED_MS).max(1) as f64;
    (now.saturating_sub(j.queued_at) as f64 + expected) / expected
}

/// Per queued job: running jobs of its lane plus its index among its lane's
/// queued jobs (arrival order).
fn lane_ranks(s: &Snapshot<'_>) -> Vec<usize> {
    s.queued
        .iter()
        .map(|j| {
            let running = s.running.iter().filter(|r| r.lane == j.lane).count();
            let ahead = s
                .queued
                .iter()
                .filter(|o| {
                    o.lane == j.lane && (o.queued_at, o.id.as_str()) < (j.queued_at, j.id.as_str())
                })
                .count();
            running + ahead
        })
        .collect()
}

fn compare(s: &Snapshot<'_>, lanes: &[usize], a: usize, b: usize) -> Ordering {
    let (ja, jb) = (&s.queued[a], &s.queued[b]);
    let p = s.policy;
    let (oa, ob) = (overdue(s, ja), overdue(s, jb));
    let arrival = || (ja.queued_at, ja.id.as_str()).cmp(&(jb.queued_at, jb.id.as_str()));
    match (oa, ob) {
        // Overdue jobs outrank everything and are FIFO among themselves.
        (true, true) => return arrival(),
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (false, false) => {}
    }
    let mut ord = Ordering::Equal;
    if p.human_first {
        ord = ord.then(ja.class.cmp(&jb.class));
    }
    if p.fairness {
        ord = ord.then(lanes[a].cmp(&lanes[b]));
    }
    if p.order == Order::Sjf {
        ord = ord.then_with(|| hrrn(s.now, jb).total_cmp(&hrrn(s.now, ja)));
    }
    ord.then_with(arrival)
}

/// Indices of `s.queued` in priority order (ADR-0011 section 6.1).
pub fn rank(s: &Snapshot<'_>) -> Vec<usize> {
    let lanes = lane_ranks(s);
    let mut idx: Vec<usize> = (0..s.queued.len()).collect();
    idx.sort_by(|&a, &b| compare(s, &lanes, a, b));
    idx
}

/// The admission decision of one pass: (1) PSI blocking → only the guard
/// may start the first ranked job, within the budget; (2) a nominal pass in
/// rank order with backfill and head reservation; (3) at most one
/// over-admission (the reserved head, else the first ranked job under the
/// cap) when the pressure allows it and nothing started nominally.
#[allow(
    clippy::too_many_lines,
    reason = "the three steps of ADR-0011 section 8, in order"
)]
pub fn plan(s: &Snapshot<'_>) -> Plan {
    let p = s.policy;
    let lanes = lane_ranks(s);
    let mut order: Vec<usize> = (0..s.queued.len()).collect();
    order.sort_by(|&a, &b| compare(s, &lanes, a, b));
    // Effective weights from the current learned statistics, by queue index.
    let w: Vec<u32> = s.queued.iter().map(|j| queued_weight(s, j)).collect();
    let mut plan = Plan {
        order: order.iter().map(|&i| s.queued[i].id.clone()).collect(),
        weights: order.iter().map(|&i| w[i]).collect(),
        ..Plan::default()
    };
    let mut held: Vec<(&Path, u32)> = s
        .running
        .iter()
        .map(|r| (r.repo.as_path(), r.weight))
        .collect();
    let used_of = |held: &[(&Path, u32)]| held.iter().map(|(_, w)| *w).sum::<u32>();
    let fits_repo = |held: &[(&Path, u32)], i: usize| {
        let j = &s.queued[i];
        p.repo_caps.get(&j.repo).is_none_or(|&cap| {
            let repo_used: u32 = held
                .iter()
                .filter(|(r, _)| *r == j.repo.as_path())
                .map(|(_, w)| *w)
                .sum();
            repo_used + w[i] <= cap
        })
    };
    let used0 = used_of(&held);

    if s.pressure.blocking {
        for (pos, &i) in order.iter().enumerate() {
            let j = &s.queued[i];
            if pos == 0 && s.pressure.guard_due && used0 + w[i] <= p.budget && fits_repo(&held, i) {
                plan.start.push((j.id.clone(), Admission::Guard));
                continue;
            }
            plan.waits.push(Wait {
                id: j.id.clone(),
                cause: WaitCause::Psi,
                detail: None,
            });
        }
        return plan;
    }

    let mut reserved: Option<usize> = None;
    let mut first_waiting = true;
    let mut started: Vec<usize> = Vec::new();
    let mut waiting: Vec<usize> = Vec::new();
    for &i in &order {
        let j = &s.queued[i];
        let used = used_of(&held);
        let fits_budget = used + w[i] <= p.budget;
        let fits_cap = fits_repo(&held, i);
        if reserved.is_none() && fits_budget && fits_cap {
            plan.start.push((j.id.clone(), Admission::Nominal));
            held.push((j.repo.as_path(), w[i]));
            started.push(i);
            continue;
        }
        // Only a head blocked by the budget reserves: a head blocked by its
        // repository cap alone must not stop other repositories.
        if first_waiting && reserved.is_none() && !fits_budget && overdue(s, j) {
            reserved = Some(i);
        }
        first_waiting = false;
        let (cause, detail) = if !fits_budget {
            if !started.is_empty() && used0 + w[i] <= p.budget {
                (WaitCause::Order, Some(order_detail(s, &lanes, &started, i)))
            } else {
                (WaitCause::Budget, None)
            }
        } else if !fits_cap {
            (WaitCause::RepoCap, None)
        } else {
            (WaitCause::Reserved, None)
        };
        waiting.push(i);
        plan.waits.push(Wait {
            id: j.id.clone(),
            cause,
            detail,
        });
    }
    plan.reserved = reserved.map(|i| s.queued[i].id.clone());

    let interval_ok = s
        .last_overcommit_at
        .is_none_or(|t| s.now.saturating_sub(t) >= p.overcommit_interval_ms);
    if p.cap > p.budget && plan.start.is_empty() && s.pressure.calm && interval_ok {
        let used = used_of(&held);
        let candidate = reserved.or_else(|| {
            waiting
                .iter()
                .copied()
                .find(|&i| used + w[i] <= p.cap && fits_repo(&held, i))
        });
        if let Some(i) = candidate
            && used + w[i] <= p.cap
            && fits_repo(&held, i)
            && s.pressure
                .idle_cpus
                .is_some_and(|idle| idle >= f64::from(w[i].div_ceil(3).max(1)))
        {
            let id = &s.queued[i].id;
            plan.start.push((id.clone(), Admission::Over));
            plan.waits.retain(|wt| &wt.id != id);
        }
    }
    plan
}

/// Effective weight of a queued job in this pass: with learned weights on
/// and a learned weight for its key, that weight bounded to
/// `max(1, ceil(w_cfg / 4))..=w_cfg`; `w_cfg` otherwise.
pub fn queued_weight(s: &Snapshot<'_>, j: &QueuedSnap) -> u32 {
    let w_cfg = j.weight_cfg.max(1);
    if !s.policy.learned_weights {
        return w_cfg;
    }
    j.learn_key
        .as_ref()
        .and_then(|k| s.learned.get(k))
        .map_or(w_cfg, |&l| clamp_learned(w_cfg, l))
}

/// Why a job that fitted the budget before this pass lost its room.
fn order_detail(s: &Snapshot<'_>, lanes: &[usize], started: &[usize], i: usize) -> &'static str {
    let j = &s.queued[i];
    if s.policy.human_first && started.iter().any(|&k| s.queued[k].class < j.class) {
        "human-first"
    } else if s.policy.fairness && started.iter().any(|&k| lanes[k] < lanes[i]) {
        "lane"
    } else {
        "outranked"
    }
}

/// Learned weight (ADR-0011 M2): `clamp(round_half_up(P75(demand)),
/// max(1, ceil(w_cfg / 4)), w_cfg)` over the last [`LEARN_WINDOW`] samples,
/// once there are [`LEARN_AFTER`]; `w_cfg` otherwise.
pub fn effective_weight(w_cfg: u32, demands_milli: &[u32]) -> u32 {
    let w_cfg = w_cfg.max(1);
    learned_weight(demands_milli).map_or(w_cfg, |l| clamp_learned(w_cfg, l))
}

/// The unbounded learned weight: `round_half_up(P75(demand))` over the last
/// [`LEARN_WINDOW`] samples, once there are [`LEARN_AFTER`]; `None` before.
pub fn learned_weight(demands_milli: &[u32]) -> Option<u32> {
    let window = &demands_milli[demands_milli.len().saturating_sub(LEARN_WINDOW)..];
    if window.len() < LEARN_AFTER {
        return None;
    }
    let mut v = window.to_vec();
    v.sort_unstable();
    // Nearest-rank 75th percentile.
    let p75 = v[(v.len() * 3).div_ceil(4) - 1];
    Some(p75.saturating_add(500) / 1000)
}

/// Bounds a learned weight to `max(1, ceil(w_cfg / 4))..=w_cfg`.
fn clamp_learned(w_cfg: u32, learned: u32) -> u32 {
    let w_cfg = w_cfg.max(1);
    learned.clamp(w_cfg.div_ceil(4).max(1), w_cfg)
}

/// Expected run time (ADR-0011 M7): the median wall time of the last
/// [`LEARN_WINDOW`] runs; `None` without history.
pub fn expected_ms(walls_ms: &[u64]) -> Option<u64> {
    let window = &walls_ms[walls_ms.len().saturating_sub(LEARN_WINDOW)..];
    if window.is_empty() {
        return None;
    }
    let mut v = window.to_vec();
    v.sort_unstable();
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        v[n / 2 - 1].midpoint(v[n / 2])
    })
}

// ---------------------------------------------------------------------------
// Stateful scheduler
// ---------------------------------------------------------------------------

/// One job tracked by the scheduler.
#[derive(Debug, Clone)]
struct Job {
    spec: JobSpec,
    subscribers: usize,
    subscribers_max: usize,
    started_at: u64,
    admission: Option<Admission>,
    waits: [u64; 5],
    cause: WaitCause,
    detail: Option<&'static str>,
    accounted_at: u64,
}

impl Job {
    fn new(spec: JobSpec) -> Self {
        let accounted_at = spec.queued_at;
        Self {
            spec,
            subscribers: 1,
            subscribers_max: 1,
            started_at: 0,
            admission: None,
            waits: [0; 5],
            cause: WaitCause::Budget,
            detail: None,
            accounted_at,
        }
    }

    /// Adds the time since the last pass to the current cause ("the state
    /// before the sample decides").
    fn account(&mut self, now: u64) {
        let dt = now.saturating_sub(self.accounted_at);
        self.waits[self.cause.index()] += dt;
        self.accounted_at = self.accounted_at.max(now);
    }

    fn queued_snap(&self) -> QueuedSnap {
        QueuedSnap {
            id: self.spec.id.clone(),
            repo: self.spec.repo.clone(),
            lane: self.spec.lane.clone(),
            weight_cfg: self.spec.weight_cfg,
            learn_key: self.spec.learn_key.clone(),
            class: self.spec.class,
            queued_at: self.spec.queued_at,
            expected_ms: self.spec.expected_ms,
        }
    }

    fn running_snap(&self) -> RunningSnap {
        RunningSnap {
            id: self.spec.id.clone(),
            repo: self.spec.repo.clone(),
            lane: self.spec.lane.clone(),
            weight: self.spec.weight,
        }
    }
}

/// Budget scheduler: the state that [`plan`] decides over.
#[derive(Debug)]
pub struct Scheduler {
    policy: Policy,
    /// Queued jobs in arrival order.
    queue: Vec<Job>,
    running: Vec<Job>,
    /// Current learned weights by key (refreshed by [`Scheduler::set_learned`]
    /// and by the history a job carries at enqueue).
    learned: Learned,
    last_overcommit_at: Option<u64>,
    over_admissions: u64,
    /// `now` of the last pass (orders `position` between passes).
    last_now: u64,
}

impl Scheduler {
    /// Creates an ADR-0007 scheduler (every ADR-0011 switch off) with
    /// `budget` units (clamped to at least 1), the head reservation threshold
    /// in milliseconds and the per-repository caps.
    pub fn new(budget: u32, head_reserve_after_ms: u64, repo_caps: BTreeMap<PathBuf, u32>) -> Self {
        Self::with_policy(Policy::fifo(budget, head_reserve_after_ms, repo_caps))
    }

    /// Creates a scheduler with an explicit policy.
    pub fn with_policy(mut policy: Policy) -> Self {
        policy.budget = policy.budget.max(1);
        policy.cap = policy.cap.max(policy.budget);
        Self {
            policy,
            queue: Vec::new(),
            running: Vec::new(),
            learned: Learned::new(),
            last_overcommit_at: None,
            over_admissions: 0,
            last_now: 0,
        }
    }

    /// The policy in force.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Total budget units.
    pub fn budget(&self) -> u32 {
        self.policy.budget
    }

    /// Over-admission cap.
    pub fn cap(&self) -> u32 {
        self.policy.cap
    }

    /// Units held by running jobs.
    pub fn used(&self) -> u32 {
        self.running.iter().map(|j| j.spec.weight).sum()
    }

    /// Jobs started by over-admission so far.
    pub fn over_admissions(&self) -> u64 {
        self.over_admissions
    }

    /// Whether any job is queued.
    pub fn has_queued(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Replaces the learned weight of `key` from its recent demands
    /// (milli-CPUs, oldest first); removes it while there are fewer than
    /// [`LEARN_AFTER`]. Queued jobs of that key pick it up at the next pass;
    /// running jobs keep the weight they were admitted with.
    pub fn set_learned(&mut self, key: LearnKey, demands_milli: &[u32]) {
        match learned_weight(demands_milli) {
            Some(w) => {
                self.learned.insert(key, w);
            }
            None => {
                self.learned.remove(&key);
            }
        }
    }

    /// Adds a request. The configured weight is clamped to
    /// `1..=min(budget, repo cap)` (a warning is logged once per clamped
    /// job) and becomes `weight_cfg`; the history it carries refreshes the
    /// learned weight of its key, and its effective weight (recomputed at
    /// every pass while it waits) is [`queued_weight`] of it. If `spec.key` is `Some` and a
    /// queued or running job has the same key, the request is attached to it
    /// (subscribers += 1) and [`Enqueued::Coalesced`] is returned. Otherwise
    /// the job is queued with one subscriber and [`Enqueued::Created`] is
    /// returned.
    pub fn enqueue(&mut self, mut spec: JobSpec) -> Enqueued {
        if let Some(key) = &spec.key
            && let Some(id) = self.find_by_key(key).map(str::to_owned)
        {
            self.subscribe(&id);
            let position = self.position(&id);
            return Enqueued::Coalesced {
                job_id: id,
                position,
            };
        }
        let max = self.max_weight(&spec.repo);
        if spec.weight > max {
            tracing::warn!(
                job = %spec.id,
                gate = %spec.gate,
                repo = %spec.repo.display(),
                weight = spec.weight,
                clamped_to = max,
                budget = self.policy.budget,
                "gate weight exceeds the budget or the repository cap; clamped"
            );
        }
        spec.weight_cfg = spec.weight.clamp(1, max);
        if let Some(key) = &spec.learn_key
            && learned_weight(&spec.demands_milli).is_some()
        {
            self.set_learned(key.clone(), &spec.demands_milli);
        }
        let mut job = Job::new(spec);
        job.spec.weight = queued_weight(
            &self.snapshot_at(self.last_now, &[], &[]),
            &job.queued_snap(),
        );
        self.queue.push(job);
        Enqueued::Created {
            position: self.queue.len(),
        }
    }

    fn snaps(&self) -> (Vec<RunningSnap>, Vec<QueuedSnap>) {
        (
            self.running.iter().map(Job::running_snap).collect(),
            self.queue.iter().map(Job::queued_snap).collect(),
        )
    }

    fn snapshot_at<'a>(
        &'a self,
        now: u64,
        running: &'a [RunningSnap],
        queued: &'a [QueuedSnap],
    ) -> Snapshot<'a> {
        Snapshot {
            now,
            policy: &self.policy,
            pressure: Pressure::default(),
            last_overcommit_at: self.last_overcommit_at,
            running,
            queued,
            learned: &self.learned,
        }
    }

    fn plan_at(&self, now: u64, pressure: Pressure) -> Plan {
        let (running, queued) = self.snaps();
        plan(&Snapshot {
            pressure,
            ..self.snapshot_at(now, &running, &queued)
        })
    }

    /// ADR-0007 admission pass: within the budget only, PSI gating as a
    /// boolean. Returns the ids moved to running, in rank order.
    pub fn admit(&mut self, now: u64, psi_blocking: bool) -> Vec<String> {
        self.admit_with(
            now,
            Pressure {
                blocking: psi_blocking,
                ..Pressure::default()
            },
        )
        .into_iter()
        .map(|(id, _)| id)
        .collect()
    }

    /// One admission pass: accounts the elapsed wait of every queued job to
    /// its previous cause, applies [`plan`] and records the new causes.
    /// Returns the started ids, in rank order, with how each was admitted
    /// (`started_at = now`).
    pub fn admit_with(&mut self, now: u64, pressure: Pressure) -> Vec<(String, Admission)> {
        self.last_now = now;
        for job in &mut self.queue {
            job.account(now);
        }
        let plan = self.plan_at(now, pressure);
        // Every queued job takes the weight of this pass; a started one keeps
        // it while running (the units `used` and `finish` account).
        for (id, &weight) in plan.order.iter().zip(&plan.weights) {
            if let Some(job) = self.queue.iter_mut().find(|j| &j.spec.id == id) {
                job.spec.weight = weight;
            }
        }
        for (id, admission) in &plan.start {
            let Some(idx) = self.queue.iter().position(|j| &j.spec.id == id) else {
                continue;
            };
            let mut job = self.queue.remove(idx);
            job.started_at = now;
            job.admission = Some(*admission);
            if *admission == Admission::Over {
                self.last_overcommit_at = Some(now);
                self.over_admissions += 1;
            }
            self.running.push(job);
        }
        for wait in &plan.waits {
            if let Some(job) = self.queue.iter_mut().find(|j| j.spec.id == wait.id) {
                job.cause = wait.cause;
                job.detail = wait.detail;
            }
        }
        plan.start
    }

    /// The first ranked queued job at `now`: `(id, queued_at)`.
    pub fn head(&self, now: u64) -> Option<(String, u64)> {
        let (running, queued) = self.snaps();
        rank(&self.snapshot_at(now, &running, &queued))
            .first()
            .map(|&i| (queued[i].id.clone(), queued[i].queued_at))
    }

    /// Queue time of the first ranked job (as of the last pass), if any.
    pub fn head_queued_at(&self) -> Option<u64> {
        self.head(self.last_now).map(|(_, t)| t)
    }

    /// Starts the first ranked job alone, ignoring PSI (the starvation
    /// guard), when it fits the budget and its repository cap; returns its id.
    pub fn admit_head(&mut self, now: u64) -> Option<String> {
        let started = self.admit_with(
            now,
            Pressure {
                blocking: true,
                guard_due: true,
                ..Pressure::default()
            },
        );
        started.into_iter().next().map(|(id, _)| id)
    }

    /// Removes a running job and returns its spec; `None` if not running.
    pub fn finish(&mut self, id: &str) -> Option<JobSpec> {
        let idx = self.running.iter().position(|j| j.spec.id == id)?;
        Some(self.running.remove(idx).spec)
    }

    /// Removes a queued job (cancelled before start); `None` if not queued.
    pub fn remove_queued(&mut self, id: &str) -> Option<JobSpec> {
        let idx = self.queue.iter().position(|j| j.spec.id == id)?;
        Some(self.queue.remove(idx).spec)
    }

    /// Scheduling facts of a queued or running job; the wait of a queued
    /// job includes the time since the last pass.
    pub fn record(&self, id: &str, now: u64) -> Option<JobRecord> {
        let job = self.job(id)?;
        let mut job = job.clone();
        if job.admission.is_none() {
            job.account(now);
        }
        Some(JobRecord {
            weight_cfg: job.spec.weight_cfg,
            weight: job.spec.weight,
            admitted_at: job.admission.map(|_| job.started_at),
            admission: job.admission,
            waits: job.waits,
            expected_ms: job.spec.expected_ms,
        })
    }

    /// Adds a subscriber to a queued or running job; returns the new count.
    pub fn subscribe(&mut self, id: &str) -> Option<usize> {
        let job = self.job_mut(id)?;
        job.subscribers += 1;
        job.subscribers_max = job.subscribers_max.max(job.subscribers);
        Some(job.subscribers)
    }

    /// Removes a subscriber; returns the remaining count (0 means the server
    /// should apply the last-subscriber policy).
    pub fn unsubscribe(&mut self, id: &str) -> Option<usize> {
        let job = self.job_mut(id)?;
        job.subscribers = job.subscribers.saturating_sub(1);
        Some(job.subscribers)
    }

    /// Current subscribers of a queued or running job.
    pub fn subscribers(&self, id: &str) -> Option<usize> {
        self.job(id).map(|j| j.subscribers)
    }

    /// Maximum simultaneous subscribers the job has had.
    pub fn subscribers_max(&self, id: &str) -> Option<usize> {
        self.job(id).map(|j| j.subscribers_max)
    }

    /// 1-based position in the priority order (as of the last pass); `None`
    /// if running or unknown.
    pub fn position(&self, id: &str) -> Option<usize> {
        if !self.is_queued(id) {
            return None;
        }
        let (running, queued) = self.snaps();
        rank(&self.snapshot_at(self.last_now, &running, &queued))
            .iter()
            .position(|&i| queued[i].id == id)
            .map(|p| p + 1)
    }

    /// Whether the job is running.
    pub fn is_running(&self, id: &str) -> bool {
        self.running.iter().any(|j| j.spec.id == id)
    }

    /// Whether the job is queued.
    pub fn is_queued(&self, id: &str) -> bool {
        self.queue.iter().any(|j| j.spec.id == id)
    }

    /// Id of the queued or running job with that key, if any.
    pub fn find_by_key(&self, key: &CacheKey) -> Option<&str> {
        self.running
            .iter()
            .chain(self.queue.iter())
            .find(|j| j.spec.key.as_ref() == Some(key))
            .map(|j| j.spec.id.as_str())
    }

    /// Views for `status`: running jobs ordered by `started_at` and queued
    /// jobs in rank order, each with the reason it would wait in a pass at
    /// `now`: `Psi` when PSI blocks, `RepoCap` when only the repository cap
    /// blocks it, `HeadReserved` when only the reservation blocks it,
    /// `Budget` otherwise (including jobs outranked by the jobs ahead, and
    /// jobs the next pass will start).
    pub fn snapshot(&self, now: u64, psi_blocking: bool) -> (Vec<RunningView>, Vec<QueuedView>) {
        let mut running: Vec<RunningView> = self
            .running
            .iter()
            .map(|j| RunningView {
                id: j.spec.id.clone(),
                repo: j.spec.repo.clone(),
                gate: j.spec.gate.clone(),
                weight: j.spec.weight,
                weight_cfg: j.spec.weight_cfg,
                started_at: j.started_at,
                queued_at: j.spec.queued_at,
                subscribers: j.subscribers,
                admission: j.admission.unwrap_or(Admission::Nominal),
            })
            .collect();
        running.sort_by_key(|v| v.started_at);

        let plan = self.plan_at(
            now,
            Pressure {
                blocking: psi_blocking,
                ..Pressure::default()
            },
        );
        let queued = plan
            .order
            .iter()
            .enumerate()
            .zip(&plan.weights)
            .filter_map(|((pos, id), &weight)| {
                let j = self.queue.iter().find(|j| &j.spec.id == id)?;
                let wait = plan.waits.iter().find(|w| &w.id == id);
                let cause = wait.map_or(WaitCause::Budget, |w| w.cause);
                Some(QueuedView {
                    id: j.spec.id.clone(),
                    repo: j.spec.repo.clone(),
                    lane: j.spec.lane.clone(),
                    gate: j.spec.gate.clone(),
                    weight,
                    weight_cfg: j.spec.weight_cfg,
                    queued_at: j.spec.queued_at,
                    subscribers: j.subscribers,
                    reason: cause.reason(),
                    cause,
                    detail: wait.and_then(|w| w.detail),
                    rank: pos + 1,
                    expected_ms: j.spec.expected_ms,
                })
            })
            .collect();
        (running, queued)
    }

    /// Spec of a queued or running job.
    pub fn spec(&self, id: &str) -> Option<&JobSpec> {
        self.job(id).map(|j| &j.spec)
    }

    fn job(&self, id: &str) -> Option<&Job> {
        self.running
            .iter()
            .chain(self.queue.iter())
            .find(|j| j.spec.id == id)
    }

    fn job_mut(&mut self, id: &str) -> Option<&mut Job> {
        self.running
            .iter_mut()
            .chain(self.queue.iter_mut())
            .find(|j| j.spec.id == id)
    }

    /// Largest weight a job of `repo` can ever hold: the budget, or the
    /// repository cap when lower (at least 1).
    fn max_weight(&self, repo: &Path) -> u32 {
        self.policy
            .repo_caps
            .get(repo)
            .map_or(self.policy.budget, |&cap| cap.min(self.policy.budget))
            .max(1)
    }
}

#[cfg(test)]
mod tests;
