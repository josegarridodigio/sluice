//! Sluice as the git hooks entry point of a clone (ADR-0021, wave H1).
//!
//! `sluice hooks on` gives a clone a hooks directory of its own
//! (`<common git dir>/sluice/hooks/`, one script per hook git supports) and
//! points `core.hooksPath` at it through a file included by the last
//! `[include]` of the clone's git config, so a later `git config
//! core.hooksPath ...` (husky does it on every install) does not displace it.
//! Every script runs `sluice hooks run <hook>`, which executes the hook the
//! repository would have run without Sluice ([`delegate`]), untouched.
//! Nothing in the repository changes. Design:
//! `kb/sluice/design/hook-runner.md`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use sluice_gate_core::config::{EnvMap, RepoConfig};
use sluice_gate_core::git::{self, RepoPaths};

use crate::run::EXIT_ERROR;

pub mod activate;
pub mod classify;
pub mod delegate;
pub mod gates;
pub mod gitcfg;
pub mod grammar;
pub mod lefthook;
pub mod plan;
pub mod precommit;
pub mod precond;
pub mod runner;
pub mod script;
pub mod tooling;
pub mod units;

pub use delegate::run;

/// Where the managed files of a clone live.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The clone's git common directory.
    pub common: PathBuf,
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

impl Layout {
    pub fn new(common: &Path) -> Self {
        let root = common.join("sluice");
        Self {
            common: common.to_path_buf(),
            dir: root.join("hooks"),
            config: root.join("hooks.gitconfig"),
            state: root.join("hooks.json"),
        }
    }

    /// Sluice's hooks directory, the value of `core.hooksPath`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file the clone's config includes.
    pub fn config_file(&self) -> &Path {
        &self.config
    }

    /// The record of what was in effect before activation.
    pub fn state_file(&self) -> &Path {
        &self.state
    }
}

/// The recorded state of a clone, when it was activated.
pub fn read_state(layout: &Layout) -> Option<activate::State> {
    let text = std::fs::read_to_string(layout.state_file()).ok()?;
    serde_json::from_str(&text).ok()
}

fn repo() -> Result<(RepoPaths, EnvMap), String> {
    let env = sluice_gate_core::config::process_env();
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the current directory: {e}"))?;
    let paths = git::discover(&cwd, &env).map_err(|e| match e {
        sluice_gate_core::error::Error::NotARepository(_) => {
            "not inside a git repository (run 'sluice hooks' from one)".to_owned()
        }
        other => other.to_string(),
    })?;
    Ok((paths, env))
}

fn fail(message: &str) -> ExitCode {
    eprintln!("sluice: error: {message}");
    ExitCode::from(EXIT_ERROR)
}

fn previous_text(previous: Option<&str>) -> String {
    previous.map_or_else(
        || "the default hooks directory of the clone".to_owned(),
        |p| format!("`{p}`"),
    )
}

/// `sluice hooks on`.
pub fn on() -> ExitCode {
    let (paths, env) = match repo() {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    match activate::activate(&paths, &env) {
        Ok(done) => {
            println!(
                "sluice: hooks entry point on for {} ({} script(s) written)",
                paths.toplevel.display(),
                done.written
            );
            println!("  hooks directory: {}", done.state.dir);
            println!(
                "  delegating to:   {}",
                previous_text(done.state.previous.as_deref())
            );
            println!("  undo with:       sluice hooks off");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("cannot turn the hooks entry point on: {e}")),
    }
}

/// `sluice hooks off`.
pub fn off() -> ExitCode {
    let (paths, env) = match repo() {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    match activate::deactivate(&paths, &env) {
        Ok(true) => {
            println!(
                "sluice: hooks entry point off for {}",
                paths.toplevel.display()
            );
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!("sluice: hooks entry point was already off");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("cannot turn the hooks entry point off: {e}")),
    }
}

/// What would happen for each hook the repository has: run as units or
/// delegated, and why.
fn hook_plans(
    paths: &RepoPaths,
    hooks_dir: &Path,
    env: &EnvMap,
    wanted: &dyn Fn(&str) -> bool,
) -> Vec<(&'static str, units::Plan)> {
    let full = activate::resolve(hooks_dir, &paths.toplevel);
    let forced = runner::opt_out(env);
    activate::mirrored(&full)
        .into_iter()
        .filter(|hook| wanted(hook))
        .map(|hook| {
            let planned = forced.clone().map_or_else(
                || {
                    plan::plan(&plan::Input {
                        hook,
                        root: &paths.toplevel,
                        dir: hooks_dir,
                        env,
                    })
                },
                units::Plan::Delegate,
            );
            (hook, planned)
        })
        .collect()
}

/// The JSON of `status`.
fn describe(
    paths: &RepoPaths,
    ins: &activate::Inspection,
    env: &EnvMap,
    plans: &[(&'static str, units::Plan)],
    config: Option<&RepoConfig>,
) -> serde_json::Value {
    let home = env.get("HOME").map(String::as_str);
    let hooks_dir = delegate::underlying_dir(ins.underlying.as_deref(), &paths.common_dir, home);
    let manager = delegate::detect(&paths.toplevel, &hooks_dir);
    serde_json::json!({
        "schema": 1,
        "active": ins.managed(),
        "effective": ins.managed() && ins.effective,
        "directory": ins.dir,
        "previous": ins.state.as_ref().and_then(|s| s.previous.clone()),
        "underlying": ins.underlying,
        "manager": manager.label(),
        "problem": ins.problem(),
        "hooks": plans
            .iter()
            .map(|(h, p)| plan::describe(h, p, config))
            .collect::<Vec<_>>(),
    })
}

/// One line per hook for the text `status`.
fn print_plans(plans: &[(&'static str, units::Plan)], config: Option<&RepoConfig>) {
    for (hook, planned) in plans {
        match planned {
            units::Plan::Delegate(reason) => {
                println!("  hook {hook}: delegated ({reason})");
            }
            units::Plan::Units(program) => {
                let kinds: Vec<String> = program
                    .units()
                    .map(|u| format!("{} {}", u.kind.label(), u.command))
                    .collect();
                println!(
                    "  hook {hook}: {} unit(s) run through the queue: {}",
                    program.unit_count(),
                    kinds.join("; ")
                );
                if let Some(pre) = program.precondition {
                    println!("    (delegated at run time unless {})", pre.describe());
                }
                for note in program
                    .units()
                    .filter_map(|u| config.and_then(|c| plan::share_note(u, c)).map(|n| (u, n)))
                {
                    println!("    {}: {}", note.0.name(), note.1);
                }
            }
        }
    }
}

/// `sluice hooks status [--json]`.
pub fn status(json: bool) -> ExitCode {
    let (paths, env) = match repo() {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let probe = git::probe_env(&env, Some(&paths));
    let ins = match activate::inspect(&paths.common_dir, &paths.toplevel, &probe) {
        Ok(i) => i,
        Err(e) => return fail(&e),
    };
    let hooks_dir = delegate::underlying_dir(
        ins.underlying.as_deref(),
        &paths.common_dir,
        env.get("HOME").map(String::as_str),
    );
    let plans = hook_plans(&paths, &hooks_dir, &env, &|_| true);
    let config = RepoConfig::load(&paths.toplevel).ok();
    let value = describe(&paths, &ins, &env, &plans, config.as_ref());
    if json {
        println!("{value:#}");
        return ExitCode::SUCCESS;
    }
    println!(
        "hooks entry point: {}",
        if ins.managed() { "on" } else { "off" }
    );
    println!("  directory:       {}", ins.dir);
    println!(
        "  repository hooks: {}",
        previous_text(ins.underlying.as_deref())
    );
    println!(
        "  manager:         {}",
        value["manager"].as_str().unwrap_or_default()
    );
    if ins.managed() {
        println!(
            "  effective:       {}",
            if ins.effective { "yes" } else { "no" }
        );
        if let Some(problem) = ins.problem() {
            println!("  problem:         {problem}");
            println!("  fix:             sluice hooks on");
        }
    } else {
        println!("  enable with:     sluice hooks on");
    }
    print_plans(&plans, config.as_ref());
    ExitCode::SUCCESS
}

/// What the session start found out about a clone's entry point.
#[derive(Debug, PartialEq, Eq)]
pub enum Restore {
    /// The clone was never activated (or there is nothing to check).
    Untouched,
    /// Activated and working.
    Healthy,
    /// Activated, displaced, and put back.
    Restored,
    /// Activated, displaced, and not put back (the reason).
    Failed(String),
}

/// The worktree root and git common directory of the clone that holds `dir`,
/// found without running git: `.git` is a directory, or a file pointing at a
/// linked worktree's git dir, whose `commondir` names the shared one.
pub fn find_common_dir(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    for d in dir.ancestors() {
        let dot_git = d.join(".git");
        if dot_git.is_dir() {
            return Some((d.to_path_buf(), dot_git));
        }
        if dot_git.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let git_dir = d.join(text.trim().strip_prefix("gitdir:")?.trim());
            let common = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
            let common = std::fs::canonicalize(git_dir.join(common.trim())).ok()?;
            return Some((d.to_path_buf(), common));
        }
    }
    None
}

/// Session start: when the clone holding `cwd` was activated and is no longer
/// effective, activates it again. Costs one `git config` read for a healthy
/// clone and nothing for one that was never activated.
pub fn restore_if_displaced(cwd: &Path, env: &EnvMap) -> Restore {
    let Some((root, common)) = find_common_dir(cwd) else {
        return Restore::Untouched;
    };
    if !Layout::new(&common).state_file().is_file() {
        return Restore::Untouched;
    }
    match activate::inspect(&common, &root, env) {
        Ok(ins) if ins.healthy() => return Restore::Healthy,
        Ok(_) => {}
        Err(e) => return Restore::Failed(e),
    }
    let result = git::discover(cwd, env)
        .map_err(|e| e.to_string())
        .and_then(|paths| activate::activate(&paths, env));
    match result {
        Ok(_) => Restore::Restored,
        Err(e) => Restore::Failed(e),
    }
}

/// [`restore_if_displaced`] in a worker thread that may use `limit`; a late
/// worker is left behind (every step is idempotent).
pub fn restore_within(cwd: &Path, env: &EnvMap, limit: Duration) -> Restore {
    let (tx, rx) = std::sync::mpsc::channel();
    let (cwd, env) = (cwd.to_path_buf(), env.clone());
    std::thread::spawn(move || {
        let _ = tx.send(restore_if_displaced(&cwd, &env));
    });
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| Restore::Failed("timed out".to_owned()))
}
