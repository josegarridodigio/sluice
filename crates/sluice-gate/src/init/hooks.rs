//! Finding the commands that existing git hooks run, and editing them.
//!
//! Hook files are read and edited as text, line by line: husky and plain
//! hooks are shell scripts, lefthook and the pre-commit framework are YAML,
//! and no YAML is ever re-serialised. A line that cannot be matched safely
//! is reported as `manual` and the caller prints a snippet for it.

use std::fs;
use std::path::{Path, PathBuf};

use sluice_gate_core::pkgscripts::pieces;

use crate::snippets::HookManager;

/// The hooks Sluice wires.
pub const HOOKS: [&str; 2] = ["pre-commit", "pre-push"];

/// Largest hook file `init` reads.
const MAX_FILE: u64 = 1 << 20;

/// How a hook file is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A shell script (husky or plain): one command per line.
    Script,
    /// `lefthook.yml`: `run:` values.
    Lefthook,
    /// `.pre-commit-config.yaml`: `entry:` of local hooks.
    PreCommit,
}

/// A hook file read from the repository.
#[derive(Debug, Clone)]
pub struct HookFile {
    /// Path relative to the repository root, for messages.
    pub rel: String,
    /// The file to write: the symlink's target when the file is a link.
    pub target: PathBuf,
    /// False for a symlink that leaves the repository.
    pub editable: bool,
    pub format: Format,
    /// The hook a script file implements.
    pub hook: Option<&'static str>,
    pub manager: HookManager,
    /// The lines, each with its own terminator (`\n`, `\r\n` or none).
    pub lines: Vec<String>,
}

/// A command line split into environment, text and words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// `NAME=value` words that preceded the command.
    pub env: Vec<(String, String)>,
    /// The command as written, without the environment.
    pub text: String,
    /// The command's words, quotes removed.
    pub words: Vec<String>,
}

/// Extra YAML edit a rewired pre-commit hook needs: no file names appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aux {
    None,
    /// Set `pass_filenames: false`: replace the line that says otherwise or
    /// insert it after the `entry:` line.
    PassFilenames {
        replace: Option<usize>,
        indent: String,
    },
}

/// A command a hook runs.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub file: usize,
    pub line: usize,
    pub hook: String,
    pub cmd: Command,
    /// Text of the line before the command (indentation, `run: `, ...).
    pub prefix: String,
    /// Text after the command that stays (`|| exit $?`).
    pub suffix: String,
    pub aux: Aux,
    /// Why the line cannot be rewired, when it cannot.
    pub manual: Option<&'static str>,
}

/// What one hook file runs.
#[derive(Debug, Clone)]
pub enum Found {
    Command(Candidate),
    /// A line that already calls `sluice run`.
    Wired {
        gate: String,
    },
}

/// The hook files of a repository.
#[derive(Debug, Default)]
pub struct Layout {
    pub files: Vec<HookFile>,
    /// `.git/hooks` has hooks of its own and `core.hooksPath` is unset.
    pub custom_git_hooks: bool,
    /// `core.hooksPath` resolves to the `.githooks` directory already.
    pub githooks_active: bool,
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, value)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !value.contains(['"', '\''])
    })
}

/// Parses one command: a single simple command, optional leading
/// `NAME=value` words, nothing that needs a shell to understand (pipes,
/// redirections, substitutions, lists). `None` for anything else.
pub fn parse_command(text: &str) -> Option<Command> {
    let text = text.trim();
    if text.is_empty()
        || text.starts_with('#')
        || text.contains(" #")
        || text.contains([
            '&', ';', '|', '>', '<', '$', '`', '(', ')', '{', '}', '*', '!',
        ])
    {
        return None;
    }
    let mut rest = text;
    let mut env = Vec::new();
    while let Some(word) = rest.split_whitespace().next() {
        if !is_assignment(word) {
            break;
        }
        let (name, value) = word.split_once('=')?;
        env.push((name.to_owned(), value.to_owned()));
        rest = rest[word.len()..].trim_start();
    }
    let mut all = pieces(rest);
    if all.len() != 1 || rest.is_empty() {
        return None;
    }
    Some(Command {
        env,
        text: rest.to_owned(),
        words: all.remove(0),
    })
}

/// The gate a `sluice run ...` command runs, `None` when the words are not
/// `sluice run`.
pub fn wired_gate(words: &[String]) -> Option<String> {
    let first = words.first()?;
    if Path::new(first).file_name()?.to_str()? != "sluice" || words.get(1)? != "run" {
        return None;
    }
    let mut rest = words[2..].iter();
    while let Some(word) = rest.next() {
        if word == "--hook" || word == "--timeout" || word == "--label" {
            rest.next();
        } else if !word.starts_with('-') {
            return Some(word.clone());
        }
    }
    None
}

/// Splits the terminator off a line.
fn split_terminator(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else {
        (line, "")
    }
}

fn read_file(
    root: &Path,
    path: &Path,
    format: Format,
    hook: Option<&'static str>,
    manager: HookManager,
) -> Option<HookFile> {
    let meta = fs::symlink_metadata(path).ok()?;
    let (target, editable) = if meta.file_type().is_symlink() {
        let target = fs::canonicalize(path).ok()?;
        let inside = fs::canonicalize(root).is_ok_and(|r| target.starts_with(r));
        (target, inside)
    } else {
        (path.to_path_buf(), true)
    };
    let real = fs::metadata(&target).ok()?;
    if !real.is_file() || real.len() > MAX_FILE {
        return None;
    }
    let text = fs::read_to_string(&target).ok()?;
    Some(HookFile {
        rel: path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string(),
        target,
        editable,
        format,
        hook,
        manager,
        lines: text.split_inclusive('\n').map(str::to_owned).collect(),
    })
}

fn is_sample(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "sample")
}

fn has_hooks(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| !is_sample(&e.path()) && e.path().is_file())
    })
}

/// Reads every hook file of the repository at `root`.
///
/// `hooks_path` is the value of `core.hooksPath`, `common_dir` the git dir
/// that holds `hooks/`.
pub fn discover(root: &Path, common_dir: &Path, hooks_path: Option<&str>) -> Layout {
    let mut layout = Layout::default();
    let githooks = root.join(".githooks");
    let effective = hooks_path.map_or_else(|| common_dir.join("hooks"), |p| root.join(p));
    layout.custom_git_hooks = hooks_path.is_none() && has_hooks(&common_dir.join("hooks"));
    layout.githooks_active = hooks_path.is_some() && effective == githooks;
    for hook in HOOKS {
        let husky = root.join(".husky").join(hook);
        layout.files.extend(read_file(
            root,
            &husky,
            Format::Script,
            Some(hook),
            HookManager::Husky,
        ));
        let mut dirs = vec![effective.clone()];
        if githooks != effective {
            dirs.push(githooks.clone());
        }
        for dir in dirs {
            // Husky 9 points core.hooksPath at its own generated `.husky/_`.
            if dir.starts_with(root.join(".husky")) {
                continue;
            }
            let path = dir.join(hook);
            if !is_sample(&path) {
                layout.files.extend(read_file(
                    root,
                    &path,
                    Format::Script,
                    Some(hook),
                    HookManager::Plain,
                ));
            }
        }
    }
    for name in [
        "lefthook.yml",
        "lefthook.yaml",
        ".lefthook.yml",
        ".lefthook.yaml",
    ] {
        if let Some(file) = read_file(
            root,
            &root.join(name),
            Format::Lefthook,
            None,
            HookManager::Lefthook,
        ) {
            layout.files.push(file);
            break;
        }
    }
    layout.files.extend(read_file(
        root,
        &root.join(".pre-commit-config.yaml"),
        Format::PreCommit,
        None,
        HookManager::PreCommit,
    ));
    layout
}

/// The commands of every file, in file order.
pub fn scan(files: &[HookFile]) -> Vec<Found> {
    let mut found = Vec::new();
    for (index, file) in files.iter().enumerate() {
        match file.format {
            Format::Script => scan_script(index, file, &mut found),
            Format::Lefthook => scan_lefthook(index, file, &mut found),
            Format::PreCommit => scan_pre_commit(index, file, &mut found),
        }
    }
    found
}

fn found_for(mut candidate: Candidate, file: &HookFile, out: &mut Vec<Found>) {
    if let Some(gate) = wired_gate(&candidate.cmd.words) {
        out.push(Found::Wired { gate });
        return;
    }
    if !file.editable {
        candidate.manual = Some("the file is a symlink that leaves the repository");
    }
    out.push(Found::Command(candidate));
}

fn scan_script(index: usize, file: &HookFile, out: &mut Vec<Found>) {
    let Some(hook) = file.hook else { return };
    let mut continued = false;
    for (line_no, raw) in file.lines.iter().enumerate() {
        let (line, _) = split_terminator(raw);
        let was_continued = continued;
        continued = line.trim_end().ends_with('\\');
        if was_continued || continued {
            continue;
        }
        let trimmed = line.trim();
        // Husky 8 and earlier source their own bootstrap first.
        if trimmed.starts_with(". ") || trimmed.starts_with("source ") {
            continue;
        }
        // The `|| exit` tail keeps a script without `set -e` stopping.
        let (body, suffix) = match trimmed.rfind("||") {
            Some(i) if trimmed[i + 2..].trim_start().starts_with("exit") => {
                (trimmed[..i].trim_end(), format!(" {}", &trimmed[i..]))
            }
            _ => (trimmed, String::new()),
        };
        let Some(cmd) = parse_command(body) else {
            continue;
        };
        let indent = &line[..line.len() - line.trim_start().len()];
        // Environment words come back in the replacement through the gate.
        found_for(
            Candidate {
                file: index,
                line: line_no,
                hook: hook.to_owned(),
                cmd,
                prefix: indent.to_owned(),
                suffix,
                aux: Aux::None,
                manual: None,
            },
            file,
            out,
        );
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// A YAML plain or quoted one-line scalar and the comment that follows a
/// plain one (spaces included); `None` for block scalars, flow collections,
/// aliases and quoted values with escapes or a comment.
fn plain_scalar(value: &str) -> Option<(String, String)> {
    let value = value.trim_end();
    if value.is_empty() || value.starts_with(['|', '>', '[', '&', '*']) {
        return None;
    }
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return (!inner.contains(quote) && !inner.contains('\\'))
                .then(|| (inner.to_owned(), String::new()));
        }
    }
    if value.starts_with(['"', '\'']) {
        return None;
    }
    match value.find(" #") {
        Some(i) => {
            let command = value[..i].trim_end();
            Some((command.to_owned(), value[command.len()..].to_owned()))
        }
        None => Some((value.to_owned(), String::new())),
    }
}

/// `key: value` of a YAML line (a leading `- ` is skipped); returns the
/// key, the byte offset of the value and the raw value.
fn key_value(line: &str) -> Option<(&str, usize, &str)> {
    let start = indent_of(line);
    let mut body = &line[start..];
    let mut offset = start;
    if let Some(after) = body.strip_prefix("- ") {
        let skipped = body.len() - after.len();
        offset += skipped;
        let trimmed = after.trim_start();
        offset += after.len() - trimmed.len();
        body = trimmed;
    }
    let (key, value) = body.split_once(':')?;
    if key.contains(' ') || key.is_empty() {
        return None;
    }
    let after_colon = value.len() - value.trim_start().len();
    Some((
        key,
        offset + key.len() + 1 + after_colon,
        value.trim_start(),
    ))
}

fn scan_lefthook(index: usize, file: &HookFile, out: &mut Vec<Found>) {
    let mut hook: Option<&'static str> = None;
    for (line_no, raw) in file.lines.iter().enumerate() {
        let (line, _) = split_terminator(raw);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if indent_of(line) == 0 {
            let key = line.split(':').next().unwrap_or("").trim();
            hook = HOOKS.iter().copied().find(|h| *h == key);
            continue;
        }
        let Some(hook) = hook else { continue };
        let Some((key, value_at, value)) = key_value(line) else {
            continue;
        };
        if key != "run" {
            continue;
        }
        let Some((command, comment)) = plain_scalar(value) else {
            continue;
        };
        // Lefthook templates ({staged_files}) are not whole-tree commands.
        let Some(cmd) = parse_command(&command) else {
            continue;
        };
        found_for(
            Candidate {
                file: index,
                line: line_no,
                hook: hook.to_owned(),
                cmd,
                prefix: line[..value_at].to_owned(),
                suffix: comment,
                aux: Aux::None,
                manual: None,
            },
            file,
            out,
        );
    }
}

/// One hook item of a local repo in `.pre-commit-config.yaml`.
struct FrameworkHook {
    key_indent: usize,
    entry: Option<(usize, usize, String, String)>,
    stages: Option<String>,
    language: Option<String>,
    pass_filenames: Option<(usize, String)>,
    has_args: bool,
}

/// The hook items of the local repos of a pre-commit config.
fn framework_hooks(file: &HookFile) -> Vec<FrameworkHook> {
    let mut local = false;
    let mut repo_indent = 0;
    let mut current: Option<FrameworkHook> = None;
    let mut hooks: Vec<FrameworkHook> = Vec::new();
    for (line_no, raw) in file.lines.iter().enumerate() {
        let (line, _) = split_terminator(raw);
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = indent_of(line);
        let Some((key, value_at, value)) = key_value(line) else {
            continue;
        };
        if trimmed.starts_with("- repo:") {
            hooks.extend(current.take());
            local = value.trim() == "local";
            repo_indent = indent;
            continue;
        }
        if !local || indent <= repo_indent && !trimmed.starts_with("- ") {
            hooks.extend(current.take());
            continue;
        }
        if trimmed.starts_with("- id:") {
            hooks.extend(current.take());
            current = Some(FrameworkHook {
                key_indent: indent + 2,
                entry: None,
                stages: None,
                language: None,
                pass_filenames: None,
                has_args: false,
            });
            continue;
        }
        let Some(hook) = current.as_mut() else {
            continue;
        };
        if indent != hook.key_indent {
            continue;
        }
        let scalar = plain_scalar(value);
        match key {
            "entry" => hook.entry = scalar.map(|(s, c)| (line_no, value_at, s, c)),
            "stages" => hook.stages = Some(value.trim().to_owned()),
            "language" => hook.language = scalar.map(|(s, _)| s),
            "pass_filenames" => hook.pass_filenames = scalar.map(|(s, _)| (line_no, s)),
            "args" => hook.has_args = true,
            _ => {}
        }
    }
    hooks.extend(current.take());
    hooks
}

fn scan_pre_commit(index: usize, file: &HookFile, out: &mut Vec<Found>) {
    for hook in framework_hooks(file) {
        let Some((line_no, value_at, entry, comment)) = hook.entry else {
            continue;
        };
        let Some(stage) = framework_stage(hook.stages.as_deref()) else {
            continue;
        };
        let Some(cmd) = parse_command(&entry) else {
            continue;
        };
        let raw = &file.lines[line_no];
        let (line, terminator) = split_terminator(raw);
        let manual = if hook.has_args {
            Some("the hook has args")
        } else if !matches!(hook.language.as_deref(), Some("system" | "script")) {
            Some("the hook's language is not system")
        } else if terminator.is_empty() {
            Some("the entry is the last line of the file")
        } else {
            None
        };
        let replace = match &hook.pass_filenames {
            Some((_, value)) if value == "false" => None,
            Some((n, _)) => Some(*n),
            None => None,
        };
        let aux = if hook
            .pass_filenames
            .as_ref()
            .is_some_and(|(_, v)| v == "false")
        {
            Aux::None
        } else {
            Aux::PassFilenames {
                replace,
                indent: " ".repeat(hook.key_indent),
            }
        };
        found_for(
            Candidate {
                file: index,
                line: line_no,
                hook: stage.to_owned(),
                cmd,
                prefix: line[..value_at].to_owned(),
                suffix: comment,
                aux,
                manual,
            },
            file,
            out,
        );
    }
}

/// The one hook a framework hook's `stages` names (`pre-commit` when absent).
fn framework_stage(stages: Option<&str>) -> Option<&'static str> {
    let Some(stages) = stages else {
        return Some("pre-commit");
    };
    let inner = stages.trim().trim_start_matches('[').trim_end_matches(']');
    let items: Vec<&str> = inner
        .split(',')
        .map(|s| s.trim().trim_matches(['"', '\'']))
        .filter(|s| !s.is_empty())
        .collect();
    match items.as_slice() {
        ["pre-commit" | "commit"] => Some("pre-commit"),
        ["pre-push" | "push"] => Some("pre-push"),
        _ => None,
    }
}

/// The new text of `file` once each `(candidate, command)` replaces the
/// command of its line. Everything else, terminators included, is kept.
pub fn rewire(file: &HookFile, rewires: &[(&Candidate, String)]) -> String {
    let mut lines = file.lines.clone();
    let mut insert_after: Vec<(usize, String)> = Vec::new();
    for (candidate, command) in rewires {
        let (_, terminator) = split_terminator(&file.lines[candidate.line]);
        lines[candidate.line] = format!(
            "{}{command}{}{terminator}",
            candidate.prefix, candidate.suffix
        );
        if let Aux::PassFilenames { replace, indent } = &candidate.aux {
            let text = format!("{indent}pass_filenames: false{terminator}");
            match replace {
                Some(n) => lines[*n] = text,
                None => insert_after.push((candidate.line, text)),
            }
        }
    }
    let mut out = String::new();
    for (n, line) in lines.iter().enumerate() {
        out.push_str(line);
        for (_, text) in insert_after.iter().filter(|(at, _)| *at == n) {
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(format: Format, hook: Option<&'static str>, text: &str) -> HookFile {
        HookFile {
            rel: "f".to_owned(),
            target: PathBuf::from("f"),
            editable: true,
            format,
            hook,
            manager: HookManager::Plain,
            lines: text.split_inclusive('\n').map(str::to_owned).collect(),
        }
    }

    fn commands(found: &[Found]) -> Vec<String> {
        found
            .iter()
            .filter_map(|f| match f {
                Found::Command(c) => Some(c.cmd.text.clone()),
                Found::Wired { .. } => None,
            })
            .collect()
    }

    #[test]
    fn simple_commands_parse_and_shell_constructs_do_not() {
        let cmd = parse_command("CI=1 npx ng test --watch=false").unwrap();
        assert_eq!(cmd.env, [("CI".to_owned(), "1".to_owned())]);
        assert_eq!(cmd.text, "npx ng test --watch=false");
        assert!(parse_command("npm run a && npm run b").is_none());
        assert!(parse_command("npm run a | tee log").is_none());
        assert!(parse_command("npm test > out.txt").is_none());
        assert!(parse_command("echo $(date)").is_none());
        assert!(parse_command("# comment").is_none());
    }

    #[test]
    fn wired_lines_are_recognised() {
        let words = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            wired_gate(&words("sluice run --hook pre-commit lint")),
            Some("lint".to_owned())
        );
        assert_eq!(
            wired_gate(&words("sluice run test -v")),
            Some("test".to_owned())
        );
        assert_eq!(wired_gate(&words("npm run lint")), None);
    }

    #[test]
    fn script_rewire_keeps_crlf_and_missing_final_newline() {
        let f = file(
            Format::Script,
            Some("pre-commit"),
            "#!/bin/sh\r\nnpx ng lint || exit 1\r\necho done\r\nnpx ng test",
        );
        let found = scan(std::slice::from_ref(&f));
        let pick = |text: &str| {
            found
                .iter()
                .find_map(|f| match f {
                    Found::Command(c) if c.cmd.text == text => Some(c),
                    _ => None,
                })
                .expect("command")
        };
        let first = pick("npx ng lint");
        let second = pick("npx ng test");
        let text = rewire(
            &f,
            &[
                (first, "sluice run --hook pre-commit lint".to_owned()),
                (second, "sluice run --hook pre-commit test".to_owned()),
            ],
        );
        assert_eq!(
            text,
            "#!/bin/sh\r\nsluice run --hook pre-commit lint || exit 1\r\necho done\r\n\
             sluice run --hook pre-commit test"
        );
    }

    #[test]
    fn lefthook_run_values_are_found_and_comments_survive() {
        let f = file(
            Format::Lefthook,
            None,
            "# top\npre-commit:\n  parallel: true\n  commands:\n    lint:\n      run: npx ng lint # slow\n    fmt:\n      run: npx prettier --write {staged_files}\n    test:\n      run: \"yarn test --ci\"\npost-merge:\n  commands:\n    x:\n      run: npx ng build\n",
        );
        let found = scan(std::slice::from_ref(&f));
        assert_eq!(commands(&found), ["npx ng lint", "yarn test --ci"]);
        let Found::Command(c) = &found[0] else {
            panic!("no command")
        };
        let text = rewire(&f, &[(c, "sluice run --hook pre-commit lint".to_owned())]);
        assert!(text.contains("      run: sluice run --hook pre-commit lint # slow\n"));
        assert!(text.contains("run: npx prettier --write {staged_files}\n"));
    }

    #[test]
    fn pre_commit_local_hooks_get_pass_filenames_false() {
        let text = "repos:\n  - repo: https://example.com/x\n    rev: v1\n    hooks:\n      - id: other\n        entry: npx ng lint\n  - repo: local\n    hooks:\n      - id: lint\n        name: lint\n        entry: npx ng lint\n        language: system\n        stages: [pre-push]\n";
        let f = file(Format::PreCommit, None, text);
        let found = scan(std::slice::from_ref(&f));
        assert_eq!(commands(&found), ["npx ng lint"]);
        let Found::Command(c) = &found[0] else {
            panic!("no command")
        };
        assert_eq!(c.hook, "pre-push");
        assert!(c.manual.is_none());
        let out = rewire(&f, &[(c, "sluice run --hook pre-push lint".to_owned())]);
        assert!(out.contains(
            "        entry: sluice run --hook pre-push lint\n        pass_filenames: false\n        language: system\n"
        ));
        assert!(out.contains("      - id: other\n        entry: npx ng lint\n"));
    }
}
