//! Per-user directories and identifiers shared by the CLI and the daemon.
//!
//! See the "Directories" table in `kb/sluice/design/architecture.md`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Result;
use crate::git::EnvMap;

/// `$XDG_STATE_HOME/sluice` or `$HOME/.local/state/sluice`: logs and the database.
#[must_use]
pub fn state_dir(env: &EnvMap) -> PathBuf {
    if let Some(dir) = env.get("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("sluice");
    }
    let home = env
        .get("HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| "/".to_owned(), Clone::clone);
    PathBuf::from(home).join(".local/state/sluice")
}

/// Directory holding `<job-id>.log` files.
#[must_use]
pub fn logs_dir(env: &EnvMap) -> PathBuf {
    state_dir(env).join("logs")
}

/// Path of the SQLite database kept by the daemon.
#[must_use]
pub fn database_path(env: &EnvMap) -> PathBuf {
    state_dir(env).join("sluice.db")
}

/// Path of the daemon's own log file (stdout/stderr when detached).
#[must_use]
pub fn daemon_log_path(env: &EnvMap) -> PathBuf {
    state_dir(env).join("daemon.log")
}

/// Runtime directory (`$XDG_RUNTIME_DIR/sluice` or `/tmp/sluice-<uid>`), created `0700`.
///
/// # Errors
///
/// See [`crate::tree::scratch_root`].
pub fn runtime_dir(env: &EnvMap) -> Result<PathBuf> {
    crate::tree::scratch_root(env)
}

/// Daemon socket path: `$SLUICE_SOCKET` or `<runtime>/daemon.sock`.
///
/// # Errors
///
/// When the runtime directory cannot be created.
pub fn socket_path(env: &EnvMap) -> Result<PathBuf> {
    if let Some(path) = env.get("SLUICE_SOCKET").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(runtime_dir(env)?.join("daemon.sock"))
}

/// Lock held by the daemon for its whole life (`<runtime>/daemon.lock`).
///
/// # Errors
///
/// When the runtime directory cannot be created.
pub fn daemon_lock_path(env: &EnvMap) -> Result<PathBuf> {
    Ok(runtime_dir(env)?.join("daemon.lock"))
}

/// Lock taken by clients while autostarting the daemon (`<runtime>/start.lock`).
///
/// # Errors
///
/// When the runtime directory cannot be created.
pub fn start_lock_path(env: &EnvMap) -> Result<PathBuf> {
    Ok(runtime_dir(env)?.join("start.lock"))
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Current time as RFC 3339 with seconds precision.
#[must_use]
pub fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

/// A job id unique on this machine that sorts chronologically:
/// `<unix-ms>-<pid>-<nanos hex>`.
#[must_use]
pub fn job_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}-{}-{:06x}",
        now.as_millis(),
        std::process::id(),
        now.subsec_nanos() & 0xff_ffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_prefers_xdg_state_home() {
        let mut env = EnvMap::new();
        env.insert("HOME".into(), "/home/u".into());
        assert_eq!(
            state_dir(&env),
            PathBuf::from("/home/u/.local/state/sluice")
        );
        env.insert("XDG_STATE_HOME".into(), "/tmp/state".into());
        assert_eq!(state_dir(&env), PathBuf::from("/tmp/state/sluice"));
        assert_eq!(logs_dir(&env), PathBuf::from("/tmp/state/sluice/logs"));
        assert_eq!(database_path(&env), PathBuf::from("/tmp/state/sluice/sluice.db"));
    }

    #[test]
    fn socket_path_honours_override() {
        let mut env = EnvMap::new();
        env.insert("SLUICE_SOCKET".into(), "/tmp/x.sock".into());
        assert_eq!(socket_path(&env).unwrap(), PathBuf::from("/tmp/x.sock"));
    }

    #[test]
    fn job_ids_are_unique_and_well_formed() {
        let a = job_id();
        let b = job_id();
        assert_ne!(a, b);
        assert_eq!(a.split('-').count(), 3);
        assert!(now_ms() > 1_700_000_000_000);
        assert!(now_rfc3339().ends_with('Z'));
    }
}
