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
        WaitReason::Unknown => "unknown",
    }
}

/// The weight of a job as `configured\u{2192}effective` (`auto\u{2192}5`,
/// `10\u{2192}3`), or just the number when both agree or the daemon sent no
/// configured one. Same rule as `sluice status`.
fn weight_text(effective: u32, configured: Option<u32>, auto: Option<bool>) -> String {
    if auto == Some(true) {
        format!("auto\u{2192}{effective}")
    } else {
        match configured.filter(|c| *c != effective) {
            Some(c) => format!("{c}\u{2192}{effective}"),
            None => effective.to_string(),
        }
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
    if let Some(m) = &r.budget.memory {
        let _ = writeln!(out, "{}", m.status_line());
    }
    for j in &r.running {
        let _ = writeln!(
            out,
            " {}{:<16} {:<20} weight {:<2} {:>8}  {} subscriber(s){}  job {}",
            mark(&j.repo),
            j.gate,
            basename(&j.repo),
            weight_text(j.weight, j.weight_cfg, j.weight_auto),
            dur(j.elapsed_ms),
            j.subscribers,
            j.memory_text(),
            j.job_id
        );
    }
    let _ = writeln!(out, "queued:   {}", r.queued.len());
    for (i, j) in r.queued.iter().enumerate() {
        let _ = writeln!(
            out,
            " {}#{:<3} {:<16} {:<20} weight {:<2} waiting {:>8}  ({}{}){}  job {}",
            mark(&j.repo),
            i + 1,
            j.gate,
            basename(&j.repo),
            weight_text(j.weight, j.weight_cfg, j.weight_auto),
            dur(j.waiting_ms),
            reason(j.reason),
            j.wait_detail_text(),
            j.memory_text(),
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
    out.push_str(&r.containment_text());
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
    fn status_text_shows_configured_and_effective_weights() {
        use sluice_gate_proto::{QueuedJob, RunningJob};
        assert_eq!(weight_text(4, None, None), "4");
        assert_eq!(weight_text(4, Some(4), None), "4");
        assert_eq!(weight_text(3, Some(10), None), "10\u{2192}3");
        let mut r = StatusReport::default();
        r.running.push(RunningJob {
            gate: "test".into(),
            weight: 5,
            weight_cfg: Some(10),
            weight_auto: Some(true),
            ..RunningJob::default()
        });
        r.queued.push(QueuedJob {
            gate: "build".into(),
            weight: 3,
            weight_cfg: Some(10),
            ..QueuedJob::default()
        });
        let text = render_status(&r, None);
        assert!(text.contains("weight auto\u{2192}5"), "{text}");
        assert!(text.contains("weight 10\u{2192}3"), "{text}");
    }

    #[test]
    fn status_text_shows_the_memory_budget_and_the_wait_detail() {
        use sluice_gate_proto::{MemoryBudget, QueuedJob, RunningJob};
        let mut r = StatusReport::default();
        let plain = render_status(&r, None);
        assert!(
            !plain.contains("memory:") && !plain.contains("GiB"),
            "{plain}"
        );
        r.budget.memory = Some(MemoryBudget {
            available_bytes: 22 << 30,
            reserve_bytes: 3 << 30,
            headroom_bytes: 16 << 30,
        });
        r.running.push(RunningJob {
            gate: "test".into(),
            memory_bytes: Some(3 << 30),
            memory_expected_bytes: Some(18 << 30),
            memory_expected_learned: Some(true),
            ..RunningJob::default()
        });
        r.queued.push(QueuedJob {
            gate: "waiting".into(),
            wait_detail: Some("memory".into()),
            memory_expected_bytes: Some(18 << 30),
            memory_expected_learned: Some(true),
            ..QueuedJob::default()
        });
        let text = render_status(&r, None);
        assert!(
            text.contains(
                "memory:   22.0 GiB available, 3.0 GiB reserved, 16.0 GiB still to grow in running jobs"
            ),
            "{text}"
        );
        assert!(text.contains("mem 3.0 GiB/18.0 GiB"), "{text}");
        assert!(text.contains("(budget: memory)  mem 18.0 GiB"), "{text}");
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(dur(850), "850ms");
        assert_eq!(dur(12_300), "12.3s");
        assert_eq!(dur(245_000), "4m05s");
    }
}
