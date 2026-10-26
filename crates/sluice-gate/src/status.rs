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
    let env: EnvMap = sluice_gate_core::config::process_env();
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
        WaitReason::Unknown => "unknown",
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

/// The `running:` and `queued:` sections of [`render`].
fn render_jobs(out: &mut String, r: &StatusReport) {
    use std::fmt::Write as _;
    let _ = writeln!(out, "running:  {}", r.running.len());
    for j in &r.running {
        let _ = writeln!(
            out,
            "  {:<16} {:<20} weight {:<2} {:>8}  {} subscriber(s){}  job {}",
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
        let detail = j.wait_detail_text();
        let expected = j
            .expected_ms
            .map_or_else(String::new, |ms| format!("  expected {}", dur(ms)));
        let memory = j.memory_text();
        let _ = writeln!(
            out,
            "  #{:<3} {:<16} {:<20} weight {:<2} waiting {:>8}  ({}{detail}){expected}{memory}",
            j.rank.unwrap_or(i + 1),
            j.gate,
            basename(&j.repo),
            weight_text(j.weight, j.weight_cfg, j.weight_auto),
            dur(j.waiting_ms),
            reason(j.reason)
        );
    }
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
    let mut extra = Vec::new();
    if let Some(cap) = r.budget.cap.filter(|c| *c > r.budget.total) {
        extra.push(format!("cap {cap}"));
    }
    if let Some(idle) = r.budget.idle_cpus {
        let cpus = r
            .budget
            .cpus
            .map_or_else(String::new, |c| format!(" of {c}"));
        extra.push(format!("{idle:.1}{cpus} cpus idle"));
    }
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(" ({})", extra.join(", "))
    };
    let _ = writeln!(
        out,
        "budget:   {} of {} units busy{extra}",
        r.budget.used, r.budget.total
    );
    let _ = writeln!(out, "pressure: {}", pressure(&r.budget.psi));
    if let Some(m) = &r.budget.memory {
        let _ = writeln!(out, "{}", m.status_line());
    }
    render_jobs(&mut out, r);
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
    if let (Some(idle), Some(wall)) = (t.idle_while_waiting_ms, t.waiting_wall_ms)
        && wall > 0
    {
        let _ = writeln!(
            out,
            "waiting:  {} with jobs queued, {} of it CPU-idle{}",
            dur(wall),
            dur(idle),
            t.over_admissions
                .filter(|n| *n > 0)
                .map_or_else(String::new, |n| format!(", {n} over-admitted"))
        );
    }
    out.push_str(&r.containment_text());
    out
}

/// The weight of a job as `configured→effective`: `auto→5` for
/// `weight = "auto"`, `10→3` when learning lowered a declared 10, and just
/// the number when both agree (or an older daemon sent no configured one).
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
    fn weights_show_configured_and_effective() {
        assert_eq!(weight_text(4, None, None), "4");
        assert_eq!(weight_text(4, Some(4), None), "4");
        assert_eq!(weight_text(3, Some(10), None), "10\u{2192}3");
        assert_eq!(weight_text(5, Some(10), Some(true)), "auto\u{2192}5");
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
        let text = render(&r, Path::new("/s/daemon.log"), None);
        assert!(text.contains("weight auto\u{2192}5"), "{text}");
        assert!(text.contains("weight 10\u{2192}3"), "{text}");
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
        assert!(!text.contains("waiting:"), "{text}");
    }

    #[test]
    fn render_shows_the_scheduler_fields_when_present() {
        let mut r = StatusReport::default();
        r.budget.total = 20;
        r.budget.used = 24;
        r.budget.cap = Some(30);
        r.budget.cpus = Some(20);
        r.budget.idle_cpus = Some(3.0);
        r.queued.push(QueuedJob {
            gate: "test".into(),
            rank: Some(1),
            wait_detail: Some("lane".into()),
            expected_ms: Some(12_000),
            ..QueuedJob::default()
        });
        r.totals.waiting_wall_ms = Some(60_000);
        r.totals.idle_while_waiting_ms = Some(6_000);
        r.totals.over_admissions = Some(2);
        let text = render(&r, Path::new("/l"), None);
        assert!(
            text.contains("24 of 20 units busy (cap 30, 3.0 of 20 cpus idle)"),
            "{text}"
        );
        assert!(text.contains("(budget: lane)  expected 12.0s"), "{text}");
        assert!(
            text.contains("waiting:  1m00s with jobs queued, 6.0s of it CPU-idle, 2 over-admitted"),
            "{text}"
        );
    }

    #[test]
    fn render_shows_memory_admission() {
        use sluice_gate_proto::MemoryBudget;
        let mut r = StatusReport::default();
        let plain = render(&r, Path::new("/l"), None);
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
        r.running.push(RunningJob {
            gate: "guess".into(),
            memory_bytes: Some(1 << 30),
            memory_expected_bytes: Some(9 << 30),
            memory_expected_learned: Some(false),
            ..RunningJob::default()
        });
        r.queued.push(QueuedJob {
            gate: "waiting".into(),
            wait_detail: Some("memory".into()),
            memory_expected_bytes: Some(18 << 30),
            memory_expected_learned: Some(true),
            ..QueuedJob::default()
        });
        let text = render(&r, Path::new("/l"), None);
        assert!(
            text.contains(
                "memory:   22.0 GiB available, 3.0 GiB reserved, 16.0 GiB still to grow in running jobs"
            ),
            "{text}"
        );
        assert!(text.contains("mem 3.0 GiB/18.0 GiB"), "{text}");
        assert!(text.contains("mem 1.0 GiB/~9.0 GiB"), "{text}");
        assert!(text.contains("(budget: memory)  mem 18.0 GiB"), "{text}");
    }

    #[test]
    fn render_shows_containment_sessions_and_interventions() {
        use sluice_gate_proto::{ContainmentInfo, Intervention, SessionUsage};
        let mut r = StatusReport::default();
        let plain = render(&r, Path::new("/l"), None);
        assert!(!plain.contains("containment"), "{plain}");
        r.containment = Some(ContainmentInfo {
            level: "full".into(),
            outside_jobs_cpu_percent: Some(12.34),
            ..ContainmentInfo::default()
        });
        r.sessions = Some(vec![SessionUsage {
            id: "7f3a".into(),
            cpu_seconds: 61.25,
            memory_bytes: Some(900 * 1024 * 1024),
            procs: Some(31),
            ..SessionUsage::default()
        }]);
        r.interventions = Some(vec![Intervention {
            at: "2026-10-24T10:00:00Z".into(),
            session: Some("7f3a".into()),
            leaf: "s-7f3a/tools".into(),
            cause: "oomd_memory_pressure".into(),
            detail: None,
        }]);
        let text = render(&r, Path::new("/l"), None);
        assert!(text.contains("containment: full\n"), "{text}");
        assert!(
            text.contains("  s-7f3a  cpu 61.2s  mem 900 MB  procs 31\n"),
            "{text}"
        );
        assert!(text.contains("outside jobs: 12.3% CPU\n"), "{text}");
        assert!(
            text.contains(
                "interventions:\n  2026-10-24T10:00:00Z  s-7f3a  s-7f3a/tools  oomd_memory_pressure\n"
            ),
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
