//! `sluice agent session-start`: the `SessionStart` hook of the agent plugin.
//!
//! Decision: `kb/sluice/design/adr/0014-zero-config-agent-integration.md`;
//! design: `kb/sluice/design/agent-plugin.md`.
//!
//! Reads the hook JSON on stdin (`cwd`, `source`, `session_id`) and, under a
//! 300 ms watchdog:
//!
//! 1. installs or refreshes the shims in the stable shim directory (same code
//!    as `sluice shim install`; foreign files are never touched);
//! 2. appends to `$CLAUDE_ENV_FILE` a guarded line that puts that directory
//!    first in `PATH`, unless the file already holds that exact line (the hook
//!    fires again on resume and the env file is reused);
//! 3. contains the session in cgroups when `[containment]` allows it (the
//!    agent and its descendants into `ui`, the self-move line for the shells
//!    in the env file; see [`crate::session_contain`]), within the same budget;
//! 4. prints the `additionalContext` for the agent: the rules and the gate
//!    names in a repository with `.sluice.toml`, an offer to set Sluice up in
//!    a git repository without it, a one-liner elsewhere;
//! 5. logs one line to `logs/client.log`.
//!
//! It never fails closed: any error, a panic or a run over the limit prints
//! nothing and exits 0.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sluice_gate_core::config::{EnvMap, REPO_CONFIG_FILE, RepoConfig, UserConfig};
use sluice_gate_core::heavy::HeavyMatcher;

use crate::{clientlog, session_contain, shim};

/// Longest the hook may take before it gives up silently.
const WATCHDOG: Duration = Duration::from_millis(300);
/// Time after which containment is abandoned so the answer is still printed
/// before the watchdog fires.
const BUDGET: Duration = Duration::from_millis(230);
/// Test-only: replaces [`WATCHDOG`] (milliseconds; the containment budget
/// keeps the same margin below it). Tests that check what the hook answers
/// must not depend on how loaded the machine running them is; the latency
/// itself has its own test with the real limit.
pub const TEST_WATCHDOG_ENV: &str = "SLUICE_TEST_SESSION_WATCHDOG_MS";

/// The watchdog: [`WATCHDOG`], or the test override.
fn watchdog(env: &EnvMap) -> Duration {
    env.get(TEST_WATCHDOG_ENV)
        .and_then(|v| v.parse().ok())
        .map_or(WATCHDOG, Duration::from_millis)
}

/// The containment budget: the watchdog minus the margin the answer needs.
fn budget(env: &EnvMap) -> Duration {
    watchdog(env).saturating_sub(WATCHDOG.saturating_sub(BUDGET))
}

/// Largest hook input read from stdin.
const MAX_INPUT: u64 = 1 << 20;
/// Most gate names listed in the context.
const MAX_GATES: usize = 20;

/// Held while the answer is written, so the watchdog never cuts it.
static OUTPUT: Mutex<()> = Mutex::new(());

/// Where the session starts.
#[derive(Debug, PartialEq, Eq)]
pub enum Place {
    /// A `.sluice.toml` is at or above the directory; its gate names.
    Configured(Vec<String>),
    /// A git repository without `.sluice.toml`.
    GitOnly,
    /// Neither.
    Elsewhere,
}

/// Looks for `.sluice.toml` or `.git` at `dir` and its ancestors.
pub fn locate(dir: &Path) -> Place {
    let mut in_git = false;
    for d in dir.ancestors() {
        let file = d.join(REPO_CONFIG_FILE);
        if let Ok(text) = fs::read_to_string(&file) {
            let gates = RepoConfig::parse(&text, &file)
                .map(|c| c.gates.keys().cloned().collect())
                .unwrap_or_default();
            return Place::Configured(gates);
        }
        in_git |= d.join(".git").exists();
    }
    if in_git {
        Place::GitOnly
    } else {
        Place::Elsewhere
    }
}

/// The rules, in a repository with `.sluice.toml`.
fn rules_text(gates: &[String]) -> String {
    let mut names = gates
        .iter()
        .take(MAX_GATES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if gates.len() > MAX_GATES {
        names.push_str(", ...");
    }
    let gates = if names.is_empty() {
        String::new()
    } else {
        format!(" Gates of this repository: {names}.")
    };
    format!(
        "Sluice is active in this session: a queue and cache shared by every agent on this \
         machine.{gates} Rules: \
         1) Verify before every commit with the `verify` MCP tool (absolute working_directory); \
         never run a gate command through Bash. \
         2) Builds and test runners (cargo, npm, ng, make, docker build, ...) may be routed \
         through Sluice's queue automatically; never retry one without it and do not prefix \
         commands yourself. \
         3) Do not wrap builds or tests in subshells, `$(...)` or scripts to capture their \
         output; redirect with `>` or `2>&1` instead. \
         4) Do not start several dev servers or watchers while gates run. \
         5) Never approve merge trains (`queue_approve`, `sluice queue approve`) and never push \
         to the target branch. \
         6) A gate result `KILLED for lack of memory` is not a flaky failure: do not rerun it \
         unchanged or outside Sluice; lower its footprint (e.g. test concurrency) or tell the \
         human."
    )
}

/// The offer, in a git repository without `.sluice.toml`.
const INIT_TEXT: &str = "Sluice (a verification queue and cache shared by the agents on this \
    machine) is installed but this git repository has no .sluice.toml. Unless you already did in \
    this conversation, propose Sluice to the user once: run `sluice init --dry-run`, show its \
    output to the user and run `sluice init` only after the user agrees. If they decline, do not \
    mention it again. Meanwhile heavy builds and test runners are queued automatically: do not \
    wrap them in subshells, `$(...)` or scripts, and never approve merge trains.";

/// The note, outside a git repository.
const NON_GIT_TEXT: &str = "Sluice is installed; this directory is not a git repository, so \
    there is nothing to set up. Heavy builds and test runners run through its shared queue \
    automatically.";

/// The `additionalContext` for `place`, and the log decision.
pub fn context(place: &Place) -> (String, &'static str) {
    match place {
        Place::Configured(gates) => (rules_text(gates), "rules"),
        Place::GitOnly => (INIT_TEXT.to_owned(), "init-offer"),
        Place::Elsewhere => (NON_GIT_TEXT.to_owned(), "non-git"),
    }
}

/// `text` escaped for use inside double quotes in a POSIX shell.
fn dq_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '"' | '$' | '`' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The guarded line that prepends `dir` to `PATH` once.
pub fn env_line(dir: &Path) -> String {
    let dir = dir.display().to_string();
    format!(
        "case \":$PATH:\" in *\":{}:\"*) ;; *) export PATH={}:\"$PATH\" ;; esac",
        dq_escape(&dir),
        shim::quote(&dir)
    )
}

/// Appends [`env_line`] to `file` unless a line equal to it is already there.
/// Returns whether it was appended.
pub fn ensure_env_line(file: &Path, dir: &Path) -> std::io::Result<bool> {
    ensure_line(file, &env_line(dir))
}

/// Appends `line` to `file` unless a line equal to it is already there.
/// Returns whether it was appended.
pub fn ensure_line(file: &Path, line: &str) -> std::io::Result<bool> {
    let existing = match fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if existing.lines().any(|l| l == line) {
        return Ok(false);
    }
    let lead = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    f.write_all(format!("{lead}{line}\n").as_bytes())?;
    Ok(true)
}

/// The hook's `session_id`, when it is a non-empty string.
fn input_session(value: &Value) -> Option<String> {
    value
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

/// Containment of the session (see [`session_contain`]) in a worker thread
/// that may use what is left of the hook's budget; a late worker is left
/// behind (every step is idempotent) and the label is `timeout`. Appends the
/// env-file line when there is one. Returns the log label.
fn contain(
    session: Option<String>,
    env: &EnvMap,
    user: &UserConfig,
    env_file: Option<&Path>,
    start: Instant,
) -> &'static str {
    if !user.containment.enabled {
        return "off";
    }
    let limit = budget(env);
    let (tx, rx) = std::sync::mpsc::channel();
    let (env, user) = (env.clone(), user.clone());
    let file = env_file.map(Path::to_path_buf);
    std::thread::spawn(move || {
        let agent = session_contain::agent_pid(&env);
        let done = session_contain::contain_session(&env, &user, session.as_deref(), agent);
        if let (Some(file), Some(line)) = (&file, &done.line) {
            let _ = ensure_line(file, line);
        }
        let _ = tx.send(done.label);
    });
    let left = limit.saturating_sub(start.elapsed());
    rx.recv_timeout(left).unwrap_or("timeout")
}

/// What the hook does for `input`; the JSON to print, or `None`.
pub fn respond(input: &str, env: &EnvMap) -> Option<String> {
    let start = Instant::now();
    let value: Value = serde_json::from_str(input).ok()?;
    if !value.is_object() {
        return None;
    }
    let cwd = value
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;

    let user = UserConfig::load(env).ok()?;
    let names = HeavyMatcher::from_config(&user.agent).shim_names();
    let here = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let dir = shim::default_dir(env, &here).ok()?;
    let sluice = shim::current_binary().ok()?;
    let done = shim::install_into(&dir, &sluice, &names, false).ok()?;
    let env_file = env
        .get("CLAUDE_ENV_FILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    if let Some(file) = &env_file {
        ensure_env_line(file, &done.dir).ok()?;
    }
    let contained = contain(
        input_session(&value),
        env,
        &user,
        env_file.as_deref(),
        start,
    );

    let (text, decision) = context(&locate(&cwd));
    let decision = format!("{decision} contained={contained}");
    let _ = clientlog::record(
        env,
        &clientlog::Entry {
            pid: std::process::id(),
            caller: "agent",
            hook: Some("session-start"),
            gate: "-",
            tree: None,
            key: None,
            reason: None,
            decision: &decision,
            exit: 0,
            duration_ms: start.elapsed().as_millis(),
            cwd: &cwd,
        },
    );
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": text,
            }
        })
        .to_string(),
    )
}

/// `sluice agent session-start --plugin-version <X.Y.Z>`: always exits 0.
pub fn run_hook(plugin_version: &str) -> ExitCode {
    let env: EnvMap = sluice_gate_core::config::process_env();
    tracing::debug!(%plugin_version, "session-start");
    let limit = watchdog(&env);
    std::thread::spawn(move || {
        std::thread::sleep(limit);
        let _guard = OUTPUT.lock();
        std::process::exit(0);
    });
    std::panic::set_hook(Box::new(|_| {}));
    let answer = std::panic::catch_unwind(|| {
        let mut input = String::new();
        std::io::stdin()
            .take(MAX_INPUT)
            .read_to_string(&mut input)
            .ok()?;
        respond(&input, &env)
    });
    if let Ok(Some(answer)) = answer {
        let _guard = OUTPUT.lock();
        let mut out = std::io::stdout().lock();
        let _ = out
            .write_all(answer.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush());
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_are_short() {
        let many: Vec<String> = (0..40).map(|i| format!("gate-number-{i}")).collect();
        for place in [
            Place::Configured(many),
            Place::Configured(vec!["fmt".into(), "test".into()]),
            Place::GitOnly,
            Place::Elsewhere,
        ] {
            let (text, _) = context(&place);
            assert!(text.chars().count() <= 1500, "{}", text.chars().count());
        }
    }

    #[test]
    fn env_line_quotes_awkward_directories() {
        let line = env_line(Path::new("/a b/it's $x"));
        assert_eq!(
            line,
            "case \":$PATH:\" in *\":/a b/it's \\$x:\"*) ;; *) export PATH='/a b/it'\\''s $x':\"$PATH\" ;; esac"
        );
    }

    #[test]
    fn env_line_is_appended_once() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("env.sh");
        fs::write(&file, "export A=1").unwrap();
        let dir = Path::new("/x/shims");
        assert!(ensure_env_line(&file, dir).unwrap());
        assert!(!ensure_env_line(&file, dir).unwrap());
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.starts_with("export A=1\ncase"));
    }
}
