//! `sluice logs [<job-id>] [-n <lines>]`: print a job log.
//!
//! The daemon is asked first (it knows every job it ran); without a daemon,
//! or for a job it does not know (in-process fallback runs), the log
//! directory is scanned as in Phase 1.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sluice_gate_core::dirs as core_dirs;
use sluice_gate_core::git::EnvMap;

use crate::dirs;
use crate::run::EXIT_ERROR;
use sluice_gate_daemon::executor::spool;
use sluice_gate_daemon::store::{Store, Timeline};
use sluice_gate_proto::client;

/// Prints the log of `job_id`, or of the newest job when `None`.
/// With `lines`, only the last that many lines.
pub fn show(job_id: Option<&str>, lines: Option<usize>) -> ExitCode {
    let env: EnvMap = sluice_gate_core::config::process_env();
    if let Some((path, tail)) = from_daemon(&env, job_id, lines) {
        // Without `-n` the whole file is printed; the daemon only sends a tail.
        let all = read_lines(&path).unwrap_or(tail);
        print_lines(&path, &all, lines);
        return ExitCode::SUCCESS;
    }
    let dir = dirs::logs_dir(&env);
    let path = if let Some(id) = job_id {
        let log = dir.join(format!("{id}.log"));
        let spool = dir.join(format!("{id}.exec"));
        if !log.exists() && spool.exists() {
            spool
        } else {
            log
        }
    } else {
        let newest = fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(sluice_gate_core::logfile::is_job_log_name)
            })
            .map(|e| e.path())
            .max();
        let Some(newest) = newest else {
            eprintln!("sluice: no job logs in {}", dir.display());
            return ExitCode::from(EXIT_ERROR);
        };
        newest
    };
    match read_lines(&path) {
        Ok(all) => {
            print_lines(&path, &all, lines);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sluice: cannot open {}: {e}", path.display());
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// `sluice logs --timeline [<job>] [--since D]`: one line per job from the
/// database (read-only; works without a daemon).
pub fn timeline(job_id: Option<&str>, since: &str) -> ExitCode {
    let env: EnvMap = sluice_gate_core::config::process_env();
    let db = core_dirs::database_path(&env);
    let store = match Store::open_read_only(&db) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sluice: cannot read {}: {e}", db.display());
            return ExitCode::from(EXIT_ERROR);
        }
    };
    let rows = if let Some(id) = job_id {
        store
            .timeline(id)
            .map(|t| t.into_iter().collect::<Vec<_>>())
    } else {
        let period = match humantime::parse_duration(since) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("sluice: --since {since:?}: {e}");
                return ExitCode::from(2);
            }
        };
        let ms = u64::try_from(period.as_millis()).unwrap_or(u64::MAX);
        store.timelines_since(core_dirs::now_ms().saturating_sub(ms))
    };
    match rows {
        Ok(rows) if rows.is_empty() && job_id.is_some() => {
            eprintln!("sluice: no such job");
            ExitCode::from(EXIT_ERROR)
        }
        Ok(rows) => {
            if store.sched_version() == 0 {
                eprintln!(
                    "sluice: this database has no scheduler timeline yet (written by an older release)"
                );
            }
            for t in &rows {
                println!("{}", timeline_line(t));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sluice: cannot read {}: {e}", db.display());
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Milliseconds as seconds: `29.0s`.
#[allow(clippy::cast_precision_loss, reason = "millisecond durations")]
fn secs(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// Seconds relative to `base`, signed: `+31.2`.
#[allow(clippy::cast_precision_loss, reason = "millisecond offsets")]
fn rel(base: u64, at: u64) -> String {
    let d = at as f64 - base as f64;
    format!("{:+.1}", d / 1000.0)
}

/// `job gate repo: queued +0.0 admitted +31.2 started +31.3 done +43.9
/// pass; waited budget 29.0s, order 2.2s; demand 6.1/8; 3 requests`.
pub fn timeline_line(t: &Timeline) -> String {
    use std::fmt::Write as _;
    const CAUSES: [&str; 5] = ["budget", "psi", "repo-cap", "head-reserved", "order"];
    let j = &t.job;
    let base = j.queued_at;
    let repo = j.repo.file_name().map_or_else(
        || j.repo.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let mut out = format!("{} {} {repo}:", j.id, j.gate);
    if let Some(r) = t.requested_at {
        let _ = write!(out, " requested {}", rel(base, r));
    }
    let _ = write!(out, " queued +0.0");
    if let Some(a) = t.admitted_at {
        let _ = write!(out, " admitted {}", rel(base, a));
        if let Some(kind) = t.admission.as_deref().filter(|k| *k != "nominal") {
            let _ = write!(out, " ({kind})");
        }
    }
    if let Some(s) = j.started_at {
        let _ = write!(out, " started {}", rel(base, s));
    }
    match (j.finished_at, &j.outcome) {
        (Some(f), Some(o)) => {
            let _ = write!(out, " done {} {o}", rel(base, f));
        }
        _ => {
            let _ = write!(out, " {}", j.state.as_str());
        }
    }
    let waits: Vec<String> = CAUSES
        .iter()
        .zip(t.waits)
        .filter_map(|(c, w)| w.filter(|w| *w > 0).map(|w| (c, w)))
        .map(|(c, w)| format!("{c} {}", secs(w)))
        .collect();
    if !waits.is_empty() {
        let _ = write!(out, "; waited {}", waits.join(", "));
    }
    if let Some(d) = t.demand_milli {
        let cfg = t.weight_cfg.unwrap_or(j.weight);
        let _ = write!(out, "; demand {:.1}/{cfg}", f64::from(d) / 1000.0);
    } else if let Some(cfg) = t.weight_cfg.filter(|c| *c != j.weight) {
        let _ = write!(out, "; weight {}/{cfg}", j.weight);
    }
    if t.requests > 0 {
        let _ = write!(out, "; {} request(s)", t.requests);
        if t.detached > 0 {
            let _ = write!(out, " ({} left)", t.detached);
        }
    }
    if let Some(k) = &t.killed_by {
        let _ = write!(out, "; killed by {k}");
    }
    out
}

/// Asks a running daemon (never starts one) for the job's log.
fn from_daemon(
    env: &EnvMap,
    job_id: Option<&str>,
    lines: Option<usize>,
) -> Option<(PathBuf, Vec<String>)> {
    let socket = core_dirs::socket_path(env).ok()?;
    if !socket.exists() {
        return None;
    }
    let client = client::connect(&socket).ok()?;
    client.logs(job_id, lines.unwrap_or(0)).ok()
}

/// Lines of a job log; the output spool of an exec job (`<job>.exec`) is
/// decoded with stdout and stderr merged in arrival order.
fn read_lines(path: &Path) -> std::io::Result<Vec<String>> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    if reader.fill_buf()?.starts_with(spool::MAGIC) {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        let merged = spool::merged(&bytes).unwrap_or_default();
        return Ok(String::from_utf8_lossy(&merged)
            .lines()
            .map(str::to_owned)
            .collect());
    }
    Ok(reader.lines().map_while(Result::ok).collect())
}

fn print_lines(path: &Path, all: &[String], lines: Option<usize>) {
    let start = lines.map_or(0, |n| all.len().saturating_sub(n));
    eprintln!("# {}", path.display());
    for line in &all[start..] {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_gate_daemon::store::{JobRow, JobState};

    fn row() -> JobRow {
        JobRow {
            id: "j1".into(),
            key: None,
            tree: None,
            repo: PathBuf::from("/w/app"),
            gate: "test".into(),
            cmd: "npm test".into(),
            weight: 6,
            caller: "agent".into(),
            session: None,
            state: JobState::Done,
            outcome: Some("pass".into()),
            exit_code: Some(0),
            queued_at: 10_000,
            started_at: Some(41_300),
            finished_at: Some(53_900),
            log_path: None,
            pgid: None,
            proc_start_ticks: None,
            daemon_instance: "i".into(),
            subscribers_max: 3,
            coalesced: 2,
            marker_written: true,
        }
    }

    #[test]
    fn timeline_line_reads_like_the_design() {
        let t = Timeline {
            job: row(),
            requested_at: Some(9_900),
            admitted_at: Some(41_200),
            weight_cfg: Some(8),
            expected_ms: Some(12_000),
            waits: [Some(29_000), Some(0), None, Some(0), Some(2_200)],
            admission: Some("over".into()),
            killed_by: None,
            cpu_ms: Some(70_000),
            run_delay_ms: Some(3_000),
            demand_milli: Some(6_100),
            requests: 3,
            detached: 1,
        };
        assert_eq!(
            timeline_line(&t),
            "j1 test app: requested -0.1 queued +0.0 admitted +31.2 (over) started +31.3 \
             done +43.9 pass; waited budget 29.0s, order 2.2s; demand 6.1/8; 3 request(s) \
             (1 left)"
        );
        // A row written by 0.2.0: no timeline columns.
        let old = Timeline {
            job: row(),
            requested_at: None,
            admitted_at: None,
            weight_cfg: None,
            expected_ms: None,
            waits: [None; 5],
            admission: None,
            killed_by: Some("early-verdict".into()),
            cpu_ms: None,
            run_delay_ms: None,
            demand_milli: None,
            requests: 0,
            detached: 0,
        };
        assert_eq!(
            timeline_line(&old),
            "j1 test app: queued +0.0 started +31.3 done +43.9 pass; killed by early-verdict"
        );
    }
}
