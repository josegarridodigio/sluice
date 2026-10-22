//! Pure admission logic: budget, backfill, head-of-line reservation, per-repo
//! caps and request coalescing (see `kb/sluice/design/scheduler.md` and
//! ADR-0007).
//!
//! No I/O, no clocks, no async: callers pass `now` (unix milliseconds) and the
//! current PSI verdict, so every decision is deterministic and testable.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use sluice_gate_core::key::CacheKey;
use sluice_gate_proto::WaitReason;

/// What the scheduler needs to know about one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// Unique job id.
    pub id: String,
    /// Cache key; `None` means the job is never coalesced.
    pub key: Option<CacheKey>,
    /// Repository toplevel the job runs in (used for the per-repo cap).
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Budget units the job needs (clamped to `1..=budget` at enqueue).
    pub weight: u32,
    /// Time the job was queued, unix milliseconds.
    pub queued_at: u64,
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
    /// Repository toplevel.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Budget units held.
    pub weight: u32,
    /// Start time, unix milliseconds.
    pub started_at: u64,
    /// Current subscribers.
    pub subscribers: usize,
}

/// A queued job as shown by `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedView {
    /// Job id.
    pub id: String,
    /// Repository toplevel.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Budget units needed.
    pub weight: u32,
    /// Queue time, unix milliseconds.
    pub queued_at: u64,
    /// Current subscribers.
    pub subscribers: usize,
    /// Why the job is not running yet.
    pub reason: WaitReason,
}

/// One job tracked by the scheduler.
#[derive(Debug, Clone)]
struct Job {
    spec: JobSpec,
    subscribers: usize,
    subscribers_max: usize,
    started_at: u64,
}

impl Job {
    fn new(spec: JobSpec) -> Self {
        Self {
            spec,
            subscribers: 1,
            subscribers_max: 1,
            started_at: 0,
        }
    }
}

/// Budget scheduler with FIFO + backfill + head-of-line reservation.
#[derive(Debug)]
pub struct Scheduler {
    budget: u32,
    head_reserve_after_ms: u64,
    repo_caps: BTreeMap<PathBuf, u32>,
    queue: VecDeque<Job>,
    running: Vec<Job>,
}

impl Scheduler {
    /// Creates a scheduler with `budget` units (clamped to at least 1), the
    /// head reservation threshold in milliseconds and the per-repository caps.
    pub fn new(budget: u32, head_reserve_after_ms: u64, repo_caps: BTreeMap<PathBuf, u32>) -> Self {
        Self {
            budget: budget.max(1),
            head_reserve_after_ms,
            repo_caps,
            queue: VecDeque::new(),
            running: Vec::new(),
        }
    }

    /// Total budget units.
    pub fn budget(&self) -> u32 {
        self.budget
    }

    /// Units held by running jobs.
    pub fn used(&self) -> u32 {
        self.running.iter().map(|j| j.spec.weight).sum()
    }

    /// Adds a request. Weight is clamped to `1..=min(budget, repo cap)` (a
    /// warning is logged once per clamped job). If `spec.key` is
    /// `Some` and a queued or running job has the same key, the request is
    /// attached to it (subscribers += 1) and [`Enqueued::Coalesced`] is
    /// returned. Otherwise the job goes to the back of the queue with one
    /// subscriber and [`Enqueued::Created`] is returned.
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
                budget = self.budget,
                "gate weight exceeds the budget or the repository cap; clamped"
            );
        }
        spec.weight = spec.weight.clamp(1, max);
        self.queue.push_back(Job::new(spec));
        Enqueued::Created {
            position: self.queue.len(),
        }
    }

    /// Runs the admission loop (oldest first). A job fits when
    /// `used + weight <= budget`, its repository cap holds and PSI is not
    /// blocking. Lighter jobs behind the head backfill while the head has
    /// waited less than `head_reserve_after`; after that nothing behind the
    /// head starts until it fits. Returns the ids moved to running, in queue
    /// order, with `started_at = now`.
    pub fn admit(&mut self, now: u64, psi_blocking: bool) -> Vec<String> {
        let mut started = Vec::new();
        if psi_blocking {
            return started;
        }
        let mut remaining = VecDeque::with_capacity(self.queue.len());
        let mut reserved = false;
        while let Some(mut job) = self.queue.pop_front() {
            let is_head = remaining.is_empty();
            if reserved {
                remaining.push_back(job);
                continue;
            }
            if self.fits_budget(&job) && self.fits_repo_cap(&job) {
                job.started_at = now;
                started.push(job.spec.id.clone());
                self.running.push(job);
                continue;
            }
            // Only a head blocked by the budget reserves: a head blocked by its
            // repository cap alone must not stop other repositories.
            if is_head && !self.fits_budget(&job) && self.head_waited_too_long(&job, now) {
                reserved = true;
            }
            remaining.push_back(job);
        }
        self.queue = remaining;
        started
    }

    /// Removes a running job and returns its spec; `None` if not running.
    pub fn finish(&mut self, id: &str) -> Option<JobSpec> {
        let idx = self.running.iter().position(|j| j.spec.id == id)?;
        Some(self.running.remove(idx).spec)
    }

    /// Removes a queued job (cancelled before start); `None` if not queued.
    pub fn remove_queued(&mut self, id: &str) -> Option<JobSpec> {
        let idx = self.queue.iter().position(|j| j.spec.id == id)?;
        self.queue.remove(idx).map(|j| j.spec)
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

    /// 1-based position in the queue; `None` if running or unknown.
    pub fn position(&self, id: &str) -> Option<usize> {
        self.queue
            .iter()
            .position(|j| j.spec.id == id)
            .map(|i| i + 1)
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
    /// jobs in queue order, each with the reason it is waiting: `Psi` when PSI
    /// blocks, `RepoCap` when only the repository cap blocks it,
    /// `HeadReserved` when only the reservation blocks it, `Budget` otherwise.
    pub fn snapshot(&self, now: u64, psi_blocking: bool) -> (Vec<RunningView>, Vec<QueuedView>) {
        let mut running: Vec<RunningView> = self
            .running
            .iter()
            .map(|j| RunningView {
                id: j.spec.id.clone(),
                repo: j.spec.repo.clone(),
                gate: j.spec.gate.clone(),
                weight: j.spec.weight,
                started_at: j.started_at,
                subscribers: j.subscribers,
            })
            .collect();
        running.sort_by_key(|v| v.started_at);

        let reserved = self
            .queue
            .front()
            .is_some_and(|head| !self.fits_budget(head) && self.head_waited_too_long(head, now));
        let queued = self
            .queue
            .iter()
            .enumerate()
            .map(|(i, j)| {
                let reason = if psi_blocking {
                    WaitReason::Psi
                } else if !self.fits_budget(j) {
                    WaitReason::Budget
                } else if !self.fits_repo_cap(j) {
                    WaitReason::RepoCap
                } else if reserved && i > 0 {
                    WaitReason::HeadReserved
                } else {
                    WaitReason::Budget
                };
                QueuedView {
                    id: j.spec.id.clone(),
                    repo: j.spec.repo.clone(),
                    gate: j.spec.gate.clone(),
                    weight: j.spec.weight,
                    queued_at: j.spec.queued_at,
                    subscribers: j.subscribers,
                    reason,
                }
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
    fn max_weight(&self, repo: &std::path::Path) -> u32 {
        self.repo_caps
            .get(repo)
            .map_or(self.budget, |&cap| cap.min(self.budget))
            .max(1)
    }

    fn fits_budget(&self, job: &Job) -> bool {
        self.used() + job.spec.weight <= self.budget
    }

    fn fits_repo_cap(&self, job: &Job) -> bool {
        let Some(&cap) = self.repo_caps.get(&job.spec.repo) else {
            return true;
        };
        let repo_used: u32 = self
            .running
            .iter()
            .filter(|j| j.spec.repo == job.spec.repo)
            .map(|j| j.spec.weight)
            .sum();
        repo_used + job.spec.weight <= cap
    }

    fn head_waited_too_long(&self, head: &Job, now: u64) -> bool {
        now.saturating_sub(head.spec.queued_at) >= self.head_reserve_after_ms
    }
}

#[cfg(test)]
mod tests {
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
            gate: "test".into(),
            weight,
            queued_at,
        }
    }

    fn sched(budget: u32) -> Scheduler {
        Scheduler::new(budget, RESERVE, BTreeMap::new())
    }

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
}
