//! Memory telemetry of running jobs (ADR-0028, part A).
//!
//! The 2026-11-05 incident could not be reconstructed: the daemon kept the
//! working set of each job, but not how much it stalled on memory, how much
//! it swapped, which of its processes had left its cgroup, nor how the
//! desktop session fared meanwhile. Every sampler tick this module reads,
//! for each running job with a cgroup leaf:
//!
//! - the leaf's `memory.pressure` (`some` and `full` `avg10`),
//!   `memory.swap.current` and the `anon` / `file` lines of `memory.stat`
//!   ([`read_leaf`]);
//! - the processes descended from the leaf's processes that live in another
//!   cgroup ([`find_escapees`]): Chromium, for one, moves its browser process
//!   into a systemd scope of its own, so its memory escapes the job's
//!   accounting, ceiling and `cgroup.kill`;
//!
//! and, once per tick, the `memory.pressure` of the user's session
//! (`session.slice` of the user manager, read only, [`session_cgroup`]).
//! [`JobTelemetry`] keeps the peaks of a job and says what is worth a log
//! line; the daemon stores the peaks when the job ends.
//!
//! Readers take plain paths so tests use fake trees.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::sampler::ProcEntry;

/// `full avg10` of a job's memory pressure, percent, from which the job is
/// reported as stalled on memory (its tasks all waited on memory for a tenth
/// of the last ten seconds). Logged once per job.
pub const JOB_FULL_WARN: f64 = 10.0;

/// `some avg10` of the session's memory pressure, percent, from which the
/// desktop is reported as degraded: at 10 % some task of the session (the
/// compositor, the shell) waited on memory for one second in every ten, the
/// level at which input lag is visible.
pub const SESSION_SOME_WARN: f64 = 10.0;

/// The session's pressure must fall under this `some avg10` before another
/// warning (hysteresis, so a value hovering around the threshold logs once).
pub const SESSION_SOME_CLEAR: f64 = 5.0;

/// The `avg10` values of a cgroup `memory.pressure` file, percent.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Pressure {
    /// `some avg10`: share of time at least one task waited on memory.
    pub some_avg10: f64,
    /// `full avg10`: share of time every non-idle task waited on memory
    /// (zero when the line is missing, as in the root cgroup of old kernels).
    pub full_avg10: f64,
}

/// Parses a `memory.pressure` text; `None` without a `some` line.
#[must_use]
pub fn parse_pressure(text: &str) -> Option<Pressure> {
    let avg10 = |kind: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(kind)?.strip_prefix(' ')?;
            rest.split_whitespace()
                .find_map(|f| f.strip_prefix("avg10="))?
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
        })
    };
    Some(Pressure {
        some_avg10: avg10("some")?,
        full_avg10: avg10("full").unwrap_or(0.0),
    })
}

/// Reads `<dir>/memory.pressure`.
#[must_use]
pub fn read_pressure(dir: &Path) -> Option<Pressure> {
    parse_pressure(&std::fs::read_to_string(dir.join("memory.pressure")).ok()?)
}

/// One line (`key value`) of a `memory.stat`, bytes.
#[must_use]
pub fn stat_field(stat: &str, key: &str) -> Option<u64> {
    stat.lines().find_map(|l| {
        let rest = l.strip_prefix(key)?.strip_prefix(' ')?;
        rest.trim().parse().ok()
    })
}

/// One tick's reading of a job leaf; every field is `None` when its file is
/// missing (no swap accounting, no PSI, the leaf already gone).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LeafReading {
    /// `memory.pressure`.
    pub pressure: Option<Pressure>,
    /// `memory.swap.current`, bytes.
    pub swap_bytes: Option<u64>,
    /// `anon` of `memory.stat`, bytes.
    pub anon_bytes: Option<u64>,
    /// `file` of `memory.stat` (page cache), bytes.
    pub file_bytes: Option<u64>,
}

/// Reads a job leaf.
#[must_use]
pub fn read_leaf(leaf: &Path) -> LeafReading {
    let stat = std::fs::read_to_string(leaf.join("memory.stat")).ok();
    LeafReading {
        pressure: read_pressure(leaf),
        swap_bytes: std::fs::read_to_string(leaf.join("memory.swap.current"))
            .ok()
            .and_then(|t| t.trim().parse().ok()),
        anon_bytes: stat.as_deref().and_then(|s| stat_field(s, "anon")),
        file_bytes: stat.as_deref().and_then(|s| stat_field(s, "file")),
    }
}

/// The pids listed in `<leaf>/cgroup.procs` (empty when unreadable).
#[must_use]
pub fn leaf_procs(leaf: &Path) -> HashSet<u32> {
    std::fs::read_to_string(leaf.join("cgroup.procs"))
        .map(|t| t.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// The cgroup v2 path of `pid` (`/user.slice/...`, the `0::` line of
/// `/proc/<pid>/cgroup`); `None` when it is gone or has no v2 line.
#[must_use]
pub fn cgroup_of(proc_root: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_owned())
}

/// Whether the cgroup path `cg` is the leaf `leaf_rel` or below it.
fn inside(cg: &str, leaf_rel: &str) -> bool {
    cg == leaf_rel
        || cg
            .strip_prefix(leaf_rel)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A process descended from a job that lives outside the job's leaf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escapee {
    /// Process id.
    pub pid: u32,
    /// Start time (ticks since boot): with the pid, its identity.
    pub start: u64,
    /// `comm` of the process.
    pub comm: String,
    /// The cgroup it lives in.
    pub cgroup: String,
    /// Resident set, bytes (`VmRSS`).
    pub rss_bytes: u64,
}

/// The processes that descend from the leaf's processes, or that were seen
/// escaping earlier (`known`: pid and start time, so an escapee orphaned
/// since still counts), and live outside the leaf. `table` is this tick's
/// process table, `leaf_rel` the leaf's cgroup path as `/proc/<pid>/cgroup`
/// prints it. Only the processes found outside the leaf's `cgroup.procs`
/// have their `cgroup` file read.
#[must_use]
pub fn find_escapees<S: std::hash::BuildHasher>(
    proc_root: &Path,
    table: &[ProcEntry],
    procs: &HashSet<u32, S>,
    leaf_rel: &str,
    known: &[(u32, u64)],
) -> Vec<Escapee> {
    let mut children: HashMap<u32, Vec<&ProcEntry>> = HashMap::new();
    let by_pid: HashMap<u32, &ProcEntry> = table.iter().map(|p| (p.pid, p)).collect();
    for p in table {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut stack: Vec<u32> = procs.iter().copied().collect();
    let mut seen: HashSet<u32> = stack.iter().copied().collect();
    let mut out = Vec::new();
    // Known escapees still alive (same start time) are roots of their own.
    let mut roots: Vec<&ProcEntry> = known
        .iter()
        .filter_map(|&(pid, start)| by_pid.get(&pid).filter(|p| p.start == start).copied())
        .collect();
    loop {
        let next = if let Some(pid) = stack.pop() {
            children
                .get(&pid)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .to_vec()
        } else if let Some(root) = roots.pop() {
            vec![root]
        } else {
            break;
        };
        for child in next {
            if !seen.insert(child.pid) {
                continue;
            }
            stack.push(child.pid);
            if procs.contains(&child.pid) {
                continue;
            }
            let Some(cg) = cgroup_of(proc_root, child.pid) else {
                continue;
            };
            if inside(&cg, leaf_rel) {
                continue;
            }
            out.push(Escapee {
                pid: child.pid,
                start: child.start,
                comm: std::fs::read_to_string(proc_root.join(child.pid.to_string()).join("comm"))
                    .map(|c| c.trim().to_owned())
                    .unwrap_or_default(),
                cgroup: cg,
                rss_bytes: crate::memory::rss_bytes(proc_root, child.pid).unwrap_or(0),
            });
        }
    }
    out.sort_by_key(|e| e.pid);
    out
}

/// Moves `pid` (its whole thread group) back into the job leaf (ADR-0028).
/// Permitted to the daemon for any process of its own uid under the user
/// manager: the kernel only asks for write access to `cgroup.procs` of the
/// common ancestor of both cgroups (`user@UID.service`, owned by the user).
///
/// # Errors
/// The write's error (`ESRCH` when the process is gone, `EACCES` when the
/// machine does not let the daemon move it, `ENOENT` once the leaf is gone).
pub fn reclaim(leaf: &Path, pid: u32) -> std::io::Result<()> {
    std::fs::write(leaf.join("cgroup.procs"), pid.to_string())
}

/// Kills whatever is left in a job's leaf (`cgroup.kill`, Linux 5.14 and
/// later) and every known escapee still alive outside it, identified by pid
/// and start time through a pidfd so a reused pid is never signalled
/// (ADR-0028). Returns how many escapees were signalled and whether the leaf
/// was killed.
#[must_use]
pub fn kill_leftovers(proc_root: &Path, leaf: &Path, escapees: &[(u32, u64)]) -> (u32, bool) {
    let leaf_killed = leaf.join("cgroup.kill").exists()
        && !leaf_procs(leaf).is_empty()
        && std::fs::write(leaf.join("cgroup.kill"), "1").is_ok();
    let mut signalled = 0;
    for &(pid, start) in escapees {
        if kill_if_same(proc_root, pid, start) {
            signalled += 1;
        }
    }
    (signalled, leaf_killed)
}

/// SIGKILL to `pid` when it is still the process that started at `start`:
/// the pidfd pins the process, then its start time is checked.
fn kill_if_same(proc_root: &Path, pid: u32, start: u64) -> bool {
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
    let Some(raw) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    let Ok(fd) = pidfd_open(raw, PidfdFlags::empty()) else {
        return false;
    };
    let same = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat"))
        .ok()
        .and_then(|s| crate::sampler::parse_proc_entry(pid, &s))
        .is_some_and(|e| e.start == start);
    same && pidfd_send_signal(&fd, Signal::KILL).is_ok()
}

/// The cgroup whose pressure stands for the user's desktop: `session.slice`
/// of the systemd user manager the daemon runs under (where GNOME and KDE
/// put the compositor and the shell), or the manager itself when it has no
/// `session.slice`. `None` when the daemon is not under a user manager (a
/// system service, a container) or its own cgroup is unreadable. The daemon
/// only ever reads this cgroup.
#[must_use]
pub fn session_cgroup(proc_root: &Path, mount: &Path) -> Option<PathBuf> {
    let own = std::fs::read_to_string(proc_root.join("self/cgroup")).ok()?;
    let rel = own.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let mut dir = mount.to_path_buf();
    for part in rel.trim_start_matches('/').split('/') {
        dir.push(part);
        if part.starts_with("user@") && part.ends_with(".service") {
            let session = dir.join("session.slice");
            return Some(if session.is_dir() { session } else { dir });
        }
    }
    None
}

/// Something about a job worth one log line.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// A process of the job was seen outside its leaf for the first time.
    Escaped(Escapee),
    /// A process seen outside the leaf was moved back into it.
    Reclaimed(Escapee),
    /// Moving it back failed.
    ReclaimFailed(Escapee, String),
    /// The job's `full avg10` reached [`JOB_FULL_WARN`] (once per job).
    Stalled {
        /// The `full avg10` read.
        full_avg10: f64,
        /// `memory.swap.current` of the leaf, bytes.
        swap_bytes: u64,
    },
}

/// The memory telemetry of one running job.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobTelemetry {
    samples: u32,
    now: LeafReading,
    some_max: f64,
    full_max: f64,
    swap_peak: u64,
    file_peak: u64,
    session_some_max: Option<f64>,
    session_full_max: Option<f64>,
    /// Every escapee seen, by pid and start time.
    escaped: BTreeMap<(u32, u64), Escapee>,
    escaped_rss_now: u64,
    escaped_rss_peak: u64,
    escaped_now: u32,
    reclaimed: u32,
    stall_noted: bool,
}

impl JobTelemetry {
    /// Adds one tick: the leaf's reading, the session's pressure and the
    /// escapees found this tick. Returns what deserves a log line.
    pub fn observe(
        &mut self,
        leaf: LeafReading,
        session: Option<Pressure>,
        escapees: Vec<Escapee>,
    ) -> Vec<Note> {
        let mut notes = Vec::new();
        self.samples += 1;
        self.now = leaf;
        if let Some(p) = leaf.pressure {
            self.some_max = self.some_max.max(p.some_avg10);
            self.full_max = self.full_max.max(p.full_avg10);
            if p.full_avg10 >= JOB_FULL_WARN && !self.stall_noted {
                self.stall_noted = true;
                notes.push(Note::Stalled {
                    full_avg10: p.full_avg10,
                    swap_bytes: leaf.swap_bytes.unwrap_or(0),
                });
            }
        }
        self.swap_peak = self.swap_peak.max(leaf.swap_bytes.unwrap_or(0));
        self.file_peak = self.file_peak.max(leaf.file_bytes.unwrap_or(0));
        if let Some(s) = session {
            self.session_some_max = Some(self.session_some_max.unwrap_or(0.0).max(s.some_avg10));
            self.session_full_max = Some(self.session_full_max.unwrap_or(0.0).max(s.full_avg10));
        }
        self.escaped_now = u32::try_from(escapees.len()).unwrap_or(u32::MAX);
        self.escaped_rss_now = escapees
            .iter()
            .map(|e| e.rss_bytes)
            .fold(0, u64::saturating_add);
        self.escaped_rss_peak = self.escaped_rss_peak.max(self.escaped_rss_now);
        for e in escapees {
            let key = (e.pid, e.start);
            if !self.escaped.contains_key(&key) {
                notes.push(Note::Escaped(e.clone()));
            }
            self.escaped.insert(key, e);
        }
        notes
    }

    /// Records the outcome of moving an escapee back into the leaf; returns
    /// the note to log.
    pub fn reclaimed(&mut self, e: Escapee, result: std::io::Result<()>) -> Note {
        match result {
            Ok(()) => {
                self.reclaimed += 1;
                self.escaped_now = self.escaped_now.saturating_sub(1);
                self.escaped_rss_now = self.escaped_rss_now.saturating_sub(e.rss_bytes);
                Note::Reclaimed(e)
            }
            Err(err) => Note::ReclaimFailed(e, err.to_string()),
        }
    }

    /// The escapees seen so far, as `(pid, start)`: tracked while alive even
    /// after they are orphaned.
    #[must_use]
    pub fn known_escapees(&self) -> Vec<(u32, u64)> {
        self.escaped.keys().copied().collect()
    }

    /// Ticks observed.
    #[must_use]
    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// The last reading of the leaf.
    #[must_use]
    pub fn now(&self) -> LeafReading {
        self.now
    }

    /// Processes outside the leaf at the last tick and their resident set.
    #[must_use]
    pub fn escaped_now(&self) -> (u32, u64) {
        (self.escaped_now, self.escaped_rss_now)
    }

    /// The peaks of the job, `None` before the first tick.
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        (self.samples > 0).then(|| Summary {
            some_avg10_max: self.some_max,
            full_avg10_max: self.full_max,
            swap_peak_bytes: self.swap_peak,
            file_peak_bytes: self.file_peak,
            session_some_avg10_max: self.session_some_max,
            session_full_avg10_max: self.session_full_max,
            escaped_processes: u32::try_from(self.escaped.len()).unwrap_or(u32::MAX),
            escaped_rss_peak_bytes: self.escaped_rss_peak,
            reclaimed_processes: self.reclaimed,
            escaped_names: {
                let mut names: Vec<String> =
                    self.escaped.values().map(|e| e.comm.clone()).collect();
                names.sort();
                names.dedup();
                names
            },
        })
    }
}

/// The peaks of a job's memory telemetry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    /// Largest `some avg10` of the leaf, percent.
    pub some_avg10_max: f64,
    /// Largest `full avg10` of the leaf, percent.
    pub full_avg10_max: f64,
    /// Largest `memory.swap.current`, bytes.
    pub swap_peak_bytes: u64,
    /// Largest page cache (`file`), bytes.
    pub file_peak_bytes: u64,
    /// Largest `some avg10` of the session while the job ran, percent.
    pub session_some_avg10_max: Option<f64>,
    /// Largest `full avg10` of the session while the job ran, percent.
    pub session_full_avg10_max: Option<f64>,
    /// Distinct processes seen outside the leaf.
    pub escaped_processes: u32,
    /// Largest summed resident set of those processes at one tick, bytes.
    pub escaped_rss_peak_bytes: u64,
    /// Times one of them was moved back into the leaf.
    pub reclaimed_processes: u32,
    /// Their distinct `comm` names, sorted.
    pub escaped_names: Vec<String>,
}

impl Summary {
    /// One line for `sluice cache status`: `swap 1.2 GiB, memory stall
    /// 30.0% some / 4.0% full, desktop 12.0%, 1 process outside its cgroup
    /// (chrome, 95 MiB peak; moved back 1x)`; parts with nothing to say are
    /// left out, `no swap, no memory stall` when all are.
    #[must_use]
    pub fn text(&self, gib: impl Fn(u64) -> String) -> String {
        let mut parts = Vec::new();
        if self.swap_peak_bytes > 0 {
            parts.push(format!("swap {}", gib(self.swap_peak_bytes)));
        }
        if self.some_avg10_max >= 1.0 {
            parts.push(format!(
                "memory stall {:.1}% some / {:.1}% full",
                self.some_avg10_max, self.full_avg10_max
            ));
        }
        if let Some(s) = self.session_some_avg10_max.filter(|s| *s >= 1.0) {
            parts.push(format!("desktop {s:.1}%"));
        }
        if self.escaped_processes > 0 {
            parts.push(format!(
                "{} process{} outside its cgroup ({}, {} peak; moved back {}x)",
                self.escaped_processes,
                if self.escaped_processes == 1 {
                    ""
                } else {
                    "es"
                },
                self.escaped_names.join(" "),
                gib(self.escaped_rss_peak_bytes),
                self.reclaimed_processes
            ));
        }
        if parts.is_empty() {
            "no swap, no memory stall".to_owned()
        } else {
            parts.join(", ")
        }
    }

    /// Whether the job left anything worth a log line at its end: some
    /// swap, a measurable stall, or an escapee.
    #[must_use]
    pub fn notable(&self) -> bool {
        self.swap_peak_bytes > 0 || self.some_avg10_max >= 1.0 || self.escaped_processes > 0
    }
}

/// The desktop warning's state (hysteresis between [`SESSION_SOME_WARN`] and
/// [`SESSION_SOME_CLEAR`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionWatch {
    warned: bool,
}

/// A change of the desktop warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionChange {
    /// The session's pressure reached the warning level.
    Degraded,
    /// It fell back under the clear level.
    Recovered,
}

impl SessionWatch {
    /// Whether the desktop is reported degraded now.
    #[must_use]
    pub fn degraded(&self) -> bool {
        self.warned
    }

    /// Adds one reading of the session's pressure.
    pub fn observe(&mut self, p: Option<Pressure>) -> Option<SessionChange> {
        let p = p?;
        if !self.warned && p.some_avg10 >= SESSION_SOME_WARN {
            self.warned = true;
            Some(SessionChange::Degraded)
        } else if self.warned && p.some_avg10 < SESSION_SOME_CLEAR {
            self.warned = false;
            Some(SessionChange::Recovered)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    const PRESSURE: &str = "some avg10=12.50 avg60=3.00 avg300=1.00 total=123\n\
                            full avg10=4.25 avg60=1.00 avg300=0.50 total=45\n";

    #[test]
    fn pressure_reads_some_and_full_and_tolerates_a_missing_full_line() {
        assert_eq!(
            parse_pressure(PRESSURE),
            Some(Pressure {
                some_avg10: 12.5,
                full_avg10: 4.25
            })
        );
        assert_eq!(
            parse_pressure("some avg10=1.00 avg60=0 avg300=0 total=1\n"),
            Some(Pressure {
                some_avg10: 1.0,
                full_avg10: 0.0
            })
        );
        assert_eq!(parse_pressure("full avg10=1.00\n"), None);
        assert_eq!(parse_pressure("some avg10=nan\n"), None);
        assert_eq!(parse_pressure(""), None);
    }

    #[test]
    fn a_leaf_reading_takes_every_file_it_finds() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = dir.path();
        assert_eq!(read_leaf(leaf), LeafReading::default(), "nothing there");
        write(leaf, "memory.pressure", PRESSURE);
        write(leaf, "memory.swap.current", "4096\n");
        write(leaf, "memory.stat", "anon 1000\nfile 2000\nanon_thp 0\n");
        let r = read_leaf(leaf);
        assert_eq!(r.swap_bytes, Some(4096));
        assert_eq!(r.anon_bytes, Some(1000));
        assert_eq!(r.file_bytes, Some(2000));
        assert_eq!(r.pressure.unwrap().full_avg10, 4.25);
        assert_eq!(stat_field("anon_thp 5\nanon 7\n", "anon"), Some(7));
    }

    fn entry(id: u32, parent: u32, start: u64) -> ProcEntry {
        ProcEntry {
            pid: id,
            ppid: parent,
            pgrp: 1,
            start,
        }
    }

    #[test]
    fn escapees_are_the_descendants_living_outside_the_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let leaf = "/user.slice/u.service/app.slice/sluice-daemon.scope/jobs/j1";
        let scope = "/user.slice/u.service/app.slice/app-org.chromium.Chromium-12.scope";
        // 10 (in the leaf) -> 11 (in a sub-cgroup of the leaf) and 12 (moved
        // to a scope) -> 13 (child of the escapee, also outside); 20 is an
        // unrelated process outside.
        for (pid, cg, comm) in [
            (10, leaf.to_owned(), "node"),
            (11, format!("{leaf}/sub"), "worker"),
            (12, scope.to_owned(), "chrome-headless"),
            (13, scope.to_owned(), "chrome-headless"),
            (20, "/user.slice/other".to_owned(), "bash"),
        ] {
            write(root, &format!("{pid}/cgroup"), &format!("0::{cg}\n"));
            write(root, &format!("{pid}/comm"), &format!("{comm}\n"));
            write(root, &format!("{pid}/status"), "VmRSS:\t  1024 kB\n");
        }
        let table = [
            entry(10, 1, 100),
            entry(11, 10, 101),
            entry(12, 10, 102),
            entry(13, 12, 103),
            entry(20, 1, 104),
        ];
        let procs = HashSet::from([10]);
        let found = find_escapees(root, &table, &procs, leaf, &[]);
        let pids: Vec<u32> = found.iter().map(|e| e.pid).collect();
        assert_eq!(pids, [12, 13]);
        assert_eq!(found[0].comm, "chrome-headless");
        assert_eq!(found[0].cgroup, scope);
        assert_eq!(found[0].rss_bytes, 1024 * 1024);
        // The job's own processes gone, 12 orphaned (ppid 1): still tracked
        // through `known`, with its child; a reused pid (other start) is not.
        let orphaned = [entry(12, 1, 102), entry(13, 12, 103), entry(20, 1, 104)];
        let found = find_escapees(root, &orphaned, &HashSet::new(), leaf, &[(12, 102)]);
        assert_eq!(found.iter().map(|e| e.pid).collect::<Vec<_>>(), [12, 13]);
        let reused = [entry(12, 1, 999)];
        assert_eq!(
            find_escapees(root, &reused, &HashSet::new(), leaf, &[(12, 102)]),
            Vec::new()
        );
        // A leaf path that is only a prefix of another name is not "inside".
        assert!(!inside("/a/jobs/j10", "/a/jobs/j1"));
        assert!(inside("/a/jobs/j1/x", "/a/jobs/j1"));
    }

    #[test]
    fn leftovers_are_killed_only_when_the_pid_is_still_the_same_process() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let leaf = root.join("leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        // An empty leaf is not killed; no cgroup.kill file, nothing written.
        let me = std::process::id();
        // Our own pid with a wrong start time: never signalled.
        write(
            root,
            &format!("{me}/stat"),
            &format!("{me} (t) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 12345 0 0\n"),
        );
        assert_eq!(kill_leftovers(root, &leaf, &[(me, 999)]), (0, false));
        // A leaf with processes and a cgroup.kill file gets "1".
        write(&leaf, "cgroup.procs", "4242\n");
        write(&leaf, "cgroup.kill", "");
        assert_eq!(kill_leftovers(root, &leaf, &[]), (0, true));
        assert_eq!(
            std::fs::read_to_string(leaf.join("cgroup.kill")).unwrap(),
            "1"
        );
        // reclaim writes the pid into cgroup.procs.
        reclaim(&leaf, 77).unwrap();
        assert_eq!(
            std::fs::read_to_string(leaf.join("cgroup.procs")).unwrap(),
            "77"
        );
        assert!(reclaim(&root.join("gone"), 77).is_err());
    }

    #[test]
    fn the_session_cgroup_is_the_session_slice_of_the_user_manager() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mount = root.join("cg");
        let manager = "user.slice/user-1000.slice/user@1000.service";
        write(
            root,
            "proc/self/cgroup",
            &format!("0::/{manager}/app.slice/sluice-daemon.scope/daemon\n"),
        );
        let proc_root = root.join("proc");
        assert_eq!(
            session_cgroup(&proc_root, &mount),
            Some(mount.join(manager)),
            "no session.slice: the manager"
        );
        std::fs::create_dir_all(mount.join(manager).join("session.slice")).unwrap();
        assert_eq!(
            session_cgroup(&proc_root, &mount),
            Some(mount.join(manager).join("session.slice"))
        );
        write(
            root,
            "proc/self/cgroup",
            "0::/system.slice/sluice.service\n",
        );
        assert_eq!(
            session_cgroup(&proc_root, &mount),
            None,
            "not a user manager"
        );
    }

    fn reading(some: f64, full: f64, swap: u64) -> LeafReading {
        LeafReading {
            pressure: Some(Pressure {
                some_avg10: some,
                full_avg10: full,
            }),
            swap_bytes: Some(swap),
            anon_bytes: Some(1),
            file_bytes: Some(swap / 2),
        }
    }

    fn escapee(pid: u32, rss: u64) -> Escapee {
        Escapee {
            pid,
            start: u64::from(pid) * 10,
            comm: "chrome".to_owned(),
            cgroup: "/elsewhere".to_owned(),
            rss_bytes: rss,
        }
    }

    #[test]
    fn job_telemetry_keeps_peaks_and_notes_each_event_once() {
        let mut t = JobTelemetry::default();
        assert_eq!(t.summary(), None);
        let session = |s| {
            Some(Pressure {
                some_avg10: s,
                full_avg10: s / 2.0,
            })
        };
        assert_eq!(
            t.observe(reading(2.0, 0.0, 0), session(1.0), vec![]),
            Vec::new()
        );
        let notes = t.observe(
            reading(30.0, 12.0, 4096),
            session(20.0),
            vec![escapee(7, 100)],
        );
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(matches!(
            notes[0],
            Note::Stalled {
                swap_bytes: 4096,
                ..
            }
        ));
        assert!(matches!(&notes[1], Note::Escaped(e) if e.pid == 7));
        // Same escapee and still stalled: nothing new.
        let notes = t.observe(reading(5.0, 20.0, 1024), None, vec![escapee(7, 300)]);
        assert_eq!(notes, Vec::new());
        assert_eq!(t.escaped_now(), (1, 300));
        assert_eq!(t.known_escapees(), [(7, 70)]);
        let s = t.summary().unwrap();
        assert_eq!(s.some_avg10_max, 30.0);
        assert_eq!(s.full_avg10_max, 20.0);
        assert_eq!(s.swap_peak_bytes, 4096);
        assert_eq!(s.file_peak_bytes, 2048);
        assert_eq!(s.session_some_avg10_max, Some(20.0));
        assert_eq!(s.session_full_avg10_max, Some(10.0));
        assert_eq!(s.escaped_processes, 1);
        assert_eq!(s.escaped_rss_peak_bytes, 300);
        assert_eq!(s.escaped_names, ["chrome"]);
        assert_eq!(s.reclaimed_processes, 0);
        assert!(s.notable());
        // Moving it back: counted and no longer outside.
        assert!(matches!(
            t.reclaimed(escapee(7, 300), Ok(())),
            Note::Reclaimed(_)
        ));
        assert_eq!(t.escaped_now(), (0, 0));
        let failed = t.reclaimed(escapee(7, 300), Err(std::io::Error::other("denied")));
        assert!(matches!(failed, Note::ReclaimFailed(_, ref why) if why == "denied"));
        assert_eq!(t.summary().unwrap().reclaimed_processes, 1);
        assert!(!Summary::default().notable());
        let gib = |b: u64| format!("{b}B");
        assert_eq!(Summary::default().text(gib), "no swap, no memory stall");
        assert_eq!(
            s.text(gib),
            "swap 4096B, memory stall 30.0% some / 20.0% full, desktop 20.0%, \
             1 process outside its cgroup (chrome, 300B peak; moved back 0x)"
        );
        assert_eq!(t.samples(), 3);
    }

    #[test]
    fn the_session_watch_warns_once_and_rearms_under_the_clear_level() {
        let p = |s| {
            Some(Pressure {
                some_avg10: s,
                full_avg10: 0.0,
            })
        };
        let mut w = SessionWatch::default();
        assert_eq!(w.observe(None), None);
        assert_eq!(w.observe(p(9.9)), None);
        assert!(!w.degraded());
        assert_eq!(w.observe(p(10.0)), Some(SessionChange::Degraded));
        assert!(w.degraded());
        assert_eq!(w.observe(p(30.0)), None);
        assert_eq!(w.observe(p(6.0)), None, "between the levels");
        assert_eq!(w.observe(p(4.0)), Some(SessionChange::Recovered));
        assert_eq!(w.observe(p(12.0)), Some(SessionChange::Degraded));
    }
}
