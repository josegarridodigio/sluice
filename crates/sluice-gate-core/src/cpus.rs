//! CPUs this process may use: the affinity mask (`Cpus_allowed_list` of
//! `/proc/self/status`) bounded by the cgroup CPU limit (cgroup v2
//! `cpu.max`, cgroup v1 `cpu.cfs_quota_us` / `cpu.cfs_period_us`, the
//! tightest one of the process's cgroup and its ancestors).
//!
//! Every reader takes a filesystem root so tests point it at a fake tree
//! (`<root>/proc/self/...`, `<root>/sys/fs/cgroup/...`). Design:
//! `kb/sluice/design/scheduler-throughput.md` section 3 (M1).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Parses a kernel CPU list (`0-3,8,10-11`) into the set of CPU numbers.
/// `None` when the text is not a well-formed list.
pub fn parse_cpu_list(text: &str) -> Option<BTreeSet<u32>> {
    let mut set = BTreeSet::new();
    let text = text.trim();
    if text.is_empty() {
        return Some(set);
    }
    for part in text.split(',') {
        let part = part.trim();
        if let Some((a, b)) = part.split_once('-') {
            let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
            if b < a || b - a > 65_536 {
                return None;
            }
            set.extend(a..=b);
        } else {
            set.insert(part.parse().ok()?);
        }
    }
    Some(set)
}

/// The CPUs of the affinity mask, from the text of a `/proc/<pid>/status`.
pub fn allowed_from_status(status: &str) -> Option<BTreeSet<u32>> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
        .and_then(parse_cpu_list)
        .filter(|s| !s.is_empty())
}

/// CPUs of the affinity mask of this process under `root`
/// (`<root>/proc/self/status`).
pub fn allowed_cpus_in(root: &Path) -> Option<BTreeSet<u32>> {
    let status = std::fs::read_to_string(root.join("proc/self/status")).ok()?;
    allowed_from_status(&status)
}

/// `ceil(quota / period)` of a cgroup v2 `cpu.max` line (`max 100000` means
/// no limit).
fn parse_cpu_max(text: &str) -> Option<u32> {
    let mut fields = text.split_whitespace();
    let quota = fields.next()?;
    let period: u64 = fields.next()?.parse().ok()?;
    if quota == "max" || period == 0 {
        return None;
    }
    let quota: u64 = quota.parse().ok()?;
    u32::try_from(quota.div_ceil(period).max(1)).ok()
}

/// `ceil(quota / period)` of a cgroup v1 pair (a negative quota means none).
fn parse_cfs(quota: &str, period: &str) -> Option<u32> {
    let quota: i64 = quota.trim().parse().ok()?;
    let period: i64 = period.trim().parse().ok()?;
    if quota <= 0 || period <= 0 {
        return None;
    }
    u32::try_from(quota.unsigned_abs().div_ceil(period.unsigned_abs()).max(1)).ok()
}

/// The cgroup directory `top/rel` and its ancestors up to `top`.
fn ancestors(top: &Path, rel: &str) -> Vec<PathBuf> {
    let mut out = vec![top.to_path_buf()];
    let mut cur = top.to_path_buf();
    for comp in Path::new(rel).components() {
        if let std::path::Component::Normal(c) = comp {
            cur.push(c);
            out.push(cur.clone());
        }
    }
    out
}

/// The tightest CPU limit of the cgroups of this process under `root`;
/// `None` when no limit applies or nothing can be read.
pub fn cgroup_cpu_limit_in(root: &Path) -> Option<u32> {
    let cgroups = std::fs::read_to_string(root.join("proc/self/cgroup")).ok()?;
    let fs = root.join("sys/fs/cgroup");
    let mut best: Option<u32> = None;
    let mut keep = |v: Option<u32>| {
        if let Some(v) = v {
            best = Some(best.map_or(v, |b| b.min(v)));
        }
    };
    for line in cgroups.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(_id), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if controllers.is_empty() {
            // cgroup v2 (unified hierarchy).
            for dir in ancestors(&fs, path) {
                keep(
                    std::fs::read_to_string(dir.join("cpu.max"))
                        .ok()
                        .as_deref()
                        .and_then(parse_cpu_max),
                );
            }
        } else if controllers.split(',').any(|c| c == "cpu") {
            // cgroup v1: the cpu controller is mounted as `cpu,cpuacct` or `cpu`.
            for mount in [controllers, "cpu,cpuacct", "cpu"] {
                let top = fs.join(mount);
                if !top.is_dir() {
                    continue;
                }
                for dir in ancestors(&top, path) {
                    let quota = std::fs::read_to_string(dir.join("cpu.cfs_quota_us"));
                    let period = std::fs::read_to_string(dir.join("cpu.cfs_period_us"));
                    if let (Ok(q), Ok(p)) = (quota, period) {
                        keep(parse_cfs(&q, &p));
                    }
                }
                break;
            }
        }
    }
    best
}

/// CPUs available to this process under `root`: the affinity mask bounded by
/// the cgroup CPU limit. `None` when the affinity mask cannot be read.
pub fn available_cpus_in(root: &Path) -> Option<u32> {
    let allowed = u32::try_from(allowed_cpus_in(root)?.len()).unwrap_or(u32::MAX);
    let limit = cgroup_cpu_limit_in(root).unwrap_or(u32::MAX);
    Some(allowed.min(limit).max(1))
}

/// CPUs available to this process (affinity mask and cgroup limit); falls
/// back to [`std::thread::available_parallelism`] (which applies the same
/// two limits) when `/proc` cannot be read.
pub fn available_cpus() -> u32 {
    available_cpus_in(Path::new("/")).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map_or(2, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
    })
}

/// Default budget: every available CPU, at least 2.
pub fn default_budget() -> u32 {
    available_cpus().max(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn cpu_lists() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-11\n").unwrap().len(),
            7,
            "ranges and singles"
        );
        assert_eq!(parse_cpu_list("").unwrap().len(), 0);
        assert!(parse_cpu_list("3-1").is_none());
        assert!(parse_cpu_list("a").is_none());
        let status = "Name:\tx\nCpus_allowed:\tff\nCpus_allowed_list:\t0-5\n";
        assert_eq!(allowed_from_status(status).unwrap().len(), 6);
        assert!(allowed_from_status("Name:\tx\n").is_none());
    }

    #[test]
    fn affinity_alone() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(available_cpus_in(dir.path()), None);
        write(dir.path(), "proc/self/status", "Cpus_allowed_list:\t0-5\n");
        assert_eq!(available_cpus_in(dir.path()), Some(6), "taskset -c 0-5");
        write(dir.path(), "proc/self/cgroup", "0::/user.slice/a.scope\n");
        assert_eq!(
            available_cpus_in(dir.path()),
            Some(6),
            "no cpu.max: no limit"
        );
        write(
            dir.path(),
            "sys/fs/cgroup/user.slice/cpu.max",
            "max 100000\n",
        );
        assert_eq!(available_cpus_in(dir.path()), Some(6));
    }

    #[test]
    fn cgroup_v2_quota_of_an_ancestor_applies() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "proc/self/status", "Cpus_allowed_list:\t0-19\n");
        write(dir.path(), "proc/self/cgroup", "0::/user.slice/a.scope\n");
        write(
            dir.path(),
            "sys/fs/cgroup/user.slice/a.scope/cpu.max",
            "max 100000\n",
        );
        write(
            dir.path(),
            "sys/fs/cgroup/user.slice/cpu.max",
            "350000 100000\n",
        );
        assert_eq!(cgroup_cpu_limit_in(dir.path()), Some(4), "ceil(3.5)");
        assert_eq!(available_cpus_in(dir.path()), Some(4));
        // A tighter leaf wins.
        write(
            dir.path(),
            "sys/fs/cgroup/user.slice/a.scope/cpu.max",
            "50000 100000\n",
        );
        assert_eq!(available_cpus_in(dir.path()), Some(1));
    }

    #[test]
    fn cgroup_v1_quota() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "proc/self/status", "Cpus_allowed_list:\t0-7\n");
        write(
            dir.path(),
            "proc/self/cgroup",
            "12:memory:/x\n4:cpu,cpuacct:/docker/abc\n",
        );
        write(
            dir.path(),
            "sys/fs/cgroup/cpu,cpuacct/docker/abc/cpu.cfs_quota_us",
            "200000\n",
        );
        write(
            dir.path(),
            "sys/fs/cgroup/cpu,cpuacct/docker/abc/cpu.cfs_period_us",
            "100000\n",
        );
        assert_eq!(available_cpus_in(dir.path()), Some(2));
        write(
            dir.path(),
            "sys/fs/cgroup/cpu,cpuacct/docker/abc/cpu.cfs_quota_us",
            "-1\n",
        );
        assert_eq!(available_cpus_in(dir.path()), Some(8), "-1: unlimited");
    }

    #[test]
    fn real_machine_has_a_budget() {
        assert!(available_cpus() >= 1);
        assert!(default_budget() >= 2);
    }
}
