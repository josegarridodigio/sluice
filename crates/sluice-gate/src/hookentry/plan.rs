//! From a hook to a [`Plan`]: the husky and plain-hook adapters.
//!
//! The principle of ADR-0021: understand the hook completely or delegate it.
//! Every way out is a [`Plan::Delegate`] with a reason that `sluice hooks
//! status` shows. lefthook and pre-commit are not adapted yet (wave H2b) and
//! delegate.

use std::fs;
use std::path::{Path, PathBuf};

use sluice_gate_core::config::EnvMap;

use super::activate::resolve;
use super::classify;
use super::delegate::{Manager, detect};
use super::grammar::{self, Script};
use super::units::{Kind, Plan, Program, Unit};

/// Hooks that receive data on stdin. A unit runs as a daemon job without the
/// hook's stdin, so such a hook is only run as units when it has one.
const STDIN_HOOKS: [&str; 6] = [
    "pre-push",
    "pre-receive",
    "post-receive",
    "post-rewrite",
    "reference-transaction",
    "proc-receive",
];

/// What the planner needs to know about a hook.
#[derive(Debug, Clone, Copy)]
pub struct Input<'a> {
    pub hook: &'a str,
    /// The worktree root, where git runs hooks.
    pub root: &'a Path,
    /// The hooks directory git would use without Sluice (may be relative).
    pub dir: &'a Path,
    pub env: &'a EnvMap,
}

/// Decides what to run for `input.hook`.
pub fn plan(input: &Input<'_>) -> Plan {
    let manager = detect(input.root, input.dir);
    let planned = match manager {
        Manager::Husky => husky(input),
        Manager::Plain => plain(input),
        Manager::Lefthook | Manager::PreCommit => Err(format!(
            "{} hooks are delegated (their units come in a later wave)",
            manager.label()
        )),
        Manager::None => Err("no hook manager recognised".to_owned()),
    };
    match planned.and_then(|p| check_stdin(input.hook, p)) {
        Ok(program) => Plan::Units(program),
        Err(reason) => Plan::Delegate(reason),
    }
}

/// A unit runs as a daemon job without the hook's stdin, so a hook that
/// receives stdin runs as units only when every unit is a check (the
/// recognised tools do not read stdin).
fn check_stdin(hook: &str, program: Program) -> Result<Program, String> {
    if STDIN_HOOKS.contains(&hook)
        && let Some(unit) = program.units().find(|u| u.kind != Kind::Check)
    {
        return Err(format!(
            "{hook} receives stdin and `{}` is not a known read-only tool, which might read it",
            unit.command
        ));
    }
    Ok(program)
}

fn home_file(env: &EnvMap, rest: &str) -> Option<PathBuf> {
    env.get("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| Path::new(h).join(rest))
}

/// What husky does at run time that a unit runner cannot see: `HUSKY` (skip or
/// trace), the user's `init.sh` (sourced, arbitrary shell) and `~/.huskyrc`.
fn husky_runtime(env: &EnvMap) -> Result<(), String> {
    if env.get("HUSKY").is_some_and(|v| !v.is_empty()) {
        return Err("HUSKY is set in the environment".to_owned());
    }
    let config = env
        .get("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(|d| Path::new(d).join("husky/init.sh"))
        .or_else(|| home_file(env, ".config/husky/init.sh"));
    if config.is_some_and(|p| p.is_file()) {
        return Err("husky's init.sh exists (arbitrary shell run before the hook)".to_owned());
    }
    if home_file(env, ".huskyrc").is_some_and(|p| p.is_file()) {
        return Err("~/.huskyrc exists".to_owned());
    }
    Ok(())
}

/// husky v9: `<d>/_/<hook>` is a stub that sources `<d>/_/h`, which runs
/// `<d>/<hook>` with `sh -e` after prepending `node_modules/.bin` to `PATH`.
fn husky(input: &Input<'_>) -> Result<Program, String> {
    let dir = resolve(input.dir, input.root);
    let h = fs::read_to_string(dir.join("h")).map_err(|_| "husky's _/h is missing".to_owned())?;
    if !(h.contains("export PATH=\"node_modules/.bin:$PATH\"") && h.contains("sh -e \"$s\"")) {
        return Err("husky's _/h is not a version whose behaviour is known".to_owned());
    }
    husky_runtime(input.env)?;
    let user = dir
        .parent()
        .map(|d| d.join(input.hook))
        .filter(|p| p.is_file())
        .ok_or_else(|| format!("no user file for {} (husky exits 0)", input.hook))?;
    let script = read_script(&user)?;
    build(input, &script, Manager::Husky, &["node_modules/.bin"])
}

/// A hook file git runs directly.
fn plain(input: &Input<'_>) -> Result<Program, String> {
    let dir = resolve(input.dir, input.root);
    let script = read_script(&dir.join(input.hook))?;
    if script.sources_husky {
        husky_runtime(input.env)?;
    }
    // Run directly, a file only stops at a failure with `set -e` (husky's
    // bootstrap re-runs it with `sh -e`); several unguarded lines would need
    // "run them all, report the last status".
    if !script.errexit && !script.sources_husky && script.lists.len() > 1 {
        return Err("has several lines and no `set -e`".to_owned());
    }
    build(input, &script, Manager::Plain, &[])
}

fn read_script(path: &Path) -> Result<Script, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    grammar::parse(&text).map_err(|e| format!("{} {e}", path.display()))
}

/// Turns the parsed file into units; a command that already runs Sluice makes
/// the whole hook a delegation (the hook is already routed).
fn build(
    input: &Input<'_>,
    script: &Script,
    manager: Manager,
    path_prepend: &[&str],
) -> Result<Program, String> {
    let mut index = 0;
    let mut lists = Vec::new();
    for list in &script.lists {
        let mut units = Vec::new();
        for cmd in list {
            index += 1;
            if cmd
                .argv
                .first()
                .is_some_and(|p| p.rsplit('/').next() == Some("sluice"))
            {
                return Err("already runs sluice".to_owned());
            }
            units.push(Unit {
                manager,
                hook: input.hook.to_owned(),
                index,
                command: cmd.text.clone(),
                argv: cmd.argv.clone(),
                cwd: input.root.to_path_buf(),
                path_prepend: path_prepend.iter().map(|p| (*p).to_owned()).collect(),
                kind: classify::classify(&cmd.argv, input.root),
            });
        }
        lists.push(units);
    }
    Ok(Program {
        lists,
        exit_ok: script.exit_ok,
    })
}

/// The JSON of one hook in `hooks status`: `mode` is `units` or `delegate`
/// (with its `reason`).
pub fn describe(hook: &str, plan: &Plan) -> serde_json::Value {
    match plan {
        Plan::Delegate(reason) => {
            serde_json::json!({"hook": hook, "mode": "delegate", "reason": reason})
        }
        Plan::Units(program) => {
            let units: Vec<_> = program
                .units()
                .map(|u| {
                    serde_json::json!({
                        "index": u.index,
                        "kind": u.kind.label(),
                        "command": u.command,
                    })
                })
                .collect();
            serde_json::json!({"hook": hook, "mode": "units", "units": units})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    const H: &str = "export PATH=\"node_modules/.bin:$PATH\"\nsh -e \"$s\" \"$@\"\n";

    fn husky_repo(user: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".husky/_/h", H);
        write(tmp.path(), ".husky/_/pre-commit", "#!/usr/bin/env sh\n");
        write(tmp.path(), ".husky/pre-commit", user);
        tmp
    }

    fn plan_for(root: &Path, dir: &str, hook: &str, env: &EnvMap) -> Plan {
        plan(&Input {
            hook,
            root,
            dir: Path::new(dir),
            env,
        })
    }

    #[test]
    fn a_husky_hook_becomes_units_with_the_path_addition() {
        let tmp = husky_repo("npx lint-staged\nnpm run lint && npm test\n");
        let Plan::Units(p) = plan_for(tmp.path(), ".husky/_", "pre-commit", &EnvMap::new()) else {
            panic!("expected units");
        };
        assert_eq!(p.unit_count(), 3);
        let first = &p.lists[0][0];
        assert_eq!(first.name(), "husky:pre-commit#1");
        assert_eq!(first.kind, Kind::Mutating);
        assert_eq!(first.path_prepend, ["node_modules/.bin"]);
        assert_eq!(p.lists[1].len(), 2);
        assert_eq!(p.lists[1][1].index, 3);
    }

    #[test]
    fn husky_hooks_that_cannot_be_modelled_delegate_with_a_reason() {
        let env = EnvMap::new();
        let tmp = husky_repo("if [ -z \"$CI\" ]; then\n  npm test\nfi\n");
        let reason = |p: Plan| match p {
            Plan::Delegate(r) => r,
            Plan::Units(_) => String::new(),
        };
        assert!(reason(plan_for(tmp.path(), ".husky/_", "pre-commit", &env)).contains("line 1"));
        // A hook with no user file: husky exits 0.
        assert!(
            reason(plan_for(tmp.path(), ".husky/_", "pre-push", &env)).contains("no user file")
        );
        // A different `h`.
        write(tmp.path(), ".husky/_/h", "echo other\n");
        assert!(
            reason(plan_for(tmp.path(), ".husky/_", "pre-commit", &env)).contains("not a version")
        );
    }

    #[test]
    fn husky_runtime_switches_delegate() {
        let tmp = husky_repo("npm test\n");
        let mut env = EnvMap::new();
        env.insert("HUSKY".into(), "0".into());
        assert!(matches!(
            plan_for(tmp.path(), ".husky/_", "pre-commit", &env),
            Plan::Delegate(r) if r.contains("HUSKY")
        ));
        let mut env = EnvMap::new();
        env.insert("HOME".into(), tmp.path().display().to_string());
        write(tmp.path(), ".config/husky/init.sh", "export X=1\n");
        assert!(matches!(
            plan_for(tmp.path(), ".husky/_", "pre-commit", &env),
            Plan::Delegate(r) if r.contains("init.sh")
        ));
    }

    #[test]
    fn plain_hooks_need_set_e_for_several_lines_and_never_wrap_sluice() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvMap::new();
        write(
            tmp.path(),
            "hooks/pre-commit",
            "#!/bin/sh\nnpm run a\nnpm run b\n",
        );
        assert!(matches!(
            plan_for(tmp.path(), "hooks", "pre-commit", &env),
            Plan::Delegate(r) if r.contains("set -e")
        ));
        write(
            tmp.path(),
            "hooks/pre-commit",
            "#!/bin/sh\nset -e\nnpm run a\nnpm run b\n",
        );
        assert!(matches!(
            plan_for(tmp.path(), "hooks", "pre-commit", &env),
            Plan::Units(p) if p.unit_count() == 2 && p.lists[0][0].path_prepend.is_empty()
        ));
        write(
            tmp.path(),
            "hooks/pre-commit",
            "#!/bin/sh\n'/bin/sluice' run --hook pre-commit test\n",
        );
        assert!(matches!(
            plan_for(tmp.path(), "hooks", "pre-commit", &env),
            Plan::Delegate(r) if r.contains("already runs sluice")
        ));
    }

    #[test]
    fn a_hook_with_stdin_runs_as_units_only_when_every_unit_is_a_check() {
        let tmp = tempfile::tempdir().unwrap();
        let env = EnvMap::new();
        write(
            tmp.path(),
            "hooks/pre-push",
            "#!/bin/sh\nset -e\neslint .\njest --ci\n",
        );
        assert!(matches!(
            plan_for(tmp.path(), "hooks", "pre-push", &env),
            Plan::Units(p) if p.unit_count() == 2
        ));
        write(
            tmp.path(),
            "hooks/pre-push",
            "#!/bin/sh\n./scripts/check.sh\n",
        );
        assert!(matches!(
            plan_for(tmp.path(), "hooks", "pre-push", &env),
            Plan::Delegate(r) if r.contains("stdin")
        ));
    }

    #[test]
    fn lefthook_and_pre_commit_delegate_for_now() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "lefthook.yml", "pre-commit: {}\n");
        write(
            tmp.path(),
            ".git/hooks/pre-commit",
            "#!/bin/sh\n# LEFTHOOK\n",
        );
        assert!(matches!(
            plan_for(tmp.path(), ".git/hooks", "pre-commit", &EnvMap::new()),
            Plan::Delegate(r) if r.contains("lefthook")
        ));
    }
}
