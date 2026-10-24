//! `sluice daemon [--foreground]`, `sluice daemon stop [--force]` and the
//! hidden `sluice daemon reaper` (the gate reaper the daemon starts, see
//! `sluice_gate_daemon::reaper`).
//!
//! Detaching: without `--foreground` the process calls
//! `nix::unistd::daemon(true, false)` (fork, `setsid`, stdio to
//! `/dev/null`) while it is still single-threaded, then points stdout and
//! stderr at `<state>/daemon.log` with the safe `dup2_stdout` /
//! `dup2_stderr` wrappers, and only then builds the tokio runtime.
//!
//! Logging goes to stderr through `tracing_subscriber`; the filter comes
//! from `SLUICE_LOG` (same syntax as `RUST_LOG`), then `[debug] level` of
//! the user config, then `info`. `daemon.log` is rotated at start (when
//! larger than `[debug] daemon_log_max_mib`) and by the daemon's daily task.
//!
//! `sluice daemon restart [--debug] [--force]` stops the daemon (refusing
//! when gates are running unless `--force`) and starts a fresh detached one,
//! with `SLUICE_LOG=debug` for that instance only when `--debug` is given.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use nix::fcntl::{Flock, FlockArg};
use sluice_gate_core::config::{IonicClass, UserConfig, resolve_log_level};
use sluice_gate_core::dirs;
use sluice_gate_core::git::EnvMap;
use sluice_gate_core::logfile;
use sluice_gate_daemon::containment::ContainmentSetup;
use sluice_gate_daemon::executor::{IoClass, IoPriority};
use sluice_gate_daemon::server::{self, DaemonConfig, LogSettings, ReaperCommand};
use sluice_gate_queue::{QueueExtension, QueueOptions};

use crate::run::EXIT_ERROR;
use sluice_gate_proto::client::{self, ClientError};

/// How long `stop --force` waits for the daemon process to exit.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// How long `stop` waits for a draining daemon to release its lock.
const DRAIN_WAIT: Duration = Duration::from_secs(60);

/// Builds the daemon configuration from the user config and the environment.
fn daemon_config(env: &EnvMap, user: &UserConfig, detached: bool) -> Result<DaemonConfig> {
    let ionice = match user.daemon.ionice_class {
        IonicClass::None => None,
        IonicClass::BestEffort => Some(IoPriority {
            class: IoClass::BestEffort,
            level: user.daemon.ionice_level,
        }),
        IonicClass::Idle => Some(IoPriority {
            class: IoClass::Idle,
            level: user.daemon.ionice_level,
        }),
    };
    let repo_caps: BTreeMap<_, _> = user
        .repos
        .iter()
        .filter_map(|(path, over)| over.budget_share_max.map(|cap| (path.clone(), cap)))
        .collect();
    Ok(DaemonConfig {
        socket_path: dirs::socket_path(env).context("locating the socket")?,
        lock_path: dirs::daemon_lock_path(env).context("locating the daemon lock")?,
        db_path: dirs::database_path(env),
        logs_dir: dirs::logs_dir(env),
        budget: user.daemon.budget.max(1),
        head_reserve_after: user.scheduler.head_reserve_after,
        repo_caps,
        nice: user.daemon.nice,
        ionice,
        last_subscriber_gone: user.daemon.last_subscriber_gone,
        log_retention: Duration::from_hours(u64::from(user.daemon.log_retention_days) * 24),
        tail_lines: user.cache.tail_lines,
        marker_ttl: Duration::from_hours(u64::from(user.cache.ttl_days) * 24),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        admission: user.admission.clone(),
        psi_root: PathBuf::from(sluice_gate_daemon::psi::DEFAULT_ROOT),
        proc_root: PathBuf::from("/proc"),
        user: user.clone(),
        log: LogSettings {
            level: Some(log_level(env, user)),
            file: detached.then(|| dirs::daemon_log_path(env)),
            max_bytes: log_max_bytes(user),
        },
        extensions: vec![Arc::new(QueueExtension::new(QueueOptions::default()))],
        reaper: Some(reaper_command()),
        containment: containment_setup(env, user),
    })
}

/// Test-only: the runtime directory where the systemd user manager's socket
/// lives, when the test runs the daemon with a private `XDG_RUNTIME_DIR`
/// (so that its lock and socket never collide with the real daemon's).
pub const TEST_MANAGER_RUNTIME_ENV: &str = "SLUICE_TEST_MANAGER_RUNTIME_DIR";

/// Containment for the daemon: `None` when `[containment] enabled = false`.
/// Detection and unit creation happen later, inside the detached daemon.
fn containment_setup(env: &EnvMap, user: &UserConfig) -> Option<ContainmentSetup> {
    if !user.containment.enabled {
        return None;
    }
    // Test-only prefix, shared with the session hook: it must start with
    // `sluice-test-`, so a test never touches the real `sluice-*` units.
    let unit_prefix = env
        .get(crate::session_contain::TEST_PREFIX_VAR)
        .filter(|p| {
            p.starts_with("sluice-test-")
                && p.len() <= 64
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
        .cloned();
    let mut tool_env = env.clone();
    if let Some(runtime) = env.get(TEST_MANAGER_RUNTIME_ENV).filter(|v| !v.is_empty()) {
        tool_env.insert("XDG_RUNTIME_DIR".to_owned(), runtime.clone());
    }
    Some(ContainmentSetup {
        env: tool_env,
        unit_prefix,
        exe: PathBuf::from("/proc/self/exe"),
        holder: self_command(&["daemon", "holder"]),
    })
}

/// `sluice daemon holder`: keeps `sluice-agents.scope` alive. Blocks reading
/// its stdin, whose write end the daemon holds for its whole life (the
/// kernel closes it when the daemon exits, however it exits), then returns.
pub fn holder() -> ExitCode {
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0_u8; 64];
    loop {
        match std::io::Read::read(&mut stdin, &mut buf) {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return ExitCode::SUCCESS,
        }
    }
}

/// `sluice daemon reaper` through `/proc/self/exe`, so it still starts
/// after `sluice update` replaced the binary under the running daemon.
fn self_command(args: &[&str]) -> ReaperCommand {
    ReaperCommand {
        program: PathBuf::from("/proc/self/exe"),
        arg0: "sluice".into(),
        args: args.iter().map(Into::into).collect(),
    }
}

/// What the daemon runs: the intermediate that starts the reaper.
fn reaper_command() -> ReaperCommand {
    self_command(&["daemon", "reaper", "--detach"])
}

/// `sluice daemon reaper [--detach]`: the gate reaper. With `--detach`
/// (run by the daemon) it starts `sluice daemon reaper --daemon <daemon
/// pid>` with the same stdin, prints its pid and exits, so the reaper is
/// reparented away from the daemon's tree. Without it, it reads the
/// daemon's gate registrations on stdin and, when the daemon is gone (end
/// of file), terminates the gate process groups it left behind.
pub fn reaper(detach: bool) -> ExitCode {
    if detach {
        let daemon = std::os::unix::process::parent_id().to_string();
        return match sluice_gate_daemon::reaper::detach(&self_command(&[
            "daemon", "reaper", "--daemon", &daemon,
        ])) {
            Ok(pid) => {
                println!("{pid}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("sluice: cannot start the gate reaper: {e}");
                ExitCode::from(EXIT_ERROR)
            }
        };
    }
    let stdin = std::io::stdin();
    sluice_gate_daemon::reaper::serve(stdin.lock(), sluice_gate_daemon::executor::KILL_GRACE);
    ExitCode::SUCCESS
}

/// The effective log filter: `SLUICE_LOG`, then `[debug] level`, then `info`.
fn log_level(env: &EnvMap, user: &UserConfig) -> String {
    resolve_log_level(env, Some(&user.debug.level))
}

/// `[debug] daemon_log_max_mib` in bytes (0 disables the rotation).
fn log_max_bytes(user: &UserConfig) -> u64 {
    user.debug.daemon_log_max_mib.saturating_mul(1024 * 1024)
}

/// True when another process holds the daemon lock right now.
fn lock_is_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
    else {
        return false;
    };
    // Dropping the guard releases the probe lock immediately.
    Flock::lock(file, FlockArg::LockExclusiveNonblock).is_err()
}

/// Fails when `socket` cannot be bound: too long for `sun_path` or its
/// directory cannot be created.
fn check_socket_path(socket: &Path) -> Result<()> {
    std::os::unix::net::SocketAddr::from_pathname(socket)
        .with_context(|| format!("unusable socket path {}", socket.display()))?;
    if let Some(parent) = socket.parent().filter(|p| !p.as_os_str().is_empty()) {
        dirs::create_private_dir(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    Ok(())
}

/// Redirects stdout and stderr to the daemon log after `daemon(3)`.
fn detach(env: &EnvMap, max_bytes: u64) -> Result<()> {
    let log_path = dirs::daemon_log_path(env);
    if let Some(parent) = log_path.parent() {
        dirs::create_private_dir(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Rotate before opening, while nothing else writes to the file (the
    // previous daemon is gone; a live one makes us exit before this point).
    if let Err(e) = logfile::rotate_if_larger(&log_path, max_bytes, server::DAEMON_LOG_KEEP) {
        eprintln!("sluice: cannot rotate {}: {e}", log_path.display());
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    nix::unistd::daemon(true, false).context("detaching")?;
    nix::unistd::dup2_stdout(&log).context("redirecting stdout")?;
    nix::unistd::dup2_stderr(&log).context("redirecting stderr")?;
    Ok(())
}

fn init_tracing(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Resolves when the daemon receives SIGTERM or SIGINT.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::warn!("cannot install signal handlers; stop with `sluice daemon stop`");
        return std::future::pending::<()>().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// `sluice daemon [--foreground]`.
pub fn start(foreground: bool) -> ExitCode {
    let env: EnvMap = sluice_gate_core::config::process_env();
    let config = match UserConfig::load(&env)
        .map_err(anyhow::Error::from)
        .and_then(|user| daemon_config(&env, &user, !foreground))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sluice: error: {e:#}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    if lock_is_held(&config.lock_path) {
        eprintln!(
            "sluice: a daemon is already running (lock {})",
            config.lock_path.display()
        );
        return ExitCode::from(client::EXIT_ALREADY_RUNNING);
    }
    // Catch an unusable socket path before detaching, so an autostarting
    // client sees the failure as an exit code instead of waiting for it.
    if let Err(e) = check_socket_path(&config.socket_path) {
        eprintln!("sluice: error: {e:#}");
        return ExitCode::from(EXIT_ERROR);
    }
    // Fork only while single-threaded: before the runtime exists.
    if !foreground && let Err(e) = detach(&env, config.log.max_bytes) {
        eprintln!("sluice: error: {e:#}");
        return ExitCode::from(EXIT_ERROR);
    }
    init_tracing(config.log.level.as_deref().unwrap_or("info"));
    if let Some(reason) = dirs::plan_socket(&env)
        .ok()
        .and_then(|l| l.fallback_reason())
    {
        tracing::info!(
            socket = %config.socket_path.display(),
            "socket path fallback in use: {reason}"
        );
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("sluice: error: cannot start the async runtime: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    match runtime.block_on(server::run(config, shutdown_signal())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(sluice_gate_daemon::Error::AlreadyRunning(lock)) => {
            eprintln!(
                "sluice: a daemon is already running (lock {})",
                lock.display()
            );
            ExitCode::from(client::EXIT_ALREADY_RUNNING)
        }
        Err(e) => {
            eprintln!("sluice: error: {e}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// True while `pid` exists and is not a zombie.
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().next())
            .is_some_and(|state| state != "Z")
    })
}

/// Waits (100 ms polls, up to 60 s) until the stopping daemon releases
/// `daemon.lock`, so the next client starts a fresh daemon.
fn wait_for_drain(env: &EnvMap, running: usize) {
    let Ok(lock) = dirs::daemon_lock_path(env) else {
        return;
    };
    if !lock_is_held(&lock) {
        return;
    }
    if running > 0 {
        println!("sluice: waiting for {running} running jobs to finish");
    }
    let deadline = Instant::now() + DRAIN_WAIT;
    while lock_is_held(&lock) {
        if Instant::now() >= deadline {
            eprintln!(
                "sluice: the daemon is still finishing its jobs after {}s; not waiting any longer",
                DRAIN_WAIT.as_secs()
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `sluice daemon stop [--force]`.
pub fn stop(force: bool) -> ExitCode {
    let env: EnvMap = sluice_gate_core::config::process_env();
    let Ok(socket) = dirs::socket_path(&env) else {
        println!("sluice: no daemon running");
        return ExitCode::SUCCESS;
    };
    let client = match client::connect(&socket) {
        Ok(c) => c,
        Err(ClientError::NotRunning(_)) => {
            println!("sluice: no daemon running");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("sluice: error: {e}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    let pid = client.daemon_pid();
    match client.stop(force) {
        Ok(running) => {
            println!("sluice: daemon stopping ({running} running jobs)");
            if force {
                // Running jobs are being killed: wait for the daemon to exit
                // so the caller knows the gates are gone when we return.
                let deadline = Instant::now() + STOP_WAIT;
                while alive(pid) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
            } else {
                wait_for_drain(&env, running);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sluice: error: {e}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// `sluice daemon restart [--debug] [--force]`.
///
/// Gates that are running block a graceful restart (the message says so);
/// `--force` kills them. With `--debug` the new daemon runs with
/// `SLUICE_LOG=debug`, only for that instance.
pub fn restart(debug: bool, force: bool) -> ExitCode {
    let mut env: EnvMap = sluice_gate_core::config::process_env();
    if let Ok(socket) = dirs::socket_path(&env)
        && let Ok(client) = client::connect(&socket)
    {
        if !force {
            match client.status() {
                Ok(report) if !report.running.is_empty() => {
                    eprintln!(
                        "sluice: {} gate(s) are running; not restarting the daemon. \
                         Wait for them or use 'sluice daemon restart --force' to kill them",
                        report.running.len()
                    );
                    return ExitCode::from(EXIT_ERROR);
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("sluice: error: {e}");
                    return ExitCode::from(EXIT_ERROR);
                }
            }
        }
        let code = stop(force);
        if code != ExitCode::SUCCESS {
            return code;
        }
    } else {
        println!("sluice: no daemon running; starting one");
    }
    if debug {
        env.insert("SLUICE_LOG".to_owned(), "debug".to_owned());
    }
    match client::ensure_daemon(&env) {
        Ok(_) => {
            println!(
                "sluice: daemon started{}",
                if debug { " (log level debug)" } else { "" }
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sluice: error: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}
