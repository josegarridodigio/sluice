//! The neutral environment of the daemon (ADR-0018 section 1).
//!
//! The daemon outlives the client that autostarts it, so whatever that
//! client's environment says about *its own* invocation (an agent session, a
//! running job, a disabled cache, the shim directory in `PATH`) must not
//! become the daemon's environment: every train push and every gate the
//! daemon runs would inherit it. [`daemon_env`] is the one function that
//! removes those variables; it is applied when a client spawns the daemon and
//! again by the daemon itself, so a daemon started by hand is neutral too.

use std::path::{Path, PathBuf};

use crate::config::{AgentDetectionConfig, EnvMap};
use crate::which;

/// Variables removed by exact name, besides the prefixes below and the
/// user's `[agent_detection]` variables. Every one describes the invoking
/// client's own session (a running job, a forced agent or human mode, a
/// disabled cache, the shim directory, a per-session shim or hook toggle).
/// Not here, because they are configuration: `SLUICE_CONFIG`, `SLUICE_LOG`,
/// `SLUICE_SOCKET`, `SLUICE_UPDATE_BASE_URL`, `SLUICE_INSTALL_DIR`.
const STRIPPED: [&str; 8] = [
    crate::JOB_ENV_VAR,
    "SLUICE_AGENT",
    "SLUICE_CACHE",
    "SLUICE_DISABLE",
    "SLUICE_SESSION_LEAF",
    which::SHIM_DIR_VAR,
    // `SLUICE_SHIMS=0|1`: whether the client's shims act.
    "SLUICE_SHIMS",
    // `SLUICE_GUARD=0|deny`: the client's command-guard mode.
    "SLUICE_GUARD",
];

/// Removes `dir` from a `PATH`-style list, comparing lexically and, when both
/// exist, by canonical path; every other entry (empty ones included) is kept
/// in order.
fn path_without(path: &str, dir: &Path) -> String {
    let canon = std::fs::canonicalize(dir).ok();
    path.split(':')
        .filter(|entry| {
            if entry.is_empty() {
                return true;
            }
            let p = Path::new(entry);
            if p == dir {
                return false;
            }
            !(canon.is_some() && std::fs::canonicalize(p).ok() == canon)
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// The environment the daemon runs in, derived from `env`.
///
/// Removes `GIT_*`, `CLAUDE*`, `SLUICE_JOB`, `SLUICE_AGENT`, `SLUICE_CACHE`,
/// `SLUICE_DISABLE`, `SLUICE_SESSION_LEAF`, `SLUICE_SHIM_DIR`, `SLUICE_SHIMS`,
/// `SLUICE_GUARD`, every variable
/// named by `detection` (`env_any_set` and the keys of `env_any_equals`) and,
/// from `PATH`, the Sluice shim directory (`SLUICE_SHIM_DIR`, else the
/// default one) and nothing else. Everything else (`CI`, `LC_*`, `HOME`, the
/// rest of `PATH`) is the user's and stays.
pub fn daemon_env(env: &EnvMap, detection: &AgentDetectionConfig) -> EnvMap {
    let shim_dir: Option<PathBuf> = env
        .get(which::SHIM_DIR_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| which::default_shim_dir(env));
    let mut out: EnvMap = env
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            !(k.starts_with("GIT_")
                || k.starts_with("CLAUDE")
                || STRIPPED.contains(&k)
                || detection.env_any_set.iter().any(|n| n == k)
                || detection.env_any_equals.contains_key(k))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let (Some(dir), Some(path)) = (shim_dir, out.get_mut("PATH")) {
        *path = path_without(path, &dir);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn strips_the_fixed_variables_and_keeps_the_rest() {
        let e = env(&[
            ("GIT_DIR", "/x"),
            ("GIT_INDEX_FILE", "/y"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_ENTRYPOINT", "cli"),
            ("SLUICE_JOB", "j1"),
            ("SLUICE_AGENT", "1"),
            ("SLUICE_CACHE", "off"),
            ("SLUICE_DISABLE", "1"),
            ("SLUICE_SESSION_LEAF", "/leaf"),
            ("SLUICE_SHIM_DIR", "/shims"),
            ("SLUICE_SHIMS", "1"),
            ("SLUICE_GUARD", "deny"),
            ("SLUICE_SOCKET", "/s.sock"),
            ("SLUICE_LOG", "debug"),
            ("CI", "true"),
            ("LC_ALL", "C"),
            ("HOME", "/h"),
            ("SLUICE_CONFIG", "/c.toml"),
            ("PATH", "/bin"),
        ]);
        let out = daemon_env(&e, &AgentDetectionConfig::default());
        assert_eq!(
            out,
            env(&[
                ("CI", "true"),
                ("LC_ALL", "C"),
                ("HOME", "/h"),
                ("SLUICE_CONFIG", "/c.toml"),
                ("SLUICE_SOCKET", "/s.sock"),
                ("SLUICE_LOG", "debug"),
                ("PATH", "/bin"),
            ])
        );
    }

    #[test]
    fn strips_every_agent_detection_variable() {
        let det = AgentDetectionConfig {
            env_any_set: vec!["MY_AGENT".to_owned()],
            env_any_equals: BTreeMap::from([("TERM_PROGRAM".to_owned(), "agent".to_owned())]),
        };
        let e = env(&[
            ("MY_AGENT", "1"),
            ("TERM_PROGRAM", "agent"),
            ("TERM", "xterm"),
        ]);
        assert_eq!(daemon_env(&e, &det), env(&[("TERM", "xterm")]));
    }

    #[test]
    fn removes_only_the_shim_directory_from_path() {
        let e = env(&[
            ("SLUICE_SHIM_DIR", "/data/shims"),
            ("PATH", "/usr/bin:/data/shims:/shims-not:/opt/bin:"),
        ]);
        let out = daemon_env(&e, &AgentDetectionConfig::default());
        assert_eq!(out["PATH"], "/usr/bin:/shims-not:/opt/bin:");
    }

    #[test]
    fn removes_the_default_shim_directory_without_the_variable() {
        let e = env(&[
            ("XDG_DATA_HOME", "/xdg"),
            ("PATH", "/xdg/sluice/shims:/usr/bin"),
        ]);
        let out = daemon_env(&e, &AgentDetectionConfig::default());
        assert_eq!(out["PATH"], "/usr/bin");
    }

    #[test]
    fn matches_the_shim_directory_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let e = env(&[
            ("SLUICE_SHIM_DIR", real.to_str().unwrap()),
            ("PATH", &format!("/usr/bin:{}", link.display())),
        ]);
        let out = daemon_env(&e, &AgentDetectionConfig::default());
        assert_eq!(out["PATH"], "/usr/bin");
    }

    #[test]
    fn an_env_without_path_stays_without_path() {
        let out = daemon_env(&env(&[("CI", "1")]), &AgentDetectionConfig::default());
        assert_eq!(out, env(&[("CI", "1")]));
    }
}
