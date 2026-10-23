//! Stack detection and gate classification for `sluice init`.
//!
//! Pure functions over the file system at the repository root: which
//! package manager, which stack, which gates a project of that stack needs,
//! and how a command found in a hook is named and weighted.

use std::fs;
use std::path::Path;

use super::{GatePlan, Source, WeightSpec};

/// Longest gate name `init` generates (the config allows 64).
const MAX_NAME: usize = 40;

/// A Node package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pm {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl Pm {
    /// The manager's executable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        }
    }

    /// The manager behind an executable word (`npx` is npm's).
    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "npm" | "npx" => Some(Self::Npm),
            "pnpm" => Some(Self::Pnpm),
            "yarn" => Some(Self::Yarn),
            "bun" | "bunx" => Some(Self::Bun),
            _ => None,
        }
    }

    /// `npm run <script>` and its equivalents.
    pub fn run(self, script: &str) -> String {
        format!("{} run {script}", self.name())
    }

    /// The command that runs the `test` script.
    pub fn test(self) -> String {
        match self {
            Self::Bun => "bun run test".to_owned(),
            _ => format!("{} test", self.name()),
        }
    }

    /// The prefix that runs a locally installed binary.
    pub fn exec(self) -> &'static str {
        match self {
            Self::Npm => "npx",
            Self::Pnpm => "pnpm exec",
            Self::Yarn => "yarn",
            Self::Bun => "bunx",
        }
    }

    /// The `key_inputs` entry for the manager's version.
    pub fn version_cmd(self) -> String {
        format!("{} --version", self.name())
    }
}

/// The package manager of the project at `root` (`None` without a
/// `package.json`), from its lockfile; npm when there is none.
pub fn package_manager(root: &Path) -> Option<Pm> {
    if !root.join("package.json").is_file() {
        return None;
    }
    let has = |f: &str| root.join(f).exists();
    Some(if has("pnpm-lock.yaml") {
        Pm::Pnpm
    } else if has("yarn.lock") {
        Pm::Yarn
    } else if has("bun.lockb") || has("bun.lock") {
        Pm::Bun
    } else {
        Pm::Npm
    })
}

/// What a command is for; decides weight, timeout and caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fmt,
    Lint,
    Test,
    Build,
    Other,
}

impl Kind {
    /// The kind a command with these words (entry words, script name, gate
    /// name) belongs to.
    pub fn of<S: AsRef<str>>(words: &[S]) -> Self {
        let text = words
            .iter()
            .map(|w| w.as_ref().to_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
        if has(&[
            "test",
            "jest",
            "vitest",
            "pytest",
            "e2e",
            "cypress",
            "playwright",
        ]) {
            Self::Test
        } else if has(&["fmt", "format"]) {
            Self::Fmt
        } else if has(&[
            "lint",
            "clippy",
            "vet",
            "typecheck",
            "type-check",
            "tsc",
            "mypy",
            "ruff",
        ]) {
            Self::Lint
        } else if has(&["build"]) {
            Self::Build
        } else {
            Self::Other
        }
    }

    pub fn weight(self) -> WeightSpec {
        match self {
            Self::Fmt => WeightSpec::Fixed(1),
            Self::Lint => WeightSpec::Fixed(2),
            Self::Test | Self::Build | Self::Other => WeightSpec::Auto,
        }
    }

    pub fn timeout(self) -> &'static str {
        match self {
            Self::Fmt | Self::Lint => "10m",
            Self::Test | Self::Build | Self::Other => "30m",
        }
    }

    /// Hook of a detected gate when `init` creates the hooks itself.
    pub fn default_hook(self) -> &'static str {
        match self {
            Self::Test | Self::Build | Self::Other => "pre-push",
            Self::Fmt | Self::Lint => "pre-commit",
        }
    }
}

/// A gate name (`[a-z0-9][a-z0-9_-]*`) from free text.
pub fn sanitize_name(text: &str) -> String {
    let mut out = String::new();
    for c in text.to_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '_');
    let name: String = trimmed.chars().take(MAX_NAME).collect();
    if name.is_empty() {
        "gate".to_owned()
    } else {
        name
    }
}

/// The gate name for a heavy command: the package script it runs, else the
/// canonical words of the matching heavy-list entry (`ng test` -> `test`,
/// `run typecheck` -> `typecheck`, `cargo clippy` -> `clippy`).
pub fn gate_name(entry: &[String], script: Option<&str>) -> String {
    if let Some(script) = script {
        return sanitize_name(script);
    }
    let name = match entry {
        [first, second, ..] if matches!(first.as_str(), "ng" | "cargo" | "go" | "dotnet") => {
            second.clone()
        }
        [first, second, ..]
            if matches!(first.as_str(), "nx" | "turbo" | "bazel")
                && matches!(second.as_str(), "build" | "test" | "lint" | "e2e") =>
        {
            second.clone()
        }
        [first, rest @ ..] if first == "run" && !rest.is_empty() => rest.join("-"),
        words => words.join("-"),
    };
    sanitize_name(&name)
}

fn read(root: &Path, file: &str) -> Option<String> {
    fs::read_to_string(root.join(file)).ok()
}

fn gate(
    name: &str,
    cmd: String,
    kind: Kind,
    reason: &'static str,
    key_inputs: Vec<String>,
) -> GatePlan {
    GatePlan {
        name: name.to_owned(),
        cmd,
        weight: kind.weight(),
        timeout: kind.timeout().to_owned(),
        hooks: vec![kind.default_hook().to_owned()],
        key_inputs,
        env: Vec::new(),
        cache: kind != Kind::Build,
        source: Source::Detected(reason),
        carried: None,
    }
}

/// True when `angular.json` declares a target called `target` in any project.
fn angular_has_target(json: &serde_json::Value, target: &str) -> bool {
    json.get("projects")
        .and_then(|p| p.as_object())
        .is_some_and(|projects| {
            projects.values().any(|project| {
                ["architect", "targets"]
                    .iter()
                    .any(|key| project.get(key).and_then(|t| t.get(target)).is_some())
            })
        })
}

fn angular(root: &Path, pm: Pm) -> Vec<GatePlan> {
    let Some(json) = read(root, "angular.json").and_then(|t| serde_json::from_str(&t).ok()) else {
        return Vec::new();
    };
    let versions = vec!["node --version".to_owned(), pm.version_cmd()];
    let mut gates = Vec::new();
    if angular_has_target(&json, "lint") {
        gates.push(gate(
            "lint",
            format!("{} ng lint", pm.exec()),
            Kind::Lint,
            "angular.json",
            versions.clone(),
        ));
    }
    if angular_has_target(&json, "test") {
        gates.push(gate(
            "test",
            format!("{} ng test --watch=false", pm.exec()),
            Kind::Test,
            "angular.json",
            versions,
        ));
    }
    gates
}

fn node(root: &Path, pm: Pm) -> Vec<GatePlan> {
    let Some(json) =
        read(root, "package.json").and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return Vec::new();
    };
    let script = |name: &str| {
        json.get("scripts")
            .and_then(|s| s.get(name))
            .and_then(|s| s.as_str())
    };
    let versions = vec!["node --version".to_owned(), pm.version_cmd()];
    let mut gates = Vec::new();
    if script("lint").is_some() {
        gates.push(gate(
            "lint",
            pm.run("lint"),
            Kind::Lint,
            "package.json",
            versions.clone(),
        ));
    }
    for name in ["typecheck", "type-check"] {
        if script(name).is_some() {
            gates.push(gate(
                "typecheck",
                pm.run(name),
                Kind::Lint,
                "package.json",
                versions.clone(),
            ));
            break;
        }
    }
    if let Some(body) = script("test")
        && !body.contains("no test specified")
    {
        let mut test = gate("test", pm.test(), Kind::Test, "package.json", versions);
        test.env.push(("CI".to_owned(), "1".to_owned()));
        gates.push(test);
    }
    gates
}

fn rust(root: &Path) -> Vec<GatePlan> {
    if !root.join("Cargo.toml").is_file() {
        return Vec::new();
    }
    let rustc = vec!["rustc --version".to_owned()];
    let jobs = ("CARGO_BUILD_JOBS".to_owned(), "4".to_owned());
    let fmt = gate(
        "fmt",
        "cargo fmt --check".to_owned(),
        Kind::Fmt,
        "Cargo.toml",
        rustc.clone(),
    );
    let mut clippy = gate(
        "clippy",
        "cargo clippy --workspace --all-targets -- -D warnings".to_owned(),
        Kind::Lint,
        "Cargo.toml",
        vec![
            "rustc --version".to_owned(),
            "cargo clippy --version".to_owned(),
        ],
    );
    // A multi-threaded build: it takes what the machine can spare.
    clippy.weight = WeightSpec::Auto;
    "20m".clone_into(&mut clippy.timeout);
    clippy.env.push(jobs.clone());
    let mut test = gate(
        "test",
        "cargo test --workspace".to_owned(),
        Kind::Test,
        "Cargo.toml",
        rustc,
    );
    test.env.push(jobs);
    vec![fmt, clippy, test]
}

fn python(root: &Path) -> Vec<GatePlan> {
    let has_requirements = fs::read_dir(root).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("requirements") && name.ends_with(".txt")
        })
    });
    if !has_requirements
        && !["pyproject.toml", "setup.cfg"]
            .iter()
            .any(|m| root.join(m).is_file())
    {
        return Vec::new();
    }
    let bin = if root.join(".venv/bin").is_dir() {
        ".venv/bin/"
    } else {
        ""
    };
    let pyproject = read(root, "pyproject.toml").unwrap_or_default();
    let setup_cfg = read(root, "setup.cfg").unwrap_or_default();
    let interpreter = if bin.is_empty() {
        "python3 --version".to_owned()
    } else {
        format!("{bin}python --version")
    };
    let mut gates = Vec::new();
    if root.join("ruff.toml").is_file()
        || root.join(".ruff.toml").is_file()
        || pyproject.contains("[tool.ruff")
    {
        gates.push(gate(
            "ruff",
            format!("{bin}ruff check . && {bin}ruff format --check ."),
            Kind::Lint,
            "pyproject.toml",
            vec![interpreter.clone(), format!("{bin}ruff --version")],
        ));
    }
    if root.join("mypy.ini").is_file()
        || pyproject.contains("[tool.mypy")
        || setup_cfg.contains("[mypy")
    {
        gates.push(gate(
            "mypy",
            format!("{bin}mypy ."),
            Kind::Lint,
            "pyproject.toml",
            vec![interpreter.clone(), format!("{bin}mypy --version")],
        ));
    }
    gates.push(gate(
        "pytest",
        format!("{bin}pytest -q"),
        Kind::Test,
        "pyproject.toml",
        vec![interpreter, format!("{bin}pytest --version")],
    ));
    gates
}

fn go(root: &Path) -> Vec<GatePlan> {
    if !root.join("go.mod").is_file() {
        return Vec::new();
    }
    let version = vec!["go version".to_owned()];
    vec![
        gate(
            "vet",
            "go vet ./...".to_owned(),
            Kind::Lint,
            "go.mod",
            version.clone(),
        ),
        gate(
            "test",
            "go test -p 4 ./...".to_owned(),
            Kind::Test,
            "go.mod",
            version,
        ),
    ]
}

/// What the root looks like: the stacks found and the gates they need.
#[derive(Debug, Default)]
pub struct Stack {
    /// Names of the stacks found (`angular`, `node (pnpm)`, `rust`, ...).
    pub names: Vec<String>,
    pub gates: Vec<GatePlan>,
    /// Projects below the root that `init` does not configure.
    pub nested: Vec<String>,
}

/// Detects the stack at `root` (never below it).
pub fn detect(root: &Path) -> Stack {
    let mut stack = Stack::default();
    if let Some(pm) = package_manager(root) {
        let ng = angular(root, pm);
        if !ng.is_empty() {
            stack.names.push("angular".to_owned());
            stack.gates.extend(ng);
        }
        stack.names.push(format!("node ({})", pm.name()));
        stack.gates.extend(node(root, pm));
    }
    for (name, gates) in [
        ("rust", rust(root)),
        ("python", python(root)),
        ("go", go(root)),
    ] {
        if !gates.is_empty() {
            stack.names.push(name.to_owned());
            stack.gates.extend(gates);
        }
    }
    // A name found twice (Angular and Node both offer `lint`) keeps the first.
    let mut seen: Vec<String> = Vec::new();
    stack.gates.retain(|g| {
        let new = !seen.contains(&g.name);
        seen.push(g.name.clone());
        new
    });
    stack.nested = nested_projects(root);
    stack
}

/// Project files found one or two levels below the root.
fn nested_projects(root: &Path) -> Vec<String> {
    const FILES: [&str; 5] = [
        "package.json",
        "Cargo.toml",
        "go.mod",
        "pyproject.toml",
        "angular.json",
    ];
    const SKIP: [&str; 6] = ["node_modules", "target", "vendor", "dist", "build", "venv"];
    let mut found = Vec::new();
    let mut level = vec![root.to_path_buf()];
    for depth in 0..2 {
        let mut next = Vec::new();
        for dir in &level {
            let mut entries: Vec<_> = fs::read_dir(dir).into_iter().flatten().flatten().collect();
            entries.sort_by_key(fs::DirEntry::file_name);
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                if name.starts_with('.')
                    || SKIP.contains(&name.as_str())
                    || path.is_symlink()
                    || !path.is_dir()
                {
                    continue;
                }
                if let Some(file) = FILES.iter().find(|f| path.join(f).is_file()) {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    found.push(format!("{} ({file})", rel.display()));
                }
                if depth == 0 {
                    next.push(path);
                }
            }
        }
        level = next;
    }
    found.truncate(12);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn names_come_from_canonical_words() {
        assert_eq!(gate_name(&words(&["ng", "test"]), None), "test");
        assert_eq!(gate_name(&words(&["ng", "lint"]), None), "lint");
        assert_eq!(gate_name(&words(&["run", "typecheck"]), None), "typecheck");
        assert_eq!(gate_name(&words(&["cargo", "clippy"]), None), "clippy");
        assert_eq!(gate_name(&words(&["nx", "affected"]), None), "nx-affected");
        assert_eq!(gate_name(&words(&["jest"]), None), "jest");
        assert_eq!(
            gate_name(&words(&["ng", "e2e"]), Some("axe:authenticated")),
            "axe-authenticated"
        );
    }

    #[test]
    fn kinds_decide_weight_and_timeout() {
        assert_eq!(Kind::of(&["ng", "lint"]), Kind::Lint);
        assert_eq!(Kind::of(&["ng", "test"]), Kind::Test);
        assert_eq!(Kind::of(&["cargo", "fmt"]), Kind::Fmt);
        assert_eq!(Kind::of(&["run", "build"]), Kind::Build);
        assert_eq!(Kind::Lint.weight(), WeightSpec::Fixed(2));
        assert_eq!(Kind::Fmt.weight(), WeightSpec::Fixed(1));
        assert_eq!(Kind::Test.weight(), WeightSpec::Auto);
        assert_eq!(Kind::Lint.timeout(), "10m");
        assert_eq!(Kind::Test.timeout(), "30m");
    }

    #[test]
    fn sanitized_names_are_valid() {
        assert_eq!(sanitize_name("Type Check!"), "type-check");
        assert_eq!(sanitize_name("--"), "gate");
    }
}
