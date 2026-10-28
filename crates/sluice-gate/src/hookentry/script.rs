//! The hook scripts of Sluice's hooks directory.

use std::path::Path;

use crate::shim::quote;

/// Every hook git can run (`githooks(5)`). Sluice's directory holds a script
/// for each of these that the repository has, and for no other: git fires
/// some of them very often, so a script for a hook that does not exist would
/// cost a process start for nothing.
pub const ALL_HOOKS: [&str; 28] = [
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "pre-receive",
    "update",
    "proc-receive",
    "post-receive",
    "post-update",
    "reference-transaction",
    "push-to-checkout",
    "pre-auto-gc",
    "post-rewrite",
    "sendemail-validate",
    "fsmonitor-watchman",
    "p4-changelist",
    "p4-prepare-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
    "post-index-change",
];

/// What a script needs to know, written into it at activation.
#[derive(Debug, Clone, Copy)]
pub struct ScriptInput<'a> {
    /// The clone's git common directory.
    pub common: &'a Path,
    /// Sluice's hooks directory (where the script lives).
    pub dir: &'a Path,
    /// Absolute path of the `sluice` binary that activated the clone.
    pub sluice: &'a Path,
    /// `core.hooksPath` as it was when the clone was activated.
    pub previous: Option<&'a str>,
}

/// The script for `hook`: `exec sluice hooks run` with the binary that
/// activated the clone (an older `sluice` earlier in `PATH` may not know
/// `hooks run`), else the one in `PATH`; when none can be found, the
/// repository's own hook run directly.
pub fn render(hook: &str, input: &ScriptInput<'_>) -> String {
    let common = quote(&input.common.to_string_lossy());
    let ours = quote(&input.dir.to_string_lossy());
    let sluice = quote(&input.sluice.to_string_lossy());
    let previous = quote(input.previous.unwrap_or(""));
    format!(
        "#!/bin/sh\n\
         # Managed by sluice (`sluice hooks on`); `sluice hooks off` removes it. Do not edit.\n\
         # Entry point of this clone's git hooks: the repository's own hook runs through\n\
         # `sluice hooks run`, and runs directly when no `sluice` binary can be found.\n\
         hook={hook}\n\
         common={common}\n\
         bin=\n\
         [ -x {sluice} ] && bin={sluice}\n\
         [ -n \"$bin\" ] || bin=$(command -v sluice 2>/dev/null) || bin=\n\
         if [ -n \"$bin\" ]; then\n\
         \x20 exec \"$bin\" hooks run --common-dir \"$common\" \"$hook\" -- \"$@\"\n\
         fi\n\
         prev=$(git config --file \"$common/config\" --get-all core.hooksPath 2>/dev/null | tail -n 1)\n\
         [ -n \"$prev\" ] || prev={previous}\n\
         case $prev in \"~/\"*) prev=$HOME/${{prev#\"~/\"}} ;; esac\n\
         if [ -z \"$prev\" ] || [ \"$prev\" = {ours} ]; then prev=$common/hooks; fi\n\
         if [ -f \"$prev/$hook\" ] && [ -x \"$prev/$hook\" ]; then exec \"$prev/$hook\" \"$@\"; fi\n\
         exit 0\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_names_are_unique_and_include_the_ones_sluice_gates() {
        let mut sorted = ALL_HOOKS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ALL_HOOKS.len());
        for hook in ["pre-commit", "pre-push", "commit-msg", "post-index-change"] {
            assert!(ALL_HOOKS.contains(&hook), "{hook}");
        }
    }

    #[test]
    fn the_script_quotes_awkward_paths_and_names_the_hook() {
        let text = render(
            "pre-push",
            &ScriptInput {
                common: Path::new("/a b/it's/.git"),
                dir: Path::new("/a b/it's/.git/sluice/hooks"),
                sluice: Path::new("/bin/sluice"),
                previous: Some(".husky/_"),
            },
        );
        assert!(text.starts_with("#!/bin/sh\n"));
        assert!(text.contains("hook=pre-push\n"));
        assert!(text.contains("common='/a b/it'\\''s/.git'\n"));
        assert!(text.contains("prev='.husky/_'"));
    }
}
