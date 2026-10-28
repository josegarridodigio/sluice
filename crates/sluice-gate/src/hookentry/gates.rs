//! The hook units as gates (ADR-0021 decision 7, wave H3).
//!
//! In a repository without `.sluice.toml`, `verify`, `gate_list`,
//! `sluice run` and `sluice gate list` use the check units of the commit-time
//! and push hooks as gates. A gate **is** the unit's synthetic gate
//! ([`super::runner::gate_of`]) under the unit's own name
//! (`<manager>:<hook>#<index>`), so the cache key `verify` derives is the one
//! the hook run derives for the same tree: tree snapshot, name, command, the
//! `PATH` addition and the manager's key inputs all come from the same code.
//!
//! What is not a gate, and why, is part of the result:
//!
//! - a hook Sluice delegates (it does not understand it completely);
//! - a [`Kind::Mutating`] unit: `verify` must never edit the user's files or
//!   index outside a commit;
//! - the units of a hook with a [`super::units::Precondition`] that does not
//!   hold now (lefthook needs something staged and nothing partially staged;
//!   pre-commit would stash unstaged changes): both depend on the staged set,
//!   which only a commit fixes;
//! - `commit-msg` and `prepare-commit-msg`: they receive the message file.
//!
//! With a `.sluice.toml` nothing is derived; [`share`] decides which unit may
//! reuse the cache entry of a configured gate.

use std::time::Duration;

use sluice_gate_core::config::{EnvMap, Gate, GateMode, RepoConfig};
use sluice_gate_core::git::{self, RepoPaths};
use sluice_gate_core::hookgate::{HookExclusion, HookGate, HookGates};

use super::units::{Kind, Plan, Program, Unit};
use super::{activate, delegate, hook_plans, precond, runner};

/// Hooks whose check units are gates.
pub const GATE_HOOKS: [&str; 3] = ["pre-commit", "pre-merge-commit", "pre-push"];

/// Hooks that receive the commit message file: never gates.
const MESSAGE_HOOKS: [&str; 2] = ["prepare-commit-msg", "commit-msg"];

/// The hook a unit name (`<manager>:<hook>#<index>`) belongs to.
pub fn hook_of_name(name: &str) -> Option<&str> {
    let (_, rest) = name.split_once(':')?;
    let (hook, index) = rest.rsplit_once('#')?;
    let known = GATE_HOOKS.contains(&hook) && index.parse::<usize>().is_ok();
    known.then_some(hook)
}

/// The gates of the repository's hooks, derived the way a hook run would plan
/// them. `only` limits the planning to one hook (`sluice run <name>`: no need
/// to ask lefthook about the others).
pub fn derive(paths: &RepoPaths, env: &EnvMap, only: Option<&str>) -> HookGates {
    let mut out = HookGates::default();
    let probe = git::probe_env(env, Some(paths));
    let Ok(ins) = activate::inspect(&paths.common_dir, &paths.toplevel, &probe) else {
        return out;
    };
    let dir = delegate::underlying_dir(
        ins.underlying.as_deref(),
        &paths.common_dir,
        env.get("HOME").map(String::as_str),
    );
    let wanted = |hook: &str| {
        only.is_none_or(|o| o == hook)
            && (GATE_HOOKS.contains(&hook) || MESSAGE_HOOKS.contains(&hook))
    };
    for (hook, planned) in hook_plans(paths, &dir, env, &wanted) {
        if MESSAGE_HOOKS.contains(&hook) {
            out.excluded.push(whole_hook(
                hook,
                "it receives the commit message file, which only a commit provides",
            ));
            continue;
        }
        match planned {
            Plan::Delegate(reason) => out.excluded.push(whole_hook(hook, &reason)),
            Plan::Units(program) => add_program(&mut out, paths, env, &program),
        }
    }
    out
}

fn whole_hook(hook: &str, reason: &str) -> HookExclusion {
    HookExclusion {
        hook: hook.to_owned(),
        unit: None,
        command: None,
        reason: format!("delegated, not a gate: {reason}"),
    }
}

fn excluded_unit(unit: &Unit, reason: String) -> HookExclusion {
    HookExclusion {
        hook: unit.hook.clone(),
        unit: Some(unit.name()),
        command: Some(unit.command.clone()),
        reason,
    }
}

fn add_program(out: &mut HookGates, paths: &RepoPaths, env: &EnvMap, program: &Program) {
    let blocked = program.precondition.and_then(|pre| {
        precond::check(pre, &paths.toplevel, env).err().map(|why| {
            format!(
                "it needs that {} when it runs (a commit decides that), and it does not hold now: {why}",
                pre.describe()
            )
        })
    });
    let key_env = runner::key_env(env, &paths.toplevel);
    for unit in program.units() {
        if unit.kind == Kind::Mutating {
            out.excluded.push(excluded_unit(
                unit,
                "it may change files or the index (a formatter, `lint-staged` or a program not \
                 known to be read-only): it only runs inside the hook"
                    .to_owned(),
            ));
        } else if let Some(why) = &blocked {
            out.excluded.push(excluded_unit(unit, why.clone()));
        } else {
            out.gates.push(HookGate {
                name: unit.name(),
                manager: unit.manager.slug().to_owned(),
                hook: unit.hook.clone(),
                gate: runner::gate_of(unit, &key_env),
            });
        }
    }
}

/// What a unit may do with the configured gates of `.sluice.toml`.
#[derive(Debug)]
pub enum Share {
    /// No configured gate has the unit's command.
    NoMatch,
    /// A gate has the same command but its cache entry cannot be shared
    /// exactly; the reason.
    Differs(String, &'static str),
    /// The unit runs under this gate's name and definition, so `verify` of the
    /// gate and the hook hit each other's entries.
    Exact(String, Box<Gate>),
}

/// Whether `unit` can reuse the cache entry of a configured gate: only when
/// running the unit as that gate is the same run. The key hashes the gate's
/// name, command, `env` and key inputs, so the unit must use the gate's name
/// and definition; that is exact only when:
///
/// - the unit is a check that sees no git variables and the manager adds
///   nothing to `PATH` (husky's `node_modules/.bin` first would run another
///   binary than the gate's `sh -c`);
/// - exactly one gate has the same command text (ambiguity: none);
/// - the gate is cached, not queue-only, not path-scoped (its key would cover
///   fewer files than the hook sees) and declares no `env` (the hook would run
///   with variables the repository's hook does not set).
///
/// The gate's `timeout` is dropped (a hook has none); its `weight` stays.
pub fn share(unit: &Unit, config: &RepoConfig) -> Share {
    let same: Vec<(&String, &Gate)> = config
        .gates
        .iter()
        .filter(|(_, g)| g.cmd.trim() == unit.command.trim())
        .collect();
    let [(name, gate)] = same.as_slice() else {
        return if same.is_empty() {
            Share::NoMatch
        } else {
            Share::Differs(
                same.iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                "several gates have this command, so there is no single entry to share",
            )
        };
    };
    let name = (*name).clone();
    let why = if unit.kind != Kind::Check || unit.git_env {
        Some("the unit is not a plain check")
    } else if !unit.path_prepend.is_empty() {
        Some("the hook puts directories before PATH (husky: node_modules/.bin), the gate does not")
    } else if gate.mode != GateMode::Gate || !gate.cache {
        Some("the gate is not cached")
    } else if gate.paths.is_some() {
        Some("the gate is path-scoped, its key covers fewer files than the hook sees")
    } else if !gate.env.is_empty() {
        Some("the gate sets environment variables the hook does not")
    } else {
        None
    };
    if let Some(why) = why {
        return Share::Differs(name, why);
    }
    let mut shared = (*gate).clone();
    shared.timeout = Duration::ZERO;
    Share::Exact(name, Box::new(shared))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hookentry::delegate::Manager;
    use std::path::PathBuf;

    fn unit(manager: Manager, command: &str, prepend: &[&str]) -> Unit {
        Unit {
            manager,
            hook: "pre-commit".to_owned(),
            index: 1,
            command: command.to_owned(),
            argv: command.split(' ').map(str::to_owned).collect(),
            cwd: PathBuf::from("/r"),
            path_prepend: prepend.iter().map(|p| (*p).to_owned()).collect(),
            kind: Kind::Check,
            git_env: false,
            key_inputs: Vec::new(),
            key_files: Vec::new(),
        }
    }

    fn config(gates: &str) -> RepoConfig {
        RepoConfig::parse(
            &format!("version = 1\n{gates}"),
            std::path::Path::new(".sluice.toml"),
        )
        .unwrap()
    }

    #[test]
    fn a_unit_name_tells_its_hook() {
        assert_eq!(hook_of_name("husky:pre-commit#2"), Some("pre-commit"));
        assert_eq!(hook_of_name("pre-commit:pre-push#10"), Some("pre-push"));
        assert_eq!(hook_of_name("husky:commit-msg#1"), None);
        assert_eq!(hook_of_name("test"), None);
        assert_eq!(hook_of_name("husky:pre-commit#x"), None);
    }

    #[test]
    fn a_plain_check_shares_the_entry_of_the_gate_with_its_command_and_drops_the_timeout() {
        let cfg = config("[gates.unit]\ncmd = \"jest --ci\"\ntimeout = \"5m\"\nweight = 3\n");
        let Share::Exact(name, gate) = share(&unit(Manager::Plain, "jest --ci", &[]), &cfg) else {
            panic!("expected a shared entry");
        };
        assert_eq!(name, "unit");
        assert!(gate.timeout.is_zero());
        assert_eq!(gate.weight, sluice_gate_core::config::Weight::Fixed(3));
        assert!(matches!(
            share(&unit(Manager::Plain, "tsc", &[]), &cfg),
            Share::NoMatch
        ));
    }

    #[test]
    fn a_unit_does_not_share_when_the_run_would_differ() {
        let cfg = config("[gates.unit]\ncmd = \"jest --ci\"\n");
        let husky = unit(Manager::Husky, "jest --ci", &["node_modules/.bin"]);
        assert!(
            matches!(share(&husky, &cfg), Share::Differs(n, w) if n == "unit" && w.contains("PATH"))
        );
        let mut git_env = unit(Manager::Lefthook, "jest --ci", &[]);
        git_env.git_env = true;
        assert!(matches!(share(&git_env, &cfg), Share::Differs(..)));
        for (gate, why) in [
            ("cmd = \"jest --ci\"\nenv = { A = \"1\" }", "environment"),
            ("cmd = \"jest --ci\"\npaths = [\"src/**\"]", "path-scoped"),
            ("cmd = \"jest --ci\"\ncache = false", "not cached"),
        ] {
            let cfg = config(&format!("[gates.unit]\n{gate}\n"));
            let shared = share(&unit(Manager::Plain, "jest --ci", &[]), &cfg);
            assert!(
                matches!(&shared, Share::Differs(_, w) if w.contains(why)),
                "{gate}: {shared:?}"
            );
        }
        let two = config("[gates.a]\ncmd = \"jest --ci\"\n[gates.b]\ncmd = \"jest --ci\"\n");
        assert!(matches!(
            share(&unit(Manager::Plain, "jest --ci", &[]), &two),
            Share::Differs(n, _) if n == "a, b"
        ));
    }
}
