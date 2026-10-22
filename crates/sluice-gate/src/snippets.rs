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

/// Returns the ready-to-paste integration text for a manager.
///
/// `pre_commit` and `pre_push` are the gate names to run in each hook; an
/// empty list omits that hook.
#[must_use]
pub fn snippet(manager: HookManager, pre_commit: &[String], pre_push: &[String]) -> String {
    let hooks: [(&str, &[String]); 2] = [("pre-commit", pre_commit), ("pre-push", pre_push)];
    let mut out = String::new();
    match manager {
        HookManager::Husky => {
            for (hook, gates) in hooks.iter().filter(|(_, g)| !g.is_empty()) {
                let _ = writeln!(out, "# .husky/{hook}");
                for gate in *gates {
                    let _ = writeln!(out, "sluice run --hook {hook} {gate} || exit $?");
                }
                out.push('\n');
            }
        }
        HookManager::Lefthook => {
            out.push_str("# lefthook.yml\n");
            for (hook, gates) in hooks.iter().filter(|(_, g)| !g.is_empty()) {
                let _ = writeln!(out, "{hook}:\n  commands:");
                for gate in *gates {
                    let _ = writeln!(
                        out,
                        "    sluice-{gate}:\n      run: sluice run --hook {hook} {gate}"
                    );
                }
            }
            out.push('\n');
        }
        HookManager::PreCommit => {
            out.push_str("# .pre-commit-config.yaml\nrepos:\n  - repo: local\n    hooks:\n");
            for (hook, gates) in hooks.iter().filter(|(_, g)| !g.is_empty()) {
                for gate in *gates {
                    let _ = writeln!(
                        out,
                        "      - id: sluice-{gate}\n        name: sluice {gate}\n        \
                         entry: sluice run --hook {hook} {gate}\n        language: system\n        \
                         pass_filenames: false\n        always_run: true\n        \
                         stages: [{hook}]"
                    );
                }
            }
        }
        HookManager::Plain => {
            for (hook, gates) in hooks.iter().filter(|(_, g)| !g.is_empty()) {
                let _ = writeln!(out, "# .githooks/{hook}\n#!/bin/sh\nset -e");
                for gate in *gates {
                    let _ = writeln!(out, "sluice run --hook {hook} {gate}");
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
            "# .githooks/pre-commit\n#!/bin/sh\nset -e\nsluice run --hook pre-commit lint\n"
        ));
        assert!(!s.contains(".githooks/pre-push"));
        assert!(s.contains("chmod +x .githooks/*\ngit config core.hooksPath .githooks\n"));
        assert!(s.ends_with(FOOTER));
    }
}
