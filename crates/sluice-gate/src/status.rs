//! `sluice status [--json]`: daemon, budget, jobs and totals.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use sluice_gate_core::dirs;
use sluice_gate_core::git::EnvMap;
use sluice_gate_proto::{PsiInfo, StatusReport, WaitReason};

use crate::run::{EXIT_ERROR, age};
use sluice_gate_proto::client::{self, ClientError};

/// Entry point of `sluice status`.
pub fn show(json: bool) -> ExitCode {
    let env: EnvMap = std::env::vars().collect();
    let report = dirs::socket_path(&env)
        .map_err(|e| ClientError::Other(e.into()))
        .and_then(|socket| client::connect(&socket))
        .and_then(client::Client::status);
    match report {
        Ok(report) => {
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(e) => {
                        eprintln!("sluice: error: {e}");
                        return ExitCode::from(EXIT_ERROR);
                    }
                }
            } else {
                let fallback = dirs::plan_socket(&env)
                    .ok()
                    .filter(|l| l.path == report.daemon.socket)
                    .and_then(|l| l.fallback_reason());
                print!(
                    "{}",
                    render(&report, &dirs::daemon_log_path(&env), fallback.as_deref())
                );
            }
            ExitCode::SUCCESS
        }
        Err(ClientError::NotRunning(_)) => {
            if json {
                println!("{{\"schema\":1,\"daemon\":null}}");
            } else {
                println!("sluice: no daemon running");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sluice: error: {e}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Short human duration: `850ms`, `12.3s`, `4m05s`, `2h10m`.
fn dur(ms: u64) -> String {
    match ms {
        m if m < 1000 => format!("{m}ms"),
        m if m < 60_000 => format!("{:.1}s", Duration::from_millis(m).as_secs_f64()),
        m if m < 3_600_000 => format!("{}m{:02}s", m / 60_000, (m / 1000) % 60),
        m => format!("{}h{:02}m", m / 3_600_000, (m / 60_000) % 60),
    }
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

/// The `pressure:` line: `cpu 12.3% mem 0.4% (ok)`.
fn pressure(psi: &PsiInfo) -> String {
    let pct = |v: Option<f64>| v.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.1}%"));
    let state = if !psi.enabled {
        "unavailable"
    } else if psi.blocking {
        "blocking admission"
    } else {
        "ok"
    };
    format!(
        "cpu {} mem {} ({state})",
        pct(psi.cpu_some_avg10),
        pct(psi.memory_some_avg10)
    )
}

/// Renders the text form of a status report; `socket_fallback` is the
/// reason the short socket fallback is in use, if it is.
fn render(r: &StatusReport, log_path: &Path, socket_fallback: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "daemon:   pid {} (version {}), up {}, socket {}",
        r.daemon.pid,
        r.daemon.version,
        age(&r.daemon.started_at),
        r.daemon.socket.display()
    );
    if let Some(reason) = socket_fallback {
        let _ = writeln!(out, "socket:   fallback in use ({reason})");
    }
    if let Some(level) = &r.daemon.log_level {
        let _ = writeln!(out, "log:      level {level}, {}", log_path.display());
    }
    let _ = writeln!(
        out,
        "budget:   {} of {} units busy",
        r.budget.used, r.budget.total
    );
    let _ = writeln!(out, "pressure: {}", pressure(&r.budget.psi));
    let _ = writeln!(out, "running:  {}", r.running.len());
    for j in &r.running {
        let _ = writeln!(
            out,
            "  {:<16} {:<20} weight {:<2} {:>8}  {} subscriber(s)  job {}",
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
            "  #{:<3} {:<16} {:<20} weight {:<2} waiting {:>8}  ({})",
            i + 1,
            j.gate,
            basename(&j.repo),
            j.weight,
            dur(j.waiting_ms),
            reason(j.reason)
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
    use sluice_gate_proto::{QueuedJob, RunningJob};

    #[test]
    fn durations_are_short() {
        assert_eq!(dur(850), "850ms");
        assert_eq!(dur(12_300), "12.3s");
        assert_eq!(dur(245_000), "4m05s");
        assert_eq!(dur(7_800_000), "2h10m");
    }

    #[test]
    fn render_lists_jobs_and_totals() {
        let mut r = StatusReport::default();
        r.budget.total = 8;
        r.budget.used = 4;
        r.running.push(RunningJob {
            gate: "test".into(),
            repo: "/x/repo".into(),
            weight: 4,
            subscribers: 2,
            ..RunningJob::default()
        });
        r.queued.push(QueuedJob {
            gate: "lint".into(),
            reason: WaitReason::RepoCap,
            ..QueuedJob::default()
        });
        r.totals.runs = 3;
        r.totals.time_saved_ms = 90_000;
        r.daemon.log_level = Some("debug".into());
        let text = render(&r, Path::new("/s/daemon.log"), None);
        assert!(!text.contains("fallback"), "{text}");
        let with = render(&r, Path::new("/s/daemon.log"), Some("/long is 130 bytes"));
        assert!(
            with.contains("socket:   fallback in use (/long is 130 bytes)"),
            "{with}"
        );
        assert!(
            text.contains("log:      level debug, /s/daemon.log"),
            "{text}"
        );
        assert!(text.contains("4 of 8 units busy"), "{text}");
        assert!(text.contains("test") && text.contains("repo") && text.contains("2 subscriber"));
        assert!(text.contains("#1") && text.contains("repo-cap"));
        assert!(
            text.contains("3 runs") && text.contains("1m30s saved"),
            "{text}"
        );
        assert!(
            text.contains("pressure: cpu n/a mem n/a (unavailable)"),
            "{text}"
        );
    }

    #[test]
    fn pressure_line() {
        let mut psi = PsiInfo {
            enabled: true,
            cpu_some_avg10: Some(12.34),
            memory_some_avg10: Some(0.4),
            blocking: false,
        };
        assert_eq!(pressure(&psi), "cpu 12.3% mem 0.4% (ok)");
        psi.blocking = true;
        psi.cpu_some_avg10 = Some(75.0);
        assert_eq!(pressure(&psi), "cpu 75.0% mem 0.4% (blocking admission)");
    }
}
