//! The environment merge trains run their tools in (ADR-0020).
//!
//! Train gates, the train's `prepare` and base probe, and the train's
//! `key_inputs` run in the *daemon's* process, not in a client's, so their
//! `PATH` must not depend on whoever started the daemon (an SSH shell without
//! the user's toolchain, cron, an agent session). It is also never the
//! enqueuer's: whoever enqueues would choose the binaries that judge their own
//! branch. [`resolve`] computes it once, when the queue extension starts.
//!
//! Rule for tests: when the extension is given a `base_env`
//! ([`crate::QueueOptions::base_env`]) the login shell is **not** resolved
//! (the machine running the tests would decide the result); the tool
//! environment is that `base_env`, unless the test injects a login shell
//! (`QueueOptions::login_shell`) or `[queue] tool_path` is set.

use std::path::Path;
use std::time::Duration;

use sluice_gate_core::config::{AgentDetectionConfig, ToolEnv, UserQueueConfig};
use sluice_gate_core::env::{self, daemon_env};
use sluice_gate_proto::QueueToolEnv;

use crate::git::EnvMap;

/// Where the train tool environment comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEnvSource {
    /// The user's login environment (the daemon's own variables underneath).
    LoginShell,
    /// The daemon's own sanitised environment: `[queue] tool_env = "daemon"`
    /// (`reason: None`) or a login shell that could not be resolved.
    Daemon {
        /// Why the login environment is not in use, when it was wanted.
        reason: Option<String>,
    },
    /// The daemon's environment with the `PATH` of `[queue] tool_path`.
    ToolPath,
}

impl ToolEnvSource {
    /// The wire name of the source (`QueueToolEnv::source`).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::LoginShell => "login-shell",
            Self::Daemon { .. } => "daemon",
            Self::ToolPath => "tool-path",
        }
    }
}

/// The resolved train tool environment.
#[derive(Debug, Clone)]
pub struct ResolvedToolEnv {
    /// The environment of train gates, `prepare`, base probe and `key_inputs`.
    pub env: EnvMap,
    /// Where it came from.
    pub source: ToolEnvSource,
}

impl ResolvedToolEnv {
    /// The status view, see [`view`].
    #[must_use]
    pub fn view(&self) -> QueueToolEnv {
        view(&self.env, &self.source)
    }
}

/// The status view of a tool environment: the source, the fallback reason and
/// the `PATH` (never the rest of the environment).
#[must_use]
pub fn view(env: &EnvMap, source: &ToolEnvSource) -> QueueToolEnv {
    QueueToolEnv {
        source: source.name().to_owned(),
        reason: match source {
            ToolEnvSource::Daemon { reason } => reason.clone(),
            _ => None,
        },
        path: env.get("PATH").cloned(),
    }
}

/// What [`resolve`] needs besides the config.
#[derive(Debug)]
pub struct Inputs<'a> {
    /// The daemon's sanitised environment (or the tests' `base_env`).
    pub daemon_env: &'a EnvMap,
    /// `base_env` was given (tests): do not resolve the login shell unless
    /// `login_shell` is set.
    pub test_env: bool,
    /// A login shell to use instead of the user's (tests).
    pub login_shell: Option<&'a Path>,
    /// Agent-detection variables to strip from the login environment.
    pub detection: &'a AgentDetectionConfig,
    /// Deadline of the login shell.
    pub deadline: Duration,
}

/// Computes the train tool environment once; logs the outcome.
pub fn resolve(queue: &UserQueueConfig, inputs: &Inputs<'_>) -> ResolvedToolEnv {
    let resolved = compute(queue, inputs);
    match &resolved.source {
        ToolEnvSource::Daemon {
            reason: Some(reason),
        } => tracing::warn!(
            "train tool environment: daemon environment ({reason}); trains use the PATH the \
             daemon was started with"
        ),
        source => tracing::info!(
            "train tool environment: {} ({} vars, PATH has {} entries)",
            source.name(),
            resolved.env.len(),
            path_entries(&resolved.env)
        ),
    }
    resolved
}

fn path_entries(env: &EnvMap) -> usize {
    env.get("PATH")
        .map_or(0, |p| p.split(':').filter(|e| !e.is_empty()).count())
}

/// The trains' `PATH`: the login shell's entries first, then the daemon's
/// entries the login `PATH` lacks. A login profile may rewrite `PATH` from
/// scratch (`/etc/profile` does), which would drop directories only the
/// daemon's starter had (an activated virtualenv, a direnv); appending them
/// keeps those tools, and putting them last means they never shadow a tool the
/// user's login environment provides.
fn merged_path(login: Option<&String>, daemon: Option<&String>) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for entry in login
        .into_iter()
        .chain(daemon)
        .flat_map(|p| p.split(':'))
        .filter(|e| !e.is_empty())
    {
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    (!out.is_empty()).then(|| out.join(":"))
}

fn compute(queue: &UserQueueConfig, inputs: &Inputs<'_>) -> ResolvedToolEnv {
    let mut env = inputs.daemon_env.clone();
    if let Some(path) = &queue.tool_path {
        env.insert("PATH".to_owned(), path.clone());
        return ResolvedToolEnv {
            env,
            source: ToolEnvSource::ToolPath,
        };
    }
    let daemon = |reason: Option<String>| ResolvedToolEnv {
        env: inputs.daemon_env.clone(),
        source: ToolEnvSource::Daemon { reason },
    };
    if queue.tool_env == ToolEnv::Daemon {
        return daemon(None);
    }
    if inputs.test_env && inputs.login_shell.is_none() {
        return daemon(Some("test environment".to_owned()));
    }
    let resolved = match inputs.login_shell {
        Some(shell) => env::login_env(inputs.daemon_env, shell, inputs.deadline)
            .map(|raw| daemon_env(&raw, inputs.detection)),
        None => env::login_tool_env(inputs.daemon_env, inputs.detection, inputs.deadline)
            .map(|login| login.env),
    };
    match resolved {
        Ok(login) => {
            let path = merged_path(login.get("PATH"), inputs.daemon_env.get("PATH"));
            env.extend(login);
            if let Some(path) = path {
                env.insert("PATH".to_owned(), path);
            }
            ResolvedToolEnv {
                env,
                source: ToolEnvSource::LoginShell,
            }
        }
        Err(e) => daemon(Some(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn fake_shell(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("shell");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn inputs<'a>(
        daemon_env: &'a EnvMap,
        detection: &'a AgentDetectionConfig,
        shell: Option<&'a Path>,
    ) -> Inputs<'a> {
        Inputs {
            daemon_env,
            test_env: false,
            login_shell: shell,
            detection,
            deadline: Duration::from_secs(20),
        }
    }

    #[test]
    fn the_trains_path_is_login_first_then_what_only_the_daemon_had() {
        let p = |s: &str| Some(s.to_owned());
        assert_eq!(
            merged_path(p("/a:/b").as_ref(), p("/b:/c::/a").as_ref()).as_deref(),
            Some("/a:/b:/c")
        );
        assert_eq!(merged_path(None, p("/c").as_ref()).as_deref(), Some("/c"));
        assert_eq!(merged_path(p("/a").as_ref(), None).as_deref(), Some("/a"));
        assert_eq!(merged_path(None, None), None);
    }

    #[test]
    fn login_values_win_over_the_daemon_env() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell(
            dir.path(),
            "export PATH=/login/bin:/usr/bin NVM_DIR=/nvm\neval \"$4\"",
        );
        let det = AgentDetectionConfig::default();
        let daemon = env(&[("PATH", "/venv/bin:/usr/bin"), ("ONLY_DAEMON", "1")]);
        let r = resolve(
            &UserQueueConfig::default(),
            &inputs(&daemon, &det, Some(&shell)),
        );
        assert_eq!(r.source, ToolEnvSource::LoginShell);
        // Login entries first; the daemon's entries the login PATH lacks after
        // them (a profile that rewrites PATH must not lose them).
        assert_eq!(r.env["PATH"], "/login/bin:/usr/bin:/venv/bin");
        assert_eq!(r.env["NVM_DIR"], "/nvm");
        assert_eq!(r.env["ONLY_DAEMON"], "1");
        assert_eq!(
            r.view().path.as_deref(),
            Some("/login/bin:/usr/bin:/venv/bin")
        );
    }

    #[test]
    fn a_failing_login_shell_falls_back_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell(dir.path(), "exit 3");
        let det = AgentDetectionConfig::default();
        let daemon = env(&[("PATH", "/usr/bin")]);
        let r = resolve(
            &UserQueueConfig::default(),
            &inputs(&daemon, &det, Some(&shell)),
        );
        let ToolEnvSource::Daemon { reason: Some(why) } = &r.source else {
            panic!("{:?}", r.source);
        };
        assert!(why.contains("exited"), "{why}");
        assert_eq!(r.env, daemon);
        assert_eq!(r.view().source, "daemon");
        assert_eq!(r.view().reason.as_deref(), Some(why.as_str()));
    }

    #[test]
    fn daemon_mode_and_tool_path_do_not_start_a_shell() {
        let det = AgentDetectionConfig::default();
        let daemon = env(&[("PATH", "/usr/bin"), ("HOME", "/h")]);
        let missing = Path::new("/nonexistent/shell");
        let q = UserQueueConfig {
            tool_env: ToolEnv::Daemon,
            ..UserQueueConfig::default()
        };
        let r = resolve(&q, &inputs(&daemon, &det, Some(missing)));
        assert_eq!(r.source, ToolEnvSource::Daemon { reason: None });
        assert_eq!(r.env, daemon);
        let q = UserQueueConfig {
            tool_path: Some("/opt/x:/usr/bin".to_owned()),
            ..UserQueueConfig::default()
        };
        let r = resolve(&q, &inputs(&daemon, &det, Some(missing)));
        assert_eq!(r.source, ToolEnvSource::ToolPath);
        assert_eq!(r.env["PATH"], "/opt/x:/usr/bin");
        assert_eq!(r.env["HOME"], "/h");
    }

    #[test]
    fn a_test_base_env_skips_the_login_shell() {
        let det = AgentDetectionConfig::default();
        let daemon = env(&[("PATH", "/usr/bin")]);
        let mut i = inputs(&daemon, &det, None);
        i.test_env = true;
        let r = resolve(&UserQueueConfig::default(), &i);
        assert_eq!(r.env, daemon);
        assert!(matches!(r.source, ToolEnvSource::Daemon { .. }));
    }
}
