//! Once-per-tick measurements (ADR-0011 L1 and M2): CPU utilisation of the
//! daemon's CPUs from `/proc/stat` (the `idle_while_waiting_ms` integral and
//! the idle CPUs of over-admission), and the CPU demand of running gate jobs
//! from `/proc/<pid>/task/<tid>/schedstat`.
//!
//! Readers take the proc root (normally `/proc`) so tests use a fake tree;
//! the accumulators are pure (callers pass the samples and `now`). Design:
//! `kb/sluice/design/scheduler-throughput.md` sections 4 and 7.1.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

/// Clock ticks per second of `/proc/<pid>/stat` times (`USER_HZ`, 100 on
/// every Linux architecture Sluice supports).
pub const USER_HZ: u64 = 100;

/// Cumulative idle and total time of one CPU (`/proc/stat`, in ticks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// `idle + iowait`.
    pub idle: u64,
    /// `user + nice + system + idle + iowait + irq + softirq + steal`
    /// (`guest` and `guest_nice` are already inside `user` and `nice`).
    pub total: u64,
}

/// Parses the per-CPU lines (`cpuN ...`) of `/proc/stat`, keeping the CPUs
/// in `allowed` (all when `None` or when none of them is listed).
pub fn parse_proc_stat(text: &str, allowed: Option<&BTreeSet<u32>>) -> BTreeMap<u32, CpuTimes> {
    let mut all = BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(n) = fields
            .next()
            .and_then(|f| f.strip_prefix("cpu"))
            .filter(|n| !n.is_empty())
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let v: Vec<u64> = fields.take(8).filter_map(|f| f.parse().ok()).collect();
        if v.len() < 5 {
            continue;
        }
        all.insert(
            n,
            CpuTimes {
                idle: v[3] + v[4],
                total: v.iter().sum(),
            },
        );
    }
    match allowed {
        Some(set) if all.keys().any(|k| set.contains(k)) => {
            all.retain(|k, _| set.contains(k));
            all
        }
        _ => all,
    }
}

/// Reads `<proc>/stat` for the CPUs in `allowed`.
pub fn read_proc_stat(
    proc_root: &Path,
    allowed: Option<&BTreeSet<u32>>,
) -> Option<BTreeMap<u32, CpuTimes>> {
    let text = std::fs::read_to_string(proc_root.join("stat")).ok()?;
    let times = parse_proc_stat(&text, allowed);
    (!times.is_empty()).then_some(times)
}

/// The utilisation sampler: idle CPUs and the `idle_while_waiting_ms`
/// integral (ADR-0011 section 7.1).
#[derive(Debug, Clone, Default)]
pub struct Utilisation {
    prev: Option<(BTreeMap<u32, CpuTimes>, u64)>,
    /// Whether the queue was non-empty at the previous sample.
    queue_was_nonempty: bool,
    /// Idle CPUs of the last two samples.
    recent: VecDeque<f64>,
    idle_while_waiting: f64,
    waiting_wall_ms: u64,
    cpus: usize,
}

impl Utilisation {
    /// Feeds one `/proc/stat` reading taken at `now_ms` and whether jobs
    /// are queued now. The queue state before the sample decides whether the
    /// elapsed interval counts as waiting time.
    #[allow(clippy::cast_precision_loss, reason = "tick and ms counts")]
    pub fn observe(
        &mut self,
        times: Option<BTreeMap<u32, CpuTimes>>,
        now_ms: u64,
        queue_nonempty: bool,
    ) {
        let Some(times) = times else {
            self.queue_was_nonempty = queue_nonempty;
            return;
        };
        if let Some((prev, at)) = &self.prev {
            let dt = now_ms.saturating_sub(*at);
            let (mut d_idle, mut d_total) = (0u64, 0u64);
            for (cpu, t) in &times {
                if let Some(p) = prev.get(cpu) {
                    d_idle += t.idle.saturating_sub(p.idle);
                    d_total += t.total.saturating_sub(p.total);
                }
            }
            let idle_frac = (d_total > 0).then(|| (d_idle as f64 / d_total as f64).clamp(0.0, 1.0));
            if let Some(f) = idle_frac {
                self.recent.push_back(f * times.len() as f64);
                while self.recent.len() > 2 {
                    self.recent.pop_front();
                }
            }
            if self.queue_was_nonempty {
                self.waiting_wall_ms += dt;
                self.idle_while_waiting += dt as f64 * idle_frac.unwrap_or(0.0);
            }
        }
        self.cpus = times.len();
        self.prev = Some((times, now_ms));
        self.queue_was_nonempty = queue_nonempty;
    }

    /// Mean idle CPUs over the last two samples; `None` before two readings.
    #[allow(clippy::cast_precision_loss, reason = "at most two samples")]
    pub fn idle_cpus(&self) -> Option<f64> {
        (!self.recent.is_empty())
            .then(|| self.recent.iter().sum::<f64>() / self.recent.len() as f64)
    }

    /// CPUs sampled (the daemon's affinity mask); 0 before the first sample.
    pub fn cpus(&self) -> usize {
        self.cpus
    }

    /// Integral of the idle fraction while jobs were queued, milliseconds.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "non-negative, far below u64::MAX"
    )]
    pub fn idle_while_waiting_ms(&self) -> u64 {
        self.idle_while_waiting.round() as u64
    }

    /// Wall time with at least one job queued, milliseconds.
    pub fn waiting_wall_ms(&self) -> u64 {
        self.waiting_wall_ms
    }
}

/// One task (thread) of a process in a gate's process group: thread id,
/// start time (ticks since boot, against tid reuse) and the cumulative CPU
/// time and run-queue wait in nanoseconds (`schedstat` fields 1 and 2;
/// `None` without `CONFIG_SCHED_INFO`).
pub type TaskReading = (u32, u64, Option<(u64, u64)>);

/// Fields after the `(comm)` of a `/proc/<pid>/stat` line (field 3 is
/// index 0).
fn stat_fields(stat: &str) -> Option<Vec<&str>> {
    let rest = &stat[stat.rfind(')')? + 1..];
    Some(rest.split_whitespace().collect())
}

/// The process group of each process under `proc_root` whose group is in
/// `pgids`: `pgid -> pids`.
pub fn group_members<S: std::hash::BuildHasher>(
    proc_root: &Path,
    pgids: &HashSet<i32, S>,
) -> HashMap<i32, Vec<u32>> {
    let mut out: HashMap<i32, Vec<u32>> = HashMap::new();
    if pgids.is_empty() {
        return out;
    }
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // Field 5 is the process group (index 2 after the comm).
        let Some(pgrp) =
            stat_fields(&stat).and_then(|f| f.get(2).and_then(|v| v.parse::<i32>().ok()))
        else {
            continue;
        };
        if pgids.contains(&pgrp) {
            out.entry(pgrp).or_default().push(pid);
        }
    }
    out
}

/// The tasks of `pid` with their `schedstat`; `None` for a task whose
/// `schedstat` is missing (kernels without `CONFIG_SCHED_INFO`).
pub fn read_tasks(proc_root: &Path, pid: u32) -> Vec<TaskReading> {
    let dir = proc_root.join(pid.to_string()).join("task");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        // Field 22 is the start time (index 19 after the comm).
        let start = std::fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|s| stat_fields(&s).and_then(|f| f.get(19).and_then(|v| v.parse().ok())))
            .unwrap_or(0);
        let sched = std::fs::read_to_string(entry.path().join("schedstat"))
            .ok()
            .and_then(|s| {
                let mut f = s.split_whitespace();
                Some((f.next()?.parse().ok()?, f.next()?.parse().ok()?))
            });
        out.push((tid, start, sched));
    }
    out
}

/// `utime + stime + cutime + cstime` of `pid` (fields 14–17), in ticks.
pub fn leader_ticks(proc_root: &Path, pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let f = stat_fields(&stat)?;
    // Fields 14..=17 are indices 11..=14 after the comm.
    (11..=14).try_fold(0u64, |acc, i| {
        let v: i64 = f.get(i)?.parse().ok()?;
        Some(acc + v.unsigned_abs())
    })
}

/// Measured demand of one finished job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Demand {
    /// CPU time, milliseconds (the larger of the sampled tasks and the
    /// leader's cumulative ticks; both are lower bounds).
    pub cpu_ms: u64,
    /// Run-queue wait, milliseconds; `None` without `schedstat`.
    pub run_delay_ms: Option<u64>,
    /// `(cpu + delay) / wall`, in milli-CPUs.
    pub demand_milli: u32,
}

/// Per-job accumulator of the task sampler (ADR-0011 section 4).
#[derive(Debug, Clone, Default)]
pub struct JobUsage {
    tasks: HashMap<(u32, u64), (u64, u64)>,
    cpu_ns: u64,
    delay_ns: u64,
    leader_ticks: u64,
    schedstat: bool,
    observed: u32,
    /// CPU `some avg10` below 10 when the job started (fallback check).
    pub calm_at_start: bool,
}

impl JobUsage {
    /// Adds one tick's samples (`(tid, start, schedstat)`) of every task of
    /// the job's group, and the leader's cumulative ticks.
    pub fn update(&mut self, tasks: &[TaskReading], leader: Option<u64>) {
        self.observed += 1;
        for &(tid, start, sched) in tasks {
            let Some((cpu, delay)) = sched else { continue };
            self.schedstat = true;
            let prev = self
                .tasks
                .insert((tid, start), (cpu, delay))
                .unwrap_or((0, 0));
            self.cpu_ns += cpu.saturating_sub(prev.0);
            self.delay_ns += delay.saturating_sub(prev.1);
        }
        if let Some(t) = leader {
            self.leader_ticks = self.leader_ticks.max(t);
        }
    }

    /// Mean runnable threads so far over `wall_ms`; `None` before a sample.
    #[allow(clippy::cast_precision_loss, reason = "ms counts")]
    pub fn demand_so_far(&self, wall_ms: u64) -> Option<f64> {
        if self.observed == 0 || wall_ms == 0 {
            return None;
        }
        let cpu_ms = (self.cpu_ns / 1_000_000).max(self.leader_ticks * 1000 / USER_HZ);
        Some((cpu_ms + self.delay_ns / 1_000_000) as f64 / wall_ms as f64)
    }

    /// The demand of the finished job over `wall_ms`. `None` when the job
    /// was never sampled (shorter than a tick), or when only the CPU
    /// fallback is available and the machine was not calm at both ends
    /// (`calm_at_end`), because CPU over wall falls under contention.
    pub fn finish(&self, wall_ms: u64, calm_at_end: bool) -> Option<Demand> {
        if self.observed == 0 || wall_ms == 0 {
            return None;
        }
        let cpu_ms = (self.cpu_ns / 1_000_000).max(self.leader_ticks * 1000 / USER_HZ);
        let delay_ms = self.schedstat.then_some(self.delay_ns / 1_000_000);
        if delay_ms.is_none() && !(self.calm_at_start && calm_at_end) {
            return None;
        }
        let milli = (cpu_ms + delay_ms.unwrap_or(0)).saturating_mul(1000) / wall_ms;
        Some(Demand {
            cpu_ms,
            run_delay_ms: delay_ms,
            demand_milli: u32::try_from(milli).unwrap_or(u32::MAX),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 0 100 800 0 0 0 0 0 0\n\
                        cpu0 50 0 50 400 0 0 0 0 0 0\n\
                        cpu1 50 0 50 400 0 0 0 0 0 0\n\
                        intr 1 2 3\nctxt 4\n";

    fn stat(idle: [u64; 2], busy: [u64; 2]) -> BTreeMap<u32, CpuTimes> {
        let text = format!(
            "cpu  0 0 0 0 0 0 0 0 0 0\ncpu0 {} 0 0 {} 0 0 0 0 0 0\ncpu1 {} 0 0 {} 0 0 0 0 0 0\n",
            busy[0], idle[0], busy[1], idle[1]
        );
        parse_proc_stat(&text, None)
    }

    #[test]
    fn parses_per_cpu_lines_and_the_affinity_mask() {
        let all = parse_proc_stat(STAT, None);
        assert_eq!(all.len(), 2);
        assert_eq!(
            all[&0],
            CpuTimes {
                idle: 400,
                total: 500
            }
        );
        let only1 = parse_proc_stat(STAT, Some(&BTreeSet::from([1, 7])));
        assert_eq!(only1.keys().copied().collect::<Vec<_>>(), [1]);
        // A mask that names no listed CPU keeps them all.
        assert_eq!(parse_proc_stat(STAT, Some(&BTreeSet::from([9]))).len(), 2);
        // iowait counts as idle, guest fields are not added twice.
        let t = parse_proc_stat("cpu3 10 0 10 70 10 0 0 0 99 99\n", None);
        assert_eq!(
            t[&3],
            CpuTimes {
                idle: 80,
                total: 100
            }
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(read_proc_stat(dir.path(), None).is_none());
        std::fs::write(dir.path().join("stat"), STAT).unwrap();
        assert_eq!(read_proc_stat(dir.path(), None).unwrap().len(), 2);
    }

    #[test]
    fn idle_while_waiting_integrates_the_idle_fraction_while_queued() {
        let mut u = Utilisation::default();
        u.observe(Some(stat([0, 0], [0, 0])), 0, true);
        assert_eq!(u.idle_cpus(), None, "one reading is not a rate");
        // 1 s later: CPU 0 fully idle, CPU 1 fully busy, queue was non-empty.
        u.observe(Some(stat([100, 0], [0, 100])), 1_000, true);
        assert_eq!(u.idle_cpus(), Some(1.0));
        assert_eq!(u.waiting_wall_ms(), 1_000);
        assert_eq!(u.idle_while_waiting_ms(), 500);
        // Queue empties at 2 s: the interval before it still counts.
        u.observe(Some(stat([200, 100], [0, 100])), 2_000, false);
        assert_eq!(u.idle_cpus(), Some(1.5), "mean of 1 and 2");
        assert_eq!(u.idle_while_waiting_ms(), 1_500);
        assert_eq!(u.waiting_wall_ms(), 2_000);
        // Empty queue before the sample: nothing accrues.
        u.observe(Some(stat([300, 200], [0, 100])), 3_000, false);
        assert_eq!(u.idle_while_waiting_ms(), 1_500);
        assert_eq!(u.waiting_wall_ms(), 2_000);
        assert_eq!(u.cpus(), 2);
        // A failed read keeps the totals and updates the queue state.
        u.observe(None, 4_000, true);
        assert_eq!(u.waiting_wall_ms(), 2_000);
        u.observe(Some(stat([300, 200], [100, 200])), 5_000, true);
        assert_eq!(u.waiting_wall_ms(), 4_000, "from the last reading at 3 s");
        assert_eq!(u.idle_while_waiting_ms(), 1_500, "both CPUs busy");
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// A `/proc/<pid>/stat` line: pgrp, cumulative ticks and start time.
    fn pid_stat(pid: u32, pgrp: i32, ticks: [u64; 4], start: u64) -> String {
        format!(
            "{pid} (sh x) S 1 {pgrp} {pgrp} 0 -1 0 0 0 0 0 {} {} {} {} 20 0 1 0 {start} 0 0\n",
            ticks[0], ticks[1], ticks[2], ticks[3]
        )
    }

    #[test]
    fn task_sampler_reads_groups_tasks_and_leader_ticks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "100/stat", &pid_stat(100, 100, [10, 5, 3, 2], 7));
        write(root, "101/stat", &pid_stat(101, 100, [0; 4], 8));
        write(root, "200/stat", &pid_stat(200, 200, [0; 4], 9));
        write(root, "self/stat", "garbage");
        write(root, "100/task/100/stat", &pid_stat(100, 100, [0; 4], 7));
        write(root, "100/task/100/schedstat", "2000000000 500000000 10\n");
        write(root, "100/task/102/stat", &pid_stat(102, 100, [0; 4], 11));
        write(root, "100/task/102/schedstat", "1000000000 0 3\n");
        let groups = group_members(root, &HashSet::from([100]));
        let mut members = groups[&100].clone();
        members.sort_unstable();
        assert_eq!(members, [100, 101]);
        assert_eq!(group_members(root, &HashSet::<i32>::new()), HashMap::new());
        let mut tasks = read_tasks(root, 100);
        tasks.sort_unstable();
        assert_eq!(
            tasks,
            [
                (100, 7, Some((2_000_000_000, 500_000_000))),
                (102, 11, Some((1_000_000_000, 0)))
            ]
        );
        assert_eq!(read_tasks(root, 101), Vec::new());
        assert_eq!(leader_ticks(root, 100), Some(20));
        assert_eq!(leader_ticks(root, 999), None);
    }

    #[test]
    fn job_usage_accumulates_deltas_and_survives_tid_reuse() {
        let mut u = JobUsage::default();
        assert_eq!(u.finish(1_000, true), None, "never sampled");
        u.update(&[(1, 10, Some((1_000_000_000, 0)))], Some(0));
        u.update(
            &[
                (1, 10, Some((3_000_000_000, 1_000_000_000))),
                (2, 11, Some((1_000_000_000, 0))),
            ],
            Some(50),
        );
        // tid 1 reused by a new thread (new start time): counted from zero.
        u.update(&[(1, 99, Some((500_000_000, 0)))], None);
        // 3 + 1 + 0.5 s CPU, 1 s delay over 2 s wall: 2.75 CPUs.
        let d = u.finish(2_000, false).unwrap();
        assert_eq!(d.cpu_ms, 4_500);
        assert_eq!(d.run_delay_ms, Some(1_000));
        assert_eq!(d.demand_milli, 2_750);
        assert!((u.demand_so_far(2_000).unwrap() - 2.75).abs() < 1e-9);
    }

    #[test]
    fn without_schedstat_cpu_over_wall_needs_calm_at_both_ends() {
        let mut u = JobUsage::default();
        u.update(&[(1, 10, None)], Some(300)); // 3 s of leader ticks
        assert_eq!(u.finish(1_000, true), None, "not calm at the start");
        u.calm_at_start = true;
        assert_eq!(u.finish(1_000, false), None, "not calm at the end");
        let d = u.finish(1_000, true).unwrap();
        assert_eq!(d.cpu_ms, 3_000);
        assert_eq!(d.run_delay_ms, None);
        assert_eq!(d.demand_milli, 3_000);
    }
}
