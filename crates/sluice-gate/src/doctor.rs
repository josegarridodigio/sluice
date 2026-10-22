//! `sluice doctor [--json] [--bundle [PATH]]`: health checks of the local
//! installation and a redacted diagnostics bundle for bug reports.
//!
//! Every check reports `ok`, `warn` or `fail` with a one-line detail. The
//! exit code is 0 unless a check fails. Doctor never uses the network.
//!
//! The bundle is a `.tar.gz` (built with the `tar` binary) holding the
//! doctor report, versions, configs, `status.json`, `queue-status.json`,
//! recent daemon, client and MCP logs and the logs of the last failed
//! jobs. Every text file goes through [`redact`] first.

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use sluice_gate_core::config::{REPO_CONFIG_FILE, RepoConfig, UserConfig};
use sluice_gate_core::dirs;
use sluice_gate_core::git::{self, EnvMap, RepoPaths};
use sluice_gate_core::logfile;
use sluice_gate_daemon::server::LEASE_NAME;
use sluice_gate_daemon::store::{JobState, Store};
use sluice_gate_proto::client;
use sluice_gate_proto::{ClientMessage, FEATURE_QUEUE, ServerMessage};

use crate::run::EXIT_ERROR;

/// Lines kept from the tail of each log in the bundle.
const BUNDLE_LOG_LINES: usize = 2000;
/// Newest MCP logs included in the bundle.
const BUNDLE_MCP_LOGS: usize = 3;
/// Failed job logs included in the bundle.
const BUNDLE_FAILED_JOBS: usize = 5;
/// Markers directory size above which doctor warns.
const MARKERS_WARN_BYTES: u64 = 100 * 1024 * 1024;
/// Minimum supported git version.
const MIN_GIT: (u32, u32) = (2, 31);

/// Arguments of `sluice doctor`.
#[derive(Debug)]
pub struct DoctorArgs {
    /// Print JSON instead of text.
    pub json: bool,
    /// `Some(None)`: write the bundle to the default path; `Some(Some(p))`: to `p`.
    #[allow(clippy::option_option)]
    pub bundle: Option<Option<PathBuf>>,
}

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Fine.
    Ok,
    /// Works, but worth a look.
    Warn,
    /// Broken.
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Stable check name (`git`, `psi`, `daemon`, `repo-config`, ...).
    pub name: &'static str,
    /// Result.
    pub status: Status,
    /// One-line explanation.
    pub detail: String,
}

fn check(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
    let detail: String = detail.into();
    Check {
        name,
        status,
        detail: detail.replace('\n', " "),
    }
}

/// The full report.
#[derive(Debug, Serialize)]
struct Report<'a> {
    schema: u32,
    version: &'static str,
    ok: bool,
    checks: &'a [Check],
}

/// Entry point of `sluice doctor`.
pub fn run(args: &DoctorArgs) -> ExitCode {
    let env: EnvMap = std::env::vars().collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    let checks = collect(&env, &cwd);
    let ok = checks.iter().all(|c| c.status != Status::Fail);
    let report = Report {
        schema: 1,
        version: env!("CARGO_PKG_VERSION"),
        ok,
        checks: &checks,
    };
    let report_json = serde_json::to_string_pretty(&report).unwrap_or_default();
    if args.json {
        println!("{report_json}");
    } else {
        for c in &checks {
            println!("[{:<4}] {:<14} {}", c.status.label(), c.name, c.detail);
        }
        println!(
            "{}",
            if ok {
                "sluice: doctor found no failures"
            } else {
                "sluice: doctor found failures"
            }
        );
    }
    if let Some(target) = &args.bundle {
        match write_bundle(&env, &cwd, target.as_deref(), &report_json) {
            Ok(path) => {
                // Keep stdout machine-readable under --json.
                eprintln!("sluice: wrote {}", path.display());
                eprintln!(
                    "sluice: review the bundle before attaching it to an issue: logs can still \
                     contain project names, commands and paths that redaction does not know about"
                );
            }
            Err(e) => {
                eprintln!("sluice: error: cannot write the bundle: {e:#}");
                return ExitCode::from(EXIT_ERROR);
            }
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_ERROR)
    }
}

// ---------------------------------------------------------------- checks

/// Runs every check.
fn collect(env: &EnvMap, cwd: &Path) -> Vec<Check> {
    let mut out = vec![check_git(), check_psi()];
    out.push(check_state_dir(env));
    out.push(check_runtime_dir(env));
    out.extend(check_daemon(env));
    out.push(check_user_config(env));
    match git::discover(cwd, env) {
        Ok(paths) => out.extend(check_repo(env, &paths)),
        Err(_) => out.push(check(
            "repository",
            Status::Ok,
            "not inside a git repository; repository checks skipped",
        )),
    }
    out.push(check_path(env));
    out
}

/// Parses `git version 2.43.0` (or `2.39.5 (Apple Git-154)`).
fn parse_git_version(text: &str) -> Option<(u32, u32)> {
    let version = text
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = version.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn check_git() -> Check {
    match git::version() {
        Ok(text) => match parse_git_version(&text) {
            Some(v) if v >= MIN_GIT => check("git", Status::Ok, text),
            Some(_) => check(
                "git",
                Status::Fail,
                format!("{text}: Sluice needs git >= {}.{}", MIN_GIT.0, MIN_GIT.1),
            ),
            None => check(
                "git",
                Status::Warn,
                format!("cannot parse the version {text:?}"),
            ),
        },
        Err(e) => check("git", Status::Fail, format!("git not found: {e}")),
    }
}

fn check_psi() -> Check {
    let unreadable: Vec<&str> = ["cpu", "memory"]
        .into_iter()
        .filter(|f| fs::read_to_string(format!("/proc/pressure/{f}")).is_err())
        .collect();
    if unreadable.is_empty() {
        check("psi", Status::Ok, "/proc/pressure/{cpu,memory} readable")
    } else {
        check(
            "psi",
            Status::Warn,
            format!(
                "cannot read /proc/pressure/{}; admission control by system pressure is off",
                unreadable.join(", /proc/pressure/")
            ),
        )
    }
}

fn check_state_dir(env: &EnvMap) -> Check {
    let dir = dirs::state_dir(env);
    match fs::metadata(&dir) {
        Ok(m) if m.is_dir() && m.permissions().mode() & 0o200 != 0 => {
            check("state-dir", Status::Ok, dir.display().to_string())
        }
        Ok(_) => check(
            "state-dir",
            Status::Fail,
            format!("{} is not a writable directory", dir.display()),
        ),
        Err(_) => check(
            "state-dir",
            Status::Warn,
            format!(
                "{} does not exist yet (created on first use)",
                dir.display()
            ),
        ),
    }
}

fn check_runtime_dir(env: &EnvMap) -> Check {
    let Some(dir) = dirs::runtime_dir_path(env) else {
        return check(
            "runtime-dir",
            Status::Fail,
            "cannot determine the runtime directory",
        );
    };
    let own_uid = fs::metadata("/proc/self").map(|m| m.uid()).ok();
    match fs::symlink_metadata(&dir) {
        Ok(m) if !m.is_dir() => check(
            "runtime-dir",
            Status::Fail,
            format!("{} is not a directory", dir.display()),
        ),
        Ok(m) if Some(m.uid()) != own_uid => check(
            "runtime-dir",
            Status::Fail,
            format!("{} is not owned by the current user", dir.display()),
        ),
        Ok(m) if m.mode() & 0o777 != 0o700 => check(
            "runtime-dir",
            Status::Fail,
            format!(
                "{} has mode {:o}, expected 700",
                dir.display(),
                m.mode() & 0o777
            ),
        ),
        Ok(_) => check(
            "runtime-dir",
            Status::Ok,
            format!("{} (mode 700)", dir.display()),
        ),
        Err(_) => check(
            "runtime-dir",
            Status::Warn,
            format!(
                "{} does not exist yet (created on first use)",
                dir.display()
            ),
        ),
    }
}

/// Socket, daemon version and lease checks.
fn check_daemon(env: &EnvMap) -> Vec<Check> {
    let mut out = Vec::new();
    let installed = env!("CARGO_PKG_VERSION");
    let socket = dirs::socket_path_no_create(env);
    let mut live_pid = None;
    match socket {
        Some(socket) if socket.exists() => {
            match client::connect(&socket).and_then(client::Client::status) {
                Ok(report) => {
                    live_pid = Some(report.daemon.pid);
                    out.push(check(
                        "daemon",
                        Status::Ok,
                        format!(
                            "answering on {} (pid {}, version {})",
                            socket.display(),
                            report.daemon.pid,
                            report.daemon.version
                        ),
                    ));
                    out.push(if report.daemon.version == installed {
                    check("daemon-version", Status::Ok, format!("daemon and binary are {installed}"))
                } else {
                    check(
                        "daemon-version",
                        Status::Warn,
                        format!(
                            "daemon is {} but this binary is {installed}; run 'sluice daemon restart'",
                            report.daemon.version
                        ),
                    )
                });
                }
                Err(e) => out.push(check(
                    "daemon",
                    Status::Fail,
                    format!(
                        "socket {} exists but the daemon does not answer ({e}); \
                     run 'sluice daemon restart --force'",
                        socket.display()
                    ),
                )),
            }
        }
        _ => out.push(check(
            "daemon",
            Status::Ok,
            "not running (it starts on demand)",
        )),
    }
    out.push(check_lease(env, live_pid));
    out
}

/// True while `pid` exists and is not a zombie.
fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().next())
            .is_some_and(|state| state != "Z")
    })
}

fn check_lease(env: &EnvMap, live_pid: Option<u32>) -> Check {
    let db = dirs::database_path(env);
    if !db.exists() {
        return check("lease", Status::Ok, "no database yet");
    }
    let lease = match Store::open_read_only(&db).and_then(|s| s.lease(LEASE_NAME)) {
        Ok(l) => l,
        Err(e) => return check("lease", Status::Warn, format!("cannot read the lease: {e}")),
    };
    let Some((owner, ..)) = lease else {
        return check(
            "lease",
            Status::Ok,
            "no lease row (no daemon has run, or it stopped cleanly)",
        );
    };
    let owner_pid = owner.rsplit(':').next().and_then(|p| p.parse::<u32>().ok());
    match (live_pid, owner_pid) {
        (Some(live), Some(owner)) if live == owner => check(
            "lease",
            Status::Ok,
            format!("held by the live daemon (pid {live})"),
        ),
        (Some(live), _) => check(
            "lease",
            Status::Fail,
            format!("lease owner is {owner:?} but the answering daemon is pid {live}"),
        ),
        (None, Some(pid)) if alive(pid) => check(
            "lease",
            Status::Warn,
            format!("lease held by pid {pid}, which is alive but not answering on the socket"),
        ),
        (None, _) => check(
            "lease",
            Status::Warn,
            format!("stale lease of {owner:?} (no live daemon); it expires on its own"),
        ),
    }
}

fn check_user_config(env: &EnvMap) -> Check {
    let path = UserConfig::path(env);
    if !path.exists() {
        return check(
            "user-config",
            Status::Ok,
            format!("{} not present (defaults)", path.display()),
        );
    }
    match UserConfig::load(env) {
        Ok(_) => check(
            "user-config",
            Status::Ok,
            format!("{} parses", path.display()),
        ),
        Err(e) => check("user-config", Status::Fail, e.to_string()),
    }
}

/// Checks that need a repository.
fn check_repo(env: &EnvMap, paths: &RepoPaths) -> Vec<Check> {
    let mut out = Vec::new();
    out.push(match RepoConfig::load(&paths.toplevel) {
        Ok(cfg) => check(
            "repo-config",
            Status::Ok,
            format!(
                "{} parses ({} gate(s))",
                paths.toplevel.join(REPO_CONFIG_FILE).display(),
                cfg.gates.len()
            ),
        ),
        Err(e) => check("repo-config", Status::Fail, e.to_string()),
    });
    out.push(check_hooks(env, paths));
    out.push(check_markers(paths));
    out
}

fn check_hooks(env: &EnvMap, paths: &RepoPaths) -> Check {
    let probe = git::probe_env(env, Some(paths));
    let dir = git::run(
        &["config", "--get", "core.hooksPath"],
        &paths.toplevel,
        &probe,
        true,
    )
    .ok()
    .map(|v| v.trim().to_owned())
    .filter(|v| !v.is_empty())
    .map_or_else(
        || paths.common_dir.join("hooks"),
        |v| {
            let v = v.strip_prefix("~/").map_or_else(
                || v.clone(),
                |rest| format!("{}/{rest}", env.get("HOME").map_or("", String::as_str)),
            );
            paths.toplevel.join(v)
        },
    );
    let calls_sluice = fs::read_dir(&dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name();
            !name.to_string_lossy().ends_with(".sample")
                && fs::read_to_string(e.path()).is_ok_and(|text| text.contains("sluice"))
        })
    });
    if calls_sluice {
        check(
            "hooks",
            Status::Ok,
            format!("a hook in {} calls sluice", dir.display()),
        )
    } else {
        check(
            "hooks",
            Status::Warn,
            format!(
                "no hook in {} calls sluice (see 'sluice hook print'); ignore this if a hook manager owns the hooks",
                dir.display()
            ),
        )
    }
}

fn dir_size(dir: &Path) -> (u64, usize) {
    let mut total = (0, 0);
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_dir() {
                let (b, n) = dir_size(&entry.path());
                total = (total.0 + b, total.1 + n);
            } else {
                total = (total.0 + meta.len(), total.1 + 1);
            }
        }
    }
    total
}

fn check_markers(paths: &RepoPaths) -> Check {
    let dir = paths.common_dir.join("sluice").join("markers");
    let (bytes, files) = dir_size(&dir);
    let detail = format!(
        "{} file(s), {} KiB in {}",
        files,
        bytes / 1024,
        dir.display()
    );
    if bytes > MARKERS_WARN_BYTES {
        check(
            "markers",
            Status::Warn,
            format!("{detail}; run 'sluice cache clear' to reclaim space"),
        )
    } else {
        check("markers", Status::Ok, detail)
    }
}

fn check_path(env: &EnvMap) -> Check {
    let Ok(exe) = std::env::current_exe().and_then(fs::canonicalize) else {
        return check(
            "path",
            Status::Warn,
            "cannot resolve the running executable",
        );
    };
    let found = env
        .get("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(p))
        .map(|d| d.join("sluice"))
        .find(|c| {
            fs::metadata(c).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        });
    match found {
        None => check(
            "path",
            Status::Warn,
            format!(
                "no 'sluice' in PATH; hooks that call 'sluice' will fail (running {})",
                exe.display()
            ),
        ),
        Some(p) => match fs::canonicalize(&p) {
            Ok(real) if real == exe => check(
                "path",
                Status::Ok,
                format!("{} is this binary", p.display()),
            ),
            Ok(real) => check(
                "path",
                Status::Warn,
                format!(
                    "'sluice' in PATH is {} but you are running {}",
                    real.display(),
                    exe.display()
                ),
            ),
            Err(e) => check(
                "path",
                Status::Warn,
                format!("cannot resolve {}: {e}", p.display()),
            ),
        },
    }
}

// ---------------------------------------------------------------- redaction

static USERINFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s@]+@").expect("valid regex"));

static SECRET_PAIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([A-Za-z0-9_.-]*(?:token|secret|password|passwd|auth|key|credential)[A-Za-z0-9_.-]*)(["']?\s*[=:]\s*)((?:Bearer|Basic|Token)\s+\S+|"[^"\n]*"|'[^'\n]*'|[^\s"',;]+)"#,
    )
    .expect("valid regex")
});

/// Replaces `home` by `~` where it ends a path component (`/home/jo` must
/// not match inside `/home/joe`).
fn replace_path_prefix(text: &str, home: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(home) {
        let after = &rest[i + home.len()..];
        out.push_str(&rest[..i]);
        if after.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '-' || c == '.') {
            out.push_str(home);
        } else {
            out.push('~');
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Removes what should not leave the machine: the home directory becomes
/// `~`, values of keys that look like secrets (`token`, `secret`,
/// `password`, `passwd`, `auth`, `key`, `credential`) become `<redacted>`
/// and URLs lose their `user:password@` part.
#[must_use]
pub fn redact(text: &str, home: Option<&str>) -> String {
    let mut out = match home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| h.len() > 1)
    {
        Some(h) => replace_path_prefix(text, h),
        None => text.to_owned(),
    };
    out = USERINFO.replace_all(&out, "$1").into_owned();
    SECRET_PAIR
        .replace_all(&out, "${1}${2}<redacted>")
        .into_owned()
}

// ---------------------------------------------------------------- bundle

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = lines[start..].join("\n");
    out.push('\n');
    out
}

fn timestamp_compact() -> String {
    dirs::now_rfc3339().replace(['-', ':'], "")
}

/// Stages the files, redacts them and packs them with `tar`.
fn write_bundle(
    env: &EnvMap,
    cwd: &Path,
    target: Option<&Path>,
    report_json: &str,
) -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    let stamp = timestamp_compact();
    let name = format!("sluice-doctor-{stamp}");
    let out_path = match target {
        Some(p) => std::path::absolute(p)?,
        None => std::path::absolute(format!("{name}.tar.gz"))?,
    };
    let staging = tempfile::tempdir().context("creating the staging directory")?;
    let root = staging.path().join(&name);
    fs::create_dir_all(root.join("logs/jobs"))?;
    let home = env.get("HOME").cloned();
    let put = |rel: &str, text: &str| -> anyhow::Result<()> {
        fs::write(root.join(rel), redact(text, home.as_deref()))
            .with_context(|| format!("writing {rel}"))
    };

    put("doctor.json", report_json)?;
    put("version.txt", &version_text())?;

    let user_cfg = UserConfig::path(env);
    if let Ok(text) = fs::read_to_string(&user_cfg) {
        put("user-config.toml", &text)?;
    }
    let repo = git::discover(cwd, env).ok();
    if let Some(paths) = &repo
        && let Ok(text) = fs::read_to_string(paths.toplevel.join(REPO_CONFIG_FILE))
    {
        put("repo-sluice.toml", &text)?;
    }

    let socket = dirs::socket_path_no_create(env).filter(|s| s.exists());
    let status = socket
        .as_deref()
        .and_then(|s| client::connect(s).and_then(client::Client::status).ok());
    put(
        "status.json",
        &match &status {
            Some(r) => serde_json::to_string_pretty(r)?,
            None => "{\"schema\":1,\"daemon\":null}".to_owned(),
        },
    )?;
    if let Some(socket) = &socket
        && let Some(text) = queue_status_json(socket, repo.as_ref())
    {
        put("queue-status.json", &text)?;
    }

    let logs = dirs::logs_dir(env);
    for (src, rel) in [
        (dirs::daemon_log_path(env), "logs/daemon.log"),
        (logs.join(logfile::CLIENT_LOG), "logs/client.log"),
    ] {
        if let Ok(bytes) = fs::read(&src) {
            put(
                rel,
                &tail_lines(&String::from_utf8_lossy(&bytes), BUNDLE_LOG_LINES),
            )?;
        }
    }
    for src in newest_mcp_logs(&logs) {
        if let (Ok(bytes), Some(file)) = (fs::read(&src), src.file_name()) {
            put(
                &format!("logs/{}", file.to_string_lossy()),
                &tail_lines(&String::from_utf8_lossy(&bytes), BUNDLE_LOG_LINES),
            )?;
        }
    }
    for (id, src) in failed_job_logs(env) {
        if let Ok(bytes) = fs::read(&src) {
            put(
                &format!("logs/jobs/{id}.log"),
                &tail_lines(&String::from_utf8_lossy(&bytes), BUNDLE_LOG_LINES),
            )?;
        }
    }

    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let result = Command::new("tar")
        .arg("-czf")
        .arg(&out_path)
        .arg("-C")
        .arg(staging.path())
        .arg(&name)
        .output()
        .context("running tar")?;
    if !result.status.success() {
        anyhow::bail!(
            "tar failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(out_path)
}

fn version_text() -> String {
    let mut out = format!(
        "sluice {} (cache format {})\n",
        env!("CARGO_PKG_VERSION"),
        sluice_gate_core::CACHE_FORMAT_VERSION
    );
    let _ = writeln!(
        out,
        "{}",
        git::version().unwrap_or_else(|e| format!("git: {e}"))
    );
    if let Ok(v) = fs::read_to_string("/proc/version") {
        out.push_str(v.trim());
        out.push('\n');
    }
    out
}

fn queue_status_json(socket: &Path, repo: Option<&RepoPaths>) -> Option<String> {
    let conn = client::connect(socket).ok()?;
    if !conn.supports(FEATURE_QUEUE) {
        return None;
    }
    let msg = ClientMessage::QueueStatus {
        id: 1,
        repo: repo.map(|r| r.common_dir.clone()),
    };
    match conn.request(&msg) {
        Ok(ServerMessage::QueueStatus { status, .. }) => serde_json::to_string_pretty(&status).ok(),
        _ => None,
    }
}

fn newest_mcp_logs(logs: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(logs)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(logfile::is_mcp_log_name))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files
        .into_iter()
        .take(BUNDLE_MCP_LOGS)
        .map(|(_, p)| p)
        .collect()
}

/// Logs of the most recent failed, timed out or errored jobs, newest first.
fn failed_job_logs(env: &EnvMap) -> Vec<(String, PathBuf)> {
    let logs = dirs::logs_dir(env);
    let Ok(store) = Store::open_read_only(&dirs::database_path(env)) else {
        return Vec::new();
    };
    let Ok(mut jobs) = store.jobs_in_state(JobState::Done) else {
        return Vec::new();
    };
    jobs.retain(|j| matches!(j.outcome.as_deref(), Some("fail" | "timeout" | "error")));
    jobs.sort_by_key(|j| std::cmp::Reverse(j.finished_at));
    jobs.into_iter()
        .filter(|j| !j.id.is_empty() && !j.id.contains(['/', '\\']) && !j.id.contains(".."))
        .take(BUNDLE_FAILED_JOBS)
        .map(|j| {
            let path = j
                .log_path
                .clone()
                .unwrap_or_else(|| logs.join(format!("{}.log", j.id)));
            (j.id, path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_versions_parse() {
        assert_eq!(parse_git_version("git version 2.43.0"), Some((2, 43)));
        assert_eq!(
            parse_git_version("git version 2.39.5 (Apple Git-154)"),
            Some((2, 39))
        );
        assert_eq!(parse_git_version("nonsense"), None);
        assert!(parse_git_version("git version 2.30.9").unwrap() < MIN_GIT);
    }

    #[test]
    fn redaction_covers_home_secrets_and_urls() {
        let text = "path /home/jose/x GITHUB_TOKEN=abc\n\
                    {\"api_key\": \"s3cr3t\"} Authorization: Bearer xyz\n\
                    clone https://user:pw@github.com/o/r.git ok\n\
                    plain line gate=test";
        let out = redact(text, Some("/home/jose"));
        assert!(out.contains("path ~/x GITHUB_TOKEN=<redacted>"), "{out}");
        assert!(
            out.contains("\"api_key\": <redacted>") || out.contains("api_key\": <redacted>"),
            "{out}"
        );
        assert!(out.contains("Authorization: <redacted>"), "{out}");
        assert!(out.contains("https://github.com/o/r.git"), "{out}");
        assert!(out.contains("plain line gate=test"), "{out}");
        assert!(!out.contains("abc") && !out.contains("s3cr3t") && !out.contains("pw@"));
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc\n");
    }
}
