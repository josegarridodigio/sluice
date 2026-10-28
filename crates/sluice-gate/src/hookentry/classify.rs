//! Which units may change files or the index (`Mutating`) and which only
//! look (`Check`).
//!
//! The rule is a whitelist: a unit is a `Check` only when it is recognised as
//! read-only (a test runner, a linter without a fix flag, a formatter in
//! check mode, a package script whose every piece is one of those). Unknown
//! programs, `git`, tools that read the staged state and anything that looks
//! like it writes are `Mutating`: they always run and are never cached.

use std::path::Path;

use sluice_gate_core::pkgscripts::{self, FsScripts, MAX_DEPTH, ScriptSource, Scripts};

use super::units::Kind;

/// Flags that make most tools write files, or read the staged state.
const WRITE_FLAGS: &[&str] = &[
    "--fix",
    "--write",
    "-w",
    "--apply",
    "--apply-unsafe",
    "--staged",
    "--cached",
    "--in-place",
    "-i",
    "--update",
    "--updateSnapshot",
    "--update-snapshots",
    "-u",
    "--fix-type",
];

/// Tools that only read, whatever their arguments (flags above aside).
const CHECK_TOOLS: &[&str] = &[
    "tsc",
    "vue-tsc",
    "svelte-check",
    "eslint",
    "oxlint",
    "biome",
    "stylelint",
    "markdownlint",
    "markdownlint-cli2",
    "cspell",
    "knip",
    "depcheck",
    "madge",
    "jscpd",
    "tslint",
    "jest",
    "vitest",
    "mocha",
    "ava",
    "tap",
    "jasmine",
    "karma",
    "playwright",
    "cypress",
    "prettier",
    "gofmt",
    "pytest",
    "mypy",
    "pyright",
    "flake8",
    "pylint",
    "bandit",
    "tox",
    "nox",
    "mvn",
    "gradle",
    "gradlew",
    "rustc",
    "shellcheck",
    "hadolint",
    "actionlint",
    "yamllint",
    "ng",
    "nx",
    "turbo",
    "vite",
    "webpack",
    "rollup",
    "esbuild",
    "tsup",
];

/// The package managers whose run-script forms are looked through.
const PACKAGE_MANAGERS: &[&str] = &["npm", "pnpm", "yarn", "bun"];

/// Package manager subcommands that change the tree or the environment.
const PM_WRITING: &[&str] = &[
    "install",
    "i",
    "ci",
    "add",
    "remove",
    "rm",
    "uninstall",
    "update",
    "up",
    "upgrade",
    "link",
    "publish",
    "version",
    "dedupe",
    "prune",
    "rebuild",
    "init",
    "create",
    "pack",
    "unlink",
    "patch",
];

/// Package manager subcommands that run another program.
const PM_EXEC: &[&str] = &["exec", "dlx", "x"];

/// Launchers that run the program after their flags.
const LAUNCHERS: &[&str] = &["npx", "bunx", "pnpx"];

fn base(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// True when `argv` has any of `flags` (also as `--flag=value`).
fn has_flag(argv: &[String], flags: &[&str]) -> bool {
    argv.iter().any(|a| {
        let name = a.split_once('=').map_or(a.as_str(), |(n, _)| n);
        flags.contains(&name)
    })
}

/// Classifies one command (`argv` after quote removal) run from `root`.
pub fn classify(argv: &[String], root: &Path) -> Kind {
    classify_with(argv, root, &FsScripts, 0)
}

fn classify_with(argv: &[String], root: &Path, src: &dyn ScriptSource, depth: usize) -> Kind {
    let Some(first) = argv.first() else {
        return Kind::Mutating;
    };
    if has_flag(argv, WRITE_FLAGS) {
        return Kind::Mutating;
    }
    let program = base(first);
    if LAUNCHERS.contains(&program) {
        return after_launcher(&argv[1..], root, src, depth);
    }
    if PACKAGE_MANAGERS.contains(&program) {
        return package_manager(program, &argv[1..], root, src, depth);
    }
    known_tool(program, &argv[1..])
}

/// `npx [flags] program args`: the program decides.
fn after_launcher(rest: &[String], root: &Path, src: &dyn ScriptSource, depth: usize) -> Kind {
    let inner: Vec<String> = rest
        .iter()
        .skip_while(|a| a.starts_with('-'))
        .cloned()
        .collect();
    // `-p pkg` and `--package pkg` take a value: not worth guessing.
    if rest.iter().any(|a| a == "-p" || a == "--package") {
        return Kind::Mutating;
    }
    classify_with(&inner, root, src, depth + 1)
}

/// `npm|pnpm|yarn|bun <sub> ...`.
fn package_manager(
    program: &str,
    rest: &[String],
    root: &Path,
    src: &dyn ScriptSource,
    depth: usize,
) -> Kind {
    let Some(sub) = rest.first().map(String::as_str) else {
        return Kind::Mutating;
    };
    if PM_WRITING.contains(&sub) && !(sub == "version" && rest.len() == 1) {
        return Kind::Mutating;
    }
    if PM_EXEC.contains(&sub) || (program == "bun" && sub == "x") {
        let inner: Vec<String> = rest[1..]
            .iter()
            .skip_while(|a| a.starts_with('-'))
            .cloned()
            .collect();
        return classify_with(&inner, root, src, depth + 1);
    }
    // `audit` answers from an advisory database on the network, not from the
    // tree: never cacheable, so it is not a check (and not a script name).
    if sub == "audit" {
        return Kind::Mutating;
    }
    let named = if matches!(sub, "run" | "run-script") {
        rest.get(1)
    } else {
        Some(&rest[0])
    };
    named.map_or(Kind::Mutating, |name| script(name, root, src, depth))
}

/// `npm run <name>`: the script's body, `pre<name>` and `post<name>` included
/// (npm runs them implicitly), classified piece by piece.
fn script(name: &str, root: &Path, src: &dyn ScriptSource, depth: usize) -> Kind {
    if depth >= MAX_DEPTH {
        return Kind::Mutating;
    }
    let lower = name.to_ascii_lowercase();
    if ["fix", "format", "fmt", "write", "stage", "generate"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return Kind::Mutating;
    }
    let Some(scripts) = Scripts::find(src, root) else {
        return Kind::Mutating;
    };
    let Some(body) = scripts.body(name) else {
        return Kind::Mutating;
    };
    let mut bodies = vec![body];
    bodies.extend(scripts.body(&format!("pre{name}")));
    bodies.extend(scripts.body(&format!("post{name}")));
    if bodies.iter().all(|b| body_is_check(b, root, src, depth)) {
        Kind::Check
    } else {
        Kind::Mutating
    }
}

fn body_is_check(body: &str, root: &Path, src: &dyn ScriptSource, depth: usize) -> bool {
    if body.contains(['>', '<', '`', '$', '(']) {
        return false;
    }
    let pieces = pkgscripts::pieces(body);
    !pieces.is_empty()
        && pieces
            .iter()
            .all(|p| classify_with(p, root, src, depth + 1) == Kind::Check)
}

/// A program run directly.
fn known_tool(program: &str, args: &[String]) -> Kind {
    let sub = args.first().map(String::as_str);
    let checking = match program {
        "cargo" => match sub {
            Some("fmt") => has_flag(args, &["--check"]),
            Some(s) => [
                "test", "check", "clippy", "build", "doc", "bench", "nextest", "deny", "audit",
                "machete",
            ]
            .contains(&s),
            None => false,
        },
        "go" => matches!(sub, Some("test" | "vet" | "build")),
        "black" => has_flag(args, &["--check", "--diff"]),
        "isort" => has_flag(args, &["--check", "--check-only", "-c", "--diff"]),
        "ruff" => ruff_checks(args),
        "dotnet" => match sub {
            Some("format") => has_flag(args, &["--verify-no-changes"]),
            Some(s) => ["test", "build"].contains(&s),
            None => false,
        },
        "terraform" => match sub {
            Some("fmt") => has_flag(args, &["-check"]),
            Some(s) => ["validate", "plan"].contains(&s),
            None => false,
        },
        _ => CHECK_TOOLS.contains(&program),
    };
    if checking {
        Kind::Check
    } else {
        Kind::Mutating
    }
}

/// `ruff check` (the default) and `ruff format --check`.
fn ruff_checks(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("format") => has_flag(args, &["--check", "--diff"]),
        Some("check") | None => true,
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_gate_core::pkgscripts::Scripts;
    use std::path::PathBuf;

    struct Fake(&'static str);

    impl ScriptSource for Fake {
        fn package_json(&self, dir: &Path) -> Option<String> {
            (dir == Path::new("/r")).then(|| self.0.to_owned())
        }

        fn is_repo_root(&self, dir: &Path) -> bool {
            dir == Path::new("/r")
        }
    }

    fn kind(line: &str, package_json: &'static str) -> Kind {
        let argv: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        classify_with(&argv, &PathBuf::from("/r"), &Fake(package_json), 0)
    }

    const PKG: &str = r#"{"scripts":{
        "lint":"eslint . && tsc --noEmit",
        "lint:fix":"eslint . --fix",
        "check":"npm run lint",
        "build":"tsc",
        "prebuild":"node scripts/clean.js",
        "test":"jest --ci",
        "fmt":"prettier --write .",
        "sneaky":"eslint . --fix",
        "redir":"jest > out.txt"
    }}"#;

    #[test]
    fn writers_and_unknown_programs_are_mutating() {
        for line in [
            "lint-staged",
            "npx lint-staged",
            "pnpm pretty-quick --staged",
            "npx nano-staged",
            "git add .",
            "git diff --cached --check",
            "prettier --write src",
            "eslint . --fix",
            "eslint --fix=true .",
            "cargo fmt",
            "cargo fmt --all",
            "cargo fix",
            "black .",
            "ruff format .",
            "ruff check --fix .",
            "gofmt -w .",
            "go fmt ./...",
            "node scripts/generate.js",
            "./scripts/hook.sh",
            "npm install",
            "pnpm install --frozen-lockfile",
            "yarn add x",
            "jest -u",
            "npx -p some-pkg tool",
            "npm audit",
            "pnpm audit",
            "yarn audit",
        ] {
            assert_eq!(kind(line, "{}"), Kind::Mutating, "{line}");
        }
    }

    #[test]
    fn readers_are_checks() {
        for line in [
            "eslint .",
            "tsc --noEmit",
            "prettier --check .",
            "cargo fmt --check",
            "cargo test --workspace",
            "cargo clippy -- -D warnings",
            "black --check .",
            "ruff format --check .",
            "ruff check .",
            "go test ./...",
            "pytest -q",
            "npx jest --ci",
            "pnpm exec vitest run",
            "terraform fmt -check",
        ] {
            assert_eq!(kind(line, "{}"), Kind::Check, "{line}");
        }
    }

    #[test]
    fn package_scripts_are_classified_by_their_bodies() {
        assert_eq!(kind("npm run lint", PKG), Kind::Check);
        assert_eq!(kind("npm test", PKG), Kind::Check);
        assert_eq!(kind("pnpm run check", PKG), Kind::Check);
        assert_eq!(kind("yarn lint", PKG), Kind::Check);
        // A name with a writing word, a body with a fix flag, a pre- script of
        // an unknown program, a redirection, a script that is not there.
        for line in [
            "npm run lint:fix",
            "npm run fmt",
            "npm run sneaky",
            "npm run build",
            "npm run redir",
            "npm run missing",
        ] {
            assert_eq!(kind(line, PKG), Kind::Mutating, "{line}");
        }
        assert_eq!(kind("npm run lint", "not json"), Kind::Mutating);
        assert!(Scripts::parse(PKG).is_some());
    }
}
