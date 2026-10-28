//! The pre-commit adapter (ADR-0021, wave H2b): the hooks of
//! `.pre-commit-config.yaml` at the git hook's stage become one unit per hook
//! id, each run by pre-commit itself:
//!
//! ```text
//! <launcher> run <id> --hook-stage <stage> --config=<config>
//! ```
//!
//! `<launcher>` and `<config>` are what the installed stub uses
//! (`INSTALL_PYTHON -mpre_commit`, else `pre-commit` on `PATH`; the baked
//! `--config=`). Only the stages whose `hook-impl` run takes no arguments
//! and reads no stdin are adapted: `pre-commit` and `pre-merge-commit`. The
//! others (`pre-push`: refs from stdin, `commit-msg`: the message file, ...)
//! are delegations.
//!
//! The stash: `pre-commit run` (without `--files`) stashes the unstaged
//! changes of tracked files on every invocation (`git checkout -- .`, a patch
//! re-applied afterwards). One unit per hook would do it once per hook, so the
//! units are used only while no tracked file differs from the index
//! ([`Precondition::WorktreeEqualsIndex`]); then the stash is a no-op and the
//! working tree equals the tree the cache key describes. Otherwise the hook is
//! delegated and pre-commit stashes once, as it always did.
//!
//! What a unit needs to be exact is given by the configuration alone only for
//! hooks Sluice can place at the stage: `repo: local` hooks, and remote hooks
//! whose config entry lists `stages` (a remote hook's own stages are in its
//! manifest, in a clone Sluice does not read). Anything else delegates.
//!
//! Caching: a hook is a [`Kind::Check`] only if it is local, `language:
//! system` (or `unsupported`), `always_run: true` (otherwise pre-commit skips
//! it, successfully, when no staged file matches, and a cached pass would
//! claim a run that never happened), `pass_filenames: false`, with a
//! recognised read-only `entry`. The key adds the configuration file's bytes
//! and the pre-commit version. Everything else (every fixer, every remote
//! hook) always runs.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sluice_gate_core::config::EnvMap;

use super::classify;
use super::delegate::{Manager, is_executable};
use super::grammar;
use super::plan::Input;
use super::tooling::{find_on_path, parse_version, sh_line};
use super::units::{Kind, Precondition, Program, Unit};

/// The newest `minimum_pre_commit_version` accepted (the version read).
const KNOWN_VERSION: (u64, u64, u64) = (4, 7, 0);

/// Largest configuration read.
const MAX_CONFIG: u64 = 1 << 20;

const STAGES: [&str; 11] = [
    "commit-msg",
    "post-checkout",
    "post-commit",
    "post-merge",
    "post-rewrite",
    "pre-commit",
    "pre-merge-commit",
    "pre-push",
    "pre-rebase",
    "prepare-commit-msg",
    "manual",
];

/// Hook keys of a config entry (`CONFIG_HOOK_DICT`, plus the local ones).
const HOOK_KEYS: [&str; 22] = [
    "id",
    "alias",
    "name",
    "entry",
    "language",
    "files",
    "exclude",
    "types",
    "types_or",
    "exclude_types",
    "always_run",
    "fail_fast",
    "verbose",
    "pass_filenames",
    "require_serial",
    "description",
    "language_version",
    "log_file",
    "minimum_pre_commit_version",
    "args",
    "stages",
    "additional_dependencies",
];

const TOP_KEYS: [&str; 9] = [
    "repos",
    "default_stages",
    "default_language_version",
    "default_install_hook_types",
    "files",
    "exclude",
    "fail_fast",
    "minimum_pre_commit_version",
    "ci",
];

/// What the installed stub says.
struct Stub {
    python: Option<String>,
    config: String,
}

/// A hook placed at the stage.
#[derive(Debug)]
struct HookDef {
    id: String,
    alias: Option<String>,
    local: bool,
    language: String,
    entry: String,
    args: Vec<String>,
    /// `always_run: true`, `pass_filenames: false` and no `log_file`: the
    /// hook runs on every call and its outcome depends on no file list.
    unconditional: bool,
}

/// What `hook` becomes, or why it is delegated.
pub fn plan(input: &Input<'_>) -> Result<Program, String> {
    if !matches!(input.hook, "pre-commit" | "pre-merge-commit") {
        return Err(format!(
            "pre-commit's {} run needs git's arguments, refs or stdin",
            input.hook
        ));
    }
    if input.env.get("SKIP").is_some_and(|v| !v.is_empty()) {
        return Err("SKIP is set (pre-commit skips hooks successfully)".to_owned());
    }
    let stub = read_stub(input)?;
    let launcher = launcher(&stub, input.env)?;
    let text = read_config(&input.root.join(&stub.config))?;
    let config = parse_config(&text)?;
    let hooks = hooks_at_stage(&config, input.hook)?;
    if hooks.is_empty() {
        return Err(format!(
            "no hook of the configuration runs at {}",
            input.hook
        ));
    }
    check_ids(&hooks)?;
    Ok(program(input, &stub, &launcher, &hooks))
}

/// `INSTALL_PYTHON` and `--config` as the stub in the hooks directory bakes
/// them in.
fn read_stub(input: &Input<'_>) -> Result<Stub, String> {
    let path = super::activate::resolve(input.dir, input.root).join(input.hook);
    let text = fs::read_to_string(&path)
        .map_err(|_| format!("cannot read pre-commit's script {}", path.display()))?;
    let python = text
        .lines()
        .find_map(|l| l.strip_prefix("INSTALL_PYTHON="))
        .map(unquote)
        .ok_or("the hook file is not pre-commit's generated script")??;
    let args = text
        .lines()
        .find_map(|l| l.strip_prefix("ARGS=(").and_then(|r| r.strip_suffix(')')))
        .ok_or("the hook file has no ARGS line")?;
    let mut config = None;
    let mut hook_type = None;
    for (n, word) in args.split_whitespace().enumerate() {
        if let Some(value) = word.strip_prefix("--config=") {
            config = Some(value.to_owned());
        } else if let Some(value) = word.strip_prefix("--hook-type=") {
            hook_type = Some(value.to_owned());
        } else if !((n == 0 && word == "hook-impl") || word == "--skip-on-missing-config") {
            return Err(format!("the stub's ARGS has the unknown word `{word}`"));
        }
    }
    if hook_type.as_deref() != Some(input.hook) {
        return Err("the stub is for another hook type".to_owned());
    }
    let config = config.filter(|c| !c.is_empty() && !c.contains('\'') && !c.contains('"'));
    Ok(Stub {
        python: Some(python).filter(|p| !p.is_empty()),
        config: config.ok_or("the stub has no usable --config")?,
    })
}

/// `shlex.quote` of a path: bare, or `'...'` without inner quotes.
fn unquote(value: &str) -> Result<String, String> {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        return if inner.contains('\'') {
            Err("INSTALL_PYTHON is quoted in a way that is not read".to_owned())
        } else {
            Ok(inner.to_owned())
        };
    }
    if value
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)))
    {
        return Err("INSTALL_PYTHON is quoted in a way that is not read".to_owned());
    }
    Ok(value.to_owned())
}

/// The stub's own order: `INSTALL_PYTHON -mpre_commit` while it is executable,
/// else `pre-commit` on `PATH`, else the stub fails (exit 1): a delegation.
fn launcher(stub: &Stub, env: &EnvMap) -> Result<Vec<String>, String> {
    if let Some(python) = stub
        .python
        .as_deref()
        .filter(|p| is_executable(Path::new(p)))
    {
        return Ok(vec![python.to_owned(), "-mpre_commit".to_owned()]);
    }
    find_on_path("pre-commit", env)
        .map(|p| vec![p.display().to_string()])
        .ok_or_else(|| {
            "pre-commit is not found the way its script finds it (INSTALL_PYTHON, PATH)".to_owned()
        })
}

fn read_config(path: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.len() > MAX_CONFIG {
        return Err("the configuration is too large".to_owned());
    }
    fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// The YAML as JSON values. Merge keys (`<<`) are expanded by pre-commit's
/// loader and not here, so a file that uses them is not understood.
fn parse_config(text: &str) -> Result<Map<String, Value>, String> {
    let value: Value =
        serde_norway::from_str(text).map_err(|e| format!("the YAML is not read plainly: {e}"))?;
    if has_merge_key(&value) {
        return Err("the YAML uses merge keys (`<<`)".to_owned());
    }
    let Value::Object(map) = value else {
        return Err("the configuration is not a mapping".to_owned());
    };
    if let Some(key) = map.keys().find(|k| !TOP_KEYS.contains(&k.as_str())) {
        return Err(format!("the configuration has the unknown key `{key}`"));
    }
    check_versions_and_fail_fast(&map)?;
    Ok(map)
}

fn has_merge_key(value: &Value) -> bool {
    match value {
        Value::Object(m) => m.contains_key("<<") || m.values().any(has_merge_key),
        Value::Array(a) => a.iter().any(has_merge_key),
        _ => false,
    }
}

/// `fail_fast` makes pre-commit stop after one failure (across hooks);
/// `minimum_pre_commit_version` is enforced by pre-commit itself but only
/// versions up to the one read are understood.
fn check_versions_and_fail_fast(map: &Map<String, Value>) -> Result<(), String> {
    if map
        .get("fail_fast")
        .is_some_and(|v| v != &Value::Bool(false))
    {
        return Err("the configuration sets fail_fast".to_owned());
    }
    match map.get("minimum_pre_commit_version") {
        None => Ok(()),
        Some(Value::String(v)) if parse_version(v).is_some_and(|p| p <= KNOWN_VERSION) => Ok(()),
        Some(_) => Err("minimum_pre_commit_version is newer than the version read".to_owned()),
    }
}

/// Normalised stage names (`commit` is `pre-commit`, ...).
fn stage_list(value: &Value) -> Result<Vec<String>, String> {
    let list = value.as_array().ok_or("`stages` is not a list")?;
    list.iter()
        .map(|v| {
            let name = match v.as_str() {
                Some("commit") => "pre-commit",
                Some("push") => "pre-push",
                Some("merge-commit") => "pre-merge-commit",
                Some(s) if STAGES.contains(&s) => s,
                _ => return Err("a stage is not a known name".to_owned()),
            };
            Ok(name.to_owned())
        })
        .collect()
}

/// The hooks of the configuration that pre-commit would select at `stage`, in
/// file order.
fn hooks_at_stage(config: &Map<String, Value>, stage: &str) -> Result<Vec<HookDef>, String> {
    let default_stages = match config.get("default_stages") {
        Some(v) => Some(stage_list(v)?),
        None => None,
    };
    let repos = config
        .get("repos")
        .and_then(Value::as_array)
        .ok_or("the configuration has no `repos` list")?;
    let mut hooks = Vec::new();
    for repo in repos {
        let repo = repo.as_object().ok_or("a repo is not a mapping")?;
        let kind = repo
            .get("repo")
            .and_then(Value::as_str)
            .ok_or("a repo has no name")?;
        if kind == "meta" {
            return Err("the configuration uses meta hooks".to_owned());
        }
        for entry in repo
            .get("hooks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let object = entry.as_object().ok_or("a hook is not a mapping")?;
            let (hook, stages) = read_hook(object, kind == "local", default_stages.as_deref())?;
            if stages.iter().any(|s| s == stage) {
                hooks.push(hook);
            }
        }
    }
    Ok(hooks)
}

fn string_field(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{key}` is not a plain string")),
    }
}

fn bool_field(object: &Map<String, Value>, key: &str, default: bool) -> Result<bool, String> {
    match object.get(key) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("`{key}` is not a plain boolean")),
    }
}

/// A hook of a config entry and the stages it runs at; a remote hook without
/// `stages` is an error (they are in its manifest).
fn read_hook(
    object: &Map<String, Value>,
    local: bool,
    default_stages: Option<&[String]>,
) -> Result<(HookDef, Vec<String>), String> {
    if let Some(key) = object.keys().find(|k| !HOOK_KEYS.contains(&k.as_str())) {
        return Err(format!("a hook has the unknown key `{key}`"));
    }
    let id = string_field(object, "id")?.ok_or("a hook has no id")?;
    if bool_field(object, "fail_fast", false)? {
        return Err(format!("the hook `{id}` sets fail_fast"));
    }
    if let Some(v) = object.get("minimum_pre_commit_version") {
        let ok = v
            .as_str()
            .and_then(parse_version)
            .is_some_and(|p| p <= KNOWN_VERSION);
        if !ok {
            return Err(format!(
                "the hook `{id}` needs a newer pre-commit than the one read"
            ));
        }
    }
    let stages = match (object.get("stages"), local, default_stages) {
        (Some(v), _, _) if !v.as_array().is_some_and(Vec::is_empty) => stage_list(v)?,
        (_, true, Some(d)) => d.to_vec(),
        (_, true, None) => STAGES.iter().map(|s| (*s).to_owned()).collect(),
        (..) => {
            return Err(format!(
                "the remote hook `{id}` does not say at which stages it runs (its manifest does)"
            ));
        }
    };
    let always_run = bool_field(object, "always_run", false)?;
    let pass_filenames = bool_field(object, "pass_filenames", true)?;
    let hook = HookDef {
        alias: string_field(object, "alias")?,
        local,
        language: string_field(object, "language")?.unwrap_or_default(),
        entry: string_field(object, "entry")?.unwrap_or_default(),
        args: read_args(object, &id)?,
        unconditional: always_run && !pass_filenames && !object.contains_key("log_file"),
        id,
    };
    Ok((hook, stages))
}

fn read_args(object: &Map<String, Value>, id: &str) -> Result<Vec<String>, String> {
    let Some(value) = object.get("args") else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .and_then(|a| a.iter().map(|v| v.as_str().map(str::to_owned)).collect())
        .ok_or_else(|| format!("the args of `{id}` are not a list of plain strings"))
}

/// `pre-commit run <id>` selects every hook whose id or alias is `<id>`.
fn check_ids(hooks: &[HookDef]) -> Result<(), String> {
    for hook in hooks {
        let selected = hooks
            .iter()
            .filter(|h| h.id == hook.id || h.alias.as_deref() == Some(hook.id.as_str()))
            .count();
        if selected != 1 {
            return Err(format!("the id `{}` selects more than one hook", hook.id));
        }
    }
    Ok(())
}

fn program(input: &Input<'_>, stub: &Stub, launcher: &[String], hooks: &[HookDef]) -> Program {
    let version = format!("{} --version", sh_line(launcher));
    let lists = hooks
        .iter()
        .enumerate()
        .map(|(i, hook)| {
            let mut argv = launcher.to_vec();
            argv.extend(
                ["run", &hook.id, "--hook-stage", input.hook]
                    .map(str::to_owned)
                    .into_iter()
                    .chain([format!("--config={}", stub.config)]),
            );
            let check = kind_of(hook, input.root) == Kind::Check;
            vec![Unit {
                manager: Manager::PreCommit,
                hook: input.hook.to_owned(),
                index: i + 1,
                command: sh_line(&argv),
                argv,
                cwd: input.root.to_path_buf(),
                path_prepend: Vec::new(),
                kind: if check { Kind::Check } else { Kind::Mutating },
                git_env: true,
                key_inputs: if check {
                    vec![version.clone()]
                } else {
                    Vec::new()
                },
                key_files: if check {
                    vec![PathBuf::from(&stub.config)]
                } else {
                    Vec::new()
                },
            }]
        })
        .collect();
    Program {
        lists,
        exit_ok: false,
        keep_going: true,
        precondition: Some(Precondition::WorktreeEqualsIndex),
    }
}

/// See the module documentation for the reasons of each condition.
fn kind_of(hook: &HookDef, root: &Path) -> Kind {
    let simple = hook.local
        && matches!(hook.language.as_str(), "system" | "unsupported")
        && hook.unconditional;
    if !simple {
        return Kind::Mutating;
    }
    match grammar::parse(&hook.entry) {
        Ok(script) if script.lists.len() == 1 && script.lists[0].len() == 1 && !script.exit_ok => {
            let mut argv = script.lists[0][0].argv.clone();
            argv.extend(hook.args.iter().cloned());
            classify::classify(&argv, root)
        }
        _ => Kind::Mutating,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stubs_python_is_read_as_shlex_wrote_it() {
        assert_eq!(unquote("/usr/bin/python3").unwrap(), "/usr/bin/python3");
        assert_eq!(
            unquote("'/opt/my env/bin/python'").unwrap(),
            "/opt/my env/bin/python"
        );
        assert!(unquote("'it'\"'\"'s'").is_err());
        assert!(unquote("$HOME/python").is_err());
    }

    #[test]
    fn stages_are_normalised_and_unknown_ones_refused() {
        let list = |text: &str| stage_list(&serde_json::from_str(text).unwrap());
        assert_eq!(
            list(r#"["commit","push","merge-commit","manual"]"#).unwrap(),
            ["pre-commit", "pre-push", "pre-merge-commit", "manual"]
        );
        assert!(list(r#"["nope"]"#).is_err());
        assert!(list(r#""pre-commit""#).is_err());
    }

    #[test]
    fn yaml_that_pre_commits_loader_would_read_differently_is_refused() {
        assert!(parse_config("repos: []\nfail_fast: true\n").is_err());
        assert!(parse_config("repos: []\nminimum_pre_commit_version: '4.7.0'\n").is_ok());
        assert!(parse_config("repos: []\nminimum_pre_commit_version: '4.8.0'\n").is_err());
        assert!(parse_config("a: &a {x: 1}\nrepos: []\nb: {<<: *a}\n").is_err());
        assert!(parse_config("- not a mapping\n").is_err());
        assert!(parse_config("repos: [\n").is_err());
    }
}
