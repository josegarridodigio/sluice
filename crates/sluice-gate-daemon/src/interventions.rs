//! Recording of interventions on Sluice's cgroups (containment design,
//! section 5, daemon side).
//!
//! Two sources, both read-only:
//!
//! - [`MemoryWatcher`]: the `oom_kill` and `oom_group_kill` counters of
//!   `memory.events` in the leaves under the agents scope and `jobs/` (the
//!   kernel OOM killer acted inside that cgroup);
//! - [`parse_oomd_journal`]: `systemd-oomd` lines of the journal that name a
//!   Sluice cgroup (`Killed <path> due to memory pressure ...`).
//!
//! Everything here is pure or reads plain files, so tests feed fixtures. The
//! daemon polls both every few seconds, stores new rows in its database and
//! reports the recent ones in the `status` reply.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sluice_gate_core::cgroup::tree::{MemoryEvents, parse_memory_events};

/// Cause recorded for an `oom_kill` increment.
pub const CAUSE_OOM_KILL: &str = "oom_kill";
/// Cause recorded for an `oom_group_kill` increment.
pub const CAUSE_OOM_GROUP_KILL: &str = "oom_group_kill";
/// Cause recorded for a `systemd-oomd` kill under memory pressure.
pub const CAUSE_OOMD_PRESSURE: &str = "oomd_memory_pressure";
/// Cause recorded for a `systemd-oomd` kill on swap usage.
pub const CAUSE_OOMD_SWAP: &str = "oomd_swap";

/// One recorded intervention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterventionRow {
    /// Unix time, milliseconds.
    pub at_ms: u64,
    /// Agent session id (without the `s-` prefix) when the leaf is in a session.
    pub session: Option<String>,
    /// Cgroup path inside the Sluice scope (`s-abc/tools`, `jobs/<id>`).
    pub leaf: String,
    /// One of the `CAUSE_*` constants.
    pub cause: String,
    /// Human-readable detail.
    pub detail: Option<String>,
}

/// Session id and display path of a cgroup directory below `scope_dir`
/// (`<scope>/s-abc/tools` gives `(Some("abc"), "s-abc/tools")`; a job leaf
/// gives `(None, "jobs/<id>")`).
#[must_use]
pub fn leaf_identity(scope_dir: &Path, dir: &Path) -> (Option<String>, String) {
    let rel = dir.strip_prefix(scope_dir).unwrap_or(dir);
    let leaf = rel.to_string_lossy().into_owned();
    let session = rel
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .and_then(|first| first.strip_prefix("s-"))
        .map(str::to_owned);
    (session, leaf)
}

/// Watches `memory.events` of a set of leaves for kill counters that grow.
///
/// The first [`MemoryWatcher::poll`] only records the counters found (kills
/// that happened before the daemon started are not events of this run); a
/// leaf first seen later started at zero, so its counters count in full.
#[derive(Debug, Default)]
pub struct MemoryWatcher {
    seen: BTreeMap<PathBuf, (u64, u64)>,
    primed: bool,
}

impl MemoryWatcher {
    /// A watcher that has seen nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `memory.events` of each of `leaves` (`(scope_dir, leaf_dir)`
    /// pairs) and returns one row per counter that grew since the last poll.
    /// Leaves that disappeared are forgotten.
    pub fn poll(&mut self, leaves: &[(PathBuf, PathBuf)], now_ms: u64) -> Vec<InterventionRow> {
        let mut rows = Vec::new();
        let mut alive: BTreeMap<PathBuf, (u64, u64)> = BTreeMap::new();
        for (scope, dir) in leaves {
            let Ok(text) = fs::read_to_string(dir.join("memory.events")) else {
                continue;
            };
            let MemoryEvents {
                oom_kill,
                oom_group_kill,
                ..
            } = parse_memory_events(&text);
            let previous = match self.seen.get(dir) {
                Some(p) => Some(*p),
                None if self.primed => Some((0, 0)),
                None => None,
            };
            alive.insert(dir.clone(), (oom_kill, oom_group_kill));
            let Some((kill0, group0)) = previous else {
                continue;
            };
            let (session, leaf) = leaf_identity(scope, dir);
            for (cause, now, before, what) in [
                (
                    CAUSE_OOM_KILL,
                    oom_kill,
                    kill0,
                    "process(es) killed by the kernel OOM killer",
                ),
                (
                    CAUSE_OOM_GROUP_KILL,
                    oom_group_kill,
                    group0,
                    "group kill(s) by the kernel OOM killer",
                ),
            ] {
                if now > before {
                    rows.push(InterventionRow {
                        at_ms: now_ms,
                        session: session.clone(),
                        leaf: leaf.clone(),
                        cause: cause.to_owned(),
                        detail: Some(format!("{} {what}", now - before)),
                    });
                }
            }
        }
        self.seen = alive;
        self.primed = true;
        rows
    }
}

/// Leaf cgroups to watch: `s-*/{ui,tools}` under `agents_dir` and every
/// directory under `jobs_dir`, as `(scope_dir, leaf_dir)` pairs.
#[must_use]
pub fn watch_targets(
    agents_dir: &Path,
    daemon_scope_dir: &Path,
    jobs_dir: &Path,
) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let dirs_of = |parent: &Path| -> Vec<PathBuf> {
        fs::read_dir(parent)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default()
    };
    for session in dirs_of(agents_dir) {
        let is_session = session
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("s-"));
        if !is_session {
            continue;
        }
        for leaf in dirs_of(&session) {
            out.push((agents_dir.to_path_buf(), leaf));
        }
    }
    for job in dirs_of(jobs_dir) {
        out.push((daemon_scope_dir.to_path_buf(), job));
    }
    out
}

/// The cgroup path (`/user.slice/.../sluice.slice/...`) a `systemd-oomd`
/// kill line names, when it names a Sluice one.
fn killed_path(line: &str) -> Option<&str> {
    let rest = line.split_once("Killed ")?.1;
    let path = rest.split_whitespace().next()?;
    let sluice = path.split('/').any(|c| c == "sluice.slice");
    sluice.then_some(path)
}

/// Splits a killed cgroup path into the session id and the display leaf
/// relative to its Sluice scope (the component ending in `.scope`).
fn split_killed_path(path: &str) -> (Option<String>, String) {
    let path = path.strip_suffix("/cgroup.procs").unwrap_or(path);
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let after = comps
        .iter()
        .position(|c| {
            c.starts_with("sluice") && c.rsplit_once('.').is_some_and(|(_, e)| e == "scope")
        })
        .map_or(&comps[..], |i| &comps[i + 1..]);
    let leaf = if after.is_empty() {
        comps.last().copied().unwrap_or_default().to_owned()
    } else {
        after.join("/")
    };
    let session = after
        .first()
        .and_then(|first| first.strip_prefix("s-"))
        .map(str::to_owned);
    (session, leaf)
}

/// Parses `journalctl -o short-unix` output of `systemd-oomd` into rows for
/// the kills that name a Sluice cgroup. Lines that do not look like a kill,
/// name another cgroup or have no timestamp are skipped.
#[must_use]
pub fn parse_oomd_journal(text: &str) -> Vec<InterventionRow> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let Some(path) = killed_path(line) else {
            continue;
        };
        let Some(at_ms) = line
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<f64>().ok())
            .filter(|t| t.is_finite() && *t > 0.0)
            .map(unix_ms)
        else {
            continue;
        };
        let (session, leaf) = split_killed_path(path);
        let after = line
            .split_once("Killed ")
            .map_or("", |(_, rest)| rest)
            .split_once(char::is_whitespace)
            .map_or("", |(_, r)| r.trim());
        let cause = if after.contains("swap") {
            CAUSE_OOMD_SWAP
        } else {
            CAUSE_OOMD_PRESSURE
        };
        rows.push(InterventionRow {
            at_ms,
            session,
            leaf,
            cause: cause.to_owned(),
            detail: (!after.is_empty()).then(|| format!("systemd-oomd: {after}")),
        });
    }
    rows
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn unix_ms(seconds: f64) -> u64 {
    (seconds * 1000.0).round() as u64
}

#[cfg(test)]
#[allow(clippy::assert_is_empty)]
mod tests {
    use super::*;

    fn events(kill: u64, group: u64) -> String {
        format!("low 0\nhigh 0\nmax 0\noom 1\noom_kill {kill}\noom_group_kill {group}\n")
    }

    fn leaf(root: &Path, rel: &str, kill: u64, group: u64) -> (PathBuf, PathBuf) {
        let dir = root.join(rel);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("memory.events"), events(kill, group)).unwrap();
        (root.to_path_buf(), dir)
    }

    #[test]
    fn identity_of_session_and_job_leaves() {
        let scope = Path::new("/c/agents.scope");
        assert_eq!(
            leaf_identity(scope, &scope.join("s-abc/tools")),
            (Some("abc".to_owned()), "s-abc/tools".to_owned())
        );
        let daemon = Path::new("/c/daemon.scope");
        assert_eq!(
            leaf_identity(daemon, &daemon.join("jobs/j1")),
            (None, "jobs/j1".to_owned())
        );
    }

    #[test]
    fn memory_events_growth_is_reported_after_the_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let tools = leaf(dir.path(), "s-abc/tools", 2, 0);
        let mut w = MemoryWatcher::new();
        // Baseline: kills before the daemon looked are not events.
        assert!(w.poll(std::slice::from_ref(&tools), 1000).is_empty());
        // Unchanged: nothing.
        assert!(w.poll(std::slice::from_ref(&tools), 2000).is_empty());
        fs::write(tools.1.join("memory.events"), events(5, 1)).unwrap();
        let rows = w.poll(std::slice::from_ref(&tools), 3000);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cause, CAUSE_OOM_KILL);
        assert_eq!(rows[0].session.as_deref(), Some("abc"));
        assert_eq!(rows[0].leaf, "s-abc/tools");
        assert_eq!(rows[0].at_ms, 3000);
        assert!(rows[0].detail.as_deref().unwrap().starts_with("3 "));
        assert_eq!(rows[1].cause, CAUSE_OOM_GROUP_KILL);
        // Same counters again: no duplicate.
        assert!(w.poll(std::slice::from_ref(&tools), 4000).is_empty());
    }

    #[test]
    fn a_leaf_first_seen_after_the_baseline_counts_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = MemoryWatcher::new();
        assert!(w.poll(&[], 1000).is_empty());
        let job = leaf(dir.path(), "jobs/j1", 1, 0);
        let rows = w.poll(std::slice::from_ref(&job), 2000);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session, None);
        assert_eq!(rows[0].leaf, "jobs/j1");
    }

    #[test]
    fn a_vanished_leaf_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let job = leaf(dir.path(), "jobs/j1", 0, 0);
        let mut w = MemoryWatcher::new();
        let _ = w.poll(std::slice::from_ref(&job), 1000);
        fs::remove_dir_all(&job.1).unwrap();
        assert!(w.poll(std::slice::from_ref(&job), 2000).is_empty());
        assert!(w.seen.is_empty());
    }

    #[test]
    fn watch_targets_lists_session_leaves_and_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents.scope");
        let daemon = dir.path().join("daemon.scope");
        for d in [
            "agents.scope/s-a/ui",
            "agents.scope/s-a/tools",
            "agents.scope/holder",
            "daemon.scope/jobs/j1",
            "daemon.scope/daemon",
        ] {
            fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        let mut found: Vec<String> = watch_targets(&agents, &daemon, &daemon.join("jobs"))
            .into_iter()
            .map(|(_, d)| d.strip_prefix(dir.path()).unwrap().display().to_string())
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                "agents.scope/s-a/tools",
                "agents.scope/s-a/ui",
                "daemon.scope/jobs/j1"
            ]
        );
    }

    const UM: &str = "/user.slice/user-1000.slice/user@1000.service";

    #[test]
    fn oomd_journal_lines_naming_sluice_become_rows() {
        let text = format!(
            "-- No entries --\n\
             1729782345.123456 host systemd-oomd[812]: Killed {UM}/sluice.slice/sluice-agents.scope/s-abc_1/tools due to memory pressure for {UM}/sluice.slice/sluice-agents.scope being 41.2% > 30.00% for > 5s with reclaim activity\n\
             1729782399.5 host systemd-oomd[812]: Killed {UM}/app.slice/app-firefox.scope due to memory pressure for {UM} being 70% > 50% for > 20s\n\
             1729782400.0 host systemd-oomd[812]: Killed {UM}/sluice.slice/sluice-daemon.scope/jobs/j9/cgroup.procs due to swap used (9G) / total (10G) being more than 90.00%\n\
             garbage Killed {UM}/sluice.slice/x\n"
        );
        let rows = parse_oomd_journal(&text);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].at_ms, 1_729_782_345_123);
        assert_eq!(rows[0].session.as_deref(), Some("abc_1"));
        assert_eq!(rows[0].leaf, "s-abc_1/tools");
        assert_eq!(rows[0].cause, CAUSE_OOMD_PRESSURE);
        assert!(rows[0].detail.as_deref().unwrap().contains("30.00%"));
        assert_eq!(rows[1].session, None);
        assert_eq!(rows[1].leaf, "jobs/j9");
        assert_eq!(rows[1].cause, CAUSE_OOMD_SWAP);
    }

    #[test]
    fn the_test_layout_nested_under_the_sluice_slice_is_recognised() {
        let text = format!(
            "1729782345.0 h systemd-oomd[1]: Killed {UM}/sluice.slice/sluice-test.slice/sluice-test-7.slice/sluice-test-7-agents.scope/s-x/ui due to memory pressure for y\n"
        );
        let rows = parse_oomd_journal(&text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].leaf, "s-x/ui");
        assert_eq!(rows[0].session.as_deref(), Some("x"));
    }
}
