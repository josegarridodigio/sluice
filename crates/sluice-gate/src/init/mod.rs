//! `sluice init`: write a `.sluice.toml` that fits the repository and wire
//! the existing git hooks to it (ADR-0016).
//!
//! `init` is the one user-invoked command that writes the working tree: the
//! config at the repository root, the hook files it rewires and, when there
//! is no hook manager, `.githooks/` plus a local `core.hooksPath`. It never
//! stages or commits anything. Gates come first from what the hooks already
//! run, then from the stack at the root.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sluice_gate_core::config::{
    self, Gate, REPO_CONFIG_FILE, RepoConfig, Weight, starter_repo_config,
};
use sluice_gate_core::git::{self, EnvMap, RepoPaths};
use sluice_gate_core::heavy::{HeavyMatcher, canonical_words};
use sluice_gate_core::pkgscripts::{FsScripts, Scripts, pieces};

use crate::run::EXIT_ERROR;
use crate::snippets::{HookManager, snippet};

mod detect;
mod hooks;
mod render;

use detect::{Kind, Pm};
use hooks::{Candidate, Found, HOOKS, HookFile};

/// The weight of a generated gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeightSpec {
    Fixed(u32),
    Auto,
}

/// Where a gate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A command in this hook file.
    Hook(String),
    /// A project file at the root.
    Detected(&'static str),
    /// The previous `.sluice.toml` (a hook already calls it).
    Carried,
}

/// A gate `init` is going to write.
#[derive(Debug, Clone)]
pub struct GatePlan {
    pub name: String,
    pub cmd: String,
    pub weight: WeightSpec,
    pub timeout: String,
    pub hooks: Vec<String>,
    pub key_inputs: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cache: bool,
    pub source: Source,
    /// The gate's table as the previous config had it; written verbatim.
    pub carried: Option<String>,
}

/// Executables that live in `node_modules/.bin`: from a hook they work
/// (the manager puts the directory in `PATH`), from a shell they need a
/// launcher.
const LOCAL_BINS: [&str; 14] = [
    "ng",
    "nx",
    "jest",
    "vitest",
    "tsc",
    "playwright",
    "cypress",
    "webpack",
    "vite",
    "turbo",
    "next",
    "astro",
    "nuxt",
    "react-scripts",
];

/// One file `init` writes.
struct FileChange {
    /// Path for messages, relative to the root.
    rel: String,
    target: PathBuf,
    old: Option<String>,
    new: String,
    /// Mode of a new file.
    mode: Option<u32>,
}

/// A gate/hook pair `init` could not wire by itself.
struct Manual {
    file: String,
    reason: &'static str,
    manager: HookManager,
    hook: String,
    gate: String,
}

/// `sluice init [--force] [--dry-run]`.
pub fn run(force: bool, dry_run: bool) -> ExitCode {
    match execute(force, dry_run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sluice: error: {message}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn is_mutating(words: &[String]) -> bool {
    let formatter = words.iter().any(|w| w == "prettier");
    words.iter().any(|w| {
        let base = Path::new(w)
            .file_name()
            .and_then(|b| b.to_str())
            .unwrap_or(w);
        matches!(
            w.as_str(),
            "--fix" | "--write" | "--fix-type" | "--fix-dry-run"
        ) || w.starts_with("--fix=")
            || w.starts_with("--write=")
            || (w == "-w" && formatter)
            || matches!(base, "pretty-quick" | "lint-staged")
    })
}

/// The `package.json` script `name` mutates files (somewhere in its pieces).
fn script_mutates(root: &Path, name: &str) -> bool {
    Scripts::find(&FsScripts, root)
        .and_then(|s| s.body(name).map(str::to_owned))
        .is_some_and(|body| pieces(&body).iter().any(|p| is_mutating(p)))
}

fn key_inputs_for(words: &[String], root_pm: Option<Pm>) -> Vec<String> {
    let first = words.first().map_or("", String::as_str);
    let pm = Pm::from_word(first);
    if pm.is_some() || LOCAL_BINS.contains(&first) || first == "node" {
        let mut inputs = vec!["node --version".to_owned()];
        if let Some(pm) = pm.or(root_pm) {
            inputs.push(pm.version_cmd());
        }
        inputs
    } else if first == "cargo" {
        vec!["rustc --version".to_owned()]
    } else if first == "go" {
        vec!["go version".to_owned()]
    } else if matches!(first, "pytest" | "python" | "python3") {
        vec!["python3 --version".to_owned()]
    } else {
        Vec::new()
    }
}

/// The text of a gate table in `config_text`, up to the next table.
fn gate_table(config_text: &str, name: &str) -> Option<String> {
    let header = format!("[gates.{name}]");
    let mut lines = config_text.lines();
    lines.find(|l| l.trim() == header)?;
    let mut out = vec![header];
    for line in lines {
        if line.trim_start().starts_with('[') {
            break;
        }
        out.push(line.to_owned());
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    Some(out.join("\n") + "\n")
}

fn carried(name: &str, gate: &Gate, old_text: &str) -> GatePlan {
    GatePlan {
        name: name.to_owned(),
        cmd: gate.cmd.clone(),
        weight: match gate.weight {
            Weight::Fixed(n) => WeightSpec::Fixed(n),
            Weight::Auto => WeightSpec::Auto,
        },
        timeout: humantime::format_duration(gate.timeout).to_string(),
        hooks: gate.hooks.clone(),
        key_inputs: gate.key_inputs.clone(),
        env: gate
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        cache: gate.cache,
        source: Source::Carried,
        carried: gate_table(old_text, name),
    }
}

/// The gates the existing hooks already run, and which lines became them.
struct FromHooks {
    gates: Vec<GatePlan>,
    /// `(index into found, gate name)` of every line to rewrite.
    rewires: Vec<(usize, String)>,
    notes: Vec<String>,
}

fn gates_from_hooks(
    root: &Path,
    files: &[HookFile],
    found: &[Found],
    old: Option<&(RepoConfig, String)>,
) -> FromHooks {
    let matcher = HeavyMatcher::default();
    let pm = detect::package_manager(root);
    let mut out = FromHooks {
        gates: Vec::new(),
        rewires: Vec::new(),
        notes: Vec::new(),
    };
    for (index, item) in found.iter().enumerate() {
        match item {
            Found::Wired { gate, .. } => {
                if out.gates.iter().any(|g| &g.name == gate) {
                    continue;
                }
                match old.and_then(|(cfg, text)| cfg.gates.get(gate).map(|g| carried(gate, g, text)))
                {
                    Some(plan) => out.gates.push(plan),
                    None => out.notes.push(format!(
                        "a hook runs gate \"{gate}\" but there is no such gate in {REPO_CONFIG_FILE}; \
                         define it or remove the line"
                    )),
                }
            }
            Found::Command(candidate) => {
                if let Some(plan) = gate_from_command(root, &matcher, pm, files, candidate) {
                    let name = merge_gate(&mut out.gates, plan, &candidate.hook);
                    out.rewires.push((index, name));
                }
            }
        }
    }
    out
}

/// A gate for a heavy, non-mutating command of a hook.
fn gate_from_command(
    root: &Path,
    matcher: &HeavyMatcher,
    pm: Option<Pm>,
    files: &[HookFile],
    candidate: &Candidate,
) -> Option<GatePlan> {
    let words = &candidate.cmd.words;
    if is_mutating(words) {
        return None;
    }
    let canonical = canonical_words(words);
    let found = matcher.classify(&canonical, root, &FsScripts)?;
    if found
        .script
        .as_deref()
        .is_some_and(|name| script_mutates(root, name))
    {
        return None;
    }
    let name = detect::gate_name(&found.entry, found.script.as_deref());
    let mut kind_words = found.entry.clone();
    kind_words.extend(found.script.clone());
    kind_words.push(name.clone());
    let kind = Kind::of(&kind_words);
    let first = words.first().map_or("", String::as_str);
    let cmd = match pm {
        Some(pm) if LOCAL_BINS.contains(&first) => format!("{} {}", pm.exec(), candidate.cmd.text),
        _ => candidate.cmd.text.clone(),
    };
    let mut env = candidate.cmd.env.clone();
    let node_tool = Pm::from_word(first).is_some() || LOCAL_BINS.contains(&first);
    if kind == Kind::Test && node_tool && first != "ng" && !env.iter().any(|(k, _)| k == "CI") {
        env.push(("CI".to_owned(), "1".to_owned()));
    }
    Some(GatePlan {
        name,
        cmd,
        weight: kind.weight(),
        timeout: kind.timeout().to_owned(),
        hooks: vec![candidate.hook.clone()],
        key_inputs: key_inputs_for(words, pm),
        env,
        cache: kind != Kind::Build,
        source: Source::Hook(files[candidate.file].rel.clone()),
        carried: None,
    })
}

/// Adds `plan` to `gates`: the same command under the same name joins that
/// gate's hooks, a different command gets a free name. Returns the name.
fn merge_gate(gates: &mut Vec<GatePlan>, mut plan: GatePlan, hook: &str) -> String {
    if let Some(existing) = gates
        .iter_mut()
        .find(|g| g.name == plan.name && g.cmd == plan.cmd && g.env == plan.env)
    {
        if !existing.hooks.iter().any(|h| h == hook) {
            existing.hooks.push(hook.to_owned());
        }
        return existing.name.clone();
    }
    let base = plan.name.clone();
    let mut n = 2;
    while gates.iter().any(|g| g.name == plan.name) {
        plan.name = format!("{base}-{n}");
        n += 1;
    }
    let name = plan.name.clone();
    gates.push(plan);
    name
}

/// How `init` leaves the hooks.
enum HookPlan {
    /// Hook files exist: lines that became gates are rewritten in place.
    Existing,
    /// No hooks at all: `.githooks/` is created.
    Create { set_hooks_path: bool },
    /// Hooks that `init` must not replace; the snippet is printed.
    Snippet(String),
}

fn hook_plan(root: &Path, layout: &hooks::Layout, hooks_path: Option<&str>) -> HookPlan {
    if !layout.files.is_empty() || root.join(".husky").is_dir() {
        return HookPlan::Existing;
    }
    if layout.custom_git_hooks {
        return HookPlan::Snippet(
            "`.git/hooks` already has hooks of its own, and setting core.hooksPath would stop \
             them from running"
                .to_owned(),
        );
    }
    if let Some(path) = hooks_path
        && !layout.githooks_active
    {
        return HookPlan::Snippet(format!(
            "core.hooksPath is already set to `{path}`, which `init` does not own"
        ));
    }
    HookPlan::Create {
        set_hooks_path: !layout.githooks_active,
    }
}

fn script_for(hook: &str, gates: &[&GatePlan]) -> String {
    let mut text = String::from("#!/bin/sh\nset -e\n");
    for gate in gates {
        let _ = writeln!(text, "sluice run --hook {hook} {}", gate.name);
    }
    text
}

fn discover_repo(env: &EnvMap) -> Result<RepoPaths, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot read the current directory: {e}"))?;
    git::discover(&cwd, env).map_err(|e| match e {
        sluice_gate_core::error::Error::NotARepository(_) => {
            "not inside a git repository (run 'sluice init' from one)".to_owned()
        }
        other => other.to_string(),
    })
}

/// Everything `init` decided, before anything is written.
struct Plan {
    stacks: Vec<String>,
    gates: Vec<GatePlan>,
    changes: Vec<FileChange>,
    set_hooks_path: bool,
    manual: Vec<Manual>,
    notes: Vec<String>,
    /// Snippet to print for the starter gates or a refused hooks directory.
    snippet: Option<String>,
}

/// The names of the gates that belong to `hook`.
fn gates_of(gates: &[GatePlan], hook: &str) -> Vec<String> {
    gates
        .iter()
        .filter(|g| g.hooks.iter().any(|h| h == hook))
        .map(|g| g.name.clone())
        .collect()
}

/// The edits of the hook files that hold lines that became gates, and the
/// lines that cannot be edited.
fn hook_edits(
    layout: &hooks::Layout,
    found: &[Found],
    rewires: &[(usize, String)],
) -> (Vec<FileChange>, Vec<Manual>) {
    let mut manual = Vec::new();
    let mut by_file: BTreeMap<usize, Vec<(&Candidate, String)>> = BTreeMap::new();
    for (index, name) in rewires {
        let Found::Command(candidate) = &found[*index] else {
            continue;
        };
        let file = &layout.files[candidate.file];
        if let Some(reason) = candidate.manual {
            manual.push(Manual {
                file: file.rel.clone(),
                reason,
                manager: file.manager,
                hook: candidate.hook.clone(),
                gate: name.clone(),
            });
            continue;
        }
        by_file.entry(candidate.file).or_default().push((
            candidate,
            format!("sluice run --hook {} {name}", candidate.hook),
        ));
    }
    let changes = by_file
        .iter()
        .map(|(file_index, rewires)| {
            let file = &layout.files[*file_index];
            FileChange {
                rel: file.rel.clone(),
                target: file.target.clone(),
                old: Some(file.lines.concat()),
                new: hooks::rewire(file, rewires),
                mode: None,
            }
        })
        .collect();
    (changes, manual)
}

/// The hook scripts `init` creates when there is no hook at all.
fn created_hooks(root: &Path, gates: &[GatePlan], notes: &mut Vec<String>) -> Vec<FileChange> {
    let mut changes = Vec::new();
    for hook in HOOKS {
        let hook_gates: Vec<&GatePlan> = gates
            .iter()
            .filter(|g| g.hooks.iter().any(|h| h == hook))
            .collect();
        if hook_gates.is_empty() {
            continue;
        }
        let rel = format!(".githooks/{hook}");
        if root.join(&rel).exists() {
            notes.push(format!("{rel} exists and was not touched"));
            continue;
        }
        changes.push(FileChange {
            target: root.join(&rel),
            rel,
            old: None,
            new: script_for(hook, &hook_gates),
            mode: Some(0o755),
        });
    }
    changes
}

fn plan(root: &Path, paths: &RepoPaths, env: &EnvMap, force: bool) -> Result<Plan, String> {
    let config_path = root.join(REPO_CONFIG_FILE);
    if config_path.exists() && !force {
        return Err(format!(
            "{REPO_CONFIG_FILE} already exists; not overwriting (use --force)"
        ));
    }
    let probe = git::probe_env(env, Some(paths));
    let hooks_path = git::run(&["config", "--get", "core.hooksPath"], root, &probe, true)
        .ok()
        .filter(|v| !v.is_empty());
    let layout = hooks::discover(root, &paths.common_dir, hooks_path.as_deref());
    let found = hooks::scan(&layout.files);
    let old = fs::read_to_string(&config_path).ok().and_then(|text| {
        RepoConfig::parse(&text, &config_path)
            .ok()
            .map(|c| (c, text))
    });

    let mut from_hooks = gates_from_hooks(root, &layout.files, &found, old.as_ref());
    let stack = detect::detect(root);
    let hook_plan = hook_plan(root, &layout, hooks_path.as_deref());
    let mut gates = std::mem::take(&mut from_hooks.gates);
    let mut notes = from_hooks.notes;
    let mut unwired = Vec::new();
    for mut gate in stack.gates {
        if gates.iter().any(|g| g.name == gate.name) {
            continue;
        }
        if matches!(hook_plan, HookPlan::Existing) {
            // Nothing calls it: it stays available to `sluice run` and the agents.
            gate.hooks.clear();
            unwired.push(gate.name.clone());
        }
        gates.push(gate);
    }
    if !unwired.is_empty() {
        notes.push(format!(
            "detected gate(s) {} are not called by any hook (hooks = []); run them with \
             `sluice run <gate>` or add a `sluice run --hook <hook> <gate>` line",
            unwired.join(", ")
        ));
    }

    let (config, snippet_text) = if gates.is_empty() {
        let starter = (
            vec!["lint".to_owned(), "test".to_owned()],
            vec!["test".to_owned()],
        );
        (
            starter_repo_config().to_owned(),
            Some(snippet(HookManager::Plain, &starter.0, &starter.1)),
        )
    } else {
        (
            render::config_text(&gates, &stack.names, &stack.nested),
            None,
        )
    };
    if let Err(e) = RepoConfig::parse(&config, &config_path) {
        return Err(format!(
            "the generated config is invalid (a bug in init): {e}"
        ));
    }

    let mut changes = vec![FileChange {
        rel: REPO_CONFIG_FILE.to_owned(),
        target: config_path.clone(),
        old: fs::read_to_string(&config_path).ok(),
        new: config,
        mode: None,
    }];
    let (edits, manual) = hook_edits(&layout, &found, &from_hooks.rewires);
    changes.extend(edits);

    let mut set_hooks_path = false;
    let mut snippet = snippet_text;
    match hook_plan {
        HookPlan::Existing => {}
        HookPlan::Create {
            set_hooks_path: set,
        } => {
            set_hooks_path = set && !gates.is_empty();
            changes.extend(created_hooks(root, &gates, &mut notes));
        }
        HookPlan::Snippet(reason) => {
            notes.push(format!(
                "hooks were not created: {reason}; add this by hand"
            ));
            snippet = Some(crate::snippets::snippet(
                HookManager::Plain,
                &gates_of(&gates, "pre-commit"),
                &gates_of(&gates, "pre-push"),
            ));
        }
    }
    Ok(Plan {
        stacks: stack.names,
        gates,
        changes,
        set_hooks_path,
        manual,
        notes,
        snippet,
    })
}

fn print_plan(plan: &Plan, root: &Path, dry_run: bool) {
    println!(
        "sluice: init at {}{}",
        root.display(),
        if dry_run { " (dry run)" } else { "" }
    );
    if !plan.stacks.is_empty() {
        println!("sluice: detected {}", plan.stacks.join(", "));
    }
    if plan.gates.is_empty() {
        println!("sluice: nothing detected; writing the starter template");
    } else {
        println!();
        println!("Gates:");
        print!("{}", render::summary_table(&plan.gates));
    }
    println!();
    println!("Files:");
    let verb = |new: bool| match (dry_run, new) {
        (true, true) => "would create",
        (true, false) => "would write",
        (false, true) => "created",
        (false, false) => "wrote",
    };
    for change in &plan.changes {
        let mode = change
            .mode
            .map(|m| format!(" (mode {m:04o})"))
            .unwrap_or_default();
        println!("  {} {}{mode}", verb(change.old.is_none()), change.rel);
    }
    if plan.set_hooks_path {
        println!(
            "  {} git config --local core.hooksPath .githooks",
            if dry_run { "would run" } else { "ran" }
        );
    }
    let diffs: Vec<String> = plan
        .changes
        .iter()
        .skip(1)
        .map(|c| match &c.old {
            Some(old) => render::unified_diff(&c.rel, old, &c.new),
            None => render::new_file_diff(&c.rel, &c.new),
        })
        .collect();
    if !diffs.is_empty() {
        println!();
        println!("Hook edits:");
        for diff in diffs {
            print!("{diff}");
        }
    }
    for note in &plan.notes {
        println!();
        println!("sluice: note: {note}");
    }
    let mut files: Vec<&str> = plan.manual.iter().map(|m| m.file.as_str()).collect();
    files.dedup();
    for file in files {
        let items: Vec<&Manual> = plan.manual.iter().filter(|m| m.file == file).collect();
        println!();
        println!(
            "sluice: could not edit {file} ({}); add this by hand:",
            items[0].reason
        );
        let names = |hook: &str| -> Vec<String> {
            items
                .iter()
                .filter(|m| m.hook == hook)
                .map(|m| m.gate.clone())
                .collect()
        };
        print!(
            "{}",
            snippet(items[0].manager, &names("pre-commit"), &names("pre-push"))
        );
    }
    if let Some(text) = &plan.snippet {
        println!();
        print!("{text}");
    }
    println!();
    if dry_run {
        println!("sluice: dry run, nothing was written");
    } else {
        println!("sluice: wrote {REPO_CONFIG_FILE}");
    }
}

fn write_changes(plan: &Plan, root: &Path, env: &EnvMap, paths: &RepoPaths) -> Result<(), String> {
    for change in &plan.changes {
        if let Some(parent) = change.target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        fs::write(&change.target, &change.new)
            .map_err(|e| format!("cannot write {}: {e}", change.rel))?;
        if let Some(mode) = change.mode {
            fs::set_permissions(&change.target, fs::Permissions::from_mode(mode))
                .map_err(|e| format!("cannot set the mode of {}: {e}", change.rel))?;
        }
    }
    if plan.set_hooks_path {
        let probe = git::probe_env(env, Some(paths));
        git::run(
            &["config", "--local", "core.hooksPath", ".githooks"],
            root,
            &probe,
            false,
        )
        .map_err(|e| format!("cannot set core.hooksPath: {e}"))?;
    }
    Ok(())
}

fn execute(force: bool, dry_run: bool) -> Result<(), String> {
    let env = config::process_env();
    let paths = discover_repo(&env)?;
    let root = paths.toplevel.clone();
    let plan = plan(&root, &paths, &env, force)?;
    if !dry_run {
        write_changes(&plan, &root, &env, &paths)?;
    }
    print_plan(&plan, &root, dry_run);
    Ok(())
}
