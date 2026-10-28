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
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sluice_gate_core::key::CacheKey;
use sluice_gate_proto::{WaitReason, gib_text};

mod shadow;

pub use shadow::BackfillRule;
use shadow::{Head, Holder, Machine, Room, ends_in_time, predicted_end, room, shadow};

/// Expected run time of a job without history (HRRN), milliseconds.
pub const DEFAULT_EXPECTED_MS: u64 = 60_000;
/// Samples needed before a learned weight is used.
pub const LEARN_AFTER: usize = 3;
/// Samples (most recent) a learned weight and an expected time look at.
pub const LEARN_WINDOW: usize = 10;

/// Key of a job's learned statistics: repository identity (the canonical cwd
/// outside a repository), gate name (`exec:<label>` for `sluice exec`) and
/// command hash (empty for `exec`). Jobs without a key never learn.
pub type LearnKey = (PathBuf, String, String);

/// Learned weights by statistics key: the rounded P75 demand of the last
/// [`LEARN_WINDOW`] runs ([`learned_weight`]), before the per-job clamp.
pub type Learned = BTreeMap<LearnKey, u32>;

/// The expected peak (bytes) of every learned key with the weight it last ran
/// with: what a job with no history of its own is compared to
/// ([`prior_peak`], ADR-0019 amended in 0.6.1 and by ADR-0022).
pub type PeakPriors = Vec<(u32, u64)>;

/// Floor of the capped prior (ADR-0022): a gate is never guessed below it
/// from other gates' peaks.
const PRIOR_FLOOR_BYTES: u64 = 1 << 30;

/// The prior of a job of effective weight `weight` (ADR-0022): the 75th
/// percentile (nearest rank) of the learned peaks of the keys whose weight is
/// at most `weight`; `None` without such a key (the weight proxy then stands
/// alone). [`MemoryFacts::estimate_primed`] bounds it to `1 GiB..=share`.
pub fn prior_peak(priors: &PeakPriors, weight: u32) -> Option<u64> {
    let mut peaks: Vec<u64> = priors
        .iter()
        .filter(|(w, _)| *w <= weight)
        .map(|&(_, bytes)| bytes)
        .collect();
    peaks.sort_unstable();
    let rank = (peaks.len() * 3).div_ceil(4);
    rank.checked_sub(1).map(|i| peaks[i])
}

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
    /// The budget has room, but the memory the job is expected to use does
    /// not fit what is available (ADR-0019). Shares the budget accumulator
    /// (index 0) and the `budget` wire reason, like [`WaitCause::Order`]
    /// shares the reason; the wire detail is `memory`.
    Memory,
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
            // The `jobs.wait_*_ms` columns are frozen: memory waits count as
            // budget waits.
            Self::Budget | Self::Memory => 0,
            Self::Psi => 1,
            Self::RepoCap => 2,
            Self::Reserved => 3,
            Self::Order => 4,
        }
    }

    /// The frozen wire reason (an outranked or memory-blocked job reports
    /// `budget`).
    pub fn reason(self) -> WaitReason {
        match self {
            Self::Budget | Self::Order | Self::Memory => WaitReason::Budget,
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
            Self::Memory => "memory",
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
    /// The configured weight is `"auto"` (ADR-0015): `weight` on input is
    /// already `ceil(budget / 2)` of this daemon. Only shown by `status`.
    pub weight_auto: bool,
    /// Priority class.
    pub class: Class,
    /// Time the job was queued, unix milliseconds.
    pub queued_at: u64,
    /// Expected run time from history ([`expected_ms`]).
    pub expected_ms: Option<u64>,
    /// Learned-statistics key; `None` for jobs that never learn.
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
    /// The configured weight is `"auto"`.
    pub weight_auto: bool,
    /// Start time, unix milliseconds.
    pub started_at: u64,
    /// Queue time, unix milliseconds.
    pub queued_at: u64,
    /// Current subscribers.
    pub subscribers: usize,
    /// How it was admitted.
    pub admission: Admission,
    /// Working set now (last sample), bytes; `None` with memory-aware
    /// admission off.
    pub memory_current: Option<u64>,
    /// Peak it is expected to reach; `None` with memory-aware admission off.
    pub memory_expected: Option<Expected>,
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
    /// The configured weight is `"auto"`.
    pub weight_auto: bool,
    /// Queue time, unix milliseconds.
    pub queued_at: u64,
    /// Current subscribers.
    pub subscribers: usize,
    /// Why the job is not running yet (frozen wire values).
    pub reason: WaitReason,
    /// The detailed cause.
    pub cause: WaitCause,
    /// `outranked`, `lane` or `human-first` for [`WaitCause::Order`];
    /// `memory` for [`WaitCause::Memory`].
    pub detail: Option<&'static str>,
    /// 1-based position in the priority order.
    pub rank: usize,
    /// Expected run time from history.
    pub expected_ms: Option<u64>,
    /// Peak the job is expected to reach; `None` with memory-aware
    /// admission off.
    pub memory_expected: Option<Expected>,
    /// The job is the reserved head and is predicted to start then (its
    /// shadow time, unix milliseconds, ADR-0023).
    pub shadow_at: Option<u64>,
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
    /// Period of the utilisation and PSI sampler: an over-admission needs a
    /// sample taken at least this long after the latest admission.
    pub sample_tick_ms: u64,
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
    /// What an overdue head's reservation lets through.
    pub reservation: HeadReservation,
}

/// What an overdue head's reservation lets through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadReservation {
    /// Nothing behind the head starts (ADR-0007).
    Strict,
    /// Jobs shown not to delay the head start before it (ADR-0023).
    Backfill,
}

impl HeadReservation {
    /// `Backfill` when `on`, else `Strict` (the `[scheduler] backfill` key).
    pub fn backfill(on: bool) -> Self {
        if on { Self::Backfill } else { Self::Strict }
    }
}

impl Policy {
    /// ADR-0007 behaviour: FIFO with backfill and a strict reservation, every
    /// ADR-0011 switch and the ADR-0023 backfill off.
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
            sample_tick_ms: 1_000,
            order: Order::Fifo,
            fairness: false,
            human_first: false,
            learned_weights: false,
            repo_caps,
            reservation: HeadReservation::Strict,
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
    /// Time of the newest utilisation sample (unix milliseconds).
    pub sampled_at: Option<u64>,
}

/// Memory the machine offers at one admission pass (ADR-0019).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_field_names,
    reason = "the unit in every name keeps byte counts apart from weights"
)]
pub struct MemoryFacts {
    /// `MemAvailable`, bytes. Swap is never counted.
    pub available_bytes: u64,
    /// `MemTotal`, bytes.
    pub total_bytes: u64,
    /// Memory kept for everything that is not a gate, bytes.
    pub reserve_bytes: u64,
}

/// Where an expected peak comes from (ADR-0022).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Earlier passing (or memory-killed) runs of the same gate.
    Learned,
    /// `1.2 x` the largest recent peak of runs that failed or timed out: the
    /// gate needs at least that much, no run has shown more.
    LowerBound,
    /// The gates of the machine and the weight; no run of this gate says.
    Guessed,
}

impl Basis {
    /// The spelling used in `status` and the log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Learned => "learned",
            Self::LowerBound => "lower-bound",
            Self::Guessed => "guessed",
        }
    }
}

/// The peak a job is expected to reach, and where the figure comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expected {
    /// Bytes.
    pub bytes: u64,
    /// Where the figure comes from.
    pub basis: Basis,
}

impl Expected {
    /// From earlier passing runs of the same gate (only such a figure may
    /// over-admit).
    pub fn learned(self) -> bool {
        self.basis == Basis::Learned
    }
}

/// What a first-run probe needs free: a quarter of the estimate (ADR-0022).
fn probe_share(bytes: u64) -> u64 {
    bytes / 4
}

impl MemoryFacts {
    /// What gates may use at most: the machine's memory minus the reserve.
    fn usable(&self) -> u64 {
        self.total_bytes.saturating_sub(self.reserve_bytes)
    }

    /// The expected peak of a job, in the order of ADR-0022: the learned one
    /// ([`Basis::Learned`]); else `1.2 x` the largest recent lower bound
    /// ([`Basis::LowerBound`]); else a guess ([`Basis::Guessed`]): the
    /// weight share `w / B` of [`Self::usable`] (the CPU weight is the only
    /// prior the user already expressed), or, when the daemon knows gates of
    /// that weight or lighter, their `prior` clamped to `1 GiB..=share`.
    pub fn estimate_primed(
        &self,
        learned: Option<u64>,
        lower_bound: Option<u64>,
        prior: Option<u64>,
        weight: u32,
        budget: u32,
    ) -> Expected {
        if let Some(bytes) = learned {
            return Expected {
                bytes,
                basis: Basis::Learned,
            };
        }
        if let Some(bound) = lower_bound {
            return Expected {
                bytes: bound.saturating_add(bound / 5),
                basis: Basis::LowerBound,
            };
        }
        let share =
            u128::from(self.usable()) * u128::from(weight.min(budget)) / u128::from(budget.max(1));
        let proxy = u64::try_from(share).unwrap_or(u64::MAX);
        Expected {
            bytes: prior.map_or(proxy, |p| p.max(PRIOR_FLOOR_BYTES).min(proxy)),
            basis: Basis::Guessed,
        }
    }

    /// [`Self::estimate_primed`] with neither a lower bound nor a prior: the
    /// weight proxy alone.
    pub fn estimate(&self, learned: Option<u64>, weight: u32, budget: u32) -> Expected {
        self.estimate_primed(learned, None, None, weight, budget)
    }
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
    /// Peak memory learned from earlier runs of the key, bytes; `None`
    /// guesses it from the weight.
    pub expected_peak: Option<u64>,
    /// Largest recent peak of the key's failed or timed-out runs, bytes (a
    /// lower bound; used when `expected_peak` is `None`).
    pub lower_bound: Option<u64>,
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
    /// Working set at the last sample, bytes (0 before the first).
    pub current: u64,
    /// Peak learned from earlier runs of the key, bytes; `None` guesses it
    /// from the weight held.
    pub expected_peak: Option<u64>,
    /// Largest recent lower bound of the key, bytes.
    pub lower_bound: Option<u64>,
    /// Learned-statistics key; `None` never learns.
    pub learn_key: Option<LearnKey>,
    /// The job started as a first-run memory probe (ADR-0022).
    pub probe: bool,
    /// Start time, unix milliseconds (the shadow time of ADR-0023).
    pub started_at: u64,
    /// Expected run time from history; `None` without any.
    pub expected_ms: Option<u64>,
    /// The job started behind a reserved head (ADR-0023): it counts for the
    /// whole interval when later candidates are checked.
    pub backfilled: bool,
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
    /// Time of the latest admission of any kind.
    pub last_start_at: Option<u64>,
    /// Running jobs.
    pub running: &'a [RunningSnap],
    /// Queued jobs (any order).
    pub queued: &'a [QueuedSnap],
    /// Current learned weights by key.
    pub learned: &'a Learned,
    /// Memory-aware admission: `None` is the rule off (plans are those of
    /// CPU-only admission, byte for byte).
    pub memory: Option<MemoryFacts>,
    /// Largest learned peak by weight, the prior of jobs with no learned
    /// peak of their own.
    pub priors: &'a PeakPriors,
}

/// One queued job left waiting by a [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    /// Job id.
    pub id: String,
    /// Why it waits.
    pub cause: WaitCause,
    /// Detail of [`WaitCause::Order`] and [`WaitCause::Memory`].
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
    /// Expected peak of each job of `order`, parallel to it; empty with
    /// memory-aware admission off.
    pub expected: Vec<Expected>,
    /// The jobs of `start` that start as first-run memory probes: they did
    /// not fit their estimate, only a quarter of it (ADR-0022).
    pub probes: Vec<String>,
    /// When the reserved head is predicted to start (the shadow time,
    /// unix milliseconds); `Some` exactly when `reserved` is.
    pub shadow: Option<u64>,
    /// The jobs of `start` that start before the reserved head, with the
    /// rule that let them (ADR-0023).
    pub backfills: Vec<(String, BackfillRule)>,
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

/// The memory bookkeeping of one pass (ADR-0019).
struct MemPass {
    facts: MemoryFacts,
    /// Expected peak of every queued job, by queue index.
    peaks: Vec<Expected>,
    /// Headroom of the running jobs plus the full expected peak of the jobs
    /// started earlier in this pass.
    held: u64,
    /// A job runs or was started in this pass: the progress exemption of
    /// [`MemPass::fits`] no longer applies.
    busy: bool,
    /// A first-run probe runs or started in this pass (one per machine).
    probing: bool,
    /// Learn keys of the running jobs and of the jobs started in this pass
    /// (one probe per key, and none beside a running job of the key).
    keys: BTreeSet<LearnKey>,
}

impl MemPass {
    fn new(s: &Snapshot<'_>, facts: MemoryFacts, w: &[u32]) -> Self {
        let held = running_headroom_of(s, &facts);
        Self {
            facts,
            peaks: s
                .queued
                .iter()
                .zip(w)
                .map(|(j, &w)| estimate_job(s, &facts, j.expected_peak, j.lower_bound, w))
                .collect(),
            held,
            busy: !s.running.is_empty(),
            probing: s.running.iter().any(|r| r.probe),
            keys: s
                .running
                .iter()
                .filter_map(|r| r.learn_key.clone())
                .collect(),
        }
    }

    /// Memory a new job may still take: what is available minus the reserve
    /// and the growth still expected of the jobs already admitted.
    fn free(&self) -> u64 {
        self.facts
            .available_bytes
            .saturating_sub(self.facts.reserve_bytes)
            .saturating_sub(self.held)
    }

    /// The job fits, or nothing at all runs and nothing started earlier in
    /// this pass (the progress exemption: an idle machine always starts the
    /// first ranked job, however big its expected peak).
    fn fits(&self, i: usize) -> bool {
        !self.busy || self.peaks[i].bytes <= self.free()
    }

    /// The job fits on a learned peak, with no `alone` exemption
    /// (over-admission never starts a job blind).
    fn fits_learned(&self, i: usize) -> bool {
        self.peaks[i].learned() && self.peaks[i].bytes <= self.free()
    }

    /// The job may start as a first-run probe (ADR-0022): its key has no
    /// learned peak, no other job of the key runs, no probe runs on the
    /// machine, and a quarter of its estimate is free. Over-admission never
    /// probes (it needs a learned peak).
    fn probe_ok(&self, i: usize, key: Option<&LearnKey>) -> bool {
        let e = self.peaks[i];
        !e.learned()
            && !self.probing
            && key.is_none_or(|k| !self.keys.contains(k))
            && probe_share(e.bytes) <= self.free()
    }

    /// What a started job holds back: its whole expected peak, or a quarter
    /// of it for a probe (what the probe is expected to need).
    fn cost(&self, i: usize, probe: bool) -> u64 {
        let e = self.peaks[i].bytes;
        if probe { probe_share(e) } else { e }
    }

    /// The job starts: [`MemPass::cost`] counts from now on.
    fn start(&mut self, i: usize, key: Option<&LearnKey>, probe: bool) {
        self.held = self.held.saturating_add(self.cost(i, probe));
        self.busy = true;
        self.probing |= probe;
        if let Some(k) = key {
            self.keys.insert(k.clone());
        }
    }
}

/// What one running job is still expected to grow by:
/// `max(0, expected - current)`.
fn headroom_of(s: &Snapshot<'_>, facts: &MemoryFacts, r: &RunningSnap) -> u64 {
    let e = estimate_job(s, facts, r.expected_peak, r.lower_bound, r.weight).bytes;
    // A probe is expected to need a quarter of its estimate.
    let e = if r.probe { probe_share(e) } else { e };
    e.saturating_sub(r.current)
}

/// Headroom of the running jobs: what they are still expected to grow by
/// (`max(0, expected - current)`, summed).
fn running_headroom_of(s: &Snapshot<'_>, facts: &MemoryFacts) -> u64 {
    s.running
        .iter()
        .fold(0, |held, r| held.saturating_add(headroom_of(s, facts, r)))
}

/// The memory a running job hands back when it ends, in the terms of
/// admission: what it holds now plus what it is still expected to grow by.
fn running_release(s: &Snapshot<'_>, facts: &MemoryFacts, r: &RunningSnap) -> u64 {
    r.current.saturating_add(headroom_of(s, facts, r))
}

/// The expected peak of a job of effective weight `weight` with the given
/// learned peak and lower bound, under the pass's priors and budget.
fn estimate_job(
    s: &Snapshot<'_>,
    facts: &MemoryFacts,
    peak: Option<u64>,
    lower_bound: Option<u64>,
    weight: u32,
) -> Expected {
    facts.estimate_primed(
        peak,
        lower_bound,
        prior_peak(s.priors, weight),
        weight,
        s.policy.budget,
    )
}

/// What the running jobs are still expected to grow by, bytes; `None` with
/// memory-aware admission off.
pub fn running_headroom(s: &Snapshot<'_>) -> Option<u64> {
    s.memory.map(|facts| running_headroom_of(s, &facts))
}

/// The state of one admission pass: the ranking, the effective weights, what
/// is held and what the pass has started.
struct Pass<'s> {
    s: Snapshot<'s>,
    lanes: Vec<usize>,
    /// Indices of `s.queued` in rank order.
    order: Vec<usize>,
    /// Effective weights by queue index.
    w: Vec<u32>,
    /// `(repo, weight)` of the running jobs and of those started so far.
    held: Vec<(&'s Path, u32)>,
    mem: Option<MemPass>,
    /// The running jobs and those started so far, for the shadow time.
    holders: Vec<Holder<'s>>,
    /// The reserved head, once the pass has found one (ADR-0023).
    reservation: Option<Reservation<'s>>,
}

/// The head holding the reservation of a pass, when it could start and what
/// a job started behind it may still take.
struct Reservation<'s> {
    /// Queue index of the head.
    head: usize,
    /// Its shadow time, unix milliseconds.
    shadow_at: u64,
    room: Room<'s>,
}

impl<'s> Pass<'s> {
    fn new(s: Snapshot<'s>) -> Self {
        let lanes = lane_ranks(&s);
        let mut order: Vec<usize> = (0..s.queued.len()).collect();
        order.sort_by(|&a, &b| compare(&s, &lanes, a, b));
        // Effective weights from the current learned statistics, by queue index.
        let w: Vec<u32> = s.queued.iter().map(|j| queued_weight(&s, j)).collect();
        let held = s
            .running
            .iter()
            .map(|r| (r.repo.as_path(), r.weight))
            .collect();
        let mem = s.memory.map(|facts| MemPass::new(&s, facts, &w));
        let holders = s
            .running
            .iter()
            .map(|r| Holder {
                repo: r.repo.as_path(),
                weight: r.weight,
                end: predicted_end(s.now, r.started_at, r.expected_ms),
                memory: s.memory.map_or(0, |f| running_release(&s, &f, r)),
                backfilled: r.backfilled,
            })
            .collect();
        Self {
            s,
            lanes,
            order,
            w,
            held,
            mem,
            holders,
            reservation: None,
        }
    }

    /// The plan before any decision: the ranking, the weights, the peaks.
    fn skeleton(&self) -> Plan {
        Plan {
            order: self
                .order
                .iter()
                .map(|&i| self.s.queued[i].id.clone())
                .collect(),
            weights: self.order.iter().map(|&i| self.w[i]).collect(),
            expected: self
                .mem
                .iter()
                .flat_map(|m| self.order.iter().map(|&i| m.peaks[i]))
                .collect(),
            ..Plan::default()
        }
    }

    fn used(&self) -> u32 {
        self.held.iter().map(|(_, w)| *w).sum()
    }

    fn fits_repo(&self, i: usize) -> bool {
        let j = &self.s.queued[i];
        self.s.policy.repo_caps.get(&j.repo).is_none_or(|&cap| {
            let repo_used: u32 = self
                .held
                .iter()
                .filter(|(r, _)| *r == j.repo.as_path())
                .map(|(_, w)| *w)
                .sum();
            repo_used + self.w[i] <= cap
        })
    }

    fn fits_mem(&self, i: usize) -> bool {
        self.mem.as_ref().is_none_or(|m| m.fits(i))
    }

    fn fits_mem_learned(&self, i: usize) -> bool {
        self.mem.as_ref().is_none_or(|m| m.fits_learned(i))
    }

    /// Job `i` does not fit its estimate but may start as a memory probe.
    fn probe_mem(&self, i: usize) -> bool {
        let key = self.s.queued[i].learn_key.as_ref();
        self.mem.as_ref().is_some_and(|m| m.probe_ok(i, key))
    }

    /// Admits job `i` into the pass: its weight and memory count from now on.
    /// A job admitted under a reservation is a backfill: it counts for the
    /// whole interval and takes its share of the room beside the head.
    fn start(&mut self, i: usize, probe: bool) {
        let s = self.s;
        let j = &s.queued[i];
        self.held.push((j.repo.as_path(), self.w[i]));
        let peak = self.mem.as_ref().map(|m| m.peaks[i].bytes);
        let memory = self.mem.as_ref().map_or(0, |m| m.cost(i, probe));
        if let Some(m) = &mut self.mem {
            m.start(i, j.learn_key.as_ref(), probe);
        }
        self.holders.push(Holder {
            repo: j.repo.as_path(),
            weight: self.w[i],
            end: predicted_end(s.now, s.now, j.expected_ms),
            memory,
            backfilled: self.reservation.is_some(),
        });
        if let Some(res) = &mut self.reservation {
            res.room.take(self.w[i], peak);
        }
    }

    /// Job `i` becomes the reserved head: its shadow time and the room beside
    /// it are fixed for the rest of the pass.
    fn reserve(&mut self, i: usize) {
        let s = self.s;
        let j = &s.queued[i];
        let head = Head {
            repo: j.repo.as_path(),
            weight: self.w[i],
            repo_cap: s.policy.repo_caps.get(&j.repo).copied(),
            peak: self.mem.as_ref().map(|m| m.peaks[i].bytes),
        };
        let machine = Machine {
            budget: s.policy.budget,
            free: self.mem.as_ref().map_or(0, MemPass::free),
        };
        let at = shadow(s.now, &self.holders, &head, &machine);
        self.reservation = Some(Reservation {
            head: i,
            shadow_at: at.at,
            room: room(&self.holders, &at, &head, &machine),
        });
    }

    /// The rule that lets job `i`, which fits now, start before the reserved
    /// head; `None` when it may not (or backfilling is off).
    fn backfill_rule(&self, i: usize) -> Option<BackfillRule> {
        let res = self
            .reservation
            .as_ref()
            .filter(|_| self.s.policy.reservation == HeadReservation::Backfill)?;
        let j = &self.s.queued[i];
        let peak = self.mem.as_ref().map(|m| m.peaks[i].bytes);
        if res.room.admits(&j.repo, self.w[i], peak) {
            Some(BackfillRule::ExtraResources)
        } else if ends_in_time(self.s.now, j.expected_ms, res.shadow_at) {
            Some(BackfillRule::EndsInTime)
        } else {
            None
        }
    }

    /// PSI blocks: nothing starts, except the first ranked job when the
    /// starvation guard is due and it fits the budget, its cap and memory.
    fn blocked(&self, plan: &mut Plan) {
        let used0 = self.used();
        for (pos, &i) in self.order.iter().enumerate() {
            let id = &self.s.queued[i].id;
            if pos == 0
                && self.s.pressure.guard_due
                && used0 + self.w[i] <= self.s.policy.budget
                && self.fits_repo(i)
                && self.fits_mem(i)
            {
                plan.start.push((id.clone(), Admission::Guard));
                continue;
            }
            plan.waits.push(Wait {
                id: id.clone(),
                cause: WaitCause::Psi,
                detail: None,
            });
        }
    }

    /// The nominal pass in rank order with the head reservation, and the
    /// jobs that start before the reserved head (ADR-0023). Returns the
    /// reserved head and the jobs left waiting.
    fn nominal(&mut self, plan: &mut Plan) -> (Option<usize>, Vec<usize>) {
        let s = self.s;
        let used0 = self.used();
        let mut first_waiting = true;
        let mut started: Vec<usize> = Vec::new();
        let mut waiting: Vec<usize> = Vec::new();
        for i in self.order.clone() {
            let j = &s.queued[i];
            let reserved = self.reservation.is_some();
            let fits_budget = self.used() + self.w[i] <= s.policy.budget;
            let fits_cap = self.fits_repo(i);
            let mut fits_mem = self.fits_mem(i);
            // A job that does not fit its estimate may still probe it
            // (ADR-0022); behind a reserved head only a job that fits its
            // whole estimate may start, so a probe never backfills.
            let probe = !fits_mem && !reserved && fits_budget && fits_cap && self.probe_mem(i);
            fits_mem |= probe;
            let fits = fits_budget && fits_cap && fits_mem;
            let rule = if fits && reserved {
                self.backfill_rule(i)
            } else {
                None
            };
            if fits && (!reserved || rule.is_some()) {
                plan.start.push((j.id.clone(), Admission::Nominal));
                if probe {
                    plan.probes.push(j.id.clone());
                }
                if let Some(rule) = rule {
                    plan.backfills.push((j.id.clone(), rule));
                }
                self.start(i, probe);
                started.push(i);
                continue;
            }
            // Only a head blocked by the budget or by memory reserves: a
            // head blocked by its repository cap alone must not stop other
            // repositories.
            if first_waiting
                && !reserved
                && overdue(&s, j)
                && (!fits_budget || (fits_cap && !fits_mem))
            {
                self.reserve(i);
            }
            first_waiting = false;
            let (cause, detail) =
                self.wait_cause(&started, used0, i, [fits_budget, fits_cap, fits_mem]);
            waiting.push(i);
            plan.waits.push(Wait {
                id: j.id.clone(),
                cause,
                detail,
            });
        }
        plan.shadow = self.reservation.as_ref().map(|r| r.shadow_at);
        (self.reservation.as_ref().map(|r| r.head), waiting)
    }

    /// Why job `i`, which did not start, waits; `fits` is the budget, the
    /// repository cap and the memory.
    fn wait_cause(
        &self,
        started: &[usize],
        used0: u32,
        i: usize,
        fits: [bool; 3],
    ) -> (WaitCause, Option<&'static str>) {
        let [fits_budget, fits_cap, fits_mem] = fits;
        if !fits_budget {
            if !started.is_empty() && used0 + self.w[i] <= self.s.policy.budget {
                let detail = order_detail(&self.s, &self.lanes, started, i);
                (WaitCause::Order, Some(detail))
            } else {
                (WaitCause::Budget, None)
            }
        } else if !fits_cap {
            (WaitCause::RepoCap, None)
        } else if !fits_mem {
            (WaitCause::Memory, Some("memory"))
        } else {
            (WaitCause::Reserved, None)
        }
    }

    /// At most one over-admission (the reserved head, else the first ranked
    /// job under the cap) when nothing started nominally and the pressure
    /// allows it. It needs a learned peak that fits memory.
    fn over_admit(&self, plan: &mut Plan, reserved: Option<usize>, waiting: &[usize]) {
        let s = self.s;
        let p = s.policy;
        let interval_ok = s
            .last_overcommit_at
            .is_none_or(|t| s.now.saturating_sub(t) >= p.overcommit_interval_ms);
        if p.cap <= p.budget
            || !plan.start.is_empty()
            || !s.pressure.calm
            || !interval_ok
            || !samples_after_last_start(&s)
        {
            return;
        }
        let used = self.used();
        let eligible =
            |i: usize| used + self.w[i] <= p.cap && self.fits_repo(i) && self.fits_mem_learned(i);
        let candidate = reserved.or_else(|| waiting.iter().copied().find(|&i| eligible(i)));
        if let Some(i) = candidate
            && eligible(i)
            && s.pressure
                .idle_cpus
                .is_some_and(|idle| idle >= f64::from(self.w[i].div_ceil(3).max(1)))
        {
            let id = &s.queued[i].id;
            plan.start.push((id.clone(), Admission::Over));
            plan.waits.retain(|wt| &wt.id != id);
        }
    }
}

/// The admission decision of one pass: (1) PSI blocking → only the guard
/// may start the first ranked job, within the budget; (2) a nominal pass in
/// rank order with backfill and head reservation; (3) at most one
/// over-admission (the reserved head, else the first ranked job under the
/// cap) when the pressure allows it and nothing started nominally. With
/// memory facts (ADR-0019) a job also has to fit the memory it is expected
/// to use, unless nothing runs and nothing started earlier in the pass;
/// without them the rule is off.
pub fn plan(s: &Snapshot<'_>) -> Plan {
    let mut pass = Pass::new(*s);
    let mut plan = pass.skeleton();
    if s.pressure.blocking {
        pass.blocked(&mut plan);
        return plan;
    }
    let (reserved, waiting) = pass.nominal(&mut plan);
    plan.reserved = reserved.map(|i| s.queued[i].id.clone());
    pass.over_admit(&mut plan, reserved, &waiting);
    plan
}

/// Whether the pressure facts reflect the latest admission: no admission
/// yet, or a utilisation sample taken at least one sampler tick after it
/// (a sample of the interval before the job started would count its CPUs
/// as idle).
fn samples_after_last_start(s: &Snapshot<'_>) -> bool {
    s.last_start_at.is_none_or(|t| {
        s.pressure
            .sampled_at
            .is_some_and(|at| at >= t.saturating_add(s.policy.sample_tick_ms))
    })
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

/// A memory wait worth a daemon log line: a job first left memory-blocked,
/// or a job that waited for memory started. The `jobs.wait_*_ms` columns and
/// the wire reasons fold memory into the budget (ADR-0019), so the log is the
/// only place the time spent waiting for memory shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryNote {
    /// The job started (`true`) or was first seen memory-blocked (`false`).
    pub started: bool,
    /// The job started as a first-run memory probe (ADR-0022).
    pub probe: bool,
    /// Job id.
    pub job: String,
    /// Repository of the job.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Time spent memory-blocked, milliseconds, as of the note.
    pub waited_ms: u64,
    /// The peak the job was expected to reach.
    pub expected: Option<Expected>,
    /// `MemAvailable` at the pass, bytes.
    pub available_bytes: u64,
    /// Memory kept for everything that is not a gate, bytes.
    pub reserve_bytes: u64,
    /// What the running jobs were still expected to grow by, bytes.
    pub headroom_bytes: u64,
}

impl MemoryNote {
    /// `<id> (<repo basename>:<gate>)`.
    fn who(&self) -> String {
        who(&self.job, &self.repo, &self.gate)
    }

    /// `18.0 GiB learned` / `0.4 GiB lower-bound` / `18.0 GiB guessed`,
    /// `unknown` without an estimate.
    fn expected_text(&self) -> String {
        self.expected.map_or_else(
            || "unknown".to_owned(),
            |e| format!("{} {}", gib_text(e.bytes), e.basis.as_str()),
        )
    }
}

/// The line logged when a job first becomes memory-blocked.
pub fn memory_blocked_line(n: &MemoryNote) -> String {
    format!(
        "{} waits for memory: expected {}, free {}, running headroom {}",
        n.who(),
        n.expected_text(),
        gib_text(n.available_bytes),
        gib_text(n.headroom_bytes)
    )
}

/// The line of a note: blocked, started after a wait, or probe start.
pub fn memory_note_line(n: &MemoryNote) -> String {
    if n.probe {
        memory_probe_line(n)
    } else if n.started {
        memory_started_line(n)
    } else {
        memory_blocked_line(n)
    }
}

/// The line logged when a job starts as a first-run memory probe: it did
/// not fit its estimate, a quarter of it was free.
pub fn memory_probe_line(n: &MemoryNote) -> String {
    let needs = n.expected.map_or(0, |e| probe_share(e.bytes));
    format!(
        "{} starts as a memory probe: expected {}, needs a quarter ({}), free at start {}, \
         reserve {}, waited {}.{}s for memory",
        n.who(),
        n.expected_text(),
        gib_text(needs),
        gib_text(n.available_bytes),
        gib_text(n.reserve_bytes),
        n.waited_ms / 1000,
        n.waited_ms % 1000 / 100,
    )
}

/// The line logged when a job that waited for memory starts.
pub fn memory_started_line(n: &MemoryNote) -> String {
    format!(
        "{} started after waiting {}.{}s for memory (expected {}, free at start {}, reserve {})",
        n.who(),
        n.waited_ms / 1000,
        n.waited_ms % 1000 / 100,
        n.expected_text(),
        gib_text(n.available_bytes),
        gib_text(n.reserve_bytes)
    )
}

/// The note of `spec` for this pass; `plan.order` and `plan.expected` are
/// parallel, so the estimate is the one the pass decided on.
fn memory_note(
    (started, probe): (bool, bool),
    spec: &JobSpec,
    waited_ms: u64,
    plan: &Plan,
    facts: MemoryFacts,
    headroom_bytes: u64,
) -> MemoryNote {
    let expected = plan
        .order
        .iter()
        .position(|id| *id == spec.id)
        .and_then(|i| plan.expected.get(i).copied());
    MemoryNote {
        started,
        probe,
        job: spec.id.clone(),
        repo: spec.repo.clone(),
        gate: spec.gate.clone(),
        waited_ms,
        expected,
        available_bytes: facts.available_bytes,
        reserve_bytes: facts.reserve_bytes,
        headroom_bytes,
    }
}

/// A job that started before the reserved head, for the log (ADR-0023).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillNote {
    /// Job id.
    pub job: String,
    /// Repository of the job.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// The rule that let it start.
    pub rule: BackfillRule,
    /// Its learned duration, if any.
    pub expected_ms: Option<u64>,
    /// The reserved head: job id, repository and gate.
    pub head: (String, PathBuf, String),
    /// Milliseconds until the head's shadow time, as of the pass.
    pub shadow_in_ms: u64,
}

/// `<id> (<repo basename>:<gate>)`.
fn who(job: &str, repo: &Path, gate: &str) -> String {
    let repo = repo
        .file_name()
        .map_or_else(|| "?".into(), |n| n.to_string_lossy());
    format!("job {job} ({repo}:{gate})")
}

/// Milliseconds as `12.3s`.
fn seconds_text(ms: u64) -> String {
    format!("{}.{}s", ms / 1000, ms % 1000 / 100)
}

/// The line logged when a job starts before the reserved head.
pub fn backfill_line(n: &BackfillNote) -> String {
    let (head, head_repo, head_gate) = &n.head;
    format!(
        "{} starts before the reserved head {} ({}): expected {}, the head is predicted to \
         start in {}",
        who(&n.job, &n.repo, &n.gate),
        who(head, head_repo, head_gate),
        n.rule.as_str(),
        n.expected_ms
            .map_or_else(|| "unknown".to_owned(), seconds_text),
        seconds_text(n.shadow_in_ms),
    )
}

/// What backfilling did since the daemon started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillStats {
    /// Jobs that started before a reserved head.
    pub backfilled: u64,
    /// Summed over the reserved heads that started: how long after its first
    /// computed shadow time each did (zero when it started earlier),
    /// milliseconds.
    pub head_delay_ms: u64,
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
    /// Time spent with [`WaitCause::Memory`], on top of the budget
    /// accumulator it is also added to (observability only, never persisted).
    memory_wait_ms: u64,
    /// When a pass first left the job memory-blocked: the log line is
    /// written once.
    memory_blocked_at: Option<u64>,
    /// The job started as a first-run memory probe (ADR-0022).
    probe: bool,
    /// The job started before the reserved head (ADR-0023).
    backfilled: bool,
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
            memory_wait_ms: 0,
            memory_blocked_at: None,
            probe: false,
            backfilled: false,
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
        if self.cause == WaitCause::Memory {
            self.memory_wait_ms += dt;
        }
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
            expected_peak: None,
            lower_bound: None,
        }
    }

    fn running_snap(&self) -> RunningSnap {
        RunningSnap {
            id: self.spec.id.clone(),
            repo: self.spec.repo.clone(),
            lane: self.spec.lane.clone(),
            weight: self.spec.weight,
            current: 0,
            expected_peak: None,
            lower_bound: None,
            learn_key: self.spec.learn_key.clone(),
            probe: self.probe,
            started_at: self.started_at,
            expected_ms: self.spec.expected_ms,
            backfilled: self.backfilled,
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
    /// Learned peak memory (expected working set, bytes) by key.
    learned_peaks: BTreeMap<LearnKey, u64>,
    /// Largest recent lower bound (peak of a failed or timed-out run) by key.
    lower_bounds: BTreeMap<LearnKey, u64>,
    /// Learned peak of every key with its weight ([`prior_peak`]).
    priors: PeakPriors,
    /// Memory facts of the last [`Scheduler::set_memory`]: `None` is the
    /// memory rule off.
    memory: Option<MemoryFacts>,
    /// Working set of each running job at its last sample, bytes.
    currents: BTreeMap<String, u64>,
    last_overcommit_at: Option<u64>,
    /// Time of the latest admission of any kind.
    last_start_at: Option<u64>,
    over_admissions: u64,
    /// `now` of the last pass (orders `position` between passes).
    last_now: u64,
    /// Memory waits to log, drained by [`Scheduler::take_memory_notes`].
    notes: Vec<MemoryNote>,
    /// Starts before a reserved head to log, drained by
    /// [`Scheduler::take_backfill_notes`].
    backfill_notes: Vec<BackfillNote>,
    /// The first shadow time computed for each reserved head still queued.
    first_shadows: BTreeMap<String, u64>,
    /// Jobs started before a reserved head, and the time the reserved heads
    /// that have started did so after their first shadow time.
    backfill_stats: BackfillStats,
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
            learned_peaks: BTreeMap::new(),
            lower_bounds: BTreeMap::new(),
            priors: PeakPriors::new(),
            memory: None,
            currents: BTreeMap::new(),
            last_overcommit_at: None,
            last_start_at: None,
            over_admissions: 0,
            last_now: 0,
            notes: Vec::new(),
            backfill_notes: Vec::new(),
            first_shadows: BTreeMap::new(),
            backfill_stats: BackfillStats::default(),
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

    /// Replaces the learned peak memory of `key` (bytes); `None` removes it
    /// (the job is then guessed from its weight). Queued and running jobs of
    /// that key use it from the next pass.
    pub fn set_learned_peak(&mut self, key: LearnKey, peak_bytes: Option<u64>) {
        match peak_bytes {
            Some(bytes) => {
                self.learned_peaks.insert(key, bytes);
            }
            None => {
                self.learned_peaks.remove(&key);
            }
        }
    }

    /// Replaces the lower bound of `key` (bytes, the largest recent peak of
    /// its failed or timed-out runs); `None` removes it. Used while the key
    /// has no learned peak.
    pub fn set_lower_bound(&mut self, key: LearnKey, bound_bytes: Option<u64>) {
        match bound_bytes {
            Some(bytes) => {
                self.lower_bounds.insert(key, bytes);
            }
            None => {
                self.lower_bounds.remove(&key);
            }
        }
    }

    /// Replaces the priors of jobs with no learned peak (computed by the
    /// daemon from the learned keys whose weight it knows).
    pub fn set_peak_priors(&mut self, priors: PeakPriors) {
        self.priors = priors;
    }

    /// Sets what the next passes (and `status`) know about memory: the
    /// machine's facts (`None` turns the rule off) and the working set of
    /// each running job at its last sample.
    pub fn set_memory(&mut self, facts: Option<MemoryFacts>, currents: BTreeMap<String, u64>) {
        self.memory = facts;
        self.currents = currents;
    }

    /// The machine's memory at the last [`Scheduler::set_memory`]; `None`
    /// with the rule off.
    pub fn memory_facts(&self) -> Option<MemoryFacts> {
        self.memory
    }

    /// What the running jobs are still expected to grow by at the last
    /// [`Scheduler::set_memory`], bytes; `None` with the rule off.
    pub fn memory_headroom(&self) -> Option<u64> {
        let (running, queued) = self.snaps();
        running_headroom(&self.snapshot_at(self.last_now, &running, &queued))
    }

    /// The memory waits the passes since the last call left to log, oldest
    /// first (a job first memory-blocked, a job that waited for memory
    /// started).
    pub fn take_memory_notes(&mut self) -> Vec<MemoryNote> {
        std::mem::take(&mut self.notes)
    }

    /// The jobs the passes since the last call started before a reserved
    /// head, oldest first.
    pub fn take_backfill_notes(&mut self) -> Vec<BackfillNote> {
        std::mem::take(&mut self.backfill_notes)
    }

    /// Backfilled jobs and head delay since the daemon started.
    pub fn backfill_stats(&self) -> BackfillStats {
        self.backfill_stats
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

    fn peak_of(&self, key: Option<&LearnKey>) -> Option<u64> {
        key.and_then(|k| self.learned_peaks.get(k)).copied()
    }

    fn bound_of(&self, key: Option<&LearnKey>) -> Option<u64> {
        key.and_then(|k| self.lower_bounds.get(k)).copied()
    }

    fn snaps(&self) -> (Vec<RunningSnap>, Vec<QueuedSnap>) {
        let running = self
            .running
            .iter()
            .map(|j| RunningSnap {
                current: self.currents.get(&j.spec.id).copied().unwrap_or(0),
                expected_peak: self.peak_of(j.spec.learn_key.as_ref()),
                lower_bound: self.bound_of(j.spec.learn_key.as_ref()),
                ..j.running_snap()
            })
            .collect();
        let queued = self
            .queue
            .iter()
            .map(|j| QueuedSnap {
                expected_peak: self.peak_of(j.spec.learn_key.as_ref()),
                lower_bound: self.bound_of(j.spec.learn_key.as_ref()),
                ..j.queued_snap()
            })
            .collect();
        (running, queued)
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
            last_start_at: self.last_start_at,
            running,
            queued,
            learned: &self.learned,
            memory: self.memory,
            priors: &self.priors,
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
        let head = self.note_shadow(&plan);
        // What the running jobs still hold back, before this pass starts any.
        let memory = self.memory.zip(self.memory_headroom());
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
            job.probe = plan.probes.contains(id);
            if (job.memory_wait_ms > 0 || job.probe)
                && let Some((facts, headroom)) = memory
            {
                let note = memory_note(
                    (true, job.probe),
                    &job.spec,
                    job.memory_wait_ms,
                    &plan,
                    facts,
                    headroom,
                );
                self.notes.push(note);
            }
            job.started_at = now;
            job.admission = Some(*admission);
            self.record_start(&mut job, &plan, head.as_ref());
            self.last_start_at = Some(now);
            if *admission == Admission::Over {
                self.last_overcommit_at = Some(now);
                self.over_admissions += 1;
            }
            self.running.push(job);
        }
        // The waiting jobs see the jobs this pass started as running.
        self.first_shadows
            .retain(|id, _| self.queue.iter().any(|j| &j.spec.id == id));
        let memory = self.memory.zip(self.memory_headroom());
        for wait in &plan.waits {
            if let Some(job) = self.queue.iter_mut().find(|j| j.spec.id == wait.id) {
                job.cause = wait.cause;
                job.detail = wait.detail;
                if wait.cause == WaitCause::Memory
                    && job.memory_blocked_at.is_none()
                    && let Some((facts, headroom)) = memory
                {
                    job.memory_blocked_at = Some(now);
                    let note = memory_note((false, false), &job.spec, 0, &plan, facts, headroom);
                    self.notes.push(note);
                }
            }
        }
        plan.start
    }

    /// Remembers the first shadow time of the reserved head of `plan`; returns
    /// the head's id, repository and gate for the log.
    fn note_shadow(&mut self, plan: &Plan) -> Option<(String, PathBuf, String)> {
        let id = plan.reserved.as_ref()?;
        if let Some(at) = plan.shadow {
            self.first_shadows.entry(id.clone()).or_insert(at);
        }
        let head = self.queue.iter().find(|j| &j.spec.id == id)?;
        Some((id.clone(), head.spec.repo.clone(), head.spec.gate.clone()))
    }

    /// Books a job `plan` starts: a start before the reserved head is flagged
    /// (so later passes count it for the whole interval), counted and logged;
    /// a reserved head's start is measured against its first shadow time.
    fn record_start(
        &mut self,
        job: &mut Job,
        plan: &Plan,
        head: Option<&(String, PathBuf, String)>,
    ) {
        let id = &job.spec.id;
        if let Some(first) = self.first_shadows.remove(id) {
            let late = job.started_at.saturating_sub(first);
            self.backfill_stats.head_delay_ms =
                self.backfill_stats.head_delay_ms.saturating_add(late);
        }
        let rule = plan
            .backfills
            .iter()
            .find(|(b, _)| b == id)
            .map(|(_, r)| *r);
        let (Some(rule), Some(head)) = (rule, head) else {
            return;
        };
        job.backfilled = true;
        self.backfill_stats.backfilled += 1;
        self.backfill_notes.push(BackfillNote {
            job: id.clone(),
            repo: job.spec.repo.clone(),
            gate: job.spec.gate.clone(),
            rule,
            expected_ms: job.spec.expected_ms,
            head: head.clone(),
            shadow_in_ms: plan.shadow.unwrap_or(0).saturating_sub(job.started_at),
        });
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
                weight_auto: j.spec.weight_auto,
                started_at: j.started_at,
                queued_at: j.spec.queued_at,
                subscribers: j.subscribers,
                admission: j.admission.unwrap_or(Admission::Nominal),
                memory_current: self
                    .memory
                    .map(|_| self.currents.get(&j.spec.id).copied().unwrap_or(0)),
                memory_expected: self.memory.map(|f| {
                    f.estimate_primed(
                        self.peak_of(j.spec.learn_key.as_ref()),
                        self.bound_of(j.spec.learn_key.as_ref()),
                        prior_peak(&self.priors, j.spec.weight),
                        j.spec.weight,
                        self.policy.budget,
                    )
                }),
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
                    weight_auto: j.spec.weight_auto,
                    queued_at: j.spec.queued_at,
                    subscribers: j.subscribers,
                    reason: cause.reason(),
                    cause,
                    detail: wait.and_then(|w| w.detail),
                    rank: pos + 1,
                    expected_ms: j.spec.expected_ms,
                    memory_expected: plan.expected.get(pos).copied(),
                    shadow_at: plan.shadow.filter(|_| plan.reserved.as_ref() == Some(id)),
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
