//! The shadow time of a reserved head and the room left beside it (ADR-0023).
//!
//! Pure functions over the jobs that hold budget and memory: no clocks, no
//! snapshots. [`super::Pass`] builds the [`Holder`] list from the running jobs
//! and the jobs it has started, asks [`shadow`] when the reserved head could
//! start, and [`room`] how much a job started now may take without delaying it.

use std::path::Path;

use super::DEFAULT_EXPECTED_MS;

/// Added to `0.1 x expected` for a job past its expectation, milliseconds.
const OVERRUN_PAD_MS: u64 = 2_000;
/// Added to the expected time of a candidate for the duration rule.
const BOUND_PAD_MS: u64 = 10_000;

/// Which rule let a job start before the reserved head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillRule {
    /// The head still fits at its shadow time beside the job, whatever the
    /// job's duration.
    ExtraResources,
    /// The job is expected to end before the shadow time.
    EndsInTime,
}

impl BackfillRule {
    /// The spelling used in the log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExtraResources => "extra resources",
            Self::EndsInTime => "ends in time",
        }
    }
}

/// A job holding budget units and memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Holder<'a> {
    /// Repository the job runs in (for the repository cap).
    pub repo: &'a Path,
    /// Budget units held.
    pub weight: u32,
    /// Predicted end, unix milliseconds ([`predicted_end`]).
    pub end: u64,
    /// Memory handed back when it ends, bytes: what it holds now plus what it
    /// is still expected to grow by (the admission accounting of ADR-0019).
    pub memory: u64,
    /// It started behind a reserved head: it counts for the whole interval.
    pub backfilled: bool,
}

/// What the reserved head needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Head<'a> {
    pub repo: &'a Path,
    pub weight: u32,
    /// Cap of its repository, if any.
    pub repo_cap: Option<u32>,
    /// Expected peak, bytes; `None` with memory-aware admission off.
    pub peak: Option<u64>,
}

/// The machine at the pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Machine {
    pub budget: u32,
    /// Memory a new job may still take now (`MemPass::free`); ignored with
    /// the rule off.
    pub free: u64,
}

/// When the head could start and which holders have ended by then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Shadow {
    /// Unix milliseconds; `now` when the head already fits.
    pub at: u64,
    /// Parallel to the holders: released by `at`.
    pub released: Vec<bool>,
}

/// When a running job is predicted to end: `started_at + expected`, with the
/// pessimistic remainder `0.1 x expected + 2 s` once it is past its
/// expectation (`max(expected - elapsed, 0.1 x expected + 2 s)`). A job with
/// no history is taken to run [`DEFAULT_EXPECTED_MS`]; that guess only feeds
/// this prediction, never a candidate's bound.
pub(super) fn predicted_end(now: u64, started_at: u64, expected_ms: Option<u64>) -> u64 {
    let expected = expected_ms.unwrap_or(DEFAULT_EXPECTED_MS);
    let elapsed = now.saturating_sub(started_at);
    let remaining = if elapsed < expected {
        expected - elapsed
    } else {
        expected / 10 + OVERRUN_PAD_MS
    };
    now.saturating_add(remaining)
}

/// The head fits the budget, its repository cap and memory once the holders
/// flagged in `released` are gone; with none left it always fits memory (the
/// progress exemption of admission).
fn fits(holders: &[Holder<'_>], released: &[bool], head: &Head<'_>, m: &Machine) -> bool {
    let live = || {
        holders
            .iter()
            .zip(released)
            .filter(|(_, gone)| !**gone)
            .map(|(h, _)| h)
    };
    let used: u32 = live().map(|h| h.weight).sum();
    let repo_used: u32 = live()
        .filter(|h| h.repo == head.repo)
        .map(|h| h.weight)
        .sum();
    let freed: u64 = holders
        .iter()
        .zip(released)
        .filter(|(_, gone)| **gone)
        .map(|(h, _)| h.memory)
        .sum();
    used + head.weight <= m.budget
        && head
            .repo_cap
            .is_none_or(|cap| repo_used + head.weight <= cap)
        && head
            .peak
            .is_none_or(|p| live().next().is_none() || p <= m.free.saturating_add(freed))
}

/// The earliest time the head fits: the holders are released in order of
/// predicted end until it fits budget, cap and memory.
pub(super) fn shadow(now: u64, holders: &[Holder<'_>], head: &Head<'_>, m: &Machine) -> Shadow {
    let mut order: Vec<usize> = (0..holders.len()).collect();
    order.sort_by_key(|&i| (holders[i].end, i));
    let mut released = vec![false; holders.len()];
    let mut at = now;
    for i in order {
        if fits(holders, &released, head, m) {
            break;
        }
        released[i] = true;
        at = at.max(holders[i].end);
    }
    Shadow { at, released }
}

/// What a job started now may take without delaying the head at its shadow
/// time (rule "extra resources"): what is left of the budget and the memory
/// once the head and every job that is still running then or started behind
/// the reservation are counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Room<'a> {
    /// Budget units left.
    pub weight: u32,
    /// Memory left, bytes; `None` with the rule off.
    pub memory: Option<u64>,
    /// The head fits at its shadow time beside the jobs counted so far. It
    /// does not when it needs the progress exemption (an empty machine) or
    /// when jobs started behind it already take its room.
    pub open: bool,
    /// The head's repository when it has a cap: no job of it takes room.
    pub capped: Option<&'a Path>,
}

/// The [`Room`] beside a head at its `shadow`.
pub(super) fn room<'a>(
    holders: &[Holder<'_>],
    shadow: &Shadow,
    head: &Head<'a>,
    m: &Machine,
) -> Room<'a> {
    let counted = |h: &Holder<'_>, gone: bool| !gone || h.backfilled;
    let survivors: u32 = holders
        .iter()
        .zip(&shadow.released)
        .filter(|(h, gone)| counted(h, **gone))
        .map(|(h, _)| h.weight)
        .sum();
    let freed: u64 = holders
        .iter()
        .zip(&shadow.released)
        .filter(|(h, gone)| **gone && !h.backfilled)
        .map(|(h, _)| h.memory)
        .sum();
    let free_then = m.free.saturating_add(freed);
    Room {
        weight: m
            .budget
            .saturating_sub(survivors.saturating_add(head.weight)),
        memory: head.peak.map(|p| free_then.saturating_sub(p)),
        open: head.peak.is_none_or(|p| p <= free_then),
        capped: head.repo_cap.map(|_| head.repo),
    }
}

impl Room<'_> {
    /// A job of `weight` units and `peak` bytes (`None` with the rule off) of
    /// `repo` fits beside the head for the whole interval.
    pub fn admits(&self, repo: &Path, weight: u32, peak: Option<u64>) -> bool {
        self.open
            && weight <= self.weight
            && self.capped != Some(repo)
            && self.memory.zip(peak).is_none_or(|(room, p)| p <= room)
    }

    /// The job started: its units and memory are taken for the whole
    /// interval, whichever rule let it start.
    pub fn take(&mut self, weight: u32, peak: Option<u64>) {
        self.weight = self.weight.saturating_sub(weight);
        self.memory = self
            .memory
            .map(|room| room.saturating_sub(peak.unwrap_or(0)));
    }
}

/// The duration rule: the job's learned duration, as
/// `max(1.5 x expected, expected + 10 s)`, ends before the shadow time. A job
/// with no learned duration never passes (`DEFAULT_EXPECTED_MS` is no bound).
pub(super) fn ends_in_time(now: u64, expected_ms: Option<u64>, shadow_at: u64) -> bool {
    expected_ms.is_some_and(|e| {
        let bound = e.saturating_add(e / 2).max(e.saturating_add(BOUND_PAD_MS));
        now.saturating_add(bound) <= shadow_at
    })
}
