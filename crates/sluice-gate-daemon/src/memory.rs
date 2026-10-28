//! What memory-aware admission learns and reads (ADR-0019 piece 1).
//!
//! Three small pieces, all over plain files so tests use fake trees:
//!
//! - the memory the gates may use: `<proc_root>/meminfo`, capped by every
//!   limited cgroup above the daemon and by the gates' own ceiling
//!   ([`read_meminfo`], [`cap_by_cgroups`], [`facts`]); in a container, a CI
//!   job or a unit with `MemoryMax=` the machine's `MemAvailable` is far
//!   more than the daemon can have;
//! - a running job's working set, sampled every second: `anon` of the job
//!   leaf's `memory.stat` plus `memory.swap.current` (page cache is left out
//!   on purpose: `MemAvailable` already counts it as available, so counting
//!   it twice would refuse jobs that fit), or the summed `VmRSS` of the
//!   job's process group when the machine gives no leaf ([`read_cgroup`],
//!   [`read_rss`]);
//! - the expected peak of a gate, learned from its finished runs
//!   ([`expected_peak`], [`PeakCache`]), and the priors it gives the gates
//!   with no history ([`PeakCache::priors`]).

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use sluice_gate_core::config::AdmissionConfig;

use crate::scheduler::{LearnKey, MemoryFacts, PeakPriors};

/// Finished runs an expected peak looks at (the newest ones).
pub const PEAK_WINDOW: usize = 5;
/// Margin over the largest sampled working set, as a divisor (a fifth, 20 %):
/// the sampler can miss a sub-second spike.
const MARGIN_DIVISOR: u64 = 5;
/// Extra margin of a run killed for lack of memory, as a divisor (a quarter,
/// 25 %): it stopped early, so its peak is only a lower bound of what the gate
/// needs.
const KILLED_MARGIN_DIVISOR: u64 = 4;

/// `MemTotal` and `MemAvailable` of the machine, bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    /// `MemTotal`.
    pub total_bytes: u64,
    /// `MemAvailable` (swap is not part of it).
    pub available_bytes: u64,
}

/// A figure of a `meminfo`-style file (`Key:   123 kB`), in bytes.
fn kib_field(text: &str, key: &str) -> Option<u64> {
    let kib: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    kib.checked_mul(1024)
}

/// Parses the text of `/proc/meminfo`; `None` unless both fields are there
/// and the total is not zero.
#[must_use]
pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let total_bytes = kib_field(text, "MemTotal").filter(|t| *t > 0)?;
    Some(MemInfo {
        total_bytes,
        available_bytes: kib_field(text, "MemAvailable")?,
    })
}

/// Reads `<proc_root>/meminfo`.
#[must_use]
pub fn read_meminfo(proc_root: &Path) -> Option<MemInfo> {
    parse_meminfo(&std::fs::read_to_string(proc_root.join("meminfo")).ok()?)
}

/// `memory.max` and `memory.current` of a cgroup directory, bytes; `None`
/// when it has no numeric limit (`max`, or no memory controller there).
fn cgroup_limit(dir: &Path) -> Option<(u64, u64)> {
    let max: u64 = std::fs::read_to_string(dir.join("memory.max"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let current: u64 = std::fs::read_to_string(dir.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some((max, current))
}

/// `info` capped by memory limits: every cgroup from the daemon's own
/// (`<proc_root>/self/cgroup`, a cgroup v2 path under `mount`) up to the
/// root, plus `extra` (the gates' `jobs/` directory, whose `memory.max` is
/// `[containment] gates_memory_max`). A limited cgroup caps the total at its
/// `memory.max` and the available memory at `memory.max - memory.current`.
/// Without a readable own cgroup only `extra` applies.
#[must_use]
pub fn cap_by_cgroups(info: MemInfo, proc_root: &Path, mount: &Path, extra: &[&Path]) -> MemInfo {
    let own = std::fs::read_to_string(proc_root.join("self/cgroup"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("0::"))
                .map(|p| p.trim().trim_start_matches('/').to_owned())
        });
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(rel) = own.filter(|r| !r.is_empty()) {
        let mut dir = mount.join(rel);
        while dir.starts_with(mount) && dir != mount {
            dirs.push(dir.clone());
            if !dir.pop() {
                break;
            }
        }
    }
    dirs.extend(extra.iter().map(|p| p.to_path_buf()));
    dirs.iter()
        .filter_map(|d| cgroup_limit(d))
        .fold(info, |acc, (max, current)| MemInfo {
            total_bytes: acc.total_bytes.min(max),
            available_bytes: acc.available_bytes.min(max.saturating_sub(current)),
        })
}

/// The facts of one admission pass: the machine's memory and the reserve of
/// `[admission] memory_reserve`. `None` (the rule is off for the pass) when
/// memory-aware admission is disabled or `meminfo` is unreadable.
#[must_use]
pub fn facts(cfg: &AdmissionConfig, info: Option<MemInfo>) -> Option<MemoryFacts> {
    let info = info.filter(|_| cfg.memory_aware)?;
    Some(MemoryFacts {
        available_bytes: info.available_bytes,
        total_bytes: info.total_bytes,
        reserve_bytes: cfg.memory_reserve_bytes(info.total_bytes),
    })
}

/// One reading of a job's memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkingSet {
    /// Working set: `anon` + swap of the leaf, or the summed resident set.
    pub bytes: u64,
    /// `memory.peak` of the leaf (page cache included); shown, never used
    /// for admission. `None` without a leaf.
    pub peak: Option<u64>,
}

/// `anon` of a `memory.stat`.
#[must_use]
pub fn parse_anon(stat: &str) -> Option<u64> {
    stat.lines()
        .find_map(|l| l.strip_prefix("anon ")?.trim().parse().ok())
}

fn read_number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Working set of a job's cgroup leaf: `anon` of `memory.stat` plus
/// `memory.swap.current` (absent without swap accounting: zero). `None` when
/// the leaf has no readable `memory.stat` (it does not exist, or the memory
/// controller is off).
#[must_use]
pub fn read_cgroup(leaf: &Path) -> Option<WorkingSet> {
    let anon = parse_anon(&std::fs::read_to_string(leaf.join("memory.stat")).ok()?)?;
    let swap = read_number(&leaf.join("memory.swap.current")).unwrap_or(0);
    Some(WorkingSet {
        bytes: anon.saturating_add(swap),
        peak: read_number(&leaf.join("memory.peak")),
    })
}

/// Resident set of `pid`, bytes (`VmRSS` of `<proc_root>/<pid>/status`).
fn rss_of(proc_root: &Path, pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(proc_root.join(pid.to_string()).join("status")).ok()?;
    kib_field(&status, "VmRSS")
}

/// Summed resident set of `pids`: the working set of a job without a
/// cgroup. Shared pages are counted once per process, so it errs high.
#[must_use]
pub fn read_rss(proc_root: &Path, pids: &[u32]) -> WorkingSet {
    WorkingSet {
        bytes: pids
            .iter()
            .filter_map(|&pid| rss_of(proc_root, pid))
            .fold(0, u64::saturating_add),
        peak: None,
    }
}

/// One finished run a peak is learned from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeakSample {
    /// Largest sampled working set, bytes.
    pub anon_peak_bytes: u64,
    /// The run was killed for lack of memory.
    pub killed: bool,
}

/// The expected peak of a gate: the largest working set of its newest
/// [`PEAK_WINDOW`] runs (a killed run counts 25 % more: it stopped early),
/// plus a 20 % margin. A maximum, not an average, so one sample is evidence
/// enough; `None` without samples. `samples` are oldest first.
#[must_use]
pub fn expected_peak(samples: &[PeakSample]) -> Option<u64> {
    let window = &samples[samples.len().saturating_sub(PEAK_WINDOW)..];
    let peak = window
        .iter()
        .map(|s| {
            if s.killed {
                s.anon_peak_bytes
                    .saturating_add(s.anon_peak_bytes / KILLED_MARGIN_DIVISOR)
            } else {
                s.anon_peak_bytes
            }
        })
        .max()?;
    Some(peak.saturating_add(peak / MARGIN_DIVISOR))
}

/// Recent finished runs of each key (newest last, at most [`PEAK_WINDOW`])
/// and the weight each key last ran with.
#[derive(Debug, Default)]
pub struct PeakCache {
    runs: HashMap<LearnKey, VecDeque<PeakSample>>,
    /// Peaks of the newest runs of a key that ended `fail` or `timeout`
    /// (newest last, at most [`PEAK_WINDOW`]): lower bounds, never samples.
    bounds: HashMap<LearnKey, VecDeque<u64>>,
    /// Effective (admitted) weight of the newest run of a key whose weight
    /// is known; a key missing here is never used as a prior.
    weights: HashMap<LearnKey, u32>,
}

impl PeakCache {
    /// Adds a finished run of `key`.
    pub fn push(&mut self, key: LearnKey, sample: PeakSample) {
        let runs = self.runs.entry(key).or_default();
        runs.push_back(sample);
        while runs.len() > PEAK_WINDOW {
            runs.pop_front();
        }
    }

    /// Adds the peak of a run of `key` that ended `fail` or `timeout`: the
    /// run stopped before (or after a stall at) its full working set, so the
    /// gate needs at least this much.
    pub fn push_bound(&mut self, key: LearnKey, peak_bytes: u64) {
        let bounds = self.bounds.entry(key).or_default();
        bounds.push_back(peak_bytes);
        while bounds.len() > PEAK_WINDOW {
            bounds.pop_front();
        }
    }

    /// The largest of the recent lower bounds of `key`, bytes.
    #[must_use]
    pub fn lower_bound(&self, key: &LearnKey) -> Option<u64> {
        self.bounds.get(key)?.iter().copied().max()
    }

    /// Every key with a lower bound.
    pub fn lower_bounds(&self) -> impl Iterator<Item = (&LearnKey, u64)> {
        self.bounds
            .keys()
            .filter_map(|k| Some((k, self.lower_bound(k)?)))
    }

    /// Records the weight `key` last ran with.
    pub fn set_weight(&mut self, key: LearnKey, weight: u32) {
        self.weights.insert(key, weight);
    }

    /// The expected peak of `key`, bytes.
    #[must_use]
    pub fn expected(&self, key: &LearnKey) -> Option<u64> {
        let runs = self.runs.get(key)?;
        expected_peak(&runs.iter().copied().collect::<Vec<_>>())
    }

    /// Every key with its expected peak.
    pub fn expectations(&self) -> impl Iterator<Item = (&LearnKey, u64)> {
        self.runs
            .keys()
            .filter_map(|k| Some((k, self.expected(k)?)))
    }

    /// The expected peak of every learned key with its weight, for the gates
    /// with no history (`scheduler::prior_peak`); keys of unknown weight
    /// are left out.
    #[must_use]
    pub fn priors(&self) -> PeakPriors {
        self.expectations()
            .filter_map(|(key, bytes)| Some((*self.weights.get(key)?, bytes)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn limited_cgroups_above_the_daemon_and_the_gates_ceiling_cap_the_memory() {
        let dir = tempfile::tempdir().unwrap();
        let (proc_root, mount) = (dir.path().join("proc"), dir.path().join("cg"));
        let machine = MemInfo {
            total_bytes: 30 * GIB,
            available_bytes: 22 * GIB,
        };
        // No readable own cgroup: nothing caps (a fake `proc_root`).
        assert_eq!(cap_by_cgroups(machine, &proc_root, &mount, &[]), machine);
        write(&proc_root, "self/cgroup", "0::/box/daemon\n");
        // Unlimited ancestors (`max`) cap nothing.
        write(&mount, "box/memory.max", "max\n");
        write(&mount, "box/memory.current", "1\n");
        assert_eq!(cap_by_cgroups(machine, &proc_root, &mount, &[]), machine);
        // A container-like limit of 2 GiB with 1.5 GiB used.
        write(&mount, "box/memory.max", &format!("{}\n", 2 * GIB));
        write(&mount, "box/memory.current", &format!("{}\n", 3 * GIB / 2));
        assert_eq!(
            cap_by_cgroups(machine, &proc_root, &mount, &[]),
            MemInfo {
                total_bytes: 2 * GIB,
                available_bytes: GIB / 2,
            }
        );
        // The gates' ceiling on `jobs/` caps what the gates may still take.
        write(&mount, "jobs/memory.max", &format!("{GIB}\n"));
        write(&mount, "jobs/memory.current", &format!("{}\n", 3 * GIB / 4));
        let jobs = mount.join("jobs");
        assert_eq!(
            cap_by_cgroups(machine, &proc_root, &mount, &[jobs.as_path()]),
            MemInfo {
                total_bytes: GIB,
                available_bytes: GIB / 4,
            }
        );
        // Over its limit (it can happen for a moment): nothing available.
        write(&mount, "box/memory.current", &format!("{}\n", 3 * GIB));
        assert_eq!(
            cap_by_cgroups(machine, &proc_root, &mount, &[]).available_bytes,
            0
        );
    }

    fn sample(gib: u64, killed: bool) -> PeakSample {
        PeakSample {
            anon_peak_bytes: gib * GIB,
            killed,
        }
    }

    #[test]
    fn meminfo_gives_total_and_available_in_bytes() {
        let text = "MemTotal:       31457280 kB\nMemFree: 1 kB\nMemAvailable:   22020096 kB\n";
        assert_eq!(
            parse_meminfo(text),
            Some(MemInfo {
                total_bytes: 30 * GIB,
                available_bytes: 21 * GIB
            })
        );
        assert_eq!(parse_meminfo("MemTotal: 10 kB\n"), None, "no MemAvailable");
        assert_eq!(parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n"), None);
        assert_eq!(parse_meminfo("garbage"), None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_meminfo(dir.path()), None);
        write(dir.path(), "meminfo", text);
        assert_eq!(read_meminfo(dir.path()).unwrap().total_bytes, 30 * GIB);
    }

    #[test]
    fn facts_follow_the_switch_and_the_reserve() {
        let info = Some(MemInfo {
            total_bytes: 30 * GIB,
            available_bytes: 22 * GIB,
        });
        let cfg = AdmissionConfig::default();
        let f = facts(&cfg, info).unwrap();
        assert_eq!(
            (f.available_bytes, f.total_bytes, f.reserve_bytes),
            (22 * GIB, 30 * GIB, 3 * GIB)
        );
        let off = AdmissionConfig {
            memory_aware: false,
            ..AdmissionConfig::default()
        };
        assert_eq!(facts(&off, info), None, "switch off");
        assert_eq!(facts(&cfg, None), None, "meminfo unreadable");
        let fixed = AdmissionConfig {
            memory_reserve: "4G".into(),
            ..AdmissionConfig::default()
        };
        assert_eq!(facts(&fixed, info).unwrap().reserve_bytes, 4 * GIB);
    }

    #[test]
    fn a_leaf_gives_anon_plus_swap_and_the_peak() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = dir.path();
        assert_eq!(read_cgroup(leaf), None, "no memory.stat");
        write(
            leaf,
            "memory.stat",
            "anon 1000\nfile 9999999\nanon_thp 5\nkernel 3\n",
        );
        assert_eq!(
            read_cgroup(leaf),
            Some(WorkingSet {
                bytes: 1000,
                peak: None
            }),
            "page cache is not counted; no swap file means no swap"
        );
        write(leaf, "memory.swap.current", "500\n");
        write(leaf, "memory.peak", "123456\n");
        assert_eq!(
            read_cgroup(leaf),
            Some(WorkingSet {
                bytes: 1500,
                peak: Some(123_456)
            })
        );
        assert_eq!(parse_anon("file 1\n"), None);
        assert_eq!(parse_anon("anon_thp 5\nanon 7\n"), Some(7));
    }

    #[test]
    fn without_a_leaf_the_resident_sets_of_the_group_add_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "100/status", "Name:\tsh\nVmRSS:\t   2048 kB\n");
        write(root, "101/status", "VmRSS:\t1024 kB\nThreads: 1\n");
        write(root, "102/status", "Name:\tkernel-thread\n");
        let ws = read_rss(root, &[100, 101, 102, 999]);
        assert_eq!(
            ws,
            WorkingSet {
                bytes: 3 * 1024 * 1024,
                peak: None
            }
        );
        assert_eq!(read_rss(root, &[]).bytes, 0);
    }

    #[test]
    fn the_expected_peak_is_the_largest_of_the_last_five_plus_a_margin() {
        assert_eq!(expected_peak(&[]), None);
        // One sample is enough: 10 GiB plus 20 %.
        assert_eq!(expected_peak(&[sample(10, false)]), Some(12 * GIB));
        // The maximum, not the mean.
        assert_eq!(
            expected_peak(&[sample(2, false), sample(10, false), sample(3, false)]),
            Some(12 * GIB)
        );
        // A killed run counts 25 % more: 8 * 1.25 * 1.2 = 12.
        assert_eq!(expected_peak(&[sample(8, true)]), Some(12 * GIB));
        // Only the newest five count: the 100 GiB run fell out.
        let mut runs = vec![sample(100, false)];
        runs.extend((0..5).map(|_| sample(5, false)));
        assert_eq!(expected_peak(&runs), Some(6 * GIB));
    }

    #[test]
    fn the_cache_keeps_five_runs_per_key() {
        let key = |g: &str| (std::path::PathBuf::from("/r"), g.to_owned(), String::new());
        let mut cache = PeakCache::default();
        assert_eq!(cache.expected(&key("test")), None);
        cache.push(key("test"), sample(50, false));
        for _ in 0..5 {
            cache.push(key("test"), sample(1, false));
        }
        cache.push(key("lint"), sample(2, true));
        assert_eq!(
            cache.expected(&key("test")),
            Some(GIB + GIB / 5),
            "the 50 GiB run is out of the window"
        );
        let mut all: Vec<_> = cache
            .expectations()
            .map(|(k, v)| (k.1.clone(), v))
            .collect();
        all.sort();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "lint");
    }

    #[test]
    fn priors_are_the_expected_peak_of_every_key_with_a_known_weight() {
        let key = |g: &str| (std::path::PathBuf::from("/r"), g.to_owned(), String::new());
        let mut cache = PeakCache::default();
        cache.push(key("fmt"), sample(1, false));
        cache.push(key("lint"), sample(2, false));
        cache.push(key("test"), sample(10, false));
        cache.push(key("orphan"), sample(50, false));
        cache.set_weight(key("fmt"), 1);
        cache.set_weight(key("lint"), 1);
        cache.set_weight(key("test"), 8);
        // A weight without samples teaches nothing; "orphan" has no weight.
        cache.set_weight(key("never-ran"), 2);
        let mut priors = cache.priors();
        priors.sort_unstable();
        assert_eq!(
            priors,
            [
                (1, GIB + GIB / 5),
                (1, 2 * GIB + 2 * GIB / 5),
                (8, 12 * GIB)
            ],
            "one entry per learned key"
        );
    }

    #[test]
    fn lower_bounds_are_the_largest_recent_peak_of_failed_runs_and_never_samples() {
        let key = (
            std::path::PathBuf::from("/r"),
            "test".to_owned(),
            String::new(),
        );
        let mut cache = PeakCache::default();
        assert_eq!(cache.lower_bound(&key), None);
        cache.push_bound(key.clone(), 3 * GIB);
        cache.push_bound(key.clone(), GIB);
        assert_eq!(cache.lower_bound(&key), Some(3 * GIB), "the largest");
        assert_eq!(cache.expected(&key), None, "a bound teaches no expectation");
        assert_eq!(cache.lower_bounds().count(), 1);
        // The window forgets the oldest: five newer, smaller bounds push 3 GiB out.
        for _ in 0..PEAK_WINDOW {
            cache.push_bound(key.clone(), GIB);
        }
        assert_eq!(cache.lower_bound(&key), Some(GIB));
    }
}
