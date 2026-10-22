//! Text helpers: ages, durations, log tails, the status rendering and the
//! output cap of `logs`.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{Duration, SystemTime};

use sluice_gate_proto::{StatusReport, WaitReason};

/// Human-readable age of an RFC 3339 timestamp (`3s`, `5m`, `2h`, `3d`),
/// as `sluice run` prints it.
pub(crate) fn age(rfc3339: &str) -> String {
    let Ok(then) = humantime::parse_rfc3339(rfc3339) else {
        return "?".to_owned();
    };
    let secs = SystemTime::now()
        .duration_since(then)
        .unwrap_or_default()
        .as_secs();
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Short human duration: `850ms`, `12.3s`, `4m05s`, `2h10m` (as `sluice status`).
pub(crate) fn dur(ms: u64) -> String {
    match ms {
        m if m < 1000 => format!("{m}ms"),
        m if m < 60_000 => format!("{:.1}s", Duration::from_millis(m).as_secs_f64()),
        m if m < 3_600_000 => format!("{}m{:02}s", m / 60_000, (m / 1000) % 60),
        m => format!("{}h{:02}m", m / 3_600_000, (m / 60_000) % 60),
    }
}

/// Reads every line of a file.
pub(crate) fn read_lines(path: &Path) -> std::io::Result<Vec<String>> {
    let file = File::open(path)?;
    Ok(BufReader::new(file).lines().map_while(Result::ok).collect())
}

/// Last `lines` lines of a job log without Sluice's framing lines and
/// surrounding blank lines; `fallback` (the daemon's tail) when the file
/// cannot be read. Same selection as the tail `sluice run` prints.
pub(crate) fn gate_tail(path: Option<&Path>, lines: usize, fallback: &[String]) -> Vec<String> {
    let all = match path.map(read_lines) {
        Some(Ok(all)) => all,
        _ => fallback.to_vec(),
    };
    let mut all: Vec<String> = all
        .into_iter()
        .filter(|l| {
            !(l.starts_with("# sluice job ")
                || l.starts_with("# command: ")
                || l.starts_with("# exit: "))
        })
        .collect();
    while all.last().is_some_and(|l| l.trim().is_empty()) {
        all.pop();
    }
    let first = all
        .iter()
        .position(|l| !l.trim().is_empty())
        .unwrap_or(all.len());
    all.drain(..first);
    let start = all.len().saturating_sub(lines);
    all.split_off(start)
}

/// Keeps the newest lines whose total size (with newlines) fits in `cap`
/// bytes; returns them and the number of bytes omitted from the front.
pub(crate) fn cap_lines(lines: &[String], cap: usize) -> (Vec<String>, usize) {
    let mut size = 0usize;
    let mut keep = lines.len();
    for (i, line) in lines.iter().enumerate().rev() {
        if size + line.len() + 1 > cap {
            break;
        }
        size += line.len() + 1;
        keep = i;
    }
    let omitted = lines[..keep].iter().map(|l| l.len() + 1).sum();
    (lines[keep..].to_vec(), omitted)
}

fn basename(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn reason(r: WaitReason) -> &'static str {
    match r {
        WaitReason::Budget => "budget",
        WaitReason::Psi => "psi",
        WaitReason::RepoCap => "repo-cap",
        WaitReason::HeadReserved => "head-reserved",
    }
}

/// Text form of a status report, as `sluice status` prints it; jobs of
/// `this_repo` are marked with `*`.
pub(crate) fn render_status(r: &StatusReport, this_repo: Option<&Path>) -> String {
    let mark = |repo: &Path| {
        if this_repo == Some(repo) { "*" } else { " " }
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "daemon:   pid {} (version {}), up {}, socket {}",
        r.daemon.pid,
        r.daemon.version,
        age(&r.daemon.started_at),
        r.daemon.socket.display()
    );
    let _ = writeln!(
        out,
        "budget:   {} of {} units busy",
        r.budget.used, r.budget.total
    );
    let _ = writeln!(out, "running:  {}", r.running.len());
    for j in &r.running {
        let _ = writeln!(
            out,
            " {}{:<16} {:<20} weight {:<2} {:>8}  {} subscriber(s)  job {}",
            mark(&j.repo),
            j.gate,
            basename(&j.repo),
            j.weight,
            dur(j.elapsed_ms),
            j.subscribers,
            j.job_id
        );
    }
    let _ = writeln!(out, "queued:   {}", r.queued.len());
    for (i, j) in r.queued.iter().enumerate() {
        let _ = writeln!(
            out,
            " {}#{:<3} {:<16} {:<20} weight {:<2} waiting {:>8}  ({})  job {}",
            mark(&j.repo),
            i + 1,
            j.gate,
            basename(&j.repo),
            j.weight,
            dur(j.waiting_ms),
            reason(j.reason),
            j.job_id
        );
    }
    let t = &r.totals;
    let _ = writeln!(
        out,
        "totals:   {} runs ({} passed, {} failed, {} timed out), {} hits, {} coalesced, {} saved",
        t.runs,
        t.passes,
        t.failures,
        t.timeouts,
        t.hits,
        t.coalesced,
        dur(t.time_saved_ms)
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_skips_framing_and_blank_edges() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("j.log");
        std::fs::write(
            &log,
            "# sluice job j: gate 'g' in /r\n# command: x\n\nboom\nmore\n\n# exit: 7 after 0.1s\n",
        )
        .unwrap();
        assert_eq!(gate_tail(Some(&log), 40, &[]), vec!["boom", "more"]);
        assert_eq!(gate_tail(Some(&log), 1, &[]), vec!["more"]);
        let missing = dir.path().join("none.log");
        assert_eq!(gate_tail(Some(&missing), 5, &["x".into()]), vec!["x"]);
    }

    #[test]
    fn cap_keeps_the_newest_lines() {
        let lines: Vec<String> = (0..10).map(|i| format!("line{i}")).collect();
        let (kept, omitted) = cap_lines(&lines, 17);
        assert_eq!(kept, vec!["line8", "line9"]);
        assert_eq!(omitted, 8 * 6);
        let (kept, omitted) = cap_lines(&lines, 1 << 16);
        assert_eq!((kept.len(), omitted), (10, 0));
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(dur(850), "850ms");
        assert_eq!(dur(12_300), "12.3s");
        assert_eq!(dur(245_000), "4m05s");
    }
}
