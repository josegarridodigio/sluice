//! `sluice guard`: the `PreToolUse` hook of the agent plugin.
//!
//! Reads Claude Code's `PreToolUse` JSON on stdin and writes a decision on
//! stdout, or nothing. It denies `sluice queue approve` (reserved to humans),
//! rewrites gate commands to `sluice run <gate> -v` and heavy commands to
//! `sluice exec --label <L> -- <cmd>` (prefix insertion into the untouched
//! original, see [`matcher`]), picks `allow`, `ask` or no rewrite from the
//! client's permission settings ([`rules`]) and otherwise falls back to the
//! legacy deny of raw gate commands. It never fails closed: any error, a
//! panic or a run longer than 500 ms prints nothing and exits 0. It never
//! contacts the daemon. Design: `kb/sluice/design/command-control.md`
//! sections 1 and 4.

pub mod lex;
pub mod matcher;
pub mod rules;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};
use sluice_gate_core::config::{EnvMap, REPO_CONFIG_FILE, RepoConfig, UserConfig};
use sluice_gate_core::heavy::HeavyMatcher;

use matcher::{Gates, NoPlan, World};
use rules::Decision;

/// Longest the guard may take before it gives up silently.
const WATCHDOG: Duration = Duration::from_millis(500);
/// Test-only override of [`WATCHDOG`] in milliseconds.
const WATCHDOG_ENV: &str = "SLUICE_TEST_GUARD_WATCHDOG_MS";
/// Largest hook input read from stdin.
const MAX_INPUT: u64 = 1 << 20;

/// Deny text for `sluice queue approve`.
pub const APPROVE_DENY: &str = "Approval is reserved to humans.";

/// Held while the answer is written, so the watchdog never cuts it.
static OUTPUT: Mutex<()> = Mutex::new(());

/// What `SLUICE_GUARD` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Rewrite (the default).
    Rewrite,
    /// `SLUICE_GUARD=deny`: suggest the rewrite in a deny instead.
    Suggest,
    /// `SLUICE_GUARD=0` (or another falsy word): do nothing.
    Off,
}

impl Mode {
    /// The mode `SLUICE_GUARD` selects.
    pub fn from_env(env: &EnvMap) -> Self {
        match env
            .get("SLUICE_GUARD")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("deny") => Self::Suggest,
            Some("0" | "false" | "no" | "off") => Self::Off,
            _ => Self::Rewrite,
        }
    }
}

/// The real file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct Fs;

impl World for Fs {
    fn resolve_dir(&self, path: &Path) -> Option<PathBuf> {
        let canonical = std::fs::canonicalize(path).ok()?;
        canonical.is_dir().then_some(canonical)
    }

    fn gates_from(&self, dir: &Path) -> Result<Option<Gates>, ()> {
        for d in dir.ancestors() {
            let file = d.join(REPO_CONFIG_FILE);
            match std::fs::read_to_string(&file) {
                Ok(text) => {
                    let config = RepoConfig::parse(&text, &file).map_err(|_| ())?;
                    return Ok(Some(Gates {
                        dir: d.to_path_buf(),
                        gates: config
                            .gates
                            .iter()
                            .map(|(name, g)| (name.clone(), g.cmd.clone()))
                            .collect(),
                    }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(()),
            }
        }
        Ok(None)
    }
}

/// Deny text for a raw gate command (today's shell guard text).
pub fn gate_deny_text(gate: &str) -> String {
    format!(
        "This repository verifies through Sluice: call the `verify` MCP tool with \
         gates=[\"{gate}\"] (gate '{gate}' runs this command). Raw runs bypass the shared \
         queue and cache."
    )
}

/// Deny text of suggestion mode for a heavy command.
pub fn heavy_deny_text(rewritten: &str) -> String {
    format!(
        "This machine shares its CPU through Sluice: run `{rewritten}` instead (same output \
         and exit code)."
    )
}

/// The one-line note added to the agent's context after a rewrite.
pub fn context_note(rewritten: &str, gate: Option<&str>) -> String {
    match gate {
        Some(gate) => format!(
            "Sluice ran this through its shared queue as gate '{gate}' (cached per tree): \
             `{rewritten}`. The exit code is the gate's; a cached pass skips the run. Use the \
             `verify` tool for gates."
        ),
        None => format!(
            "Sluice ran this through its shared queue as: `{rewritten}`. Output and exit code \
             are the command's own. Use the `verify` tool for gates."
        ),
    }
}

fn deny(reason: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
    .to_string()
}

/// Everything the guard reads besides stdin.
#[derive(Debug)]
pub struct Inputs<'a> {
    /// The process environment.
    pub env: &'a EnvMap,
    /// Path of the managed settings file.
    pub managed_settings: &'a Path,
    /// The file system.
    pub world: &'a dyn World,
}

impl std::fmt::Debug for dyn World + '_ {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("World")
    }
}

/// The hook's answer to `input`, or `None` for "no output".
pub fn respond(input: &str, inputs: &Inputs<'_>) -> Option<String> {
    let mode = Mode::from_env(inputs.env);
    if mode == Mode::Off {
        return None;
    }
    let value: Value = serde_json::from_str(input).ok()?;
    if let Some(event) = value.get("hook_event_name")
        && event.as_str()? != "PreToolUse"
    {
        return None;
    }
    if let Some(tool) = value.get("tool_name")
        && tool.as_str()? != "Bash"
    {
        return None;
    }
    let tool_input = value.get("tool_input")?.as_object()?;
    let command = tool_input.get("command")?.as_str()?;
    if command.trim().is_empty() {
        return None;
    }
    if matcher::runs_approve(command) {
        return Some(deny(APPROVE_DENY));
    }
    let cwd = value
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let world = inputs.world;
    let legacy_gates = world
        .gates_from(&cwd)
        .ok()
        .flatten()
        .map(|g| g.gates)
        .unwrap_or_default();
    let legacy = || {
        matcher::legacy_gate_hit(command, &legacy_gates).map(|gate| deny(&gate_deny_text(&gate)))
    };
    let user = match UserConfig::load(inputs.env) {
        Ok(user) => user,
        Err(e) => {
            eprintln!("sluice: guard: {e}; no rewrites");
            return legacy();
        }
    };
    let heavy = HeavyMatcher::from_config(&user.agent);
    let plan = match matcher::plan(command, &cwd, &heavy, world) {
        Ok(plan) => plan,
        Err(NoPlan::Nothing) => return legacy(),
        Err(NoPlan::Error) => return None,
    };
    let rewritten = plan.apply(command);
    // A raw gate left in another segment is still denied, as today.
    if let Some(gate) = matcher::legacy_gate_hit(&rewritten, &legacy_gates) {
        return Some(deny(&gate_deny_text(&gate)));
    }
    let gate = plan.first_gate();
    if mode == Mode::Suggest {
        return Some(deny(
            &gate.map_or_else(|| heavy_deny_text(&rewritten), gate_deny_text),
        ));
    }
    let original_segments = lex::split(command)?;
    let Some(rewritten_segments) = lex::split(&rewritten) else {
        return legacy();
    };
    let decision = rules::settings_paths(inputs.env, inputs.managed_settings)
        .ok_or(())
        .and_then(|paths| rules::load(&paths))
        .map_or(Decision::Nothing, |rules| {
            rules::decide(
                &rules,
                value.get("permission_mode").and_then(Value::as_str),
                (command, &original_segments),
                (&rewritten, &rewritten_segments),
            )
        });
    let decision = match decision {
        Decision::Allow => "allow",
        Decision::Ask => "ask",
        Decision::Nothing => return legacy(),
    };
    let mut updated = tool_input.clone();
    updated.insert("command".to_owned(), Value::String(rewritten.clone()));
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
                "permissionDecisionReason": format!(
                    "Sluice rewrote this command to run through its shared CPU queue: {rewritten}"
                ),
                "updatedInput": Value::Object(updated),
                "additionalContext": context_note(&rewritten, gate),
            }
        })
        .to_string(),
    )
}

fn watchdog_limit(env: &EnvMap) -> Duration {
    env.get(WATCHDOG_ENV)
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(WATCHDOG, Duration::from_millis)
}

/// `sluice guard`: always exits 0; prints the decision JSON or nothing.
pub fn run_hook() -> ExitCode {
    let env: EnvMap = std::env::vars().collect();
    if Mode::from_env(&env) == Mode::Off {
        return ExitCode::SUCCESS;
    }
    let limit = watchdog_limit(&env);
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
        respond(
            &input,
            &Inputs {
                env: &env,
                managed_settings: Path::new(rules::MANAGED_SETTINGS),
                world: &Fs,
            },
        )
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
mod tests;
