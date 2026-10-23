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

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use sluice_gate_core::agent::{self, Caller};
use sluice_gate_core::config::{Gate, REPO_CONFIG_FILE, RepoConfig, UserConfig, Weight};
use sluice_gate_core::dirs;
use sluice_gate_core::git::{self, EnvMap, RepoPaths};
use sluice_gate_core::heavy::HeavyMatcher;
use sluice_gate_core::logfile;
use sluice_gate_core::marker::MarkerStore;
use sluice_gate_core::scope::{self, PathScope};
use sluice_gate_core::tree::{self, SnapshotFull, StagedEntries};
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
    let env: EnvMap = sluice_gate_core::config::process_env();
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
    out.push(check_socket_path(env));
    out.extend(check_daemon(env));
    out.push(check_user_config(env));
    out.push(check_shims(env, cwd));
    out.push(check_plugin(env));
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

/// `socket-path`: the socket path fits in `sun_path` (ok), the short
/// fallback is in use (ok, with the reason; fail when its directory is
/// unsafe), or the path is too long (fail).
fn check_socket_path(env: &EnvMap) -> Check {
    const NAME: &str = "socket-path";
    let location = match dirs::plan_socket(env) {
        Ok(l) => l,
        Err(e) => return check(NAME, Status::Fail, e.to_string()),
    };
    let len = location.path.as_os_str().len();
    let path = location.path.display();
    let max = dirs::SOCKET_PATH_MAX;
    match location.source {
        dirs::SocketSource::Explicit => check(
            NAME,
            Status::Ok,
            format!("{path} from SLUICE_SOCKET ({len} of {max} bytes)"),
        ),
        dirs::SocketSource::Default => {
            check(NAME, Status::Ok, format!("{path} ({len} of {max} bytes)"))
        }
        dirs::SocketSource::Fallback { .. } => {
            let reason = location.fallback_reason().unwrap_or_default();
            let dir = location.path.parent().unwrap_or(Path::new("/"));
            let problem = dirs::current_uid()
                .filter(|_| fs::symlink_metadata(dir).is_ok())
                .and_then(|uid| dirs::unsafe_socket_dir(dir, uid));
            match problem {
                Some(problem) => check(
                    NAME,
                    Status::Fail,
                    format!(
                        "fallback {path} refused: {} {problem} ({reason})",
                        dir.display()
                    ),
                ),
                None => check(
                    NAME,
                    Status::Ok,
                    format!("fallback in use: {path} because {reason}"),
                ),
            }
        }
    }
}

/// The daemon socket doctor may connect to: `Err` with the problem when it
/// is the short fallback and its directory exists but is unsafe (another
/// user could have planted the socket), `Ok(None)` when no path can be
/// planned. Nothing is created.
fn socket_to_contact(env: &EnvMap) -> Result<Option<PathBuf>, String> {
    let Ok(location) = dirs::plan_socket(env) else {
        return Ok(None);
    };
    if let dirs::SocketSource::Fallback { .. } = location.source {
        let dir = location.path.parent().unwrap_or(Path::new("/"));
        if let Some(problem) = dirs::current_uid()
            .filter(|_| fs::symlink_metadata(dir).is_ok())
            .and_then(|uid| dirs::unsafe_socket_dir(dir, uid))
        {
            return Err(format!("{} {problem}", dir.display()));
        }
    }
    Ok(Some(location.path))
}

/// Socket, daemon version and lease checks.
fn check_daemon(env: &EnvMap) -> Vec<Check> {
    let mut out = Vec::new();
    let installed = env!("CARGO_PKG_VERSION");
    let socket = match socket_to_contact(env) {
        Ok(socket) => socket,
        Err(problem) => {
            out.push(check(
                "daemon",
                Status::Fail,
                format!("not contacted: the fallback socket directory is unsafe: {problem}"),
            ));
            out.push(check_lease(env, None));
            out.push(check_database(env));
            return out;
        }
    };
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
    out.push(check_database(env));
    out
}

/// Opens the database read-only, as the daemon would find it: a file the
/// daemon cannot open (corrupt, foreign schema) is a failure, since the
/// daemon then cannot start and every gate runs in-process.
fn check_database(env: &EnvMap) -> Check {
    let db = dirs::database_path(env);
    if !db.exists() {
        return check(
            "database",
            Status::Ok,
            "no database yet (created on first use)",
        );
    }
    match Store::open_read_only(&db) {
        Ok(_) => check("database", Status::Ok, db.display().to_string()),
        Err(e) => check(
            "database",
            Status::Fail,
            format!(
                "the daemon cannot open {}: {e}; move it aside (the daemon creates a fresh one, \
                 job history is lost)",
                db.display()
            ),
        ),
    }
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
/// Queue-only gates listed in hooks: every commit or push runs them in full.
pub fn queue_only_hook_warning(cfg: &RepoConfig) -> Option<String> {
    let names: Vec<&str> = cfg
        .gates
        .iter()
        .filter(|(_, g)| g.is_queue_only() && !g.hooks.is_empty())
        .map(|(n, _)| n.as_str())
        .collect();
    (!names.is_empty()).then(|| {
        format!(
            "queue-only gate(s) {} run in full on every hook call (never cached); \
             consider removing them from `hooks`",
            names.join(", ")
        )
    })
}

fn check_repo(env: &EnvMap, paths: &RepoPaths) -> Vec<Check> {
    let mut out = Vec::new();
    let mut warning = None;
    let config = RepoConfig::load(&paths.toplevel);
    out.push(match &config {
        Ok(cfg) => {
            warning = queue_only_hook_warning(cfg);
            check(
                "repo-config",
                Status::Ok,
                format!(
                    "{} parses ({} gate(s))",
                    paths.toplevel.join(REPO_CONFIG_FILE).display(),
                    cfg.gates.len()
                ),
            )
        }
        Err(e) => check("repo-config", Status::Fail, e.to_string()),
    });
    if let Some(w) = warning {
        out.push(check("queue-only-hooks", Status::Warn, w));
    }
    out.push(check_hooks(env, paths));
    out.push(check_markers(paths));
    match &config {
        Ok(cfg) => {
            out.extend(check_scoped_gates(env, paths, cfg));
            out.push(check_weights(env, paths, cfg));
        }
        Err(_) if !paths.toplevel.join(REPO_CONFIG_FILE).exists() => out.push(check(
            "init",
            Status::Ok,
            "this repository has no .sluice.toml; run 'sluice init --dry-run' to see what Sluice \
             would propose",
        )),
        Err(_) => {}
    }
    out
}

/// Learned weight over this share of the declared ceiling (in percent)
/// makes `weights` warn.
const WEIGHT_WARN_PERCENT: u64 = 150;

/// `weights`: one warning naming every gate whose declared numeric ceiling
/// is more than 50 % below the unbounded learned weight (`learned` returns
/// it per gate), `ok` otherwise. Gates with `weight = "auto"` are skipped.
fn weight_check(cfg: &RepoConfig, learned: impl Fn(&str) -> Option<u32>) -> Check {
    let low: Vec<String> = cfg
        .gates
        .iter()
        .filter_map(|(name, gate)| {
            let Weight::Fixed(ceiling) = gate.weight else {
                return None;
            };
            let l = learned(name)?;
            (u64::from(l) * 100 > u64::from(ceiling) * WEIGHT_WARN_PERCENT)
                .then(|| format!("'{name}' needs about {l} but declares weight = {ceiling}"))
        })
        .collect();
    if low.is_empty() {
        check(
            "weights",
            Status::Ok,
            "no gate declares a weight well below what its runs need",
        )
    } else {
        check(
            "weights",
            Status::Warn,
            format!(
                "{}: raise the number or use weight = \"auto\" (a ceiling that low makes the \
                 gate oversubscribe the machine)",
                low.join("; ")
            ),
        )
    }
}

/// `weights`: compares each declared ceiling with the learned weight in the
/// database (opened read-only; nothing is written).
fn check_weights(env: &EnvMap, paths: &RepoPaths, cfg: &RepoConfig) -> Check {
    let db = dirs::database_path(env);
    if !db.exists() {
        return check("weights", Status::Ok, "no run history yet");
    }
    match Store::open_read_only(&db) {
        Ok(store) => weight_check(cfg, |gate| {
            store
                .unbounded_learned(&paths.toplevel, gate)
                .ok()
                .flatten()
        }),
        Err(_) => check("weights", Status::Ok, "skipped: the database is unreadable"),
    }
}

/// Index of the first `PATH` entry that is `dir` (compared canonically).
fn path_index(dir: &Path, env: &EnvMap, cwd: &Path) -> Option<usize> {
    let dir = fs::canonicalize(dir).ok()?;
    env.get("PATH")?.split(':').position(|e| {
        let entry = if e.is_empty() {
            cwd.to_path_buf()
        } else {
            cwd.join(e)
        };
        fs::canonicalize(entry).is_ok_and(|c| c == dir)
    })
}

/// `shims`: installed in the stable directory, current, and first in `PATH`
/// for this shell. Only an agent shell is warned about `PATH`: the agent
/// plugin puts the directory first for agent sessions.
fn check_shims(env: &EnvMap, cwd: &Path) -> Check {
    const NAME: &str = "shims";
    let user = UserConfig::load(env).unwrap_or_default();
    let agent_shell = env
        .get(crate::shim::SHIMS_VAR)
        .and_then(|v| agent::parse_bool(v))
        .unwrap_or_else(|| {
            matches!(
                agent::detect(
                    env,
                    &user.agent_detection.env_any_set,
                    &user.agent_detection.env_any_equals,
                ),
                Caller::Agent { .. }
            )
        });
    let dir = match crate::shim::default_dir(env, cwd) {
        Ok(d) => d,
        Err(e) => return check(NAME, Status::Warn, e),
    };
    let names = HeavyMatcher::from_config(&user.agent).shim_names();
    let Ok(sluice) = crate::shim::current_binary() else {
        return check(NAME, Status::Warn, "cannot locate the sluice binary");
    };
    let dir = fs::canonicalize(&dir).unwrap_or(dir);
    let (mut missing, mut stale) = (0usize, 0usize);
    for name in &names {
        let want = crate::shim::script(&dir, &sluice, name);
        match fs::read_to_string(dir.join(name)) {
            Ok(have) if have == want => {}
            Ok(_) => stale += 1,
            Err(_) => missing += 1,
        }
    }
    if missing == names.len() && !names.is_empty() {
        let status = if agent_shell {
            Status::Warn
        } else {
            Status::Ok
        };
        return check(
            NAME,
            status,
            format!(
                "not installed in {}; the agent plugin installs them at session start, or run \
                 'sluice shim install'",
                dir.display()
            ),
        );
    }
    if missing + stale > 0 {
        return check(
            NAME,
            Status::Warn,
            format!(
                "{missing} shim(s) missing and {stale} stale in {}; run 'sluice shim install'",
                dir.display()
            ),
        );
    }
    let detail = format!("{} shim(s) current in {}", names.len(), dir.display());
    let export = format!(
        "export PATH={}:\"$PATH\"",
        crate::shim::quote(&dir.display().to_string())
    );
    match path_index(&dir, env, cwd) {
        Some(0) => check(NAME, Status::Ok, format!("{detail}; first in PATH")),
        Some(i) if agent_shell => check(
            NAME,
            Status::Warn,
            format!(
                "{detail}; entry {} of PATH in this agent shell, not first: commands found \
                 before it skip the shims ({export})",
                i + 1
            ),
        ),
        None if agent_shell => check(
            NAME,
            Status::Warn,
            format!("{detail}; not in PATH of this agent shell ({export})"),
        ),
        _ => check(
            NAME,
            Status::Ok,
            format!(
                "{detail}; not first in PATH of this human shell: the agent plugin puts them \
                 in PATH for agent sessions"
            ),
        ),
    }
}

/// The version of the Sluice plugin in `installed_plugins.json` (the entry
/// whose key starts with `sluice@`), if any.
fn plugin_version(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let plugins = value.get("plugins")?.as_object()?;
    let (_, entries) = plugins.iter().find(|(k, _)| k.starts_with("sluice@"))?;
    let entry = entries.as_array()?.first()?;
    Some(entry.get("version")?.as_str()?.to_owned())
}

/// `plugin`: the installed agent plugin against this binary. Never fails: a
/// missing or unreadable plugin list is not a problem.
fn check_plugin(env: &EnvMap) -> Check {
    const NAME: &str = "plugin";
    let root = env
        .get("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| Path::new(h).join(".claude"))
        });
    let version = root
        .map(|r| r.join("plugins").join("installed_plugins.json"))
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|text| plugin_version(&text));
    let binary = env!("CARGO_PKG_VERSION");
    match version {
        None => check(
            NAME,
            Status::Ok,
            "no agent plugin found (nothing to compare)",
        ),
        Some(v) if v == binary => check(
            NAME,
            Status::Ok,
            format!("agent plugin and binary are {binary}"),
        ),
        Some(v) => check(
            NAME,
            Status::Warn,
            format!(
                "agent plugin is {v} but this binary is {binary}; update the older one (the \
                 plugin from its marketplace, the binary with 'sluice update')"
            ),
        ),
    }
}

/// Well-known build inputs a scoped gate usually depends on even when they
/// live outside its directories (design path-scoped-keys.md, section 8).
const BUILD_INPUTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain*",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "yarn.lock",
    "tsconfig*.json",
    "go.mod",
    "go.sum",
    "go.work",
    "pyproject.toml",
    "uv.lock",
    "poetry.lock",
    "requirements*.txt",
    "setup.cfg",
    "Makefile",
    "Dockerfile",
    ".nvmrc",
    ".tool-versions",
];

/// `scoped-gates`: one warning line per path-scoped gate whose scope looks
/// too small (never a failure). Uses one snapshot of the working tree.
fn check_scoped_gates(env: &EnvMap, paths: &RepoPaths, cfg: &RepoConfig) -> Vec<Check> {
    let scoped: Vec<_> = cfg
        .gates
        .iter()
        .filter_map(|(name, gate)| PathScope::from_gate(gate).map(|s| (name, gate, s)))
        .collect();
    if scoped.is_empty() {
        return Vec::new();
    }
    let probe = git::probe_env(env, Some(paths));
    let snapshot =
        tree::scratch_root(env).and_then(|scratch| tree::snapshot_entries(paths, &probe, &scratch));
    let entries = match snapshot {
        Ok(SnapshotFull::Known { entries, .. }) => entries,
        Ok(SnapshotFull::Unknown { reason }) => {
            return vec![check(
                "scoped-gates",
                Status::Ok,
                format!("skipped: tree unknown ({reason})"),
            )];
        }
        Err(e) => {
            return vec![check(
                "scoped-gates",
                Status::Ok,
                format!("skipped: cannot snapshot the tree ({e})"),
            )];
        }
    };
    scoped
        .into_iter()
        .map(|(name, gate, scope)| {
            let notes = scope_warnings(name, gate, &scope, &entries, &paths.toplevel);
            if notes.is_empty() {
                let count = scope.count(&entries);
                check(
                    "scoped-gates",
                    Status::Ok,
                    format!(
                        "gate '{name}': {} of {} files in scope; no well-known build input outside it",
                        count.files, count.total
                    ),
                )
            } else {
                check("scoped-gates", Status::Warn, notes.join("; "))
            }
        })
        .collect()
}

/// The warnings of one scoped gate (empty when it looks fine).
fn scope_warnings(
    name: &str,
    gate: &Gate,
    scope: &PathScope,
    entries: &StagedEntries,
    toplevel: &Path,
) -> Vec<String> {
    let mut notes = Vec::new();
    if !gate.cache {
        notes.push(format!(
            "gate '{name}' has cache = false, so paths and paths_ignore have no effect"
        ));
    }
    for i in scope.unmatched_paths(entries) {
        notes.push(format!("gate '{name}': paths[{i}] matches no file"));
    }
    // Directories holding a selected path, the root included.
    let mut dirs: HashSet<&[u8]> = HashSet::from([&b""[..]]);
    let mut selected = 0usize;
    for e in entries.iter().filter(|e| scope.matches(e.path)) {
        selected += 1;
        for (i, b) in e.path.iter().enumerate() {
            if *b == b'/' {
                dirs.insert(&e.path[..i]);
            }
        }
    }
    if selected == 0 {
        notes.push(format!(
            "gate '{name}': its scope matches no file, so it hits until .sluice.toml changes"
        ));
    }
    let key_files: HashSet<String> = gate
        .key_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    for e in entries.iter() {
        let (dir, base) = match e.path.iter().rposition(|b| *b == b'/') {
            Some(i) => (&e.path[..i], &e.path[i + 1..]),
            None => (&b""[..], e.path),
        };
        let path = String::from_utf8_lossy(e.path);
        if dirs.contains(dir)
            && BUILD_INPUTS
                .iter()
                .any(|p| scope::wildmatch(p.as_bytes(), base))
            && !scope.matches(e.path)
            && !key_files.contains(path.as_ref())
        {
            notes.push(format!(
                "gate '{name}' is scoped by paths but {path} is outside its scope; edits to it \
                 will not invalidate the cache"
            ));
        }
    }
    for e in entries
        .iter()
        .filter(|e| e.mode == b"120000" && scope.matches(e.path))
    {
        let path = String::from_utf8_lossy(e.path).into_owned();
        let Ok(target) = fs::read_link(toplevel.join(&path)) else {
            continue;
        };
        let inside = symlink_target(&path, &target).is_some_and(|t| scope.matches(t.as_bytes()));
        if !inside {
            notes.push(format!(
                "gate '{name}': symlink {path} points outside the scope ({}); edits to the target \
                 will not invalidate the cache",
                target.display()
            ));
        }
    }
    notes
}

/// Root-relative, lexically normalised target of the symlink at `link`
/// (root-relative); `None` when it is absolute or leaves the repository.
fn symlink_target(link: &str, target: &Path) -> Option<String> {
    if target.is_absolute() {
        return None;
    }
    let mut parts: Vec<String> = link.split('/').map(str::to_owned).collect();
    parts.pop();
    for c in target.components() {
        match c {
            std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            std::path::Component::ParentDir => {
                parts.pop()?;
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
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

/// Size, writability and damaged files of the marker directory.
fn check_markers(paths: &RepoPaths) -> Check {
    let store = MarkerStore::new(&paths.common_dir);
    let dir = store.dir().to_path_buf();
    let (bytes, files) = dir_size(&dir);
    let detail = format!(
        "{} file(s), {} KiB in {}",
        files,
        bytes / 1024,
        dir.display()
    );
    if dir.exists()
        && let Err(e) = nix::unistd::access(
            &dir,
            nix::unistd::AccessFlags::W_OK | nix::unistd::AccessFlags::X_OK,
        )
    {
        return check(
            "markers",
            Status::Fail,
            format!(
                "{} is not writable ({e}): passes are not cached, every gate runs again",
                dir.display()
            ),
        );
    }
    let damaged = store.damaged().unwrap_or_default();
    if let Some(first) = damaged.first() {
        return check(
            "markers",
            Status::Warn,
            format!(
                "{detail}; {} damaged file(s) (corrupt, truncated or under the wrong key), \
                 first {}: they are ignored and replaced by the next run; \
                 'sluice cache clear' removes them",
                damaged.len(),
                first.display()
            ),
        );
    }
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

    let socket = socket_to_contact(env).ok().flatten().filter(|s| s.exists());
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

    fn env_of(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn plugin_version_is_read_from_the_installed_list() {
        let json = r#"{"version":2,"plugins":{
            "other@m":[{"version":"9.9.9"}],
            "sluice@sluice":[{"scope":"user","version":"0.3.0"}]}}"#;
        assert_eq!(plugin_version(json).as_deref(), Some("0.3.0"));
        assert_eq!(plugin_version(r#"{"plugins":{}}"#), None);
        assert_eq!(plugin_version("not json"), None);
    }

    #[test]
    fn plugin_check_warns_on_a_different_version_and_never_fails() {
        let home = tempfile::tempdir().unwrap();
        let env = env_of(&[("CLAUDE_CONFIG_DIR", home.path().to_str().unwrap())]);
        let c = check_plugin(&env);
        assert_eq!(c.status, Status::Ok, "{}", c.detail);
        fs::create_dir_all(home.path().join("plugins")).unwrap();
        let file = home.path().join("plugins/installed_plugins.json");
        fs::write(&file, r#"{"plugins":{"sluice@s":[{"version":"0.0.1"}]}}"#).unwrap();
        let c = check_plugin(&env);
        assert_eq!(c.status, Status::Warn, "{}", c.detail);
        assert!(c.detail.contains("0.0.1"), "{}", c.detail);
        fs::write(
            &file,
            format!(
                r#"{{"plugins":{{"sluice@s":[{{"version":"{}"}}]}}}}"#,
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();
        assert_eq!(check_plugin(&env).status, Status::Ok);
        fs::write(&file, "garbage").unwrap();
        assert_eq!(check_plugin(&env).status, Status::Ok);
    }

    #[test]
    fn weights_warn_only_when_the_ceiling_is_far_below_the_learned_weight() {
        let cfg = RepoConfig::parse(
            "version = 1\n[gates.a]\ncmd = \"x\"\nweight = 2\n\
             [gates.b]\ncmd = \"x\"\nweight = 4\n\
             [gates.c]\ncmd = \"x\"\nweight = \"auto\"\n",
            Path::new(".sluice.toml"),
        )
        .unwrap();
        let learned = |g: &str| match g {
            "a" => Some(5),
            "b" => Some(6),
            "c" => Some(20),
            _ => None,
        };
        let c = weight_check(&cfg, learned);
        assert_eq!(c.status, Status::Warn, "{}", c.detail);
        assert!(
            c.detail.contains("'a'") && !c.detail.contains("'b'"),
            "{}",
            c.detail
        );
        assert!(!c.detail.contains("'c'"), "{}", c.detail);
        assert!(c.detail.contains("auto"), "{}", c.detail);
        assert_eq!(weight_check(&cfg, |_| None).status, Status::Ok);
    }

    #[test]
    fn shims_report_state_and_path_position_by_shell_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let shims = tmp.path().join("shims");
        let env = |agent: &str, path: &str| {
            env_of(&[
                ("HOME", tmp.path().to_str().unwrap()),
                ("SLUICE_SHIM_DIR", shims.to_str().unwrap()),
                ("SLUICE_AGENT", agent),
                ("PATH", path),
            ])
        };
        let plain = "/usr/bin:/bin";
        // Not installed: ok for a human, a warning for an agent shell.
        assert_eq!(check_shims(&env("0", plain), tmp.path()).status, Status::Ok);
        assert_eq!(
            check_shims(&env("1", plain), tmp.path()).status,
            Status::Warn
        );
        let user = UserConfig::default();
        let names = HeavyMatcher::from_config(&user.agent).shim_names();
        let done = crate::shim::install_into(
            &shims,
            &crate::shim::current_binary().unwrap(),
            &names,
            false,
        )
        .unwrap();
        // Installed but not in PATH: warn for an agent, ok with a note for a human.
        let agent = check_shims(&env("1", plain), tmp.path());
        assert_eq!(agent.status, Status::Warn, "{}", agent.detail);
        let human = check_shims(&env("0", plain), tmp.path());
        assert_eq!(human.status, Status::Ok, "{}", human.detail);
        assert!(human.detail.contains("agent plugin"), "{}", human.detail);
        // First in PATH: ok everywhere.
        let first = format!("{}:/usr/bin", done.dir.display());
        let c = check_shims(&env("1", &first), tmp.path());
        assert_eq!(c.status, Status::Ok, "{}", c.detail);
        assert!(c.detail.contains("first in PATH"), "{}", c.detail);
        // Behind another entry: warn for an agent.
        let behind = format!("/usr/bin:{}", done.dir.display());
        assert_eq!(
            check_shims(&env("1", &behind), tmp.path()).status,
            Status::Warn
        );
        // A stale shim warns for everyone.
        fs::write(done.dir.join(&names[0]), "#!/bin/sh\nold\n").unwrap();
        let c = check_shims(&env("0", plain), tmp.path());
        assert_eq!(c.status, Status::Warn, "{}", c.detail);
        assert!(c.detail.contains("stale"), "{}", c.detail);
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail_lines("a\nb\nc\n", 2), "b\nc\n");
    }
}
