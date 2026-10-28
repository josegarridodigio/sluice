//! The daemon's side of stalled-job detection (ADR-0024): the cadence, the
//! reads, the notices and the logs. The decision itself is
//! [`crate::stall`]. Nothing here ends a job.

use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::Ordering;
use std::time::Duration;

use sluice_gate_core::config::StallTimeout;
use sluice_gate_core::dirs::now_ms;
use sluice_gate_proto::StalledJob;

use super::{JobEntry, JobEvent, JobRequest, Shared, State};
use crate::sampler;
use crate::stall::{self, Change, Sample, StallWatch};

/// What a read needs to know about a running job.
struct Target {
    id: String,
    pgid: i32,
    window_ms: Option<u64>,
    log_bytes: u64,
}

/// Whether the counters are due: the first call, then once per cadence (a
/// tenth less, so a tick that comes slightly early is not skipped).
pub(super) fn due(shared: &Shared, mono: u64) -> bool {
    let every = shared.config.stall.every_ms;
    let last = shared.stall_last.load(Ordering::Relaxed);
    if last != u64::MAX && mono.saturating_sub(last) < every - every / 10 {
        return false;
    }
    shared.stall_last.store(mono, Ordering::Relaxed);
    true
}

fn setting_of(request: &JobRequest) -> StallTimeout {
    match request {
        JobRequest::Gate(r) => StallTimeout::from_wire(r.gate.stall_timeout_ms),
        JobRequest::Exec(_) => StallTimeout::Auto,
    }
}

/// The running jobs with a process, and the window of each.
fn targets(shared: &Shared) -> Vec<Target> {
    let st = shared.state();
    st.jobs
        .iter()
        .filter(|(_, e)| !e.cancelled)
        .filter_map(|(id, e)| {
            let (process, handle) = (e.process.as_ref()?, e.handle.as_ref()?);
            let expected = st.sched.spec(id).and_then(|s| s.expected_ms);
            Some(Target {
                id: id.clone(),
                pgid: process.pgid,
                window_ms: stall::window_ms(setting_of(&e.request), expected, &shared.config.stall),
                log_bytes: handle.log_bytes(),
            })
        })
        .collect()
}

/// Reads the counters of every target: the leaf cgroup, or the process
/// group where the job has no leaf.
fn read_all(shared: &Shared, targets: &[Target], mono: u64) -> Vec<Option<Sample>> {
    let jobs_dir = shared
        .containment
        .as_ref()
        .map(|c| c.tree().layout.jobs_dir());
    let leaves: Vec<Option<stall::Readings>> = targets
        .iter()
        .map(|t| {
            jobs_dir
                .as_ref()
                .and_then(|dir| stall::read_leaf(&dir.join(&t.id)))
        })
        .collect();
    let groups: HashSet<i32> = targets
        .iter()
        .zip(&leaves)
        .filter(|(_, leaf)| leaf.is_none())
        .map(|(t, _)| t.pgid)
        .collect();
    let proc_root = &shared.config.proc_root;
    let members = sampler::group_members(proc_root, &groups);
    targets
        .iter()
        .zip(leaves)
        .map(|(t, leaf)| {
            let readings = leaf.or_else(|| {
                let pids = members.get(&t.pgid)?;
                stall::read_group(proc_root, pids)
            })?;
            Some(Sample {
                at: mono,
                readings,
                log_bytes: t.log_bytes,
            })
        })
        .collect()
}

/// What a change of one job leaves to log once the lock is released.
struct Notice {
    id: String,
    name: String,
    change: Change,
    weight: u32,
    memory: Option<u64>,
    flat: Vec<&'static str>,
}

/// Feeds a sample to the job's watch; on a change tells its subscribers (a
/// `progress` that carries the stall) and returns what to log.
fn apply(
    st: &mut State,
    shared: &Shared,
    t: &Target,
    sample: Option<Sample>,
    mono: u64,
) -> Option<Notice> {
    let weight = st.sched.spec(&t.id).map_or(0, |s| s.weight);
    let entry = st.jobs.get_mut(&t.id)?;
    let watch = entry.stall.get_or_insert_with(|| StallWatch::new(mono));
    let change = watch.observe(mono, sample, t.window_ms, shared.config.stall.max_gap_ms());
    if change == Change::None {
        return None;
    }
    let stalled_ms = watch.stalled_for(mono);
    let flat = watch.view(mono).map_or_else(Vec::new, |v| v.flat);
    let elapsed_ms = entry.started_at.map_or(0, |s| now_ms().saturating_sub(s));
    let _ = entry.events.send(JobEvent::Progress {
        elapsed_ms,
        log_bytes: t.log_bytes,
        stalled_ms,
    });
    Some(Notice {
        id: t.id.clone(),
        name: entry.request.name(),
        change,
        weight,
        memory: entry.usage.memory_now(),
        flat,
    })
}

fn dur(ms: u64) -> String {
    humantime::format_duration(Duration::from_secs(ms / 1000)).to_string()
}

fn log(n: &Notice) {
    match n.change {
        Change::Stalled { quiet_ms } => tracing::warn!(
            job = %n.id,
            gate = %n.name,
            quiet_ms,
            weight = n.weight,
            memory_bytes = n.memory,
            flat = ?n.flat,
            "job stalled: no progress for {} while holding {} unit(s) of budget; reported only, \
             Sluice never ends it",
            dur(quiet_ms),
            n.weight
        ),
        Change::Resumed { quiet_ms } => tracing::info!(
            job = %n.id,
            gate = %n.name,
            quiet_ms,
            "job active again after a stall of {}",
            dur(quiet_ms)
        ),
        Change::None => {}
    }
}

/// One read of the counters of every running job (blocking: reads `/proc`
/// and the cgroup files). Returns whether any job became stalled or active
/// again, so the scheduling loop looks at its shadow time again.
pub(super) fn tick(shared: &Shared, mono: u64) -> bool {
    let targets = targets(shared);
    if targets.is_empty() {
        return false;
    }
    let samples = read_all(shared, &targets, mono);
    let notices: Vec<Notice> = {
        let mut st = shared.state();
        targets
            .iter()
            .zip(samples)
            .filter_map(|(t, s)| apply(&mut st, shared, t, s, mono))
            .collect()
    };
    notices.iter().for_each(log);
    !notices.is_empty()
}

/// The running jobs that are stalled now.
pub(super) fn stalled_ids(st: &State) -> BTreeSet<String> {
    st.jobs
        .iter()
        .filter(|(_, e)| e.stall.as_ref().is_some_and(StallWatch::is_stalled))
        .map(|(id, _)| id.clone())
        .collect()
}

/// How long job `id` has shown no progress, while it is stalled.
pub(super) fn stalled_for(shared: &Shared, id: &str) -> Option<u64> {
    let st = shared.state();
    st.jobs
        .get(id)?
        .stall
        .as_ref()?
        .stalled_for(stall::mono_ms())
}

/// The `stalled` block of a job in `status`.
pub(super) fn view(entry: Option<&JobEntry>) -> Option<StalledJob> {
    let v = entry?.stall.as_ref()?.view(stall::mono_ms())?;
    Some(StalledJob {
        for_ms: v.for_ms,
        flat: v.flat.into_iter().map(str::to_owned).collect(),
    })
}

/// The longest quiet window of a job that has finished, in `daemon.log`: the
/// data to judge the rule (ADR-0024).
pub(super) fn log_finished(id: &str, outcome: &str, entry: &JobEntry) {
    let Some(watch) = entry.stall.as_ref() else {
        return;
    };
    let now = stall::mono_ms();
    let longest = watch.longest_quiet_ms(now);
    let was_stalled = watch.is_stalled();
    tracing::info!(
        job = %id,
        outcome,
        longest_quiet_ms = longest,
        stalled = was_stalled,
        "job finished: longest quiet window {}{}",
        dur(longest),
        if was_stalled { " (it ended stalled)" } else { "" }
    );
}
