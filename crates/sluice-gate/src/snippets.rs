//! Hook-integration snippets for `sluice hook print` and `sluice init`.

use std::fmt::Write as _;

/// Hook managers `sluice hook print` knows how to generate snippets for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HookManager {
    /// Husky: scripts in `.husky/`.
    Husky,
    /// Lefthook: a `lefthook.yml` fragment.
    Lefthook,
    /// The pre-commit / prek framework: `.pre-commit-config.yaml`.
    PreCommit,
    /// Plain POSIX scripts in `.githooks/`.
    Plain,
}

const FOOTER: &str = "# A cache hit exits 0 in milliseconds; a miss runs the gate through Sluice.\n# --hook tells Sluice which hook invoked it, so the agent policy applies.\n";

/// The shell guard that is true when Sluice is installed.
pub const GUARD: &str = "command -v sluice >/dev/null 2>&1";

/// A gate to wire and, when an existing hook line ran a command, that
/// command: the line keeps running it on a machine without Sluice.
pub type Wire = (String, Option<String>);

/// `sluice run` with the original command as the fallback, for a shell hook.
#[must_use]
pub fn fallback_shell(hook: &str, gate: &str, original: &str) -> String {
    format!("if {GUARD}; then sluice run --hook {hook} {gate}; else {original}; fi")
}

/// The same inside `sh -c '...'`, for an entry that is not run by a shell;
/// `None` when the original has a single quote that cannot be nested.
#[must_use]
pub fn fallback_sh_c(hook: &str, gate: &str, original: &str) -> Option<String> {
    (!original.contains('\'')).then(|| format!("sh -c '{}'", fallback_shell(hook, gate, original)))
}

fn shell_line(hook: &str, (gate, original): &Wire) -> String {
    match original {
        Some(original) => fallback_shell(hook, gate, original),
        None => format!("sluice run --hook {hook} {gate}"),
    }
}

/// Returns the ready-to-paste integration text for a manager.
///
/// `pre_commit` and `pre_push` are the gate names to run in each hook; an
/// empty list omits that hook.
#[must_use]
pub fn snippet(manager: HookManager, pre_commit: &[String], pre_push: &[String]) -> String {
    let wires =
        |names: &[String]| -> Vec<Wire> { names.iter().map(|n| (n.clone(), None)).collect() };
    snippet_with(manager, &wires(pre_commit), &wires(pre_push))
}

/// Like [`snippet`], with the command each gate replaces where there is one.
#[must_use]
pub fn snippet_with(manager: HookManager, pre_commit: &[Wire], pre_push: &[Wire]) -> String {
    let hooks: [(&str, &[Wire]); 2] = [("pre-commit", pre_commit), ("pre-push", pre_push)];
    let mut out = String::new();
    match manager {
        HookManager::Husky => {
            for (hook, gates) in hooks.iter().filter(|(_, g)| !g.is_empty()) {
                let _ = writeln!(out, "# .husky/{hook}");
                for wire in *gates {
                    let _ = writeln!(out, "{} || exit $?", shell_line(hook, wire));
                }
                out.push('\n');
            }
        }
        HookManager::Lefthook => {
            out.push_str("# lefthook.yml\n");
            for (hook, gates) in hooks.iter().filter(|(_, g)| !g.is_empty()) {
                let _ = writeln!(out, "{hook}:\n  commands:");
                for wire in *gates {
                    let gate = &wire.0;
                    let run = match &wire.1 {
                        Some(original) if !original.contains(": ") => {
                            fallback_shell(hook, gate, original)
                        }
                        _ => format!("sluice run --hook {hook} {gate}"),
                    };
                    let _ = writeln!(out, "    sluice-{gate}:\n      run: {run}");
                }
            }
            out.push('\n');
        }
        HookManager::PreCommit => {
            out.push_str("# .pre-commit-config.yaml\nrepos:\n  - repo: local\n    hooks:\n");
            for (hook, gates) in hooks.iter().filter(|(_, g)| !g.is_empty()) {
                for (gate, original) in *gates {
                    let entry = original
                        .as_deref()
                        .filter(|o| !o.contains(": "))
                        .and_then(|o| fallback_sh_c(hook, gate, o))
                        .unwrap_or_else(|| format!("sluice run --hook {hook} {gate}"));
                    let _ = writeln!(
                        out,
                        "      - id: sluice-{gate}\n        name: sluice {gate}\n        \
                         entry: {entry}\n        language: system\n        \
                         pass_filenames: false\n        always_run: true\n        \
                         stages: [{hook}]"
                    );
                }
            }
        }
        HookManager::Plain => {
            for (hook, gates) in hooks.iter().filter(|(_, g)| !g.is_empty()) {
                let _ = writeln!(out, "# .githooks/{hook}\n#!/bin/sh\nset -e");
                // Lines that keep an original command already work without
                // Sluice; a file of bare calls must not fail a clone without it.
                if gates.iter().all(|(_, original)| original.is_none()) {
                    let _ = writeln!(out, "{GUARD} || exit 0");
                }
                for wire in *gates {
                    let _ = writeln!(out, "{}", shell_line(hook, wire));
                }
                out.push('\n');
            }
            out.push_str("chmod +x .githooks/*\ngit config core.hooksPath .githooks\n\n");
        }
    }
    out.push_str(FOOTER);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn husky_omits_empty_pre_push() {
        let s = snippet(HookManager::Husky, &names(&["lint", "test"]), &[]);
        assert!(s.contains("# .husky/pre-commit\nsluice run --hook pre-commit lint || exit $?\n"));
        assert!(s.contains("sluice run --hook pre-commit test || exit $?"));
        assert!(!s.contains("pre-push"));
        assert!(s.ends_with(FOOTER));
    }

    #[test]
    fn lefthook_omits_empty_pre_push() {
        let s = snippet(HookManager::Lefthook, &names(&["lint"]), &[]);
        assert!(
            s.contains("pre-commit:\n  commands:\n    sluice-lint:\n      run: sluice run --hook pre-commit lint")
        );
        assert!(!s.contains("pre-push"));
        assert!(s.ends_with(FOOTER));
    }

    #[test]
    fn pre_commit_framework_uses_stages() {
        let s = snippet(HookManager::PreCommit, &names(&["lint"]), &[]);
        assert!(s.contains("- repo: local"));
        assert!(s.contains("id: sluice-lint"));
        assert!(s.contains("stages: [pre-commit]"));
        assert!(!s.contains("pre-push"));
        assert!(s.ends_with(FOOTER));
    }

    #[test]
    fn plain_has_scripts_and_instructions() {
        let s = snippet(HookManager::Plain, &names(&["lint"]), &[]);
        assert!(s.contains(
            "# .githooks/pre-commit\n#!/bin/sh\nset -e\ncommand -v sluice >/dev/null 2>&1 || exit 0\n\
             sluice run --hook pre-commit lint\n"
        ));
        assert!(!s.contains(".githooks/pre-push"));
        assert!(s.contains("chmod +x .githooks/*\ngit config core.hooksPath .githooks\n"));
        assert!(s.ends_with(FOOTER));
    }

    #[test]
    fn original_commands_become_fallbacks() {
        let wires = vec![("lint".to_owned(), Some("npx ng lint".to_owned()))];
        let husky = snippet_with(HookManager::Husky, &wires, &[]);
        assert!(husky.contains(
            "if command -v sluice >/dev/null 2>&1; then sluice run --hook pre-commit lint; \
             else npx ng lint; fi || exit $?\n"
        ));
        let plain = snippet_with(HookManager::Plain, &wires, &[]);
        assert!(!plain.contains("|| exit 0"));
        let framework = snippet_with(HookManager::PreCommit, &wires, &[]);
        assert!(framework.contains(
            "entry: sh -c 'if command -v sluice >/dev/null 2>&1; then sluice run --hook \
             pre-commit lint; else npx ng lint; fi'\n"
        ));
        let quoted = vec![("t".to_owned(), Some("echo 'a b'".to_owned()))];
        let framework = snippet_with(HookManager::PreCommit, &quoted, &[]);
        assert!(framework.contains("entry: sluice run --hook pre-commit t\n"));
    }
}
