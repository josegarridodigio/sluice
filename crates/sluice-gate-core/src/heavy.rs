//! The heavy-command list and the command-word equivalences used to match it.
//!
//! Shared by the plugin guard (`sluice guard`) and, later, the shell shims.
//! Matching works on *canonical words*: the words a command runs, with the
//! package-manager spellings of the same thing folded together (`pnpm build`,
//! `npm run build` and `yarn build` all become `run build`). Canonical words
//! are used only for matching, never to build a command. Design:
//! `kb/sluice/design/command-control.md` sections 4.3 and 4.4.

use crate::config::AgentConfig;

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
];

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
/// `npx [-y|--yes] X`, `pnpm exec X`, `yarn exec X`, `bunx X` → `X`;
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

/// `npx X`, `bunx X`, `pnpm exec X`, `yarn exec X` → the words of `X`.
fn unwrap_launcher<'a>(words: &[&'a str]) -> Option<Vec<&'a str>> {
    let (&first, tail) = words.split_first()?;
    match first {
        "npx" | "bunx" => {
            let skip = tail
                .iter()
                .take_while(|w| matches!(**w, "-y" | "--yes"))
                .count();
            let inner = &tail[skip..];
            (!inner.is_empty() && !inner[0].starts_with('-')).then(|| inner.to_vec())
        }
        "pnpm" | "yarn" => {
            let i = subcommand_index(tail)?;
            (tail[i] == "exec" && tail.len() > i + 1 && !tail[i + 1].starts_with('-'))
                .then(|| tail[i + 1..].to_vec())
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

    /// True when `canonical` starts with an entry of the list.
    pub fn matches<S: AsRef<str>>(&self, canonical: &[S]) -> bool {
        self.matching_entry(canonical).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(cmd: &str) -> String {
        canonical_words(&cmd.split_whitespace().collect::<Vec<_>>()).join(" ")
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
