//! `sluice daemon [--foreground]` and `sluice daemon stop [--force]`.
//!
//! Detaching: without `--foreground` the process calls
//! `nix::unistd::daemon(true, false)` (fork, `setsid`, stdio to
//! `/dev/null`) while it is still single-threaded, then points stdout and
//! stderr at `<state>/daemon.log` with the safe `dup2_stdout` /
//! `dup2_stderr` wrappers, and only then builds the tokio runtime.
//!
//! Logging goes to stderr through `tracing_subscriber`; the filter comes
//! from `SLUICE_LOG` (same syntax as `RUST_LOG`, default `info`).

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use nix::fcntl::{Flock, FlockArg};
use sluice_gate_core::config::{IonicClass, UserConfig};
use sluice_gate_core::dirs;
use sluice_gate_core::git::EnvMap;
use sluice_gate_daemon::executor::{IoClass, IoPriority};
use sluice_gate_daemon::server::{self, DaemonConfig};

use crate::run::EXIT_ERROR;
use sluice_gate_proto::client::{self, ClientError};

/// How long `stop --force` waits for the daemon process to exit.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// How long `stop` waits for a draining daemon to release its lock.
const DRAIN_WAIT: Duration = Duration::from_secs(60);

/// Builds the daemon configuration from the user config and the environment.
fn daemon_config(env: &EnvMap, user: &UserConfig) -> Result<DaemonConfig> {
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
        user: user.clone(),
        extensions: Vec::new(),
    })
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
fn detach(env: &EnvMap) -> Result<()> {
    let log_path = dirs::daemon_log_path(env);
    if let Some(parent) = log_path.parent() {
        dirs::create_private_dir(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
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

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("SLUICE_LOG")
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
    let env: EnvMap = std::env::vars().collect();
    let config = match UserConfig::load(&env)
        .map_err(anyhow::Error::from)
        .and_then(|user| daemon_config(&env, &user))
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
    if !foreground && let Err(e) = detach(&env) {
        eprintln!("sluice: error: {e:#}");
        return ExitCode::from(EXIT_ERROR);
    }
    init_tracing();
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
    let env: EnvMap = std::env::vars().collect();
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
