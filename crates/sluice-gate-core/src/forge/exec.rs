//! Bounded subprocess runs for the forge probe (`gh`, `ssh -G`).
//!
//! The program is looked up in the caller's `PATH` (Sluice's own shims
//! skipped) and runs with exactly the caller's environment, in its own
//! process group, so a hung helper is killed with everything it started.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::git::{DEADLINE_GRACE, DEADLINE_POLL, EnvMap, drain, signal_group};
use crate::which;

/// A time budget shared by the calls of one probe.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    end: Instant,
}

impl Budget {
    /// A budget of `total` starting now.
    #[must_use]
    pub fn new(total: Duration) -> Self {
        Self {
            end: Instant::now() + total,
        }
    }

    /// The time one call may take: `cap` or what is left, whichever is
    /// shorter; `None` when nothing is left.
    #[must_use]
    pub fn slice(&self, cap: Duration) -> Option<Duration> {
        let left = self.end.saturating_duration_since(Instant::now());
        (!left.is_zero()).then_some(left.min(cap))
    }
}

/// What a finished command printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// Exit code (`None` = killed by a signal).
    pub code: Option<i32>,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

/// Why a command did not run to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The program is not in `PATH`.
    Missing,
    /// The time budget expired.
    Timeout,
    /// Any other spawn failure.
    Failed(String),
}

/// Runs `program args...` in `cwd` with exactly `env`, for at most the
/// budget's slice of `cap`.
///
/// # Errors
///
/// [`RunError`].
pub fn run(
    program: &str,
    args: &[&str],
    env: &EnvMap,
    cwd: &Path,
    budget: &Budget,
    cap: Duration,
) -> Result<Captured, RunError> {
    let path = which::resolve(program, env, cwd, true).map_err(|_| RunError::Missing)?;
    let timeout = budget.slice(cap).ok_or(RunError::Timeout)?;
    let mut child = Command::new(path)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| RunError::Failed(e.to_string()))?;
    let out = child.stdout.take().map(drain);
    let err = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(DEADLINE_POLL),
            Ok(None) | Err(_) => break None,
        }
    };
    let Some(status) = status else {
        let pgid = child.id();
        signal_group(pgid, nix::sys::signal::Signal::SIGTERM);
        let grace = Instant::now() + DEADLINE_GRACE;
        while Instant::now() < grace && matches!(child.try_wait(), Ok(None)) {
            std::thread::sleep(DEADLINE_POLL);
        }
        signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
        let _ = child.wait();
        return Err(RunError::Timeout);
    };
    let text = |rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>| {
        let bytes = rx
            .and_then(|rx| rx.recv_timeout(DEADLINE_GRACE).ok())
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok(Captured {
        code: status.code(),
        stdout: text(out),
        stderr: text(err),
    })
}
