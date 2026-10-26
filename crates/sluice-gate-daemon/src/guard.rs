//! The Sluice-side memory guard (ADR-0019 piece 7, opt-in:
//! `[admission] memory_guard = true`).
//!
//! Where `systemd-oomd` is absent or too old to order its victims, nothing
//! stops the kernel's global OOM killer from picking an agent's browser while
//! Sluice's own gate jobs hold most of the memory. The guard is the last line
//! Sluice owns: under real exhaustion it cancels **its own** running job with
//! the largest working set, through the normal cancel path (process group
//! kill), reported as `killed_by = "memory"`. It never touches anything but
//! gate and `exec` jobs.
//!
//! [`Guard::observe`] is a pure decision over one sample per tick (no clock,
//! no file; the caller passes the time): the machine's `MemAvailable` and reserve, the `full avg10`
//! memory pressure, and the running jobs' working sets (already sampled for
//! admission). Trouble needs all of:
//!
//! - `MemAvailable` below the reserve (the room left for the desktop and the
//!   agents is gone);
//! - the jobs' working sets together above the reserve (the gates are what
//!   holds the memory: a shortage the agents cause is none of the guard's
//!   business, and a job alone below the reserve is never touched);
//! - real stall: `full avg10` at or above [`PRESSURE_FULL_AVG10`], or
//!   `MemAvailable` below half the reserve (the machine is about to thrash even
//!   if the average has not caught up).
//!
//! Trouble must persist for [`TROUBLE_MS`] of wall time (from the first
//! troubled sample, at least [`MIN_TROUBLED_SAMPLES`] of them, whatever the
//! sampling period) before the guard chooses the victim; then it rests for [`COOLDOWN_MS`] (the memory of a killed job takes a
//! moment to come back, like `systemd-oomd`'s post-action delay).

use std::path::Path;

/// Wall time trouble must persist before the guard acts, milliseconds,
/// counted from the first troubled sample of the run (independent of the
/// sampling period, `psi_poll_interval`).
pub const TROUBLE_MS: u64 = 3_000;
/// Troubled samples the run must also hold: one lone sample, however late,
/// never acts.
pub const MIN_TROUBLED_SAMPLES: u32 = 2;
/// Rest after an action, milliseconds (`systemd-oomd`'s post-action delay).
pub const COOLDOWN_MS: u64 = 15_000;
/// `full avg10` (percent) that counts as real memory stall.
pub const PRESSURE_FULL_AVG10: f64 = 10.0;

/// One running gate or `exec` job and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardJob {
    /// Job id.
    pub id: String,
    /// Working set at the last sample (`anon` + swap of its leaf, or the
    /// summed resident set), bytes.
    pub working_set: u64,
}

/// What one second of the machine looks like.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// `MemAvailable`, bytes.
    pub available: u64,
    /// `[admission] memory_reserve` resolved, bytes.
    pub reserve: u64,
    /// `full avg10` of the daemon scope's `memory.pressure` (the system's
    /// when the scope has none); `None` when unreadable.
    pub full_avg10: Option<f64>,
}

/// The job the guard chose, with the figures behind the choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Victim {
    /// Job id.
    pub id: String,
    /// Its working set, bytes.
    pub working_set: u64,
    /// `MemAvailable`, bytes.
    pub available: u64,
    /// The reserve, bytes.
    pub reserve: u64,
}

/// The guard's state: the run of troubled samples (when it began, how many),
/// and when the guard may act again.
#[derive(Debug, Clone)]
pub struct Guard {
    enabled: bool,
    /// Time of the first troubled sample of the current run.
    since_ms: Option<u64>,
    troubled: u32,
    quiet_until_ms: u64,
}

/// Whether `sample` and `jobs` are in trouble (see the module documentation).
#[must_use]
pub fn in_trouble(sample: &Sample, jobs: &[GuardJob]) -> bool {
    let held = jobs
        .iter()
        .fold(0_u64, |sum, j| sum.saturating_add(j.working_set));
    let short = sample.available < sample.reserve;
    let gates_hold_it = held > sample.reserve;
    let stalled = sample.full_avg10.is_some_and(|p| p >= PRESSURE_FULL_AVG10)
        || sample.available < sample.reserve / 2;
    short && gates_hold_it && stalled
}

impl Guard {
    /// A guard; a disabled one never chooses anything.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            since_ms: None,
            troubled: 0,
            quiet_until_ms: 0,
        }
    }

    fn reset(&mut self) {
        self.since_ms = None;
        self.troubled = 0;
    }

    /// Consecutive troubled samples so far.
    #[must_use]
    pub fn troubled(&self) -> u32 {
        self.troubled
    }

    /// Takes one sample at `now_ms`; returns the job to cancel when the
    /// samples were troubled without a break for [`TROUBLE_MS`] (and at least
    /// [`MIN_TROUBLED_SAMPLES`] of them) and the guard is not resting. The victim is the job with the largest working set (the
    /// smallest id on a tie, so the choice is deterministic).
    pub fn observe(&mut self, now_ms: u64, sample: Sample, jobs: &[GuardJob]) -> Option<Victim> {
        if !self.enabled || now_ms < self.quiet_until_ms {
            // While resting nothing is counted: the next action needs a fresh
            // run of troubled samples once the memory of the victim is back.
            self.reset();
            return None;
        }
        if !in_trouble(&sample, jobs) {
            self.reset();
            return None;
        }
        let since = *self.since_ms.get_or_insert(now_ms);
        self.troubled += 1;
        if now_ms.saturating_sub(since) < TROUBLE_MS || self.troubled < MIN_TROUBLED_SAMPLES {
            return None;
        }
        let biggest = jobs
            .iter()
            .max_by(|a, b| a.working_set.cmp(&b.working_set).then(b.id.cmp(&a.id)))?;
        self.reset();
        self.quiet_until_ms = now_ms.saturating_add(COOLDOWN_MS);
        Some(Victim {
            id: biggest.id.clone(),
            working_set: biggest.working_set,
            available: sample.available,
            reserve: sample.reserve,
        })
    }
}

/// The message of a job the guard cancelled (the same words clients print for
/// a kill by the kernel, `killed_by = "memory"`).
#[must_use]
pub fn cancel_message(job: &str, v: &Victim) -> String {
    use crate::memkill::format_bytes;
    format!(
        "{job} was cancelled by Sluice for lack of memory: job working set {}, MemAvailable {} \
         below the reserve {} (the memory guard, [admission] memory_guard); running it again \
         unchanged is likely to end the same way: lower its concurrency or free memory first",
        format_bytes(v.working_set),
        format_bytes(v.available),
        format_bytes(v.reserve)
    )
}

/// `avg10` of the `full` line of a pressure file (`full avg10=3.00 ...`).
#[must_use]
pub fn parse_full_avg10(text: &str) -> Option<f64> {
    let line = text
        .lines()
        .find(|l| l.split_whitespace().next() == Some("full"))?;
    line.split_whitespace()
        .find_map(|f| f.strip_prefix("avg10=")?.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// Reads `full avg10` of the pressure file `path`.
#[must_use]
pub fn read_full_avg10(path: &Path) -> Option<f64> {
    parse_full_avg10(&std::fs::read_to_string(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn job(id: &str, gib: u64) -> GuardJob {
        GuardJob {
            id: id.to_owned(),
            working_set: gib * GIB,
        }
    }

    /// 3 GiB reserve, `available` GiB free, `full avg10` as given.
    fn sample(available_mib: u64, full: Option<f64>) -> Sample {
        Sample {
            available: available_mib << 20,
            reserve: 3 * GIB,
            full_avg10: full,
        }
    }

    /// Feeds `n` one-second samples from `start_ms`.
    fn feed(g: &mut Guard, start_ms: u64, n: u64, s: Sample, jobs: &[GuardJob]) -> Vec<Victim> {
        (0..n)
            .filter_map(|i| g.observe(start_ms + i * 1000, s, jobs))
            .collect()
    }

    #[test]
    fn three_seconds_of_trouble_choose_the_largest_job() {
        let mut g = Guard::new(true);
        let jobs = [job("small", 2), job("big", 9), job("mid", 4)];
        let s = sample(1024, Some(40.0));
        assert_eq!(g.observe(0, s, &jobs), None);
        assert_eq!(g.observe(1000, s, &jobs), None);
        assert_eq!(g.observe(2000, s, &jobs), None);
        assert_eq!(g.troubled(), 3);
        let v = g.observe(3000, s, &jobs).expect("3 s of trouble act");
        assert_eq!(v.id, "big");
        assert_eq!(v.working_set, 9 * GIB);
        assert_eq!((v.available, v.reserve), (GIB, 3 * GIB));
        assert_eq!(g.troubled(), 0);
    }

    #[test]
    fn one_calm_sample_resets_the_count() {
        let mut g = Guard::new(true);
        let jobs = [job("a", 8)];
        let bad = sample(1024, Some(40.0));
        let calm = sample(8 * 1024, Some(40.0));
        assert_eq!(feed(&mut g, 0, 3, bad, &jobs), Vec::new());
        assert_eq!(g.observe(3000 - 1, calm, &jobs), None);
        assert_eq!(g.troubled(), 0);
        // The run starts over from its new first sample.
        assert_eq!(feed(&mut g, 3000, 3, bad, &jobs), Vec::new());
        assert_eq!(feed(&mut g, 6000, 1, bad, &jobs).len(), 1);
    }

    #[test]
    fn each_condition_is_needed() {
        let jobs = [job("a", 8)];
        // plenty available: no trouble however high the pressure
        assert!(!in_trouble(&sample(4 * 1024, Some(90.0)), &jobs));
        // low but not stalled: neither pressure nor under half the reserve
        assert!(!in_trouble(&sample(2 * 1024, Some(9.9)), &jobs));
        assert!(!in_trouble(&sample(2 * 1024, None), &jobs));
        // pressure at the threshold counts, with the gates above the reserve
        assert!(in_trouble(&sample(2 * 1024, Some(10.0)), &jobs));
        // no pressure reading: under half the reserve is enough
        assert!(in_trouble(&sample(1024, None), &jobs));
        assert!(
            !in_trouble(&sample(1536, None), &jobs),
            "half is not under half"
        );
        // the gates do not hold the memory: the agents cause the shortage
        assert!(!in_trouble(&sample(512, Some(90.0)), &[job("a", 2)]));
        assert!(!in_trouble(&sample(512, Some(90.0)), &[]));
    }

    #[test]
    fn a_single_job_below_the_reserve_is_never_touched() {
        let mut g = Guard::new(true);
        // exactly the reserve is not above it
        let jobs = [job("only", 3)];
        let s = sample(100, Some(95.0));
        assert_eq!(feed(&mut g, 0, 30, s, &jobs), Vec::new());
        // several jobs that together exceed the reserve are fair game
        let jobs = [job("a", 2), job("b", 2)];
        assert_eq!(feed(&mut g, 100_000, 4, s, &jobs).len(), 1);
    }

    #[test]
    fn a_single_job_above_the_reserve_is_chosen() {
        let mut g = Guard::new(true);
        let jobs = [job("only", 10)];
        let victims = feed(&mut g, 0, 4, sample(512, Some(50.0)), &jobs);
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0].id, "only");
    }

    #[test]
    fn after_an_action_the_guard_rests_for_the_cooldown() {
        let mut g = Guard::new(true);
        let jobs = [job("a", 9), job("b", 6)];
        let s = sample(512, Some(50.0));
        assert_eq!(feed(&mut g, 0, 4, s, &jobs).len(), 1, "acts at 3000 ms");
        // Still in trouble for the whole cooldown: nothing, and nothing counted.
        assert_eq!(feed(&mut g, 4000, 14, s, &jobs), Vec::new());
        assert_eq!(g.troubled(), 0);
        // 3000 + 15000: rested; a fresh 3 s run is needed.
        assert_eq!(g.observe(18_000, s, &jobs), None);
        assert_eq!(g.observe(19_000, s, &jobs), None);
        assert_eq!(g.observe(20_000, s, &jobs), None);
        let again = g.observe(21_000, s, &jobs).expect("acts again");
        assert_eq!(again.id, "a", "the guard chooses again among what runs");
        // The victim is gone by then: the next one is chosen
        let rest = [job("b", 6)];
        assert_eq!(feed(&mut g, 22_000, 14, s, &rest), Vec::new());
        assert_eq!(feed(&mut g, 36_000, 4, s, &rest).len(), 1);
    }

    /// The 3 s are wall time, not a sample count: any sampling period.
    #[test]
    fn the_wait_is_wall_time_whatever_the_sampling_period() {
        let jobs = [job("a", 8)];
        let s = sample(1024, Some(40.0));
        // A fast tick (200 ms): acts at 3 s, not after a few samples.
        let mut g = Guard::new(true);
        let acted_at = (0..40)
            .map(|i| i * 200)
            .find(|&t| g.observe(t, s, &jobs).is_some());
        assert_eq!(acted_at, Some(3_000));
        let mut g = Guard::new(true);
        assert_eq!(feed(&mut g, 0, 1, s, &jobs), Vec::new());
        assert_eq!(g.observe(2_999, s, &jobs), None, "2999 ms is not enough");
        assert!(g.observe(3_000, s, &jobs).is_some());
        // A slow tick (5 s): the second troubled sample is already 5 s on.
        let mut g = Guard::new(true);
        assert_eq!(g.observe(0, s, &jobs), None, "one sample never acts");
        assert!(g.observe(5_000, s, &jobs).is_some());
        // A lone sample long after a calm one is still just one sample.
        let mut g = Guard::new(true);
        assert_eq!(g.observe(60_000, s, &jobs), None);
    }

    #[test]
    fn ties_choose_the_smallest_id() {
        let mut g = Guard::new(true);
        let jobs = [job("z", 5), job("a", 5), job("m", 5)];
        let victims = feed(&mut g, 0, 4, sample(512, Some(50.0)), &jobs);
        assert_eq!(victims[0].id, "a");
    }

    #[test]
    fn a_disabled_guard_never_chooses_anything() {
        let mut g = Guard::new(false);
        let jobs = [job("a", 20)];
        assert_eq!(
            feed(&mut g, 0, 100, sample(0, Some(100.0)), &jobs),
            Vec::new()
        );
        assert_eq!(g.troubled(), 0);
    }

    #[test]
    fn no_jobs_means_no_trouble() {
        let mut g = Guard::new(true);
        assert_eq!(feed(&mut g, 0, 10, sample(0, Some(100.0)), &[]), Vec::new());
    }

    #[test]
    fn the_message_names_the_figures() {
        let v = Victim {
            id: "j".into(),
            working_set: 18 * GIB,
            available: GIB,
            reserve: 3 * GIB,
        };
        let m = cancel_message("gate test", &v);
        assert!(
            m.starts_with(
                "gate test was cancelled by Sluice for lack of memory: job working set 18.0 GiB, \
                 MemAvailable 1.0 GiB below the reserve 3.0 GiB"
            ),
            "{m}"
        );
    }

    #[test]
    fn pressure_files_parse_the_full_line() {
        let text = "some avg10=1.50 avg60=0.00 avg300=0.00 total=1\n\
                    full avg10=12.25 avg60=3.00 avg300=1.00 total=9\n";
        assert_eq!(parse_full_avg10(text), Some(12.25));
        assert_eq!(parse_full_avg10("some avg10=7.00 avg60=0\n"), None);
        assert_eq!(parse_full_avg10("full avg60=1.0\n"), None);
        assert_eq!(parse_full_avg10("full avg10=nan\n"), None);
        assert_eq!(parse_full_avg10(""), None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.pressure");
        assert_eq!(read_full_avg10(&path), None);
        std::fs::write(&path, text).unwrap();
        assert_eq!(read_full_avg10(&path), Some(12.25));
    }
}
