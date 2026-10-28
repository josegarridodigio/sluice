//! `sluice hooks run <hook> -- <args>`: runs the hook the repository would
//! have run without Sluice.
//!
//! Wave H1 changes nothing about what runs, and wave H2a only replaces the
//! delegation when Sluice understands the whole hook ([`super::runner`]): the hooks directory git would
//! use without Sluice (`core.hooksPath` as the repository's own config sets
//! it, else `<git dir>/hooks`) is found at run time and its script for the
//! hook is executed in place with the same arguments, stdin, working
//! directory and environment. The process is replaced (`exec`), so signals
//! and exit codes are git's to see as they are. husky's `.husky/_/<hook>`
//! stub, a lefthook or pre-commit script and a plain hook all live there;
//! each one does its own binary discovery and argument handling. A hook with
//! no executable file there did not run before and does not run now (husky
//! exits 0 for a hook without a user file too).
//!
//! Nothing here may stop a hook from running: when something fails before
//! the delegation, the default hooks directory is used and a line goes to
//! stderr. Before delegating it also keeps Sluice's directory holding a
//! script for exactly the hooks the repository has ([`activate::sync`]).

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sluice_gate_core::config::EnvMap;
use sluice_gate_core::git;

use super::{Layout, activate, gitcfg, read_state};

/// Exit code when the hook file exists but cannot be executed.
const EXIT_CANNOT_EXEC: u8 = 126;

/// The hook manager that owns a clone's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Husky,
    Lefthook,
    PreCommit,
    /// Hook files that no manager generated.
    Plain,
    None,
}

impl Manager {
    pub fn label(self) -> &'static str {
        match self {
            Self::Husky => "husky",
            Self::Lefthook => "lefthook",
            Self::PreCommit => "pre-commit",
            Self::Plain => "plain hooks",
            Self::None => "none",
        }
    }
}

impl Manager {
    /// A word without spaces, for unit names (`husky:pre-commit#1`).
    pub fn slug(self) -> &'static str {
        match self {
            Self::Husky => "husky",
            Self::Lefthook => "lefthook",
            Self::PreCommit => "pre-commit",
            Self::Plain => "plain",
            Self::None => "none",
        }
    }
}

/// Where lefthook looks for its main configuration.
pub const LEFTHOOK_CONFIGS: [&str; 12] = [
    "lefthook.yml",
    "lefthook.yaml",
    "lefthook.json",
    "lefthook.toml",
    ".lefthook.yml",
    ".lefthook.yaml",
    ".lefthook.json",
    ".lefthook.toml",
    ".config/lefthook.yml",
    ".config/lefthook.yaml",
    ".config/lefthook.json",
    ".config/lefthook.toml",
];

/// Largest hook file read to recognise its generator.
const MAX_SCRIPT: u64 = 64 * 1024;

/// A file git would execute: a regular file (through symlinks) with an
/// execute bit.
pub fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `~/` expanded with `home`.
fn expand_home(value: &str, home: Option<&str>) -> String {
    match (value.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
        _ => value.to_owned(),
    }
}

/// The directory git would take hooks from without Sluice. A relative value
/// stays relative: git runs hooks from the worktree root, where this runs.
pub fn underlying_dir(value: Option<&str>, common: &Path, home: Option<&str>) -> PathBuf {
    match value {
        Some(v) if !v.is_empty() => PathBuf::from(expand_home(v, home)),
        _ => common.join("hooks"),
    }
}

fn read_head(path: &Path) -> String {
    use std::io::Read;
    let mut text = String::new();
    if let Ok(file) = fs::File::open(path) {
        let _ = file.take(MAX_SCRIPT).read_to_string(&mut text);
    }
    text
}

fn hook_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_none_or(|x| x != "sample") && is_executable(p))
        .collect();
    files.sort();
    files
}

/// The manager that owns the hooks in `dir`, for a clone whose worktree is
/// `root`. Generated scripts are recognised first, then the config files
/// the managers leave in the repository.
pub fn detect(root: &Path, dir: &Path) -> Manager {
    let full = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        root.join(dir)
    };
    if full.ends_with(".husky/_")
        || (full.file_name().is_some_and(|n| n == "_") && full.join("h").is_file())
    {
        return Manager::Husky;
    }
    let files = hook_files(&full);
    for file in &files {
        let text = read_head(file);
        if text.contains("LEFTHOOK") {
            return Manager::Lefthook;
        }
        if text.contains("File generated by pre-commit") || text.contains("hook-impl") {
            return Manager::PreCommit;
        }
    }
    if LEFTHOOK_CONFIGS.iter().any(|c| root.join(c).is_file()) {
        return Manager::Lefthook;
    }
    if root.join(".pre-commit-config.yaml").is_file() {
        return Manager::PreCommit;
    }
    if files.is_empty() {
        Manager::None
    } else {
        Manager::Plain
    }
}

/// What to run for a hook.
#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    /// The hook file to execute; `None`: nothing ran before, exit 0.
    pub target: Option<PathBuf>,
    /// The directory git would have taken hooks from without Sluice.
    pub dir: PathBuf,
    /// An internal problem to report on stderr (the delegation went on).
    pub warning: Option<String>,
}

/// Decides what to run for `hook` from the values git reads for
/// `core.hooksPath` (`None` when git could not be asked).
pub fn decide(
    hook: &str,
    entries: Option<&[gitcfg::Entry]>,
    layout: &Layout,
    env: &EnvMap,
) -> Decision {
    let ours = layout.dir().to_string_lossy().into_owned();
    let mut warning = None;
    let value = if let Some(entries) = entries {
        gitcfg::underlying(entries, &ours)
    } else {
        warning = Some("could not read core.hooksPath; using the recorded one".to_owned());
        read_state(layout).and_then(|s| s.previous)
    };
    let dir = underlying_dir(
        value.as_deref(),
        &layout.common,
        env.get("HOME").map(String::as_str),
    );
    let target = (dir != layout.dir()).then(|| dir.join(hook));
    Decision {
        target: target.filter(|t| is_executable(t)),
        dir,
        warning,
    }
}

/// `sluice hooks run`: never returns when a hook is executed.
pub fn run(hook: &str, args: &[OsString], common_dir: Option<&Path>) -> ExitCode {
    let env = sluice_gate_core::config::process_env();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let common = common_dir
        .map(Path::to_path_buf)
        .or_else(|| git::discover(&cwd, &env).ok().map(|p| p.common_dir));
    let Some(common) = common else {
        eprintln!("sluice: hooks run: cannot find the git directory; not running {hook}");
        return ExitCode::SUCCESS;
    };
    let layout = Layout::new(&common);
    let entries = gitcfg::hooks_path_entries(&cwd, &env).ok();
    let decision = decide(hook, entries.as_deref(), &layout, &env);
    if let Some(warning) = &decision.warning {
        eprintln!("sluice: hooks run: {warning}");
    }
    // Keep the mirror in line with the repository's hooks; this run goes on as it would have.
    if let Err(e) = activate::sync(&layout, &cwd, &decision.dir) {
        eprintln!("sluice: hooks run: cannot update the hook scripts: {e}");
    }
    let Some(target) = decision.target else {
        return ExitCode::SUCCESS;
    };
    if let Some(code) = super::runner::try_units(hook, &decision.dir, &cwd, &env) {
        return code;
    }
    tracing::debug!(%hook, target = %target.display(), "delegating");
    let error = Command::new(&target).args(args).exec();
    eprintln!(
        "sluice: hooks run: cannot execute {}: {error}",
        target.display()
    );
    ExitCode::from(EXIT_CANNOT_EXEC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_directory_is_the_common_dirs_hooks() {
        let common = Path::new("/r/.git");
        assert_eq!(
            underlying_dir(None, common, None),
            Path::new("/r/.git/hooks")
        );
        assert_eq!(
            underlying_dir(Some(""), common, None),
            Path::new("/r/.git/hooks")
        );
        assert_eq!(
            underlying_dir(Some(".husky/_"), common, None),
            Path::new(".husky/_")
        );
        assert_eq!(
            underlying_dir(Some("~/h"), common, Some("/users/u/")),
            Path::new("/users/u/h")
        );
    }

    #[test]
    fn managers_are_recognised_from_what_they_generate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert_eq!(detect(root, Path::new(".git/hooks")), Manager::None);

        fs::create_dir_all(root.join(".husky/_")).unwrap();
        fs::write(root.join(".husky/_/h"), "").unwrap();
        assert_eq!(detect(root, Path::new(".husky/_")), Manager::Husky);

        fs::create_dir_all(root.join("hooks")).unwrap();
        let script = root.join("hooks/pre-commit");
        fs::write(&script, "#!/bin/sh\n# LEFTHOOK\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(detect(root, Path::new("hooks")), Manager::Lefthook);
        fs::write(&script, "#!/bin/sh\n# File generated by pre-commit\n").unwrap();
        assert_eq!(detect(root, Path::new("hooks")), Manager::PreCommit);
        fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(detect(root, Path::new("hooks")), Manager::Plain);
    }
}
