//! Recording of interventions on Sluice's cgroups (containment design,
//! section 5, daemon side).
//!
//! Two sources, both read-only:
//!
//! - [`MemoryWatcher`]: the `oom_kill` and `oom_group_kill` counters of
//!   `memory.events` in the leaves under the agents scope and `jobs/` (the
//!   kernel OOM killer acted, counted in the victim's cgroup also when the
//!   kill was a global one). A job leaf is removed when its job ends, so the
//!   daemon reads it itself at that moment ([`MemoryWatcher::settle_job`]) and
//!   the watcher keeps a backstop on the counters of `jobs/` and of the daemon
//!   scope, which outlive their children ([`MemoryWatcher::backstop`]);
//! - [`parse_oomd_journal`]: `systemd-oomd` lines of the journal that name a
//!   Sluice cgroup (`Killed <path> due to memory pressure ...`).
//!
//! Everything here is pure or reads plain files, so tests feed fixtures. The
//! daemon polls both every few seconds, stores new rows in its database and
//! reports the recent ones in the `status` reply.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sluice_gate_core::cgroup::tree::parse_memory_events;

/// Cause recorded for an `oom_kill` increment.
pub const CAUSE_OOM_KILL: &str = "oom_kill";
/// Cause recorded for an `oom_group_kill` increment.
pub const CAUSE_OOM_GROUP_KILL: &str = "oom_group_kill";
/// Cause recorded for a `systemd-oomd` kill under memory pressure.
pub const CAUSE_OOMD_PRESSURE: &str = "oomd_memory_pressure";
/// Cause recorded for a `systemd-oomd` kill on swap usage.
pub const CAUSE_OOMD_SWAP: &str = "oomd_swap";
/// Cause recorded when the Sluice memory guard cancels one of Sluice's own jobs.
pub const CAUSE_SLUICE_MEMORY_GUARD: &str = "sluice_memory_guard";

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
    backstop: Option<Backstop>,
}

/// `[oom_kill, oom_group_kill]`.
type Kills = [u64; 2];

fn kills_of(dir: &Path) -> Option<Kills> {
    let text = fs::read_to_string(dir.join("memory.events")).ok()?;
    let e = parse_memory_events(&text);
    Some([e.oom_kill, e.oom_group_kill])
}

fn sub(a: Kills, b: Kills) -> Kills {
    [a[0].saturating_sub(b[0]), a[1].saturating_sub(b[1])]
}

fn add(a: Kills, b: Kills) -> Kills {
    [a[0] + b[0], a[1] + b[1]]
}

/// State of the aggregate counters: `memory.events` of `jobs/` and of the
/// daemon scope count the kills of every descendant, also of the leaves that
/// were removed since, so growth no leaf explained is a job that died
/// unseen.
#[derive(Debug)]
struct Backstop {
    jobs_dir: PathBuf,
    scope_dir: PathBuf,
    last_jobs: Option<Kills>,
    last_scope: Option<Kills>,
    /// Kills of job leaves already turned into rows since the last pass.
    explained: Kills,
    /// Unexplained growth of `jobs/` and of the scope outside `jobs/` left
    /// at the previous pass (reported when it is still there at the next, so
    /// a leaf read a moment later does not make a false report).
    pending_jobs: Kills,
    pending_scope: Kills,
}

/// One row per counter of `now` that exceeds `before`.
fn growth_rows(
    scope: &Path,
    dir: &Path,
    now_ms: u64,
    before: Kills,
    now: Kills,
) -> Vec<InterventionRow> {
    let (session, leaf) = leaf_identity(scope, dir);
    let mut rows = Vec::new();
    for (cause, n, b, what) in [
        (
            CAUSE_OOM_KILL,
            now[0],
            before[0],
            "process(es) killed by the kernel OOM killer",
        ),
        (
            CAUSE_OOM_GROUP_KILL,
            now[1],
            before[1],
            "group kill(s) by the kernel OOM killer",
        ),
    ] {
        if n > b {
            rows.push(InterventionRow {
                at_ms: now_ms,
                session: session.clone(),
                leaf: leaf.clone(),
                cause: cause.to_owned(),
                detail: Some(format!("{} {what}", n - b)),
            });
        }
    }
    rows
}

impl MemoryWatcher {
    /// A watcher that has seen nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A watcher that also keeps the aggregate counters of `jobs_dir` and
    /// `scope_dir` ([`MemoryWatcher::backstop`]).
    #[must_use]
    pub fn with_backstop(jobs_dir: PathBuf, scope_dir: PathBuf) -> Self {
        Self {
            backstop: Some(Backstop {
                jobs_dir,
                scope_dir,
                last_jobs: None,
                last_scope: None,
                explained: [0; 2],
                pending_jobs: [0; 2],
                pending_scope: [0; 2],
            }),
            ..Self::default()
        }
    }

    /// Reads `memory.events` of each of `leaves` (`(scope_dir, leaf_dir)`
    /// pairs) and returns one row per counter that grew since the last poll.
    /// Leaves that disappeared are forgotten.
    pub fn poll(&mut self, leaves: &[(PathBuf, PathBuf)], now_ms: u64) -> Vec<InterventionRow> {
        let mut rows = Vec::new();
        let mut alive: BTreeMap<PathBuf, (u64, u64)> = BTreeMap::new();
        for (scope, dir) in leaves {
            let Some(now) = kills_of(dir) else {
                continue;
            };
            let previous = match self.seen.get(dir) {
                Some(p) => Some(*p),
                None if self.primed => Some((0, 0)),
                None => None,
            };
            alive.insert(dir.clone(), (now[0], now[1]));
            let Some((kill0, group0)) = previous else {
                continue;
            };
            let grown = growth_rows(scope, dir, now_ms, [kill0, group0], now);
            if let Some(b) = &mut self.backstop
                && dir.parent() == Some(b.jobs_dir.as_path())
            {
                b.explained = add(b.explained, sub(now, [kill0, group0]));
            }
            rows.extend(grown);
        }
        self.seen = alive;
        self.primed = true;
        rows
    }

    /// Settles a job leaf the daemon read itself at the end of its job (the
    /// leaf is removed right after, so the periodic poll would never see
    /// it): the rows for what grew beyond what [`MemoryWatcher::poll`]
    /// already reported, and the kills counted as explained for
    /// [`MemoryWatcher::backstop`].
    pub fn settle_job(
        &mut self,
        scope: &Path,
        dir: &Path,
        now_ms: u64,
        kills: (u64, u64),
    ) -> Vec<InterventionRow> {
        let before = self.seen.remove(dir).unwrap_or((0, 0));
        let now = [kills.0, kills.1];
        let grown = sub(now, [before.0, before.1]);
        if let Some(b) = &mut self.backstop {
            b.explained = add(b.explained, grown);
        }
        growth_rows(scope, dir, now_ms, [before.0, before.1], now)
    }

    /// Rows for kills that happened under `jobs/` (or elsewhere in the
    /// daemon scope) that no job leaf explained: a gate job died of OOM and
    /// its leaf was gone before anybody read it. Growth is reported when it
    /// is still unexplained at the next pass. The first pass only takes the
    /// baseline.
    pub fn backstop(&mut self, now_ms: u64) -> Vec<InterventionRow> {
        let Some(b) = &mut self.backstop else {
            return Vec::new();
        };
        let (Some(jobs), Some(scope)) = (kills_of(&b.jobs_dir), kills_of(&b.scope_dir)) else {
            return Vec::new();
        };
        let (Some(last_jobs), Some(last_scope)) = (b.last_jobs, b.last_scope) else {
            b.last_jobs = Some(jobs);
            b.last_scope = Some(scope);
            b.explained = [0; 2];
            return Vec::new();
        };
        let jobs_delta = sub(jobs, last_jobs);
        let scope_delta = sub(scope, last_scope);
        // Kills in the scope that were not in `jobs/`: `daemon/` itself.
        let outside = sub(scope_delta, jobs_delta);
        let jobs_left = sub(add(b.pending_jobs, jobs_delta), b.explained);
        let scope_left = add(b.pending_scope, outside);
        let mut rows = Vec::new();
        let (report_jobs, keep_jobs) = settle_pending(b.pending_jobs, jobs_left);
        b.pending_jobs = keep_jobs;
        let (report_scope, keep_scope) = settle_pending(b.pending_scope, scope_left);
        b.pending_scope = keep_scope;
        let (jobs_dir, scope_dir) = (b.jobs_dir.clone(), b.scope_dir.clone());
        rows.extend(unmatched_rows(
            &jobs_dir,
            &scope_dir,
            now_ms,
            report_jobs,
            "a gate job died of OOM (job unknown)",
        ));
        rows.extend(unmatched_rows(
            &scope_dir.join("daemon"),
            &scope_dir,
            now_ms,
            report_scope,
            "a process of the daemon's own cgroup died of OOM",
        ));
        b.last_jobs = Some(jobs);
        b.last_scope = Some(scope);
        b.explained = [0; 2];
        rows
    }
}

/// `(to report now, to keep for the next pass)` of an unexplained count:
/// reported when it was already unexplained at the previous pass.
fn settle_pending(previous: Kills, left: Kills) -> (Kills, Kills) {
    let report: Kills = std::array::from_fn(|k| if previous[k] > 0 { left[k] } else { 0 });
    let keep: Kills = std::array::from_fn(|k| if report[k] > 0 { 0 } else { left[k] });
    (report, keep)
}

fn unmatched_rows(
    dir: &Path,
    scope: &Path,
    now_ms: u64,
    kills: Kills,
    what: &str,
) -> Vec<InterventionRow> {
    let mut rows = growth_rows(scope, dir, now_ms, [0; 2], kills);
    for row in &mut rows {
        let n = row.detail.take().unwrap_or_default();
        row.detail = Some(format!("{what}: {n}"));
    }
    rows
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

/// The first `systemd-oomd` kill in `text` (journal output) that names the
/// leaf of job `job_id` (`jobs/<id>`).
#[must_use]
pub fn find_job_kill(text: &str, job_id: &str) -> Option<InterventionRow> {
    let leaf = format!("jobs/{job_id}");
    parse_oomd_journal(text)
        .into_iter()
        .find(|r| r.leaf == leaf)
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

    fn set_events(dir: &Path, kill: u64, group: u64) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("memory.events"), events(kill, group)).unwrap();
    }

    /// `daemon.scope/{jobs,daemon}` with counters, and a backstop watcher.
    fn backstop_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, MemoryWatcher) {
        let dir = tempfile::tempdir().unwrap();
        let scope = dir.path().join("daemon.scope");
        let jobs = scope.join("jobs");
        set_events(&scope, 0, 0);
        set_events(&jobs, 0, 0);
        set_events(&scope.join("daemon"), 0, 0);
        let w = MemoryWatcher::with_backstop(jobs.clone(), scope.clone());
        (dir, scope, jobs, w)
    }

    #[test]
    fn growth_of_jobs_without_a_job_record_is_reported_once_it_persists() {
        let (_dir, scope, jobs, mut w) = backstop_fixture();
        assert!(w.backstop(1000).is_empty(), "baseline");
        // A job died and its leaf is gone: only the aggregates moved.
        set_events(&jobs, 2, 1);
        set_events(&scope, 2, 1);
        assert!(w.backstop(2000).is_empty(), "first sight is held back");
        let rows = w.backstop(3000);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].leaf, "jobs");
        assert_eq!(rows[0].cause, CAUSE_OOM_KILL);
        assert_eq!(rows[0].at_ms, 3000);
        let detail = rows[0].detail.as_deref().unwrap();
        assert!(
            detail.starts_with("a gate job died of OOM (job unknown)"),
            "{detail}"
        );
        assert!(detail.contains("2 process(es)"), "{detail}");
        assert_eq!(rows[1].cause, CAUSE_OOM_GROUP_KILL);
        // Reported once.
        assert!(w.backstop(4000).is_empty());
        assert!(w.backstop(5000).is_empty());
    }

    #[test]
    fn a_job_record_explains_the_growth_of_the_aggregates() {
        let (_dir, scope, jobs, mut w) = backstop_fixture();
        let _ = w.poll(&[], 500);
        assert!(w.backstop(1000).is_empty());
        // The job ended: the daemon read its leaf and settled it.
        set_events(&jobs, 1, 0);
        set_events(&scope, 1, 0);
        let job = jobs.join("j1");
        let rows = w.settle_job(&scope, &job, 1500, (1, 0));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].leaf, "jobs/j1");
        assert_eq!(rows[0].cause, CAUSE_OOM_KILL);
        assert!(w.backstop(2000).is_empty());
        assert!(w.backstop(3000).is_empty(), "nothing left over");
    }

    #[test]
    fn a_leaf_the_poll_already_reported_is_not_reported_again_at_the_end() {
        let (_dir, scope, jobs, mut w) = backstop_fixture();
        let job = jobs.join("j1");
        set_events(&job, 0, 0);
        let target = (scope.clone(), job.clone());
        assert!(w.poll(std::slice::from_ref(&target), 500).is_empty());
        assert!(w.backstop(600).is_empty());
        set_events(&job, 1, 0);
        set_events(&jobs, 1, 0);
        set_events(&scope, 1, 0);
        // The 10 s poll sees the leaf while the job still runs...
        let rows = w.poll(std::slice::from_ref(&target), 1000);
        assert_eq!(rows.len(), 1);
        assert!(w.backstop(1000).is_empty(), "explained by the leaf");
        // ...and the end-of-job read (once per job) adds nothing.
        assert!(w.settle_job(&scope, &job, 1500, (1, 0)).is_empty());
        assert!(w.backstop(3000).is_empty());
        assert!(w.backstop(4000).is_empty());
    }

    #[test]
    fn a_kill_outside_jobs_is_the_daemons_own_cgroup() {
        let (_dir, scope, _jobs, mut w) = backstop_fixture();
        assert!(w.backstop(1000).is_empty());
        set_events(&scope, 1, 0);
        assert!(w.backstop(2000).is_empty());
        let rows = w.backstop(3000);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].leaf, "daemon");
        assert!(
            rows[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("daemon's own cgroup")
        );
    }

    #[test]
    fn a_watcher_without_backstop_or_unreadable_files_reports_nothing() {
        let mut plain = MemoryWatcher::new();
        assert!(plain.backstop(1000).is_empty());
        let dir = tempfile::tempdir().unwrap();
        let mut w = MemoryWatcher::with_backstop(dir.path().join("jobs"), dir.path().join("s"));
        assert!(w.backstop(1000).is_empty());
    }

    #[test]
    fn the_journal_line_of_a_job_is_found_by_its_leaf() {
        let text = format!(
            "1729782400.5 host systemd-oomd[812]: Killed {UM}/sluice.slice/sluice-daemon.scope/jobs/j9 due to memory pressure for {UM}/sluice.slice/sluice-daemon.scope being 40% > 30% for > 3s\n\
             1729782401.0 host systemd-oomd[812]: Killed {UM}/sluice.slice/sluice-daemon.scope/jobs/j10 due to memory pressure for x\n"
        );
        let row = find_job_kill(&text, "j9").unwrap();
        assert_eq!(row.leaf, "jobs/j9");
        assert_eq!(row.cause, CAUSE_OOMD_PRESSURE);
        assert_eq!(row.at_ms, 1_729_782_400_500);
        assert_eq!(find_job_kill(&text, "j1"), None, "no prefix matches");
        assert_eq!(find_job_kill("", "j9"), None);
    }
}
