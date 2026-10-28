//! The lefthook adapter (ADR-0021, wave H2b): the effective configuration
//! (`lefthook dump --format json`, which resolves `extends`, `remotes` already
//! cloned and `lefthook-local`) becomes one unit per named command or job, each
//! run by lefthook itself:
//!
//! ```text
//! lefthook run <hook> --command <name> --no-auto-install --skip-lfs
//! ```
//!
//! `--command` selects by exact name and applies the job's own `env`,
//! `timeout` and `run` exactly as lefthook does. What a per-job run would
//! change is a delegation of the whole hook ([`plan`] explains each reason),
//! in particular everything that can make lefthook *skip* the selected job:
//! lefthook exits 1 with "no job matching" for a skipped job, and a skip is
//! indistinguishable from a failure. A hook is run as units only when no
//! selected job can be skipped: no `skip`/`only`/`exclude_tags`, no file
//! template or filter, and, for `pre-commit`, staged files at run time
//! ([`Precondition::StagedNotPartial`]).
//!
//! Sequential order is lefthook's: `jobs` as written, then `commands` sorted
//! by `priority`, leading number, name. `parallel: true` only changes
//! concurrency; the units run one after another.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sluice_gate_core::config::EnvMap;

use super::classify;
use super::delegate::{Manager, is_executable};
use super::grammar;
use super::plan::{ALREADY_ROUTED, Input, text_runs_sluice};
use super::tooling::{capture, find_on_path, parse_version, sh_line, sha256_hex};
use super::units::{Kind, Precondition, Program, Unit};

/// The oldest lefthook whose `run` flags were read from its source (2.2.2).
const MIN_VERSION: (u64, u64, u64) = (2, 2, 2);

/// Hooks lefthook runs git-lfs for (`internal/git/lfs.go`): `--skip-lfs`
/// would drop that, so they are delegated.
const LFS_HOOKS: [&str; 4] = ["post-checkout", "post-commit", "post-merge", "pre-push"];

/// One command or job, as lefthook will see it.
#[derive(Debug, PartialEq, Eq)]
struct Job {
    name: String,
    run: String,
    /// `env` values that expand variables are not a fixed input.
    dynamic_env: bool,
    priority: i64,
}

/// What `hook` becomes, or why it is delegated.
pub fn plan(input: &Input<'_>) -> Result<Program, String> {
    check_hook_name(input.hook)?;
    check_environment(input.env)?;
    let bin = load_tool(input)?;
    let (config, config_hash) = effective_config(&bin, input.root)?;
    let hook = config
        .get(input.hook)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("lefthook has no configuration for {}", input.hook))?;
    check_hook_keys(hook)?;
    let jobs = collect_jobs(hook)?;
    if jobs.is_empty() {
        return Err(format!("lefthook has no job for {}", input.hook));
    }
    Ok(program(input, &bin, &config_hash, &jobs))
}

fn check_hook_name(hook: &str) -> Result<(), String> {
    if hook == "pre-push" {
        return Err("lefthook skips a command with no files to push, which `--command` reports as a failure".to_owned());
    }
    if LFS_HOOKS.contains(&hook) {
        return Err(format!(
            "lefthook runs git-lfs for {hook}, which the per-command runs skip"
        ));
    }
    Ok(())
}

fn check_environment(env: &EnvMap) -> Result<(), String> {
    if env
        .get("LEFTHOOK")
        .is_some_and(|v| v == "0" || v == "false")
    {
        return Err("LEFTHOOK disables the hook".to_owned());
    }
    if env.get("LEFTHOOK_EXCLUDE").is_some_and(|v| !v.is_empty()) {
        return Err("LEFTHOOK_EXCLUDE can skip jobs".to_owned());
    }
    Ok(())
}

/// Finds the binary the way lefthook's hook script does, as far as Sluice
/// models it: `$LEFTHOOK_BIN`, then `lefthook` on `PATH`. The script's other
/// routes (the `lefthook:` config value, a path baked at install time,
/// `node_modules` layouts, `go tool`, `bundle exec`, ...) are delegations.
fn load_tool(input: &Input<'_>) -> Result<PathBuf, String> {
    let bin = binary(input.env, input.root)?;
    let version = capture(&bin, &["version"], input.root)?;
    let parsed = parse_version(&version)
        .ok_or_else(|| format!("cannot read the lefthook version from `{}`", version.trim()))?;
    if parsed.0 != MIN_VERSION.0 || parsed < MIN_VERSION {
        return Err(format!(
            "lefthook {}.{}.{} is not a version whose flags are known (2.x from 2.2.2)",
            parsed.0, parsed.1, parsed.2
        ));
    }
    Ok(bin)
}

fn binary(env: &EnvMap, root: &Path) -> Result<PathBuf, String> {
    if let Some(value) = env.get("LEFTHOOK_BIN").filter(|v| !v.is_empty()) {
        let path = if value.contains('/') {
            root.join(value)
        } else {
            find_on_path(value, env).unwrap_or_default()
        };
        return if is_executable(&path) {
            Ok(path)
        } else {
            Err("LEFTHOOK_BIN is not an executable".to_owned())
        };
    }
    find_on_path("lefthook", env).ok_or_else(|| {
        "the lefthook binary is not on PATH (the hook script has other ways to find it)".to_owned()
    })
}

/// `lefthook dump --format json`, checked for what changes how it is run.
fn effective_config(bin: &Path, root: &Path) -> Result<(Map<String, Value>, String), String> {
    let text = capture(bin, &["dump", "--format", "json"], root)
        .map_err(|e| format!("lefthook dump failed: {e}"))?;
    let Ok(Value::Object(config)) = serde_json::from_str::<Value>(&text) else {
        return Err("lefthook dump did not print a JSON object".to_owned());
    };
    for key in ["remotes", "rc", "lefthook"] {
        if config.get(key).is_some_and(|v| !is_empty(v)) {
            return Err(format!("the configuration sets `{key}`"));
        }
    }
    Ok((config, sha256_hex(text.as_bytes())))
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Number(_) => false,
    }
}

/// Hook-level options: `parallel` only changes concurrency; everything else
/// that matters must be absent.
fn check_hook_keys(hook: &Map<String, Value>) -> Result<(), String> {
    for (key, value) in hook {
        match key.as_str() {
            "parallel" | "jobs" | "commands" | "fail_on_changes_diff" => {}
            "piped" | "follow" | "setup" | "scripts" | "files" | "exclude" | "exclude_tags"
            | "fail_on_changes" | "skip" => {
                if !is_empty(value) {
                    return Err(format!("the hook sets `{key}`"));
                }
            }
            "only" => {
                if !value.is_null() {
                    return Err("the hook sets `only`".to_owned());
                }
            }
            other => return Err(format!("the hook has the unknown option `{other}`")),
        }
    }
    Ok(())
}

/// The jobs in lefthook's sequential order.
fn collect_jobs(hook: &Map<String, Value>) -> Result<Vec<Job>, String> {
    let mut jobs = Vec::new();
    for value in array(hook.get("jobs")) {
        let object = value.as_object().ok_or("a job is not an object")?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .ok_or("a job has no name (only named jobs can be selected)")?;
        jobs.push(read_job(name, object)?);
    }
    let mut commands = Vec::new();
    if let Some(map) = hook.get("commands").and_then(Value::as_object) {
        for (name, value) in map {
            let object = value.as_object().ok_or("a command is not an object")?;
            commands.push(read_job(name, object)?);
        }
    }
    order_commands(&mut commands)?;
    jobs.append(&mut commands);
    check_names(&jobs)?;
    Ok(jobs)
}

fn array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Validates one job or command; any key that could change whether or how it
/// runs, or that is unknown, makes the hook a delegation.
fn read_job(name: &str, object: &Map<String, Value>) -> Result<Job, String> {
    let mut job = Job {
        name: name.to_owned(),
        run: String::new(),
        dynamic_env: false,
        priority: 0,
    };
    for (key, value) in object {
        match key.as_str() {
            "name" | "fail_text" | "timeout" | "tags" => {}
            "run" => value.as_str().unwrap_or_default().clone_into(&mut job.run),
            "env" => job.dynamic_env = dynamic_env(value),
            "priority" => job.priority = value.as_i64().ok_or("a priority is not a number")?,
            "interactive" | "use_stdin" | "stage_fixed" | "skip" | "glob" | "exclude" | "root"
            | "files" | "file_types" | "args" | "script" | "runner" | "group" => {
                if !is_empty(value) {
                    return Err(format!("`{name}` sets `{key}`"));
                }
            }
            "only" => {
                if !value.is_null() {
                    return Err(format!("`{name}` sets `only`"));
                }
            }
            other => return Err(format!("`{name}` has the unknown option `{other}`")),
        }
    }
    if job.run.is_empty() {
        return Err(format!("`{name}` has no `run`"));
    }
    if text_runs_sluice(&job.run) {
        return Err(format!("`{name}` {ALREADY_ROUTED}"));
    }
    if uses_template(&job.run) {
        return Err(format!(
            "`{name}` uses a template (`{{...}}`) that depends on files or arguments"
        ));
    }
    Ok(job)
}

fn dynamic_env(value: &Value) -> bool {
    value.as_object().is_some_and(|m| {
        m.values()
            .any(|v| v.as_str().is_none_or(|s| s.contains('$')))
    })
}

/// True when `text` holds `{word}`: lefthook's templates are `{staged_files}`,
/// `{all_files}`, `{push_files}`, `{files}`, `{0}`, `{1}`, ...,
/// `{lefthook_job_name}`, `{cmd}` and the user's own `templates:`. Shell
/// text such as `${HOME}` matches too, which only costs a delegation.
fn uses_template(text: &str) -> bool {
    text.match_indices('{').any(|(i, _)| {
        text[i + 1..].find('}').is_some_and(|end| {
            let inner = &text[i + 1..i + 1 + end];
            !inner.is_empty() && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    })
}

/// lefthook's order of `commands` (`CommandsToJobs`): jobs with a priority by
/// priority, then the others by leading number, then by name. Ties are
/// resolved by an unstable sort there, so Sluice does not guess.
fn order_commands(commands: &mut [Job]) -> Result<(), String> {
    if commands.iter().any(|c| !c.name.is_ascii()) {
        return Err("a command name is not ASCII (its order is not modelled)".to_owned());
    }
    commands.sort_by(compare);
    if commands
        .windows(2)
        .any(|w| compare(&w[0], &w[1]) == Ordering::Equal)
    {
        return Err(
            "two commands have the same priority or number (their order is not defined)".to_owned(),
        );
    }
    Ok(())
}

fn leading_number(name: &str) -> Option<u64> {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn compare(a: &Job, b: &Job) -> Ordering {
    match (a.priority, b.priority) {
        (0, 0) => match (leading_number(&a.name), leading_number(&b.name)) {
            (None, None) => a.name.cmp(&b.name),
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => x.cmp(&y),
        },
        (0, _) => Ordering::Greater,
        (_, 0) => Ordering::Less,
        (x, y) => x.cmp(&y),
    }
}

/// `--command` matches by name: a name used twice would run both.
fn check_names(jobs: &[Job]) -> Result<(), String> {
    for (i, job) in jobs.iter().enumerate() {
        if jobs[..i].iter().any(|j| j.name == job.name) {
            return Err(format!("the name `{}` is used twice", job.name));
        }
    }
    Ok(())
}

fn program(input: &Input<'_>, bin: &Path, config_hash: &str, jobs: &[Job]) -> Program {
    let version = format!("{} version", sh_line(&[bin.display().to_string()]));
    let hash = format!("printf %s {config_hash}");
    let lists = jobs
        .iter()
        .enumerate()
        .map(|(i, job)| {
            let argv: Vec<String> = [
                &bin.display().to_string(),
                "run",
                input.hook,
                "--command",
                &job.name,
                "--no-auto-install",
                "--skip-lfs",
            ]
            .map(str::to_owned)
            .to_vec();
            vec![Unit {
                manager: Manager::Lefthook,
                hook: input.hook.to_owned(),
                index: i + 1,
                command: sh_line(&argv),
                argv,
                cwd: input.root.to_path_buf(),
                path_prepend: Vec::new(),
                kind: kind_of(job, input.root),
                git_env: true,
                key_inputs: vec![version.clone(), hash.clone()],
                key_files: Vec::new(),
            }]
        })
        .collect();
    Program {
        lists,
        exit_ok: false,
        keep_going: true,
        precondition: (input.hook == "pre-commit").then_some(Precondition::StagedNotPartial),
    }
}

/// A job is a cacheable check only when its `run` text is a list of simple
/// commands that are all recognised read-only tools and its `env` has no
/// expansions; everything else always runs.
fn kind_of(job: &Job, root: &Path) -> Kind {
    if job.dynamic_env {
        return Kind::Mutating;
    }
    match grammar::parse(&job.run) {
        Ok(script)
            if !script.exit_ok
                && script
                    .lists
                    .iter()
                    .flatten()
                    .all(|c| classify::classify(&c.argv, root) == Kind::Check) =>
        {
            Kind::Check
        }
        _ => Kind::Mutating,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, priority: i64) -> Job {
        Job {
            name: name.to_owned(),
            run: "jest".to_owned(),
            dynamic_env: false,
            priority,
        }
    }

    fn names(jobs: &[Job]) -> Vec<&str> {
        jobs.iter().map(|j| j.name.as_str()).collect()
    }

    #[test]
    fn templates_are_words_in_braces() {
        for text in [
            "jest {staged_files}",
            "x {1}",
            "{cmd} --fix",
            "a {lefthook_job_name}",
            "echo ${HOME}",
        ] {
            assert!(uses_template(text), "{text}");
        }
        for text in [
            "jest",
            "awk '{print $1}'",
            "echo {a,b}",
            "echo {}",
            "echo }{",
        ] {
            assert!(!uses_template(text), "{text}");
        }
    }

    #[test]
    fn commands_are_ordered_like_lefthook_does() {
        let mut jobs = vec![job("z", 0), job("10-y", 0), job("2-x", 0), job("a", 0)];
        order_commands(&mut jobs).unwrap();
        assert_eq!(names(&jobs), ["2-x", "10-y", "a", "z"]);
        let mut jobs = vec![job("a", 0), job("b", 5), job("c", 2)];
        order_commands(&mut jobs).unwrap();
        assert_eq!(
            names(&jobs),
            ["c", "b", "a"],
            "priorities first, the rest after"
        );
    }

    #[test]
    fn an_undefined_order_is_refused() {
        assert!(order_commands(&mut [job("1a", 0), job("1b", 0)]).is_err());
        assert!(order_commands(&mut [job("a", 3), job("b", 3)]).is_err());
        assert!(order_commands(&mut [job("é", 0)]).is_err());
    }

    #[test]
    fn only_read_only_run_texts_are_checks() {
        let root = Path::new("/nonexistent");
        let kind = |run: &str, dynamic_env: bool| {
            kind_of(
                &Job {
                    run: run.to_owned(),
                    dynamic_env,
                    ..job("a", 0)
                },
                root,
            )
        };
        assert_eq!(kind("jest --ci", false), Kind::Check);
        assert_eq!(kind("jest --ci && tsc", false), Kind::Check);
        assert_eq!(kind("jest --ci", true), Kind::Mutating);
        assert_eq!(kind("prettier --write .", false), Kind::Mutating);
        assert_eq!(kind("jest | tee out", false), Kind::Mutating);
        assert_eq!(kind("./script.sh", false), Kind::Mutating);
    }
}
