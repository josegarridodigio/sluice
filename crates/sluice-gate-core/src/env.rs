//! The neutral environment of the daemon (ADR-0018 section 1).
//!
//! The daemon outlives the client that autostarts it, so whatever that
//! client's environment says about *its own* invocation (an agent session, a
//! running job, a disabled cache, the shim directory in `PATH`) must not
//! become the daemon's environment: every train push and every gate the
//! daemon runs would inherit it. [`daemon_env`] is the one function that
//! removes those variables; it is applied when a client spawns the daemon and
//! again by the daemon itself, so a daemon started by hand is neutral too.

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{AgentDetectionConfig, EnvMap};
use crate::git::{DEADLINE_GRACE, DEADLINE_POLL, signal_group};
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

/// Printed by the login shell right before `env -0`; everything before its
/// last occurrence (rc-file banners, `motd`, a `printf` of the user's own) is
/// noise.
pub const LOGIN_ENV_MARKER: &str = "__SLUICE_LOGIN_ENV_START_5c1f9a7e__";

/// Default deadline of one login-shell resolution (ADR-0020).
pub const LOGIN_ENV_DEADLINE: Duration = Duration::from_secs(10);

/// Why the login environment could not be resolved.
#[derive(Debug, thiserror::Error)]
pub enum LoginEnvError {
    /// The shell could not be started.
    #[error("cannot start {shell}: {source}")]
    Spawn {
        /// The shell that was tried.
        shell: String,
        /// The spawn failure.
        source: std::io::Error,
    },
    /// The shell did not finish in time (an rc file that waits for input).
    #[error("{shell} did not finish within {timeout:?}")]
    Timeout {
        /// The shell that was tried.
        shell: String,
        /// The deadline that expired.
        timeout: Duration,
    },
    /// The shell exited with a failure.
    #[error("{shell} exited with {status}")]
    Failed {
        /// The shell that was tried.
        shell: String,
        /// Its exit status.
        status: std::process::ExitStatus,
    },
    /// The shell never reached the `env -0` it was asked to run (an rc file
    /// that `exec`s another program or exits).
    #[error("{shell} printed no environment (an rc file may exit or exec early)")]
    NoMarker {
        /// The shell that was tried.
        shell: String,
    },
    /// The environment it printed was empty.
    #[error("{shell} printed an empty environment")]
    Empty {
        /// The shell that was tried.
        shell: String,
    },
}

/// The variables of `env -0` output after the last `marker`: NUL-separated
/// `KEY=VALUE` entries (malformed ones, without `=` or with an empty key, are
/// ignored). `None` when the marker is absent.
#[must_use]
pub fn parse_login_env(output: &[u8], marker: &str) -> Option<EnvMap> {
    let m = marker.as_bytes();
    let at = output.windows(m.len()).rposition(|w| w == m)?;
    let env = output[at + m.len()..]
        .split(|b| *b == 0)
        .filter_map(|entry| {
            let eq = entry.iter().position(|b| *b == b'=')?;
            (eq > 0).then(|| {
                (
                    String::from_utf8_lossy(&entry[..eq]).into_owned(),
                    String::from_utf8_lossy(&entry[eq + 1..]).into_owned(),
                )
            })
        })
        .collect();
    Some(env)
}

/// The login shell of the user: `$SHELL` of `base`, else the passwd entry of
/// the current uid (field 7 of `/etc/passwd`), else `/bin/sh`.
#[must_use]
pub fn login_shell(base: &EnvMap) -> PathBuf {
    if let Some(shell) = base.get("SHELL").filter(|s| !s.is_empty()) {
        return PathBuf::from(shell);
    }
    let uid = rustix::process::getuid().as_raw().to_string();
    std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|text| passwd_shell(&text, &uid))
        .map_or_else(|| PathBuf::from("/bin/sh"), PathBuf::from)
}

/// The shell field of the `passwd` line whose uid is `uid`.
fn passwd_shell(passwd: &str, uid: &str) -> Option<String> {
    passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.len() >= 7 && f[2] == uid && !f[6].is_empty()).then(|| f[6].to_owned())
    })
}

/// What the reader thread of the shell's stdout has collected so far.
#[derive(Default)]
struct Collected {
    bytes: Mutex<Vec<u8>>,
    done: AtomicBool,
}

/// Reads `pipe` into the returned buffer until EOF. Unlike a plain
/// `read_to_end`, the bytes read so far stay available when a daemon an rc
/// file started keeps the pipe open after the shell exited.
fn collect(mut pipe: impl std::io::Read + Send + 'static) -> Arc<Collected> {
    let shared = Arc::new(Collected::default());
    let theirs = Arc::clone(&shared);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            theirs
                .bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(&chunk[..n]);
        }
        theirs.done.store(true, Ordering::Release);
    });
    shared
}

/// `cmd.spawn()`, retried a few times on `ETXTBSY`: an executable written a
/// moment ago can still be open for writing in a child another thread has
/// just forked (the descriptor is close-on-exec, but lives until that child's
/// own `exec`), and the kernel then refuses to run it for that instant.
fn spawn_retrying_busy(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 5 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            other => return other,
        }
    }
}

/// Runs the user's login shell as `<shell> -l -i -c 'printf <marker>; env -0'`
/// and returns the environment it printed (ADR-0020).
///
/// The shell starts with an almost empty environment (`HOME`, `USER`,
/// `LOGNAME` and `LANG` from `base` when present, `SHELL`, `TERM=dumb`), stdin
/// from `/dev/null`, stderr discarded and its own process group; when it has
/// not exited after `deadline` the whole group gets SIGTERM, then SIGKILL after
/// 2 s. The result is raw: pass it through [`daemon_env`] (or use
/// [`login_tool_env`]).
///
/// # Errors
///
/// [`LoginEnvError`]: spawn failure, timeout, non-zero exit, no marker, empty
/// result.
pub fn login_env(base: &EnvMap, shell: &Path, deadline: Duration) -> Result<EnvMap, LoginEnvError> {
    let name = shell.display().to_string();
    let script = format!("printf '%s' '{LOGIN_ENV_MARKER}'; env -0");
    let mut cmd = Command::new(shell);
    cmd.args(["-l", "-i", "-c", &script])
        .env_clear()
        .env("SHELL", shell)
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    for key in ["HOME", "USER", "LOGNAME", "LANG"] {
        if let Some(v) = base.get(key) {
            cmd.env(key, v);
        }
    }
    let mut child = spawn_retrying_busy(&mut cmd).map_err(|source| LoginEnvError::Spawn {
        shell: name.clone(),
        source,
    })?;
    let out = child.stdout.take().map(collect);
    let limit = Instant::now() + deadline;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < limit => std::thread::sleep(DEADLINE_POLL),
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
        return Err(LoginEnvError::Timeout {
            shell: name,
            timeout: deadline,
        });
    };
    if !status.success() {
        return Err(LoginEnvError::Failed {
            shell: name,
            status,
        });
    }
    // A daemon started by an rc file may hold the pipe open: give the reader
    // a moment to reach EOF, then use what it has.
    let bytes = out.map_or_else(Vec::new, |c| {
        let end = Instant::now() + Duration::from_millis(500);
        while !c.done.load(Ordering::Acquire) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        c.bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    });
    match parse_login_env(&bytes, LOGIN_ENV_MARKER) {
        None => Err(LoginEnvError::NoMarker { shell: name }),
        Some(env) if env.is_empty() => Err(LoginEnvError::Empty { shell: name }),
        Some(env) => Ok(env),
    }
}

/// A resolved login environment.
#[derive(Debug, Clone)]
pub struct LoginTools {
    /// The shell that printed it.
    pub shell: PathBuf,
    /// The environment, already neutral ([`daemon_env`]).
    pub env: EnvMap,
}

/// The login environment of the user as the environment trains run their tools
/// in: the shell of [`login_shell`]`(base)` through [`login_env`], then
/// [`daemon_env`] (per-invocation and agent variables, and the shim directory
/// in `PATH`, removed).
///
/// # Errors
///
/// See [`login_env`].
pub fn login_tool_env(
    base: &EnvMap,
    detection: &AgentDetectionConfig,
    deadline: Duration,
) -> Result<LoginTools, LoginEnvError> {
    let shell = login_shell(base);
    let raw = login_env(base, &shell, deadline)?;
    let mut env = daemon_env(&raw, detection);
    // The login shell starts with a minimal environment, so it may not know
    // where the daemon's shim directory is (`SLUICE_SHIM_DIR`,
    // `XDG_DATA_HOME`): remove the one the daemon's own environment names too.
    let daemon_shims = base
        .get(which::SHIM_DIR_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| which::default_shim_dir(base));
    if let (Some(dir), Some(path)) = (daemon_shims, env.get_mut("PATH")) {
        *path = path_without(path, &dir);
    }
    Ok(LoginTools { shell, env })
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

    const M: &str = "@@M@@";

    fn nul(pairs: &[&str]) -> Vec<u8> {
        pairs
            .iter()
            .flat_map(|p| format!("{p}\0").into_bytes())
            .collect()
    }

    #[test]
    fn parse_ignores_noise_before_the_marker() {
        let mut out = b"Welcome!\nnvm loaded\n".to_vec();
        out.extend_from_slice(M.as_bytes());
        out.extend(nul(&["PATH=/a:/b", "HOME=/h", "EMPTY="]));
        let env = parse_login_env(&out, M).unwrap();
        assert_eq!(
            env,
            env_of(&[("PATH", "/a:/b"), ("HOME", "/h"), ("EMPTY", "")])
        );
    }

    fn env_of(pairs: &[(&str, &str)]) -> EnvMap {
        env(pairs)
    }

    #[test]
    fn parse_takes_the_last_marker() {
        let mut out = format!("{M}A=from-rc\0").into_bytes();
        out.extend_from_slice(b"more noise ");
        out.extend_from_slice(M.as_bytes());
        out.extend(nul(&["A=real"]));
        assert_eq!(parse_login_env(&out, M).unwrap(), env_of(&[("A", "real")]));
    }

    #[test]
    fn parse_skips_malformed_entries_and_keeps_equals_in_values() {
        let mut out = M.as_bytes().to_vec();
        out.extend(nul(&["no-equals", "=novalue-key", "X=a=b", "", "Y=1"]));
        assert_eq!(
            parse_login_env(&out, M).unwrap(),
            env_of(&[("X", "a=b"), ("Y", "1")])
        );
    }

    #[test]
    fn parse_without_marker_is_none() {
        assert!(parse_login_env(&nul(&["PATH=/a"]), M).is_none());
        assert!(parse_login_env(b"", M).is_none());
    }

    #[test]
    fn passwd_shell_matches_the_uid_field() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nme:x:1000:1000:Me,,,:/srv/me:/usr/bin/zsh\nsvc:x:1001:1::/:\n";
        assert_eq!(
            passwd_shell(passwd, "1000").as_deref(),
            Some("/usr/bin/zsh")
        );
        assert_eq!(passwd_shell(passwd, "0").as_deref(), Some("/bin/bash"));
        assert_eq!(passwd_shell(passwd, "1001"), None);
        assert_eq!(passwd_shell(passwd, "7"), None);
    }

    #[test]
    fn login_shell_prefers_the_shell_variable() {
        assert_eq!(
            login_shell(&env_of(&[("SHELL", "/opt/fish")])),
            PathBuf::from("/opt/fish")
        );
        assert!(login_shell(&EnvMap::new()).is_absolute());
    }

    /// Writes `body` as an executable `sh` script named `name` in `dir`. The
    /// shell is called as `<script> -l -i -c <command>`: `$4` is the command.
    fn fake_shell(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn pid_is_gone(pid: i32) -> bool {
        (0..150).any(|_| {
            let dead = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err();
            if !dead {
                std::thread::sleep(Duration::from_millis(20));
            }
            dead
        })
    }

    #[test]
    fn login_env_reads_the_environment_after_rc_noise() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell(
            dir.path(),
            "shell",
            "echo 'rc banner'; echo 'rc stderr' >&2\n\
             export PATH=/fake/node/bin:/usr/bin:/bin FROM_RC=1\n\
             eval \"$4\"",
        );
        let base = env_of(&[
            ("HOME", "/h"),
            ("USER", "u"),
            ("SECRET", "no"),
            ("LANG", "C"),
        ]);
        let got = login_env(&base, &shell, Duration::from_secs(20)).unwrap();
        assert_eq!(got["PATH"], "/fake/node/bin:/usr/bin:/bin");
        assert_eq!(got["FROM_RC"], "1");
        assert_eq!(got["HOME"], "/h");
        assert_eq!(got["TERM"], "dumb");
        assert_eq!(got["SHELL"], shell.display().to_string());
        assert_eq!(got["LANG"], "C");
        assert!(!got.contains_key("SECRET"), "the base env leaked: {got:?}");
    }

    #[test]
    fn login_tool_env_applies_daemon_env() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell(
            dir.path(),
            "shell",
            "export PATH=/fake/bin:/xdg/sluice/shims:/usr/bin CLAUDECODE=1 GIT_DIR=/x\neval \"$4\"",
        );
        let base = env_of(&[
            ("SHELL", &shell.display().to_string()),
            ("XDG_DATA_HOME", "/xdg"),
        ]);
        let got = login_tool_env(
            &base,
            &AgentDetectionConfig::default(),
            Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(got.shell, shell);
        assert_eq!(got.env["PATH"], "/fake/bin:/usr/bin");
        assert!(!got.env.contains_key("CLAUDECODE") && !got.env.contains_key("GIT_DIR"));
    }

    #[test]
    fn login_env_times_out_and_kills_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("sleeper.pid");
        let shell = fake_shell(
            dir.path(),
            "shell",
            &format!("sleep 600 &\necho $! > '{}'\nwait", pid_file.display()),
        );
        let started = Instant::now();
        let err = login_env(&EnvMap::new(), &shell, Duration::from_millis(600)).unwrap_err();
        assert!(matches!(err, LoginEnvError::Timeout { .. }), "{err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(pid_is_gone(pid), "the sleeper {pid} survived the deadline");
    }

    #[test]
    fn login_env_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let failing = fake_shell(dir.path(), "fail", "exit 1");
        let err = login_env(&EnvMap::new(), &failing, Duration::from_secs(20)).unwrap_err();
        assert!(matches!(err, LoginEnvError::Failed { .. }), "{err:?}");
        let silent = fake_shell(dir.path(), "silent", "echo hello; exit 0");
        let err = login_env(&EnvMap::new(), &silent, Duration::from_secs(20)).unwrap_err();
        assert!(matches!(err, LoginEnvError::NoMarker { .. }), "{err:?}");
        let empty = fake_shell(dir.path(), "empty", "printf '%s' \"$MARKER_NOT_SET\"; echo");
        let err = login_env(&EnvMap::new(), &empty, Duration::from_secs(20)).unwrap_err();
        assert!(matches!(err, LoginEnvError::NoMarker { .. }), "{err:?}");
        let missing = dir.path().join("absent");
        let err = login_env(&EnvMap::new(), &missing, Duration::from_secs(20)).unwrap_err();
        assert!(matches!(err, LoginEnvError::Spawn { .. }), "{err:?}");
    }

    #[test]
    fn login_env_with_an_empty_environment_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let shell = fake_shell(
            dir.path(),
            "shell",
            &format!("printf '%s' '{LOGIN_ENV_MARKER}'"),
        );
        let err = login_env(&EnvMap::new(), &shell, Duration::from_secs(20)).unwrap_err();
        assert!(matches!(err, LoginEnvError::Empty { .. }), "{err:?}");
    }
}
