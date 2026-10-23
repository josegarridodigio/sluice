//! Which segments of a command the guard rewrites, and how.
//!
//! A rewrite only ever inserts a fixed prefix in front of a segment's command
//! word (`sluice exec --label <L> -- `), or replaces the gate's own words,
//! typed exactly as the gate spells them, with `sluice run <gate> -v`
//! (followed by ` --` when arguments follow). Nothing of the original is
//! re-quoted, unescaped or moved. Design: `kb/sluice/design/command-control.md`
//! sections 4.3 and 4.7.

use std::path::{Component, Path, PathBuf};

use sluice_gate_core::heavy::{HeavyMatcher, canonical_words};

use super::lex::{self, Op, Segment};

/// First words whose segment is never rewritten: Sluice itself and wrappers
/// whose arguments the guard does not look into.
const SKIP_WORDS: &[&str] = &[
    "sluice", "sudo", "doas", "env", "nohup", "nice", "ionice", "timeout", "xargs", "exec", "eval",
    "sh", "bash", "zsh", "dash", "ksh", "fish", "command", "builtin", "source", ".", "watch",
    "parallel", "flock", "setsid", "stdbuf", "chrt", "taskset", "strace",
];

/// Commands that change the directory in a way the guard does not track.
const DIR_CHANGERS: &[&str] = &["pushd", "popd"];

/// Most characters a label keeps.
const LABEL_MAX: usize = 32;

/// The gates of the `.sluice.toml` that applies to a directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gates {
    /// Directory holding the `.sluice.toml` (canonical).
    pub dir: PathBuf,
    /// `(name, cmd)` of every gate.
    pub gates: Vec<(String, String)>,
}

/// What the matcher needs from the file system.
pub trait World {
    /// The canonical form of an existing directory, `None` otherwise.
    fn resolve_dir(&self, path: &Path) -> Option<PathBuf>;
    /// The config found walking up from `dir`; `Err` when one exists but
    /// cannot be read or parsed.
    fn gates_from(&self, dir: &Path) -> Result<Option<Gates>, ()>;
}

/// One change to the original string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Insert `sluice exec --label <label> -- ` at byte offset `at`.
    Heavy {
        /// Offset of the command word.
        at: usize,
        /// Label from `[a-z0-9._-]`.
        label: String,
    },
    /// Replace `start..end` (the gate's words as typed) with
    /// `sluice run <gate> -v`, plus ` --` when `rest` (arguments follow).
    Gate {
        /// Offset of the first gate word.
        start: usize,
        /// Offset just past the last gate word.
        end: usize,
        /// Gate name (validated by the config parser: `[a-z0-9_-]`).
        gate: String,
        /// Whether argument words follow the gate words.
        rest: bool,
    },
}

impl Edit {
    fn position(&self) -> usize {
        match self {
            Self::Heavy { at, .. } => *at,
            Self::Gate { start, .. } => *start,
        }
    }

    /// The text this edit puts into the command.
    pub fn text(&self) -> String {
        match self {
            Self::Heavy { label, .. } => format!("sluice exec --label {label} -- "),
            Self::Gate { gate, rest, .. } => {
                format!("sluice run {gate} -v{}", if *rest { " --" } else { "" })
            }
        }
    }
}

/// The rewrite of one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The edits, in increasing position, never overlapping.
    pub edits: Vec<Edit>,
}

impl Plan {
    /// The first gate this plan runs, if any.
    pub fn first_gate(&self) -> Option<&str> {
        self.edits.iter().find_map(|e| match e {
            Edit::Gate { gate, .. } => Some(gate.as_str()),
            Edit::Heavy { .. } => None,
        })
    }

    /// Applies the edits to `original`.
    pub fn apply(&self, original: &str) -> String {
        let mut out = String::with_capacity(original.len() + 48 * self.edits.len());
        let mut pos = 0;
        for edit in &self.edits {
            match edit {
                Edit::Heavy { at, .. } => {
                    out.push_str(&original[pos..*at]);
                    out.push_str(&edit.text());
                    pos = *at;
                }
                Edit::Gate { start, end, .. } => {
                    out.push_str(&original[pos..*start]);
                    out.push_str(&edit.text());
                    pos = *end;
                }
            }
        }
        out.push_str(&original[pos..]);
        out
    }
}

/// The `exec` label: up to two canonical words, stopping at the first
/// option, lowercased, filtered to `[a-z0-9._-]`, at most 32 characters.
pub fn label(canonical: &[String]) -> String {
    let words: Vec<String> = canonical
        .iter()
        .take_while(|w| !w.starts_with('-'))
        .take(2)
        .map(|w| {
            w.to_ascii_lowercase()
                .chars()
                .filter(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
                })
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect();
    let joined: String = words.join("-").chars().take(LABEL_MAX).collect();
    let trimmed = joined.trim_matches('-');
    if trimmed.is_empty() || trimmed.starts_with('.') {
        "exec".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// True for package-manager directory options (`-C d`, `--dir=d` ...).
fn has_dir_option(words: &[String]) -> bool {
    words.iter().any(|w| {
        w == "-C" || w.starts_with("--dir") || w.starts_with("--prefix") || w.starts_with("--cwd")
    })
}

/// A gate command as the matcher compares it.
struct GateWords {
    name: String,
    raw: Vec<String>,
    canonical: Vec<String>,
}

fn gate_words(name: &str, cmd: &str) -> Option<GateWords> {
    let segs = lex::split(cmd.trim())?;
    let [seg] = segs.as_slice() else { return None };
    if seg.assignments > 0 || seg.redirections > 0 || seg.op.is_some() || seg.words.is_empty() {
        return None;
    }
    if !seg.words.iter().all(literal_word) {
        return None;
    }
    let raw: Vec<String> = seg.words.iter().map(|w| w.value.clone()).collect();
    let values: Vec<String> = seg.words.iter().map(|w| w.value.clone()).collect();
    Some(GateWords {
        name: name.to_owned(),
        raw,
        canonical: canonical_words(&values),
    })
}

/// An unquoted word the shell does not expand.
fn literal_word(word: &lex::Word) -> bool {
    !word.quoted
        && !word
            .value
            .chars()
            .any(|c| matches!(c, '$' | '~' | '*' | '?' | '[' | ']'))
}

/// Words that are a plain directory for `cd` (no expansion of any kind).
fn plain_dir(word: &lex::Word) -> bool {
    !word.value.is_empty()
        && word.value != "-"
        && !word
            .value
            .chars()
            .any(|c| matches!(c, '$' | '~' | '*' | '?' | '[' | ']'))
}

/// Why no rewrite is planned at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoPlan {
    /// Nothing to rewrite (or the command is outside what the guard models).
    Nothing,
    /// A config exists but cannot be read: no output at all.
    Error,
}

/// The directory-changing command a segment runs (`cd`, `pushd`, `popd`).
fn dir_cmd(s: &Segment) -> Option<&str> {
    s.command_words()
        .first()
        .map(|w| w.value.as_str())
        .filter(|w| *w == "cd" || DIR_CHANGERS.contains(w))
}

/// `base` joined with `rel` the way a shell's logical `cd` does it: `.` is
/// dropped and `..` removes the previous component of the path as typed,
/// without resolving symlinks. `None` unless the result is absolute.
pub fn lexical_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = PathBuf::from("/");
    for comp in base.join(rel).components() {
        match comp {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(c) => out.push(c),
            Component::Prefix(_) => return None,
        }
    }
    base.is_absolute().then_some(out)
}

/// True when `word` has a `..` component.
fn has_parent_component(word: &str) -> bool {
    Path::new(word)
        .components()
        .any(|c| c == Component::ParentDir)
}

/// The shell context of a command besides its text.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShellContext {
    /// `CDPATH` is set (non-empty): a relative `cd` may go elsewhere.
    pub cdpath: bool,
}

impl ShellContext {
    /// The context the hook's environment implies (the agent's shell
    /// inherits it).
    pub fn from_env(env: &sluice_gate_core::config::EnvMap) -> Self {
        Self {
            cdpath: env.get("CDPATH").is_some_and(|v| !v.is_empty()),
        }
    }
}

/// Plans the rewrite of `command` run in `cwd` (the logical working
/// directory the hook reports, as the shell's `$PWD`).
///
/// A `cd` target is resolved lexically on the logical directory, as the
/// shell's `cd` does, and only then canonicalised to find the
/// `.sluice.toml`. No rewrite is planned when `CDPATH` is set and the
/// command changes directory, nor when a `cd` argument has a `..`
/// component while the logical and canonical current directories differ
/// (a symlink on the way: the guard does not guess which one the shell
/// follows).
///
/// # Errors
/// [`NoPlan::Nothing`] when no segment is rewritten, [`NoPlan::Error`] when a
/// `.sluice.toml` on the way cannot be read.
pub fn plan(
    command: &str,
    cwd: &Path,
    shell: ShellContext,
    heavy: &HeavyMatcher,
    world: &dyn World,
) -> Result<Plan, NoPlan> {
    let segs = lex::split(command).ok_or(NoPlan::Nothing)?;
    let has_cd = segs.iter().any(|s| dir_cmd(s).is_some());
    if has_cd && segs.iter().any(|s| matches!(s.op, Some(Op::Or | Op::Semi))) {
        // `a || cd x && b`, `cd x && a; b`: which directory b runs in depends
        // on exit codes; the guard does not guess.
        return Err(NoPlan::Nothing);
    }
    if has_cd && shell.cdpath {
        return Err(NoPlan::Nothing);
    }
    let mut logical = lexical_join(cwd, ".").ok_or(NoPlan::Nothing)?;
    let mut dir = world.resolve_dir(&logical).ok_or(NoPlan::Nothing)?;
    let mut edits = Vec::new();
    let mut prev_op: Option<Op> = None;
    for seg in &segs {
        let downstream = prev_op == Some(Op::Pipe);
        prev_op = seg.op;
        match dir_cmd(seg) {
            Some("cd") => {
                let args = &seg.command_words()[1..];
                let [arg] = args else {
                    return Err(NoPlan::Nothing);
                };
                if seg.assignments > 0
                    || seg.redirections > 0
                    || seg.op != Some(Op::And)
                    || downstream
                    || !plain_dir(arg)
                {
                    return Err(NoPlan::Nothing);
                }
                if has_parent_component(&arg.value) && logical != dir {
                    return Err(NoPlan::Nothing);
                }
                logical = lexical_join(&logical, &arg.value).ok_or(NoPlan::Nothing)?;
                dir = world.resolve_dir(&logical).ok_or(NoPlan::Nothing)?;
                continue;
            }
            Some(_) => return Err(NoPlan::Nothing),
            None => {}
        }
        if let Some(edit) = plan_segment(command, seg, downstream, &dir, heavy, world)? {
            edits.push(edit);
        }
    }
    if edits.is_empty() {
        return Err(NoPlan::Nothing);
    }
    edits.sort_by_key(Edit::position);
    Ok(Plan { edits })
}

fn plan_segment(
    command: &str,
    seg: &Segment,
    downstream: bool,
    dir: &Path,
    heavy: &HeavyMatcher,
    world: &dyn World,
) -> Result<Option<Edit>, NoPlan> {
    // `PATH=... cmd` would also apply to the inserted `sluice`.
    if seg.words[..seg.assignments]
        .iter()
        .any(|w| w.value.starts_with("PATH="))
    {
        return Ok(None);
    }
    let mut words = seg.command_words();
    // An optional leading `time` (the keyword, without options) is kept.
    if words
        .first()
        .is_some_and(|w| !w.quoted && w.value == "time")
    {
        words = &words[1..];
        if words.first().is_some_and(|w| w.value.starts_with('-')) {
            return Ok(None);
        }
    }
    let Some(first) = words.first() else {
        return Ok(None);
    };
    if first.value.is_empty() {
        return Ok(None);
    }
    if SKIP_WORDS.contains(&first.value.as_str()) || first.value.starts_with("sluice") {
        return Ok(None);
    }
    // A downstream pipeline segment reads stdin, so does an argument `-`.
    if downstream || words.iter().any(|w| w.value == "-") {
        return Ok(None);
    }
    let values: Vec<String> = words.iter().map(|w| w.value.clone()).collect();
    let canonical = canonical_words(&values);
    let in_pipeline = seg.op == Some(Op::Pipe);
    if seg.assignments == 0
        && seg.redirections == 0
        && !in_pipeline
        && let Some(edit) = gate_edit(command, words, &values, &canonical, dir, world)?
    {
        return Ok(Some(edit));
    }
    if heavy.matches(&canonical) {
        return Ok(Some(Edit::Heavy {
            at: first.start,
            label: label(&canonical),
        }));
    }
    Ok(None)
}

fn gate_edit(
    command: &str,
    words: &[lex::Word],
    values: &[String],
    canonical: &[String],
    dir: &Path,
    world: &dyn World,
) -> Result<Option<Edit>, NoPlan> {
    let Some(found) = world.gates_from(dir).map_err(|()| NoPlan::Error)? else {
        return Ok(None);
    };
    if found.dir != dir {
        return Ok(None);
    }
    let raw: Vec<&str> = words.iter().map(|w| &command[w.start..w.end]).collect();
    let gates: Vec<GateWords> = found
        .gates
        .iter()
        .filter_map(|(name, cmd)| gate_words(name, cmd))
        .collect();
    // Typed as the gate spells it: replace those words, keep the rest.
    let spelled = gates
        .iter()
        .filter(|g| g.raw.len() <= raw.len() && g.raw.iter().zip(&raw).all(|(a, b)| a == b))
        .max_by_key(|g| g.raw.len());
    if let Some(g) = spelled {
        return Ok(Some(Edit::Gate {
            start: words[0].start,
            end: words[g.raw.len() - 1].end,
            gate: g.name.clone(),
            rest: raw.len() > g.raw.len(),
        }));
    }
    // Another spelling of the same command, without arguments.
    if has_dir_option(values) || !words.iter().all(literal_word) {
        return Ok(None);
    }
    Ok(gates
        .iter()
        .find(|g| !canonical.is_empty() && g.canonical == canonical)
        .map(|g| Edit::Gate {
            start: words[0].start,
            end: words[words.len() - 1].end,
            gate: g.name.clone(),
            rest: false,
        }))
}

// ---------------------------------------------------------------------------
// The legacy deny checks (today's `guard.sh` logic, on a naive split).

/// Test runners denied when the repository has a gate starting with them.
const RUNNERS: &[&str] = &[
    "cargo test",
    "cargo clippy",
    "npm test",
    "npm run test",
    "pnpm test",
    "yarn test",
    "pytest",
    "go test",
];

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has_prefix(s: &str, prefix: &str) -> bool {
    s == prefix || s.strip_prefix(prefix).is_some_and(|r| r.starts_with(' '))
}

/// Pieces of a naive split on `&& || ; | &` and newlines, normalised, with
/// leading `(`, `{` and `VAR=value` words removed.
fn naive_pieces(command: &str) -> Vec<String> {
    command
        .split(['&', '|', ';', '\n'])
        .filter_map(|piece| {
            let mut s = normalize(piece);
            loop {
                if let Some(rest) = s.strip_prefix(['(', '{']) {
                    s = normalize(rest);
                    continue;
                }
                let first = s.split(' ').next().unwrap_or("");
                let assignment = first.split_once('=').is_some_and(|(name, _)| {
                    name.chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                });
                if assignment {
                    s = s
                        .split_once(' ')
                        .map_or_else(String::new, |(_, r)| r.to_owned());
                    continue;
                }
                break;
            }
            (!s.is_empty()).then_some(s)
        })
        .collect()
}

/// True when some piece of `command` runs `sluice queue approve`, or may
/// run it and the guard cannot tell (deny when unsure).
///
/// Each segment's words are looked at after quote removal, with the program
/// as its base name (`"sluice"`, `/usr/bin/sluice`), through the wrappers
/// that run their arguments as a command (`env` with its assignments,
/// `command`, `builtin`, `exec`, `nohup`, `time`, `setsid`, `sluice exec
/// --`). A wrapper whose arguments the guard does not parse (`sh -c`,
/// `eval`, `xargs`, `sudo`, `timeout`, ...) and a command the tokenizer
/// refuses count as approving when the text mentions both `queue` and
/// `approve`.
pub fn runs_approve(command: &str) -> bool {
    if naive_pieces(command)
        .iter()
        .any(|p| has_prefix(p, "sluice queue approve"))
    {
        return true;
    }
    match lex::split(command) {
        None => mentions_approve(command),
        Some(segs) => segs.iter().any(|seg| {
            let words: Vec<&str> = seg
                .command_words()
                .iter()
                .map(|w| w.value.as_str())
                .collect();
            words_approve(&words, &command[seg.start..seg.end])
        }),
    }
}

/// The text names both `queue` and `approve`.
fn mentions_approve(text: &str) -> bool {
    text.contains("approve") && text.contains("queue")
}

/// Wrappers that run the rest of their words as a command, after their own
/// options (none of which takes a separate argument).
const PLAIN_WRAPPERS: &[&str] = &["command", "builtin", "exec", "nohup", "time", "setsid"];

/// Wrappers whose arguments the guard does not parse.
const OPAQUE_WRAPPERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "fish", "eval", "xargs", "sudo", "doas", "su", "timeout",
    "nice", "ionice", "chrt", "taskset", "strace", "stdbuf", "flock", "watch", "parallel",
    "script", "unbuffer", "source", ".",
];

fn words_approve(words: &[&str], text: &str) -> bool {
    let mut i = 0;
    while let Some(&word) = words.get(i) {
        let program = word.rsplit('/').next().unwrap_or(word);
        match program {
            "sluice" => {
                let rest = &words[i + 1..];
                match rest.iter().position(|w| !w.starts_with('-')) {
                    Some(k) if rest[k] == "queue" => {
                        return rest[k + 1..].contains(&"approve");
                    }
                    Some(k) if rest[k] == "exec" => {
                        let Some(dd) = rest[k..].iter().position(|w| *w == "--") else {
                            return mentions_approve(text);
                        };
                        i += 1 + k + dd + 1;
                    }
                    _ => return false,
                }
            }
            "env" => {
                i += 1;
                while let Some(w) = words.get(i) {
                    if matches!(
                        *w,
                        "-S" | "--split-string" | "-C" | "--chdir" | "-u" | "--unset"
                    ) || w.starts_with("-S")
                        || w.starts_with("--split-string")
                    {
                        return mentions_approve(text);
                    }
                    if w.starts_with('-') || w.contains('=') {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            _ if PLAIN_WRAPPERS.contains(&program) => {
                i += 1;
                while words.get(i).is_some_and(|w| w.starts_with('-')) {
                    i += 1;
                }
            }
            _ if OPAQUE_WRAPPERS.contains(&program) => return mentions_approve(text),
            _ => return false,
        }
    }
    false
}

/// The gate a piece of `command` runs outside Sluice (verbatim gate command,
/// or a test runner the repository defines a gate for), as today's shell
/// guard decides. `gates` are `(name, cmd)`.
pub fn legacy_gate_hit(command: &str, gates: &[(String, String)]) -> Option<String> {
    let gates: Vec<(&str, String)> = gates
        .iter()
        .filter(|(_, cmd)| !cmd.contains('\n'))
        .map(|(name, cmd)| (name.as_str(), normalize(cmd)))
        .filter(|(_, cmd)| !cmd.is_empty())
        .collect();
    if gates.is_empty() {
        return None;
    }
    for piece in naive_pieces(command) {
        if let Some((name, _)) = gates.iter().find(|(_, cmd)| has_prefix(&piece, cmd)) {
            return Some((*name).to_owned());
        }
        for runner in RUNNERS {
            if !has_prefix(&piece, runner) {
                continue;
            }
            if let Some((name, _)) = gates.iter().find(|(_, cmd)| has_prefix(cmd, runner)) {
                return Some((*name).to_owned());
            }
        }
    }
    None
}
