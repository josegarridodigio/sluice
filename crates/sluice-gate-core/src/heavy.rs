//! The heavy-command list and the command-word equivalences used to match it.
//!
//! Shared by the plugin guard (`sluice guard`) and, later, the shell shims.
//! Matching works on *canonical words*: the words a command runs, with the
//! package-manager spellings of the same thing folded together (`pnpm build`,
//! `npm run build` and `yarn build` all become `run build`). Canonical words
//! are used only for matching, never to build a command. Design:
//! `kb/sluice/design/command-control.md` sections 4.3 and 4.4.

use std::path::Path;

use crate::config::AgentConfig;
use crate::pkgscripts::{self, MAX_DEPTH, ScriptSource, Scripts};

/// Built-in heavy-command list, in canonical words (`[agent] heavy_commands`
/// replaces it, `heavy_commands_extra` extends it).
pub const DEFAULT_HEAVY_COMMANDS: &[&str] = &[
    "docker build",
    "docker buildx build",
    "docker compose build",
    "run build",
    "cargo build",
    "cargo test",
    "cargo clippy",
    "cargo nextest",
    "go build",
    "go test",
    "make",
    "gradle",
    "mvn",
    "tsc",
    "vite build",
    "webpack",
    "next build",
    "pytest",
    "jest",
    "vitest",
    "ng build",
    "ng test",
    "ng lint",
    "ng e2e",
    "nx run",
    "nx run-many",
    "nx affected",
    "nx build",
    "nx test",
    "nx lint",
    "nx e2e",
    "turbo run",
    "turbo build",
    "turbo test",
    "turbo lint",
    "playwright test",
    "cypress run",
    "react-scripts build",
    "react-scripts test",
    "astro build",
    "nuxt build",
    "dotnet build",
    "dotnet test",
    "bazel build",
    "bazel test",
    "ninja",
    "cmake --build",
];

/// Subcommands that start a dev server: never heavy, whatever the list says
/// (they never end, so queueing them would hold a slot forever).
const SERVER_COMMANDS: &[(&str, &[&str])] = &[
    ("ng", &["serve", "s"]),
    ("nx", &["serve"]),
    ("next", &["dev"]),
    ("astro", &["dev", "preview"]),
    ("nuxt", &["dev", "preview"]),
    ("react-scripts", &["start"]),
    ("vite", &["dev", "serve", "preview"]),
];

/// Tools whose `-w` is `--watch`.
const SHORT_WATCH_TOOLS: &[&str] = &["tsc", "jest", "vitest", "webpack", "vite"];

/// True for a command that never ends by itself: a dev server (`ng serve`,
/// `nx serve`, `next dev`, bare `vite`) or a watch mode (`--watch`,
/// `--watch=true`, `--watchAll`; `--watch=false` is not a watch, and `-w`
/// only counts for the tools where it means `--watch`).
pub fn is_never_heavy<S: AsRef<str>>(canonical: &[S]) -> bool {
    let Some(first) = canonical.first().map(AsRef::as_ref) else {
        return false;
    };
    let second = canonical.get(1).map(AsRef::as_ref);
    if first == "vite" && second.is_none_or(|w| w.starts_with('-')) {
        return true;
    }
    if SERVER_COMMANDS
        .iter()
        .any(|(tool, subs)| *tool == first && second.is_some_and(|s| subs.contains(&s)))
    {
        return true;
    }
    canonical.iter().skip(1).map(AsRef::as_ref).any(|w| {
        matches!(
            w,
            "--watch" | "--watch=true" | "--watchAll" | "--watchAll=true"
        ) || (w == "-w" && SHORT_WATCH_TOOLS.contains(&first))
    })
}

/// Package-manager subcommands that are builtins, not package scripts
/// (`pnpm install` is not `pnpm run install`).
const PM_BUILTINS: &[&str] = &[
    "add",
    "audit",
    "bin",
    "cache",
    "check",
    "config",
    "create",
    "dedupe",
    "dlx",
    "exec",
    "import",
    "info",
    "init",
    "install",
    "i",
    "link",
    "list",
    "ls",
    "outdated",
    "pack",
    "prune",
    "publish",
    "rebuild",
    "remove",
    "rm",
    "root",
    "store",
    "unlink",
    "up",
    "update",
    "upgrade",
    "why",
    "workspace",
    "workspaces",
];

/// Directory options of package managers: kept in what runs, skipped when
/// looking for the subcommand.
const PM_DIR_OPTIONS: &[&str] = &["-C", "--dir", "--prefix", "--cwd"];

/// Canonical words of a command given as its word values (unquoted).
///
/// Equivalences: `npm run X`, `npm run-script X`, `pnpm run X`, `yarn run X`,
/// `bun run X` → `run X`; `npm test|start|stop|restart` → `run <that>`;
/// `pnpm X` / `yarn X` → `run X` unless X is a package-manager builtin;
/// `npx [-y|--yes] [--] X`, `npm exec|x [-y|--yes] [--] X`, `pnpm exec|dlx X`,
/// `yarn exec|dlx X`, `bunx X` → `X`;
/// `pnpm -C d|--dir d`, `npm --prefix d`, `yarn --cwd d` are skipped when
/// finding the subcommand; `python -m pytest`, `python3 -m pytest` →
/// `pytest`; `./gradlew` → `gradle`, `./mvnw` → `mvn`; `docker-compose` →
/// `docker compose`.
pub fn canonical_words<S: AsRef<str>>(words: &[S]) -> Vec<String> {
    let mut rest: Vec<&str> = words.iter().map(AsRef::as_ref).collect();
    // Each round strips one launcher (`npx`, `pnpm exec`); bounded so a
    // pathological `npx npx npx ...` cannot loop long.
    for _ in 0..4 {
        match unwrap_launcher(&rest) {
            Some(inner) => rest = inner,
            None => break,
        }
    }
    let Some((&first, tail)) = rest.split_first() else {
        return Vec::new();
    };
    let owned = |ws: &[&str]| ws.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
    match first {
        "python" | "python3" if tail.len() >= 2 && tail[0] == "-m" && tail[1] == "pytest" => {
            let mut out = vec!["pytest".to_owned()];
            out.extend(owned(&tail[2..]));
            out
        }
        "./gradlew" => prepend("gradle", &owned(tail)),
        "./mvnw" => prepend("mvn", &owned(tail)),
        "docker-compose" => {
            let mut out = vec!["docker".to_owned(), "compose".to_owned()];
            out.extend(owned(tail));
            out
        }
        "npm" | "pnpm" | "yarn" | "bun" => package_manager(first, tail),
        _ => owned(&rest),
    }
}

fn prepend(word: &str, rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len() + 1);
    out.push(word.to_owned());
    out.extend_from_slice(rest);
    out
}

/// Index of the subcommand after leading directory options, or `None` when
/// an unknown option comes first (no equivalence is applied then).
fn subcommand_index(tail: &[&str]) -> Option<usize> {
    let mut i = 0;
    while let Some(word) = tail.get(i) {
        if PM_DIR_OPTIONS.contains(word) {
            i += 2;
        } else if PM_DIR_OPTIONS
            .iter()
            .any(|o| o.starts_with("--") && word.starts_with(&format!("{o}=")))
        {
            i += 1;
        } else if word.starts_with('-') {
            return None;
        } else {
            return Some(i);
        }
    }
    None
}

/// The command after `-y`/`--yes` options and an optional `--`; `None` when
/// nothing follows or another option comes first.
fn exec_inner<'a>(args: &[&'a str]) -> Option<Vec<&'a str>> {
    let skip = args
        .iter()
        .take_while(|w| matches!(**w, "-y" | "--yes"))
        .count();
    let mut inner = &args[skip..];
    if inner.first() == Some(&"--") {
        inner = &inner[1..];
    }
    (!inner.is_empty() && !inner[0].starts_with('-')).then(|| inner.to_vec())
}

/// `npx X`, `bunx X`, `npm exec X`, `npm x X`, `pnpm exec|dlx X`,
/// `yarn exec|dlx X` -> the words of `X` (`-y`/`--yes` and `--` skipped).
fn unwrap_launcher<'a>(words: &[&'a str]) -> Option<Vec<&'a str>> {
    let (&first, tail) = words.split_first()?;
    match first {
        "npx" | "bunx" => exec_inner(tail),
        "npm" => {
            let i = subcommand_index(tail)?;
            if matches!(tail[i], "exec" | "x") {
                exec_inner(&tail[i + 1..])
            } else {
                None
            }
        }
        "pnpm" | "yarn" => {
            let i = subcommand_index(tail)?;
            if !matches!(tail[i], "exec" | "dlx") {
                return None;
            }
            let after = &tail[i + 1..];
            (!after.is_empty() && !after[0].starts_with('-')).then(|| after.to_vec())
        }
        _ => None,
    }
}

fn package_manager(pm: &str, tail: &[&str]) -> Vec<String> {
    let unchanged = || {
        std::iter::once(pm)
            .chain(tail.iter().copied())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let Some(i) = subcommand_index(tail) else {
        return unchanged();
    };
    let sub = tail[i];
    let after = &tail[i + 1..];
    let script = |name: &str, args: &[&str]| {
        let mut out = vec!["run".to_owned(), name.to_owned()];
        out.extend(args.iter().map(|w| (*w).to_owned()));
        out
    };
    match (pm, sub) {
        (_, "run") | ("npm", "run-script") => match after.split_first() {
            Some((name, args)) if !name.starts_with('-') => script(name, args),
            _ => unchanged(),
        },
        ("npm", "test" | "start" | "stop" | "restart") => script(sub, after),
        ("pnpm" | "yarn", _) if !PM_BUILTINS.contains(&sub) => script(sub, after),
        _ => unchanged(),
    }
}

/// Why a command is heavy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The entry of the list that matched (the command's own, or the one a
    /// package script runs).
    pub entry: Vec<String>,
    /// The package script that led to the entry (`npm run axe` -> `axe`),
    /// `None` when the typed words matched directly.
    pub script: Option<String>,
}

/// Matches canonical words against a heavy-command list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeavyMatcher {
    entries: Vec<Vec<String>>,
}

impl Default for HeavyMatcher {
    fn default() -> Self {
        Self::new(DEFAULT_HEAVY_COMMANDS.iter().copied())
    }
}

impl HeavyMatcher {
    /// A matcher over these entries (each canonicalised; empty ones dropped).
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let entries = entries
            .into_iter()
            .map(|e| canonical_words(&e.as_ref().split_whitespace().collect::<Vec<_>>()))
            .filter(|e| !e.is_empty())
            .collect();
        Self { entries }
    }

    /// The list `[agent]` selects: `heavy_commands` (or the default list)
    /// followed by `heavy_commands_extra`.
    pub fn from_config(agent: &AgentConfig) -> Self {
        let base: Vec<String> = agent.heavy_commands.as_ref().map_or_else(
            || {
                DEFAULT_HEAVY_COMMANDS
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect()
            },
            Clone::clone,
        );
        Self::new(base.iter().chain(&agent.heavy_commands_extra))
    }

    /// The entry `canonical` starts with (whole words), if any; the longest wins.
    pub fn matching_entry<S: AsRef<str>>(&self, canonical: &[S]) -> Option<&[String]> {
        self.entries
            .iter()
            .filter(|e| {
                e.len() <= canonical.len() && e.iter().zip(canonical).all(|(a, b)| a == b.as_ref())
            })
            .max_by_key(|e| e.len())
            .map(Vec::as_slice)
    }

    /// True when `canonical` starts with an entry of the list and is not a
    /// dev server or watch mode. Words only: package scripts are not read
    /// (see [`Self::classify`]).
    pub fn matches<S: AsRef<str>>(&self, canonical: &[S]) -> bool {
        self.word_match(canonical).is_some()
    }

    fn word_match<S: AsRef<str>>(&self, canonical: &[S]) -> Option<&[String]> {
        if is_never_heavy(canonical) {
            None
        } else {
            self.matching_entry(canonical)
        }
    }

    /// Classifies `canonical` run in `dir`: heavy when its words match the
    /// list, or when it is `run <name>` and the script `<name>` of the
    /// nearest `package.json` (read through `scripts`) runs, in any of its
    /// `&&`, `||`, `;`, `|` pieces, something heavy (nested `npm run x`
    /// scripts are followed up to [`MAX_DEPTH`], cycles end the walk).
    /// Words are matched first, so a script is read only for `run <name>`
    /// that the list does not already know.
    pub fn classify<S: AsRef<str>>(
        &self,
        canonical: &[S],
        dir: &Path,
        scripts: &dyn ScriptSource,
    ) -> Option<Match> {
        if let Some(entry) = self.word_match(canonical) {
            return Some(Match {
                entry: entry.to_vec(),
                script: None,
            });
        }
        let name = script_name(canonical)?;
        let found = Scripts::find(scripts, dir)?;
        let entry = self.script_entry(&found, name, &mut Vec::new())?;
        Some(Match {
            entry,
            script: Some(name.to_owned()),
        })
    }

    fn script_entry(
        &self,
        scripts: &Scripts,
        name: &str,
        stack: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        if stack.len() >= MAX_DEPTH || stack.iter().any(|s| s == name) {
            return None;
        }
        let body = scripts.body(name)?;
        stack.push(name.to_owned());
        let found = pkgscripts::pieces(body).iter().find_map(|piece| {
            let words = canonical_words(piece);
            if let Some(entry) = self.word_match(&words) {
                return Some(entry.to_vec());
            }
            let inner = script_name(&words)?;
            self.script_entry(scripts, inner, stack)
        });
        stack.pop();
        found
    }

    /// Executable names a shell shim must intercept so that every entry of
    /// the list can be caught: the first word of each entry, with `run`
    /// (package scripts) expanded to `npm pnpm yarn bun`, `gradle` and `mvn`
    /// also as their wrappers `gradlew` and `mvnw`, plus `npx` (which can
    /// launch any entry). Names that are not plain file names are dropped.
    /// Sorted, without duplicates.
    pub fn shim_names(&self) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        for entry in &self.entries {
            let first = entry[0].as_str();
            let expanded: &[&str] = match first {
                "run" => &["npm", "pnpm", "yarn", "bun"],
                "gradle" => &["gradle", "gradlew"],
                "mvn" => &["mvn", "mvnw"],
                other => &[other][..],
            };
            for name in expanded {
                if is_shim_name(name) {
                    names.insert((*name).to_owned());
                }
            }
        }
        if !names.is_empty() {
            names.insert("npx".to_owned());
        }
        names.into_iter().collect()
    }
}

/// `<name>` of canonical words `run <name> ...`.
fn script_name<S: AsRef<str>>(canonical: &[S]) -> Option<&str> {
    match canonical {
        [run, name, ..] if run.as_ref() == "run" && !name.as_ref().starts_with('-') => {
            Some(name.as_ref())
        }
        _ => None,
    }
}

/// True for a name a shim file can carry: ASCII letters, digits and `._-`,
/// not starting with `.` or `-`, at most 64 bytes.
pub fn is_shim_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Canonical words of `name args...` as a shim sees them: the wrapper
/// names `gradlew` and `mvnw` count as `./gradlew` and `./mvnw`.
pub fn shim_canonical_words<S: AsRef<str>>(name: &str, args: &[S]) -> Vec<String> {
    let first = match name {
        "gradlew" => "./gradlew",
        "mvnw" => "./mvnw",
        other => other,
    };
    let words: Vec<&str> = std::iter::once(first)
        .chain(args.iter().map(AsRef::as_ref))
        .collect();
    canonical_words(&words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(cmd: &str) -> String {
        canonical_words(&cmd.split_whitespace().collect::<Vec<_>>()).join(" ")
    }

    #[test]
    fn shim_names_cover_the_default_list() {
        assert_eq!(
            HeavyMatcher::default().shim_names(),
            [
                "astro",
                "bazel",
                "bun",
                "cargo",
                "cmake",
                "cypress",
                "docker",
                "dotnet",
                "go",
                "gradle",
                "gradlew",
                "jest",
                "make",
                "mvn",
                "mvnw",
                "next",
                "ng",
                "ninja",
                "npm",
                "npx",
                "nuxt",
                "nx",
                "playwright",
                "pnpm",
                "pytest",
                "react-scripts",
                "tsc",
                "turbo",
                "vite",
                "vitest",
                "webpack",
                "yarn",
            ]
        );
        assert_eq!(
            HeavyMatcher::new(["make", "./build.sh x", "bazel build"]).shim_names(),
            ["bazel", "make", "npx"]
        );
        assert_eq!(
            HeavyMatcher::new(Vec::<String>::new()).shim_names(),
            Vec::<String>::new()
        );
        assert!(is_shim_name("docker-compose") && !is_shim_name("../x") && !is_shim_name(""));
    }

    #[test]
    fn shim_canonical_words_fold_wrappers() {
        let m = HeavyMatcher::default();
        assert_eq!(
            shim_canonical_words("gradlew", &["build"]),
            ["gradle", "build"]
        );
        assert_eq!(shim_canonical_words("mvnw", &["-q"]), ["mvn", "-q"]);
        assert!(m.matches(&shim_canonical_words("pnpm", &["build"])));
        assert!(!m.matches(&shim_canonical_words("pnpm", &["install"])));
        assert!(!m.matches(&shim_canonical_words("docker", &["ps"])));
        assert!(m.matches(&shim_canonical_words("npx", &["tsc", "--noEmit"])));
    }

    #[test]
    fn equivalences() {
        for (input, want) in [
            ("npm run build", "run build"),
            ("npm run-script build --x", "run build --x"),
            ("pnpm run build", "run build"),
            ("pnpm build", "run build"),
            ("yarn build", "run build"),
            ("yarn run build", "run build"),
            ("bun run build", "run build"),
            ("npm test", "run test"),
            ("npm start", "run start"),
            ("npm stop", "run stop"),
            ("npm restart", "run restart"),
            ("npx tsc --noEmit", "tsc --noEmit"),
            ("npx -y vitest run", "vitest run"),
            ("npx --yes jest", "jest"),
            ("bunx tsc", "tsc"),
            (
                "npm exec playwright test --config=x",
                "playwright test --config=x",
            ),
            ("npm x tsc --noEmit", "tsc --noEmit"),
            ("npm exec -- vitest run", "vitest run"),
            ("npm exec -y vitest run", "vitest run"),
            ("npm exec --yes -- vitest run", "vitest run"),
            ("npx -- tsc", "tsc"),
            ("pnpm dlx vitest run", "vitest run"),
            ("yarn dlx vitest run", "vitest run"),
            ("npm --prefix web exec tsc", "tsc"),
            ("pnpm exec tsc", "tsc"),
            ("yarn exec tsc", "tsc"),
            ("pnpm -C web build", "run build"),
            ("pnpm --dir web build", "run build"),
            ("pnpm --dir=web build", "run build"),
            ("npm --prefix web run build", "run build"),
            ("yarn --cwd web build", "run build"),
            ("python -m pytest -x", "pytest -x"),
            ("python3 -m pytest", "pytest"),
            ("./gradlew build", "gradle build"),
            ("./mvnw package", "mvn package"),
            ("docker-compose build web", "docker compose build web"),
            ("pnpm -C web exec tsc", "tsc"),
        ] {
            assert_eq!(canon(input), want, "{input}");
        }
    }

    #[test]
    fn non_equivalences() {
        for input in [
            "pnpm install",
            "yarn add x",
            "pnpm i",
            "yarn",
            "npm install",
            "npm run",
            "npm --silent run build",
            "npx",
            "npx -p x",
            "npm exec --help",
            "npm exec",
            "npm exec -p x y",
            "npm x",
            "pnpm dlx",
            "python -m http.server",
            "bun build",
            "cargo build",
            "make",
            "gradlew build",
        ] {
            assert_eq!(canon(input), input, "{input}");
        }
    }

    #[test]
    fn default_matcher() {
        let m = HeavyMatcher::default();
        for yes in [
            "make",
            "make -j8 all",
            "cargo build --release",
            "run build",
            "docker build -t x .",
            "docker compose build",
            "pytest -x",
            "vitest run",
            "gradle build",
        ] {
            let words: Vec<&str> = yes.split_whitespace().collect();
            assert!(m.matches(&words), "{yes}");
        }
        for no in [
            "makefile",
            "cargo",
            "cargo fmt",
            "cargo builder",
            "docker ps",
            "run lint",
            "go vet",
            "",
            "ls",
        ] {
            let words: Vec<&str> = no.split_whitespace().collect();
            assert!(!m.matches(&words), "{no}");
        }
    }

    fn words(cmd: &str) -> Vec<String> {
        canonical_words(&cmd.split_whitespace().collect::<Vec<_>>())
    }

    #[test]
    fn npm_exec_family_is_heavy_like_npx() {
        let m = HeavyMatcher::default();
        for yes in [
            "npm exec playwright test --config=x",
            "npm x playwright test",
            "npm exec -- vitest run",
            "npm exec -y vitest run",
            "npx -- vitest run",
            "pnpm dlx vitest run",
            "yarn dlx vitest run",
        ] {
            assert!(m.matches(&words(yes)), "{yes}");
        }
        for no in ["npm exec --help", "npm install", "npm exec", "pnpm dlx"] {
            assert!(!m.matches(&words(no)), "{no}");
        }
    }

    #[test]
    fn new_builtin_entries_match() {
        let m = HeavyMatcher::default();
        for yes in [
            "npx ng test --watch=false",
            "npx ng build -c x",
            "ng lint",
            "nx run-many -t build",
            "nx affected -t test",
            "turbo run build",
            "pnpm exec playwright test",
            "cypress run",
            "react-scripts build",
            "astro build",
            "nuxt build",
            "dotnet test",
            "bazel build //...",
            "ninja -C out",
            "cmake --build out",
        ] {
            assert!(m.matches(&words(yes)), "{yes}");
        }
        for no in [
            "ng serve",
            "ng s",
            "npx ng serve",
            "nx serve app",
            "next dev",
            "vite",
            "vite --port 80",
            "vite preview",
            "ng test --watch",
            "ng test --watch=true",
            "vitest --watch",
            "vitest -w",
            "jest --watchAll",
            "tsc -w",
            "react-scripts start",
            "ng version",
            "cmake .",
            "dotnet run",
        ] {
            assert!(!m.matches(&words(no)), "{no}");
        }
        // `-w` is only a watch flag for the tools where it means that.
        assert!(m.matches(&words("cargo test -w")));
        assert!(m.matches(&words("vite build --watch=false")));
        assert!(m.matches(&words("ng test --watch=false")));
    }

    struct Files(Vec<(&'static str, &'static str)>);

    impl ScriptSource for Files {
        fn package_json(&self, dir: &Path) -> Option<String> {
            self.0
                .iter()
                .find(|(d, _)| Path::new(d) == dir)
                .map(|(_, t)| (*t).to_owned())
        }

        fn is_repo_root(&self, dir: &Path) -> bool {
            dir == Path::new("/r")
        }
    }

    const PKG: &str = r#"{"scripts":{
        "test":"ng test",
        "axe":"npm run axe:a",
        "axe:a":"playwright test",
        "start":"ng serve",
        "lint":"eslint .",
        "ci":"npm run lint && CI=1 npm test",
        "all":"pnpm lint; pnpm ci",
        "a":"npm run b", "b":"npm run c", "c":"npm run d", "d":"npm run e", "e":"ng build",
        "loop1":"npm run loop2", "loop2":"npm run loop1 && echo x",
        "watchy":"ng test --watch",
        "both":"ng serve & ng build",
        "fine":"ng test --watch=false"
    }}"#;

    fn classify(files: &Files, dir: &str, cmd: &str) -> Option<Match> {
        HeavyMatcher::default().classify(&words(cmd), Path::new(dir), files)
    }

    #[test]
    fn classify_follows_package_scripts() {
        let files = Files(vec![("/r", PKG)]);
        let heavy = |cmd: &str| classify(&files, "/r", cmd).is_some();
        for yes in [
            "npm test",
            "npm run axe",
            "pnpm axe",
            "yarn run axe:a",
            "npm run ci",
            "pnpm all",
            "npm run both",
            "npm run fine",
            "npx ng test --watch=false",
            // Four scripts in a chain are followed (b, c, d, e) ...
            "npm run b",
            "npm run d",
        ] {
            assert!(heavy(yes), "{yes}");
        }
        for no in [
            "npm start",
            "npm run lint",
            "npm run watchy",
            "npm run loop1",
            "npm run missing",
            "npm install",
            // ... a fifth is not.
            "npm run a",
            "ng test --watch",
            "npx ng serve",
            "npm --version",
        ] {
            assert!(!heavy(no), "{no}");
        }
        let m = classify(&files, "/r", "npm run axe").unwrap();
        assert_eq!(m.script.as_deref(), Some("axe"));
        assert_eq!(m.entry, ["playwright", "test"]);
        let direct = classify(&files, "/r", "ng build").unwrap();
        assert_eq!(direct.script, None);
        // Found from a subdirectory; not found past the git root, in an
        // invalid file or when there is none.
        assert!(classify(&files, "/r/web/src", "npm test").is_some());
        assert!(classify(&files, "/elsewhere", "npm test").is_none());
        let bad = Files(vec![("/r", "{")]);
        assert!(classify(&bad, "/r", "npm test").is_none());
        // Words that the list knows need no file at all.
        assert!(classify(&bad, "/r", "npm run build").is_some());
        // `matches` never reads scripts.
        assert!(!HeavyMatcher::default().matches(&words("npm test")));
    }

    #[test]
    fn config_lists() {
        let replaced = HeavyMatcher::from_config(&AgentConfig {
            heavy_commands: Some(vec!["make".into(), "npm run build".into()]),
            heavy_commands_extra: vec!["bazel build".into(), "  ".into()],
            exec_weight: 0,
        });
        assert!(replaced.matches(&["make"]));
        assert!(replaced.matches(&["run", "build"]));
        assert!(replaced.matches(&["bazel", "build", "//..."]));
        assert!(!replaced.matches(&["cargo", "build"]));
        let extended = HeavyMatcher::from_config(&AgentConfig {
            heavy_commands: None,
            heavy_commands_extra: vec!["bazel build".into()],
            exec_weight: 0,
        });
        assert!(extended.matches(&["cargo", "build"]));
        assert!(extended.matches(&["bazel", "build"]));
        let empty = HeavyMatcher::from_config(&AgentConfig {
            heavy_commands: Some(Vec::new()),
            ..AgentConfig::default()
        });
        assert!(!empty.matches(&["make"]));
        assert_eq!(
            HeavyMatcher::default().matching_entry(&["docker", "compose", "build"]),
            Some(
                &[
                    "docker".to_owned(),
                    "compose".to_owned(),
                    "build".to_owned()
                ][..]
            )
        );
    }
}
