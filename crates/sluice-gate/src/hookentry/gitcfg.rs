//! Reading and editing the clone's git config for the hooks entry point.
//!
//! Everything goes through `git config`; the config file is never edited by
//! hand. Sluice's value comes from an included file, and git applies the last
//! value of `core.hooksPath` it reads, so the include must come after every
//! other source of that key.

use std::path::Path;

use sluice_gate_core::error::Error;
use sluice_gate_core::git::{self, EnvMap};

/// One value of `core.hooksPath` and the file it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `file:<path>` (or `command line:`) as `--show-origin` prints it.
    pub origin: String,
    pub value: String,
}

/// Parses the output of `git config --show-origin --get-all <key>`.
pub fn parse_entries(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let (origin, value) = line.split_once('\t')?;
            Some(Entry {
                origin: origin.to_owned(),
                value: value.to_owned(),
            })
        })
        .collect()
}

/// Every value of `core.hooksPath` git would read, in the order it reads
/// them (the last one wins). A missing key is an empty list.
///
/// # Errors
///
/// A git failure other than "key not set".
pub fn hooks_path_entries(cwd: &Path, env: &EnvMap) -> Result<Vec<Entry>, Error> {
    let args = ["config", "--show-origin", "--get-all", "core.hooksPath"];
    match git::run(&args, cwd, env, true) {
        Ok(out) => Ok(parse_entries(&out)),
        Err(Error::GitCommand { stderr, .. }) if stderr.is_empty() => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The value the repository's own configuration sets for `core.hooksPath`,
/// ignoring Sluice's directory: what git would use without Sluice.
pub fn underlying(entries: &[Entry], ours: &str) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|e| e.value != ours && !e.value.is_empty())
        .map(|e| e.value.clone())
}

/// The local `include.path` values, in file order.
pub fn local_includes(cwd: &Path, env: &EnvMap) -> Result<Vec<String>, Error> {
    let args = ["config", "--local", "--get-all", "include.path"];
    match git::run(&args, cwd, env, true) {
        Ok(out) => Ok(out.lines().map(str::to_owned).collect()),
        Err(Error::GitCommand { stderr, .. }) if stderr.is_empty() => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn config_write(cwd: &Path, env: &EnvMap, args: &[&str]) -> Result<(), String> {
    git::run(args, cwd, env, false)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Removes `file` from the local includes (no error when it is not there).
pub fn remove_include(cwd: &Path, env: &EnvMap, file: &str) -> Result<(), String> {
    let args = [
        "config",
        "--local",
        "--fixed-value",
        "--unset-all",
        "include.path",
        file,
    ];
    match git::run(&args, cwd, env, false) {
        Ok(_) => Ok(()),
        // Exit 5: nothing matched.
        Err(Error::GitCommand { status, .. }) if status.ends_with(": 5") => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Why Sluice's value is not the effective one, for messages.
pub fn displaced_reason(entries: &[Entry], ours: &str, include_present: bool) -> String {
    if !include_present {
        return "the include of Sluice's hooks file is not in the clone's .git/config".to_owned();
    }
    match entries.last() {
        Some(last) if last.value != ours => format!(
            "core.hooksPath is `{}` ({}), which wins over Sluice's include",
            last.value, last.origin
        ),
        Some(_) => "the include is present and effective".to_owned(),
        None => "git reads no core.hooksPath (is the included file missing?)".to_owned(),
    }
}

/// Makes `file` the last local include and checks that its `core.hooksPath`
/// (`ours`) is the effective value.
///
/// It is appended with `git config --add include.path`; when that does not
/// make it win (an earlier `[include]` section sits before a `[core]` entry),
/// the local includes are removed and added again with `file` last, keeping
/// their relative order.
///
/// # Errors
///
/// The git failure, or why the value still does not win.
pub fn ensure_last_include(cwd: &Path, env: &EnvMap, file: &str, ours: &str) -> Result<(), String> {
    let mut includes = local_includes(cwd, env).map_err(|e| e.to_string())?;
    if !includes.iter().any(|i| i == file) {
        config_write(
            cwd,
            env,
            &["config", "--local", "--add", "include.path", file],
        )?;
        includes.push(file.to_owned());
    }
    if wins(cwd, env, ours) {
        return Ok(());
    }
    let others: Vec<String> = includes.into_iter().filter(|i| i != file).collect();
    config_write(
        cwd,
        env,
        &["config", "--local", "--unset-all", "include.path"],
    )?;
    for other in others.iter().map(String::as_str).chain([file]) {
        config_write(
            cwd,
            env,
            &["config", "--local", "--add", "include.path", other],
        )?;
    }
    if wins(cwd, env, ours) {
        return Ok(());
    }
    let entries = hooks_path_entries(cwd, env).map_err(|e| e.to_string())?;
    Err(displaced_reason(&entries, ours, true))
}

fn wins(cwd: &Path, env: &EnvMap, ours: &str) -> bool {
    hooks_path_entries(cwd, env).is_ok_and(|e| e.last().is_some_and(|l| l.value == ours))
}

/// `text` as a git config value in double quotes.
pub fn config_quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_parsed_and_the_underlying_value_skips_ours() {
        let text = "file:.git/config\t.husky/_\nfile:/c/sluice/hooks.gitconfig\t/c/sluice/hooks";
        let entries = parse_entries(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            underlying(&entries, "/c/sluice/hooks").as_deref(),
            Some(".husky/_")
        );
        assert_eq!(underlying(&entries[1..], "/c/sluice/hooks"), None);
    }

    #[test]
    fn values_are_quoted_for_a_git_config_file() {
        assert_eq!(config_quote("/a b/c"), "\"/a b/c\"");
        assert_eq!(config_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
