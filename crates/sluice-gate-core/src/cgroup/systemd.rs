#![allow(clippy::case_sensitive_file_extension_comparisons)]
//! Shell-out wrappers for `busctl`, `systemctl` and `oomctl` against the
//! systemd user manager. The only place of the crate that spawns them.
//!
//! Choices (verified on systemd 259, documented for 252..259):
//!
//! - **Scopes** are created with `busctl --user call ... StartTransientUnit`
//!   (`PIDs`, `Slice`, `Delegate`, `CPUWeight`, `ManagedOOM*`), which adopts
//!   processes that already run. `systemd-run --user --scope` would start a
//!   new command instead of adopting one.
//! - **Slice weights** use `systemctl --user set-property --runtime`: it also
//!   works on a slice that is only implied by a child (an implicit parent such
//!   as `sluice.slice`), where `StartTransientUnit` fails with "was already
//!   loaded", and it is idempotent. `--runtime` keeps the setting in `/run`,
//!   gone with the user manager.
//! - **Late attachment** uses `AttachProcessesToUnit` (unprivileged, needs a
//!   delegated, active unit).

use std::collections::BTreeMap;
use std::io::Read as _;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config::EnvMap;

use super::{CgResult, CgroupError};

/// Deadline of every tool call; the user manager answers in milliseconds.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline of the cheap probes of detection.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const MANAGER_DEST: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";

/// `ManagedOOMMemoryPressureLimit` is a fixed-point fraction scaled to `u32::MAX`.
const FIXED_ONE: u64 = u32::MAX as u64;

fn drain(mut pipe: impl std::io::Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Runs `program` with exactly `env`, stdin from `/dev/null`, and a deadline;
/// the process is killed when it expires. Returns the raw output whatever the
/// exit status.
///
/// # Errors
///
/// [`CgroupError::ToolMissing`] when it cannot be spawned,
/// [`CgroupError::Timeout`] when the deadline expired.
pub fn run_output(
    program: &'static str,
    args: &[&str],
    env: &EnvMap,
    timeout: Duration,
) -> CgResult<Output> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CgroupError::ToolMissing { program, source })?;
    let out_rx = child.stdout.take().map(drain);
    let err_rx = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) | Err(_) => break None,
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(CgroupError::Timeout {
            program,
            args: args.join(" "),
            timeout,
        });
    };
    let collect = |rx: Option<mpsc::Receiver<Vec<u8>>>| {
        rx.and_then(|rx| rx.recv_timeout(Duration::from_secs(1)).ok())
            .unwrap_or_default()
    };
    Ok(Output {
        status,
        stdout: collect(out_rx),
        stderr: collect(err_rx),
    })
}

fn command_error(program: &'static str, args: &[&str], out: &Output) -> CgroupError {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(5)
        .collect();
    CgroupError::Command {
        program,
        args: args.join(" "),
        status: out.status.to_string(),
        stderr: lines.join("\n"),
    }
}

/// Like [`run_output`], but a non-zero exit becomes [`CgroupError::Command`];
/// returns stdout as text.
///
/// # Errors
///
/// See [`run_output`], plus [`CgroupError::Command`].
pub fn run_checked(
    program: &'static str,
    args: &[&str],
    env: &EnvMap,
    timeout: Duration,
) -> CgResult<String> {
    let out = run_output(program, args, env, timeout)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(command_error(program, args, &out))
    }
}

/// `systemctl --user is-system-running`: `running`, `degraded`, `starting`,
/// `stopping`, `offline`, ... (the command exits non-zero for all but
/// `running`, so the state is read from stdout).
///
/// # Errors
///
/// Tool missing, timeout, or no state printed.
pub fn user_manager_state(env: &EnvMap) -> CgResult<String> {
    let args = ["--user", "is-system-running"];
    let out = run_output("systemctl", &args, env, PROBE_TIMEOUT)?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if state.is_empty() {
        return Err(command_error("systemctl", &args, &out));
    }
    Ok(state)
}

/// Major version in the first line of `systemctl --version`
/// (`systemd 259 (259.5-0ubuntu3.4)`).
#[must_use]
pub fn parse_systemd_version(text: &str) -> Option<u32> {
    let mut words = text.lines().next()?.split_whitespace();
    if words.next()? != "systemd" {
        return None;
    }
    words.next()?.parse().ok()
}

/// Runs `systemctl --version` and parses it.
///
/// # Errors
///
/// Tool missing, timeout, unexpected output.
pub fn systemd_version(env: &EnvMap) -> CgResult<u32> {
    let text = run_checked("systemctl", &["--version"], env, PROBE_TIMEOUT)?;
    parse_systemd_version(&text).ok_or_else(|| CgroupError::parse("systemctl --version", &text))
}

/// `true` when `systemctl [--user] is-active <unit>` prints `active`.
/// Any failure counts as not active.
#[must_use]
pub fn is_unit_active(env: &EnvMap, unit: &str, user: bool) -> bool {
    let mut args = Vec::new();
    if user {
        args.push("--user");
    }
    args.extend(["is-active", unit]);
    run_output("systemctl", &args, env, PROBE_TIMEOUT)
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
}

/// `systemctl --user show <unit> -p ...` as a key to value map.
///
/// # Errors
///
/// Tool missing, timeout or a failing `systemctl`.
pub fn show(env: &EnvMap, unit: &str, props: &[&str]) -> CgResult<BTreeMap<String, String>> {
    let mut args = vec!["--user", "show", unit];
    for p in props {
        args.extend(["-p", p]);
    }
    let text = run_checked("systemctl", &args, env, TOOL_TIMEOUT)?;
    Ok(parse_show(&text))
}

/// Parses `systemctl show` output (`Key=value` lines).
#[must_use]
pub fn parse_show(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// `oomctl dump` output, empty when `oomctl` is missing or fails (it is only
/// a source of information for the derived pressure policy).
#[must_use]
pub fn oomctl_dump(env: &EnvMap) -> String {
    run_checked("oomctl", &["dump"], env, PROBE_TIMEOUT).unwrap_or_default()
}

/// Memory pressure policy of a scope, as understood by `systemd-oomd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OomProperties {
    /// `ManagedOOMMemoryPressureLimit` in whole percent.
    pub limit_percent: u8,
    /// `ManagedOOMMemoryPressureDurationUSec`; only sent for systemd >= 257.
    pub duration: Option<Duration>,
}

/// A transient scope to start, adopting `pids`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSpec {
    /// Unit name, ending in `.scope`.
    pub name: String,
    /// Parent slice unit name.
    pub slice: String,
    /// Processes to adopt (must not be empty: a scope needs a process).
    pub pids: Vec<u32>,
    /// Unit description.
    pub description: String,
    /// `CPUWeight`.
    pub cpu_weight: Option<u32>,
    /// `CPUQuotaPerSecUSec` (a ceiling: `800_000` is 80 % of one CPU).
    pub cpu_quota_usec_per_sec: Option<u64>,
    /// `ManagedOOMMemoryPressure=kill` with its limit; only when the caller
    /// has checked that `systemd-oomd` is active and systemd is new enough.
    pub oom: Option<OomProperties>,
}

/// `percent` as the fixed-point `u32` systemd expects.
#[must_use]
pub fn percent_to_fixed(percent: u8) -> u32 {
    let scaled = u64::from(percent.min(100)) * FIXED_ONE / 100;
    u32::try_from(scaled).unwrap_or(u32::MAX)
}

/// Whole percent of a raw fixed-point value as printed by `systemctl show`.
#[must_use]
pub fn fixed_to_percent(raw: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        raw as f64 * 100.0 / FIXED_ONE as f64
    }
}

/// The `busctl` arguments (after `--user call ...`) that start `spec`.
/// Pure, so the property list is unit-tested.
#[must_use]
pub fn start_transient_scope_args(spec: &ScopeSpec) -> Vec<String> {
    let mut props: Vec<Vec<String>> = vec![
        vec![
            "PIDs".into(),
            "au".into(),
            spec.pids.len().to_string(),
            // pids appended below
        ],
        vec!["Slice".into(), "s".into(), spec.slice.clone()],
        vec!["Delegate".into(), "b".into(), "true".into()],
        vec!["Description".into(), "s".into(), spec.description.clone()],
    ];
    for pid in &spec.pids {
        props[0].push(pid.to_string());
    }
    if let Some(w) = spec.cpu_weight {
        props.push(vec!["CPUWeight".into(), "t".into(), w.to_string()]);
    }
    if let Some(q) = spec.cpu_quota_usec_per_sec {
        props.push(vec!["CPUQuotaPerSecUSec".into(), "t".into(), q.to_string()]);
    }
    if let Some(oom) = spec.oom {
        props.push(vec![
            "ManagedOOMMemoryPressure".into(),
            "s".into(),
            "kill".into(),
        ]);
        props.push(vec![
            "ManagedOOMMemoryPressureLimit".into(),
            "u".into(),
            percent_to_fixed(oom.limit_percent).to_string(),
        ]);
        if let Some(d) = oom.duration {
            props.push(vec![
                "ManagedOOMMemoryPressureDurationUSec".into(),
                "t".into(),
                d.as_micros().to_string(),
            ]);
        }
    }
    let mut args = vec![
        "StartTransientUnit".to_owned(),
        "ssa(sv)a(sa(sv))".to_owned(),
        spec.name.clone(),
        "fail".to_owned(),
        props.len().to_string(),
    ];
    args.extend(props.into_iter().flatten());
    args.push("0".to_owned());
    args
}

fn busctl_call(env: &EnvMap, method_args: &[String]) -> CgResult<String> {
    let mut args: Vec<&str> = vec!["--user", "call", MANAGER_DEST, MANAGER_PATH, MANAGER_IFACE];
    args.extend(method_args.iter().map(String::as_str));
    run_checked("busctl", &args, env, TOOL_TIMEOUT)
}

/// Starts the transient scope `spec` through the user manager.
///
/// # Errors
///
/// [`CgroupError::Command`] with busctl's message (for example when the
/// unit already exists), or the tool errors of [`run_output`].
pub fn start_transient_scope(env: &EnvMap, spec: &ScopeSpec) -> CgResult<()> {
    if spec.pids.is_empty() {
        return Err(CgroupError::Invalid(
            "scope: at least one pid is needed".to_owned(),
        ));
    }
    if !spec.name.ends_with(".scope") || spec.name.contains(['/', ' ']) {
        return Err(CgroupError::Invalid(format!("scope name {:?}", spec.name)));
    }
    busctl_call(env, &start_transient_scope_args(spec)).map(|_| ())
}

/// Sets runtime properties (`CPUWeight=50`) on a slice with
/// `systemctl --user set-property --runtime`; creates the slice when it only
/// exists implicitly or not at all.
///
/// # Errors
///
/// Tool errors.
pub fn set_slice_properties(env: &EnvMap, slice: &str, assignments: &[String]) -> CgResult<()> {
    if !slice.ends_with(".slice") || slice.contains(['/', ' ']) {
        return Err(CgroupError::Invalid(format!("slice name {slice:?}")));
    }
    let mut args = vec!["--user", "set-property", "--runtime", slice];
    args.extend(assignments.iter().map(String::as_str));
    run_checked("systemctl", &args, env, TOOL_TIMEOUT).map(|_| ())
}

/// `AttachProcessesToUnit(unit, subpath, pids)`: moves processes of the same
/// user into `subpath` (relative to the unit's cgroup, for example
/// `/s-abc/ui`) of a delegated, active unit.
///
/// # Errors
///
/// [`CgroupError::Command`] when the manager refuses (not delegated,
/// missing path, inner node), or tool errors.
pub fn attach_processes(env: &EnvMap, unit: &str, subpath: &str, pids: &[u32]) -> CgResult<()> {
    if pids.is_empty() {
        return Ok(());
    }
    let mut args = vec![
        "AttachProcessesToUnit".to_owned(),
        "ssau".to_owned(),
        unit.to_owned(),
        subpath.to_owned(),
        pids.len().to_string(),
    ];
    args.extend(pids.iter().map(ToString::to_string));
    busctl_call(env, &args).map(|_| ())
}

/// `systemctl --user stop <units...>` (best effort for units that are gone).
///
/// # Errors
///
/// Tool errors; a unit that is not loaded is not an error.
pub fn stop_units(env: &EnvMap, units: &[&str]) -> CgResult<()> {
    for unit in units {
        let args = ["--user", "stop", unit];
        let out = run_output("systemctl", &args, env, TOOL_TIMEOUT)?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.contains("not loaded") {
                return Err(command_error("systemctl", &args, &out));
            }
        }
    }
    Ok(())
}

/// Names of the loaded user units matching a glob (`sluice-test-*`).
///
/// # Errors
///
/// Tool errors.
pub fn list_units(env: &EnvMap, pattern: &str) -> CgResult<Vec<String>> {
    let text = run_checked(
        "systemctl",
        &[
            "--user",
            "list-units",
            "--all",
            "--no-legend",
            "--plain",
            pattern,
        ],
        env,
        TOOL_TIMEOUT,
    )?;
    Ok(text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

/// Reads a `sluice` readable file to a string, mapping errors (shared helper
/// of the detection code).
pub(crate) fn read_to_string(path: &std::path::Path) -> CgResult<String> {
    let mut s = String::new();
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut s))
        .map_err(|e| CgroupError::io("reading", path, e))?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_lines() {
        assert_eq!(
            parse_systemd_version("systemd 259 (259.5-0ubuntu3.4)\n+PAM +AUDIT"),
            Some(259)
        );
        assert_eq!(
            parse_systemd_version("systemd 252 (252.39-1~deb12u1)"),
            Some(252)
        );
        assert_eq!(
            parse_systemd_version("systemd 249 (249.11-0ubuntu3.12)"),
            Some(249)
        );
        assert_eq!(parse_systemd_version("not systemd"), None);
        assert_eq!(parse_systemd_version(""), None);
    }

    #[test]
    fn show_parsing() {
        let m = parse_show(
            "ManagedOOMMemoryPressureLimit=0\nManagedOOMMemoryPressureDurationUSec=[not set]\n",
        );
        assert_eq!(m["ManagedOOMMemoryPressureLimit"], "0");
        assert_eq!(m["ManagedOOMMemoryPressureDurationUSec"], "[not set]");
    }

    #[test]
    fn fixed_point_round_trip() {
        assert_eq!(percent_to_fixed(100), u32::MAX);
        assert_eq!(percent_to_fixed(0), 0);
        for p in [1u8, 24, 30, 50, 99] {
            let back = fixed_to_percent(u64::from(percent_to_fixed(p)));
            assert!((back - f64::from(p)).abs() < 0.01, "{p}: {back}");
        }
    }

    fn spec() -> ScopeSpec {
        ScopeSpec {
            name: "sluice-agents.scope".into(),
            slice: "sluice.slice".into(),
            pids: vec![11, 12],
            description: "d".into(),
            cpu_weight: Some(30),
            cpu_quota_usec_per_sec: None,
            oom: None,
        }
    }

    #[test]
    fn scope_arguments_without_oom() {
        let args = start_transient_scope_args(&spec());
        assert_eq!(
            args.join(" "),
            "StartTransientUnit ssa(sv)a(sa(sv)) sluice-agents.scope fail 5 \
             PIDs au 2 11 12 Slice s sluice.slice Delegate b true Description s d \
             CPUWeight t 30 0"
        );
    }

    #[test]
    fn scope_arguments_with_oom_and_quota() {
        let mut s = spec();
        s.cpu_quota_usec_per_sec = Some(1_600_000);
        s.oom = Some(OomProperties {
            limit_percent: 30,
            duration: Some(Duration::from_secs(5)),
        });
        let joined = start_transient_scope_args(&s).join(" ");
        assert!(joined.contains(" fail 9 "), "{joined}");
        assert!(joined.contains("CPUQuotaPerSecUSec t 1600000"));
        assert!(joined.contains("ManagedOOMMemoryPressure s kill"));
        assert!(joined.contains(&format!(
            "ManagedOOMMemoryPressureLimit u {}",
            percent_to_fixed(30)
        )));
        assert!(joined.contains("ManagedOOMMemoryPressureDurationUSec t 5000000"));
        assert!(joined.ends_with(" 0"));
    }

    #[test]
    fn scope_without_pids_or_bad_name_is_refused_before_spawning() {
        let env = EnvMap::new();
        let mut s = spec();
        s.pids.clear();
        assert!(matches!(
            start_transient_scope(&env, &s),
            Err(CgroupError::Invalid(_))
        ));
        let mut s = spec();
        s.name = "x.service".into();
        assert!(matches!(
            start_transient_scope(&env, &s),
            Err(CgroupError::Invalid(_))
        ));
        assert!(matches!(
            set_slice_properties(&env, "a/b.slice", &[]),
            Err(CgroupError::Invalid(_))
        ));
    }

    #[test]
    fn missing_tool_is_a_typed_error() {
        let e = run_output(
            "sluice-no-such-tool-xyz",
            &[],
            &EnvMap::new(),
            PROBE_TIMEOUT,
        );
        assert!(e.is_err());
    }
}
