//! Per-user directories used by the CLI and job identifiers.
//!
//! See the "Directories" table in `kb/sluice/design/architecture.md`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sluice_gate_core::git::EnvMap;

/// `$XDG_STATE_HOME/sluice` or `$HOME/.local/state/sluice`: logs and, later, the database.
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
    }

    #[test]
    fn job_ids_are_unique_and_well_formed() {
        let a = job_id();
        let b = job_id();
        assert_ne!(a, b);
        assert_eq!(a.split('-').count(), 3);
    }
}
