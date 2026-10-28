//! Activating, inspecting and removing the hooks entry point of a clone.
//!
//! Files (all under `<common git dir>/sluice/`): `hooks/<hook>` scripts,
//! `hooks.gitconfig` (`[core] hooksPath = <hooks dir>`) and `hooks.json`
//! (what was in effect before, for delegation and `off`). The only other
//! change is one `include.path` entry in the clone's own git config, made
//! through `git config`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sluice_gate_core::git::{self, EnvMap, RepoPaths};

use super::script::{ALL_HOOKS, ScriptInput, render};
use super::{Layout, gitcfg};
use crate::init::atomic;
use crate::shim;

/// `hooks.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    pub schema: u32,
    /// Sluice's hooks directory, the value of `core.hooksPath` it sets.
    pub dir: String,
    /// `core.hooksPath` before activation (`None`: the default hooks dir).
    pub previous: Option<String>,
    /// The `sluice` binary that activated the clone.
    pub sluice: String,
}

/// The result of `on`.
#[derive(Debug)]
pub struct Activated {
    pub state: State,
    /// Scripts written, changed or removed.
    pub written: usize,
}

/// What the scripts are rendered from.
struct Inputs<'a> {
    common: &'a Path,
    sluice: &'a Path,
    previous: Option<&'a str>,
}

/// `dir` made absolute against the worktree root `root` (git runs hooks there).
fn resolve(dir: &Path, root: &Path) -> PathBuf {
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        root.join(dir)
    }
}

/// The hooks that exist as executable files in `dir`: the ones Sluice mirrors.
/// A hook without a script in Sluice's directory is not run by git at all
/// once `core.hooksPath` points there, and a script for a hook the repository
/// does not have only costs a process start, so the two sets must be equal.
fn mirrored(dir: &Path) -> Vec<&'static str> {
    ALL_HOOKS
        .iter()
        .copied()
        .filter(|h| super::delegate::is_executable(&dir.join(h)))
        .collect()
}

/// The root of every worktree of the clone, `root` first. A relative
/// `core.hooksPath` (husky's `.husky/_`) resolves against each one, and the
/// worktrees share Sluice's directory.
fn worktree_roots(root: &Path, env: &EnvMap) -> Vec<PathBuf> {
    let mut roots = vec![root.to_path_buf()];
    let listed = git::run(&["worktree", "list", "--porcelain"], root, env, true);
    for line in listed.unwrap_or_default().lines() {
        if let Some(path) = line.strip_prefix("worktree ").map(PathBuf::from)
            && !roots.contains(&path)
        {
            roots.push(path);
        }
    }
    roots
}

/// The hooks of `underlying` in any worktree of the clone. A worktree whose
/// relative hooks directory is missing (a fresh linked worktree has no
/// `.husky/_`) must not take the scripts away from the others.
fn mirrored_in_clone(underlying: &Path, root: &Path, env: &EnvMap) -> Vec<&'static str> {
    if underlying.is_absolute() {
        return mirrored(underlying);
    }
    let found: Vec<Vec<&'static str>> = worktree_roots(root, env)
        .iter()
        .map(|r| mirrored(&r.join(underlying)))
        .collect();
    ALL_HOOKS
        .iter()
        .copied()
        .filter(|h| found.iter().any(|f| f.contains(h)))
        .collect()
}

/// Makes the scripts of Sluice's directory hold `want`: writes the missing
/// ones (all of them again when `rewrite`) and, when `prune`, removes the
/// others. Returns how many files changed.
fn reconcile(
    layout: &Layout,
    inputs: &Inputs<'_>,
    want: &[&str],
    rewrite: bool,
    prune: bool,
) -> Result<usize, String> {
    let script_input = ScriptInput {
        common: inputs.common,
        dir: layout.dir(),
        sluice: inputs.sluice,
        previous: inputs.previous,
    };
    let mut changed = 0;
    fs::create_dir_all(layout.dir())
        .map_err(|e| format!("cannot create {}: {e}", layout.dir().display()))?;
    for hook in want {
        let path = layout.dir().join(hook);
        if rewrite || !super::delegate::is_executable(&path) {
            changed += usize::from(write_if_changed(
                &path,
                &render(hook, &script_input),
                0o755,
            )?);
        }
    }
    let entries = if prune {
        fs::read_dir(layout.dir()).ok()
    } else {
        None
    };
    for entry in entries.into_iter().flatten().flatten() {
        let name = entry.file_name();
        if !want.iter().any(|h| name.to_str() == Some(h)) {
            fs::remove_file(entry.path())
                .map_err(|e| format!("cannot remove {}: {e}", entry.path().display()))?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// Adds a script for each hook `underlying` holds that has none (called by
/// `sluice hooks run`; cheap when nothing changed). It never removes one:
/// `root` is a single worktree, and another may still have the hook.
/// `sluice hooks on` and the session start prune across all worktrees.
/// Does nothing for a clone that was not activated.
pub fn sync(layout: &Layout, root: &Path, underlying: &Path) -> Result<usize, String> {
    let Some(state) = super::read_state(layout) else {
        return Ok(0);
    };
    let inputs = Inputs {
        common: &layout.common,
        sluice: Path::new(&state.sluice),
        previous: state.previous.as_deref(),
    };
    reconcile(
        layout,
        &inputs,
        &mirrored(&resolve(underlying, root)),
        false,
        false,
    )
}

fn write_if_changed(path: &Path, text: &str, mode: u32) -> Result<bool, String> {
    if fs::read_to_string(path).is_ok_and(|old| old == text)
        && fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 == mode & 0o111)
    {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    atomic::write(path, text.as_bytes(), Some(mode))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot set the mode of {}: {e}", path.display()))?;
    Ok(true)
}

fn git_env(env: &EnvMap, paths: &RepoPaths) -> EnvMap {
    git::probe_env(env, Some(paths))
}

/// Activates the clone: scripts, the included file, the state and the
/// include in `.git/config`. Idempotent; the scripts and files are written
/// before the include, so `core.hooksPath` never points at a missing
/// directory.
///
/// # Errors
///
/// A file or git failure, or `core.hooksPath` still not being Sluice's after
/// the include.
pub fn activate(paths: &RepoPaths, env: &EnvMap) -> Result<Activated, String> {
    let layout = Layout::new(&paths.common_dir);
    let probe = git_env(env, paths);
    let ours = layout.dir().to_string_lossy().into_owned();
    let entries = gitcfg::hooks_path_entries(&paths.toplevel, &probe).map_err(|e| e.to_string())?;
    let previous = gitcfg::underlying(&entries, &ours);
    let sluice = shim::current_binary()?;
    let state = State {
        schema: 1,
        dir: ours.clone(),
        previous,
        sluice: sluice.to_string_lossy().into_owned(),
    };
    let home = env.get("HOME").map(String::as_str);
    let under = super::delegate::underlying_dir(state.previous.as_deref(), &paths.common_dir, home);
    let written = reconcile(
        &layout,
        &Inputs {
            common: &paths.common_dir,
            sluice: &sluice,
            previous: state.previous.as_deref(),
        },
        &mirrored_in_clone(&under, &paths.toplevel, &probe),
        true,
        true,
    )?;
    let config = format!(
        "# Managed by sluice (`sluice hooks on`); `sluice hooks off` removes it.\n[core]\n\thooksPath = {}\n",
        gitcfg::config_quote(&ours)
    );
    write_if_changed(layout.config_file(), &config, 0o644)?;
    let json = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    write_if_changed(layout.state_file(), &format!("{json}\n"), 0o644)?;
    let file = layout.config_file().to_string_lossy().into_owned();
    gitcfg::ensure_last_include(&paths.toplevel, &probe, &file, &ours)?;
    Ok(Activated { state, written })
}

/// Removes the include and the managed files. Nothing else is touched: the
/// repository's files and every other config entry stay as they are.
///
/// # Errors
///
/// A git or file failure.
pub fn deactivate(paths: &RepoPaths, env: &EnvMap) -> Result<bool, String> {
    let layout = Layout::new(&paths.common_dir);
    let probe = git_env(env, paths);
    let file = layout.config_file().to_string_lossy().into_owned();
    let was_on = layout.state_file().exists()
        || layout.dir().exists()
        || gitcfg::local_includes(&paths.toplevel, &probe).is_ok_and(|i| i.contains(&file));
    gitcfg::remove_include(&paths.toplevel, &probe, &file)?;
    let dir = layout.dir();
    if fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        fs::remove_dir_all(dir).map_err(|e| format!("cannot remove {}: {e}", dir.display()))?;
    }
    for path in [layout.config_file(), layout.state_file()] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", path.display())),
        }
    }
    Ok(was_on)
}

/// What is true of a clone's entry point right now.
#[derive(Debug)]
pub struct Inspection {
    /// Sluice's hooks directory.
    pub dir: String,
    /// The recorded state, when the clone was activated.
    pub state: Option<State>,
    /// The included file is listed in the clone's config.
    pub include_present: bool,
    /// Hooks the repository has that Sluice's directory has no script for
    /// (git would not run them).
    pub unmirrored: Vec<&'static str>,
    /// `core.hooksPath` as git reads it, in order.
    pub entries: Vec<gitcfg::Entry>,
    /// Sluice's value is the last one.
    pub effective: bool,
    /// The repository's own value, without Sluice's.
    pub underlying: Option<String>,
}

impl Inspection {
    /// The clone was activated (managed files exist).
    pub fn managed(&self) -> bool {
        self.state.is_some()
    }

    /// Activated, effective and complete.
    pub fn healthy(&self) -> bool {
        self.managed() && self.effective && self.unmirrored.is_empty()
    }

    /// Why the entry point is not working, when it is activated.
    pub fn problem(&self) -> Option<String> {
        if !self.managed() || self.healthy() {
            return None;
        }
        if self.effective && !self.unmirrored.is_empty() {
            return Some(format!(
                "the repository has hook(s) {} that Sluice's directory {} has no script for, so git does not run them",
                self.unmirrored.join(", "),
                self.dir
            ));
        }
        Some(gitcfg::displaced_reason(
            &self.entries,
            &self.dir,
            self.include_present,
        ))
    }
}

/// Inspects the clone whose git common dir is `common` and worktree root is
/// `root` (where git config is read and relative hooks paths resolve).
///
/// # Errors
///
/// Git cannot be run.
pub fn inspect(common: &Path, root: &Path, env: &EnvMap) -> Result<Inspection, String> {
    let layout = Layout::new(common);
    let state = super::read_state(&layout);
    let dir = state.as_ref().map_or_else(
        || layout.dir().to_string_lossy().into_owned(),
        |s| s.dir.clone(),
    );
    let entries = gitcfg::hooks_path_entries(root, env).map_err(|e| e.to_string())?;
    let file = layout.config_file().to_string_lossy().into_owned();
    let include_present = gitcfg::local_includes(root, env)
        .map_err(|e| e.to_string())?
        .contains(&file);
    let underlying = gitcfg::underlying(&entries, &dir);
    let under = resolve(
        &super::delegate::underlying_dir(
            underlying.as_deref(),
            common,
            env.get("HOME").map(String::as_str),
        ),
        root,
    );
    let unmirrored = mirrored(&under)
        .into_iter()
        .filter(|h| !super::delegate::is_executable(&layout.dir().join(h)))
        .collect();
    Ok(Inspection {
        effective: entries.last().is_some_and(|l| l.value == dir),
        underlying,
        dir,
        state,
        include_present,
        unmirrored,
        entries,
    })
}
