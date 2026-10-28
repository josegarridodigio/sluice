//! Runs a hook's units through the daemon (ADR-0021 decisions 5 and 6).
//!
//! Each unit is a synthetic gate run exactly like `sluice run <gate>` runs one
//! ([`crate::run::run_unit`]: cache lookup, daemon queue, budget and
//! containment, logs, in-process fallback), in the user's working tree and in
//! file order:
//!
//! - a [`Kind::Mutating`] unit always runs and is never cached;
//! - a [`Kind::Check`] unit is looked up in the cache by what it is about to
//!   see (the working tree *after* the earlier units ran, so the key is
//!   computed unit by unit, just before each one), and its pass is stored.
//!
//! Semantics are those of `sh -e` over the file: an `&&` list stops at its
//! first failure; the hook ends with the failing unit's exit code only when
//! the failing unit is the last of its list, otherwise the next line runs (a
//! non-final member of an AND list does not trigger errexit).
//!
//! Exit codes: the hook's code is the failing unit's own. Sluice itself never
//! refuses a unit (the fail-fast policy of `sluice run` is not applied: the
//! unit is what the hook would have run), so a `3` here is always the unit's
//! own exit code, never Sluice's "not verified on this tree" refusal.
//!
//! Fail open: everything that can go wrong before the first unit starts
//! ([`prepare`]) makes the whole hook a delegation. If the daemon fails later,
//! `sluice run`'s own fallback runs the unit in-process under the budget
//! semaphore, with a line on stderr.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use sluice_gate_core::config::{EnvMap, Gate, GateMode, Weight};
use sluice_gate_core::git;
use sluice_gate_core::lookup::{self, RepoContext};
use sluice_gate_core::mode::{CacheMode, cache_mode};
use sluice_gate_proto::client;

use super::delegate::Manager;
use super::plan::{self, Input};
use super::units::{Kind, Plan, Program, Unit};

/// Environment variable that forces delegation of every hook.
pub const DELEGATE_VAR: &str = "SLUICE_HOOKS_DELEGATE";

/// Why units must not be used right now, from the environment alone.
pub fn opt_out(env: &EnvMap) -> Option<String> {
    if env
        .get(DELEGATE_VAR)
        .is_some_and(|v| !v.is_empty() && v != "0")
    {
        return Some(format!("{DELEGATE_VAR} is set"));
    }
    if cache_mode(env) == CacheMode::Bypass {
        return Some("SLUICE_DISABLE is set".to_owned());
    }
    if !sluice_gate_core::config::process_env_is_utf8() {
        return Some("the environment is not valid UTF-8".to_owned());
    }
    None
}

/// Everything resolved before the first unit runs.
struct Prepared {
    repo: RepoContext,
    env: EnvMap,
}

/// Resolves the repository and checks the daemon is reachable (starting it on
/// demand). Any failure means "delegate the whole hook".
fn prepare(root: &Path, env: &EnvMap) -> Result<Prepared, String> {
    let mode = cache_mode(env);
    let repo = lookup::resolve_repo_or_implicit(root, env, &mode).map_err(|e| e.to_string())?;
    let socket = client::ensure_daemon(env).map_err(|e| format!("{e:#}"))?;
    client::connect(&socket).map_err(|e| e.to_string())?;
    Ok(Prepared {
        repo,
        env: env.clone(),
    })
}

/// Runs `hook` as units when Sluice understands it completely and the daemon
/// is there; `None` means "delegate it as H1 does".
pub fn try_units(hook: &str, dir: &Path, cwd: &Path, env: &EnvMap) -> Option<ExitCode> {
    if let Some(why) = opt_out(env) {
        tracing::debug!(%hook, %why, "delegating");
        return None;
    }
    let root = git::discover(cwd, env).ok()?.toplevel;
    let planned = plan::plan(&Input {
        hook,
        root: &root,
        dir,
        env,
    });
    let Plan::Units(program) = planned else {
        if let Plan::Delegate(why) = planned {
            tracing::debug!(%hook, %why, "delegating");
        }
        return None;
    };
    let prepared = match prepare(&root, env) {
        Ok(p) => p,
        Err(why) => {
            tracing::debug!(%hook, %why, "delegating: cannot prepare");
            return None;
        }
    };
    Some(execute(&program, hook, &prepared))
}

/// What one `&&` list did.
struct ListOutcome {
    /// Exit code of the last unit that ran (0 when all passed).
    code: u8,
    /// The failing unit was the last of the list: `sh -e` ends the hook.
    ends_hook: bool,
}

fn execute(program: &Program, hook: &str, prep: &Prepared) -> ExitCode {
    let manager = program.units().next().map_or(Manager::Plain, |u| u.manager);
    eprintln!(
        "sluice: {hook}: running {} unit(s) of the {} hook through the queue",
        program.unit_count(),
        manager.label()
    );
    let mut status = 0;
    for list in &program.lists {
        let outcome = run_list(list, hook, prep);
        status = outcome.code;
        if outcome.code != 0 && outcome.ends_hook {
            return finish(manager, hook, outcome.code);
        }
    }
    if program.exit_ok {
        status = 0;
    }
    finish(manager, hook, status)
}

/// husky prints a line on stdout when the user file fails; so do we.
fn finish(manager: Manager, hook: &str, code: u8) -> ExitCode {
    if code != 0 && manager == Manager::Husky {
        println!("husky - {hook} script failed (code {code})");
        if code == 127 {
            println!(
                "husky - command not found in PATH={}",
                std::env::var("PATH").unwrap_or_default()
            );
        }
    }
    ExitCode::from(code)
}

/// Runs an `&&` list: stops at the first failing unit.
fn run_list(list: &[Unit], hook: &str, prep: &Prepared) -> ListOutcome {
    for (position, unit) in list.iter().enumerate() {
        let code = run_one(unit, hook, prep);
        if code != 0 {
            return ListOutcome {
                code,
                ends_hook: position + 1 == list.len(),
            };
        }
    }
    ListOutcome {
        code: 0,
        ends_hook: false,
    }
}

/// `PATH` as the manager's entries and the caller's `PATH` give it.
fn path_with(unit: &Unit, env: &EnvMap) -> String {
    let current = env.get("PATH").map_or("", String::as_str);
    format!("{}:{current}", unit.path_prepend.join(":"))
}

/// The synthetic gate of a unit: no timeout (a hook has none), weight a
/// ceiling that learned weights lower, cached only when it is a check.
fn gate_of(unit: &Unit, env: &EnvMap) -> Gate {
    let mut gate_env = BTreeMap::new();
    if !unit.path_prepend.is_empty() {
        gate_env.insert("PATH".to_owned(), path_with(unit, env));
    }
    Gate {
        cmd: unit.command.clone(),
        weight: Weight::Auto,
        timeout: Duration::ZERO,
        hooks: Vec::new(),
        key_inputs: Vec::new(),
        key_files: Vec::new(),
        env: gate_env,
        depends_on: Vec::new(),
        cache: unit.kind == Kind::Check,
        mode: GateMode::Gate,
        paths: None,
        paths_ignore: Vec::new(),
    }
}

/// Runs one unit and returns its exit code. A mutating unit keeps the
/// variables git sets for the hook (it may edit or read the index it names).
fn run_one(unit: &Unit, hook: &str, prep: &Prepared) -> u8 {
    let gate = gate_of(unit, &prep.env);
    let keep_git_env = unit.kind == Kind::Mutating;
    match prep.repo.select_unit(&unit.name(), gate, keep_git_env) {
        Ok(res) => crate::run::run_unit(res, hook),
        Err(e) => {
            eprintln!(
                "sluice: cannot prepare {}: {e}; running it directly",
                unit.name()
            );
            run_direct(unit, &prep.env, &prep.repo.paths.toplevel)
        }
    }
}

/// Last resort: the unit as `sh -c`, with inherited stdio.
fn run_direct(unit: &Unit, env: &EnvMap, root: &Path) -> u8 {
    let mut command = Command::new("sh");
    command.arg("-c").arg(&unit.command).current_dir(root);
    if !unit.path_prepend.is_empty() {
        command.env("PATH", path_with(unit, env));
    }
    match command.status() {
        Ok(status) => status
            .code()
            .map_or(143, |c| u8::try_from(c & 0xff).unwrap_or(1)),
        Err(e) => {
            eprintln!("sluice: cannot start {}: {e}", unit.name());
            127
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_opt_out_is_read_from_the_environment() {
        assert!(opt_out(&env(&[])).is_none());
        assert!(opt_out(&env(&[(DELEGATE_VAR, "1")])).is_some());
        assert!(opt_out(&env(&[(DELEGATE_VAR, "0")])).is_none());
        assert!(opt_out(&env(&[("SLUICE_DISABLE", "1")])).is_some());
    }

    fn unit(kind: Kind, prepend: &[&str]) -> Unit {
        Unit {
            manager: Manager::Husky,
            hook: "pre-commit".to_owned(),
            index: 1,
            command: "npm test".to_owned(),
            argv: vec!["npm".to_owned(), "test".to_owned()],
            cwd: PathBuf::from("/r"),
            path_prepend: prepend.iter().map(|p| (*p).to_owned()).collect(),
            kind,
        }
    }

    #[test]
    fn the_gate_of_a_unit_caches_checks_only_and_prepends_the_path_like_husky() {
        let e = env(&[("PATH", "/usr/bin:/bin")]);
        let check = gate_of(&unit(Kind::Check, &["node_modules/.bin"]), &e);
        assert!(check.cache && check.timeout.is_zero());
        assert_eq!(check.env["PATH"], "node_modules/.bin:/usr/bin:/bin");
        let mutating = gate_of(&unit(Kind::Mutating, &[]), &e);
        assert!(!mutating.cache && mutating.env.is_empty());
    }
}
