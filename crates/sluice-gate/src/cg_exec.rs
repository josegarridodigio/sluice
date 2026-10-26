//! `sluice cg-exec <leaf> -- <argv>`: the placement helper of the containment
//! design (section 2).
//!
//! Writes its own pid to `<leaf>/cgroup.procs` and then `exec`s `argv`, so the
//! command and everything it starts is in the leaf from its first instruction
//! (no race, like the `nice`/`ionice` wrapper). It fails open: when the write
//! is refused (cgroupfs read-only, leaf gone, no permission) the command still
//! runs, and the reason is logged at debug level (`SLUICE_LOG=debug`).
//!
//! With `--oom-score-adj N` it also raises its own `oom_score_adj` to
//! `max(N, inherited)` before the `exec` (ADR-0019 piece 2): the kernel's OOM
//! killer then prefers the gate's processes to the agents'. An unprivileged
//! process can only raise the value, so a smaller `N` keeps the inherited one.
//! A leaf of `-` means "no leaf": only the adj is applied (cgroup v2 is not
//! available, but the kernel's preference still is).

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, ExitCode};

use sluice_gate_core::cgroup::tree::write_pid;

use crate::logging;

/// Exit status when the command cannot be found (shell convention).
const EXIT_NOT_FOUND: u8 = 127;
/// Exit status when the command exists but cannot be executed.
const EXIT_NOT_EXECUTABLE: u8 = 126;

/// The `leaf` argument that skips the move (only the adj is applied).
pub const NO_LEAF: &str = "-";

/// The adj file of the calling process.
const OOM_SCORE_ADJ_FILE: &str = "/proc/self/oom_score_adj";

/// The value to write: never below what the process already has.
#[must_use]
pub fn raised_adj(current: i64, wanted: u16) -> i64 {
    current.max(i64::from(wanted))
}

/// Raises the `oom_score_adj` in `file` to `max(wanted, current)`; returns
/// the value now in force. Nothing is written when the current value is
/// already at least `wanted` (an unprivileged write of a lower value would be
/// refused anyway).
///
/// # Errors
///
/// The file cannot be read, parsed or written.
pub fn apply_oom_score_adj(file: &Path, wanted: u16) -> std::io::Result<i64> {
    let text = std::fs::read_to_string(file)?;
    let current: i64 = text.trim().parse().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected content {:?}", text.trim()),
        )
    })?;
    let target = raised_adj(current, wanted);
    if target != current {
        std::fs::write(file, target.to_string())?;
    }
    Ok(target)
}

/// Moves this process into `leaf` (unless it is [`NO_LEAF`]), raises its
/// `oom_score_adj` when asked, then replaces it with `argv`. Returns only
/// when the `exec` failed. Both steps fail open.
pub fn run(leaf: &Path, oom_score_adj: Option<u16>, argv: &[OsString]) -> ExitCode {
    let Some((program, rest)) = argv.split_first() else {
        eprintln!("sluice cg-exec: no command given (usage: sluice cg-exec <leaf> -- <argv>)");
        return ExitCode::from(2);
    };
    let mut notes = Vec::new();
    if leaf != Path::new(NO_LEAF)
        && let Err(e) = write_pid(leaf, std::process::id())
    {
        notes.push(format!("not moved into {}: {e}", leaf.display()));
    }
    if let Some(wanted) = oom_score_adj
        && let Err(e) = apply_oom_score_adj(Path::new(OOM_SCORE_ADJ_FILE), wanted)
    {
        notes.push(format!("oom_score_adj not raised to {wanted}: {e}"));
    }
    if !notes.is_empty() {
        let env = sluice_gate_core::config::process_env();
        logging::init_cli(&env, "info");
        for note in notes {
            tracing::debug!("cg-exec: {note}");
        }
    }
    let err = Command::new(program).args(rest).exec();
    eprintln!(
        "sluice cg-exec: cannot run {}: {err}",
        program.to_string_lossy()
    );
    if err.kind() == std::io::ErrorKind::NotFound {
        ExitCode::from(EXIT_NOT_FOUND)
    } else {
        ExitCode::from(EXIT_NOT_EXECUTABLE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_adj_is_only_ever_raised() {
        assert_eq!(raised_adj(0, 800), 800);
        assert_eq!(raised_adj(200, 800), 800);
        assert_eq!(raised_adj(900, 800), 900);
        assert_eq!(raised_adj(-100, 0), 0);
    }

    #[test]
    fn apply_writes_the_higher_value_and_keeps_a_higher_current() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("oom_score_adj");
        std::fs::write(&file, "200\n").unwrap();
        assert_eq!(apply_oom_score_adj(&file, 800).unwrap(), 800);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "800");
        std::fs::write(&file, "900\n").unwrap();
        assert_eq!(apply_oom_score_adj(&file, 500).unwrap(), 900);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "900\n");
    }

    #[test]
    fn apply_reports_an_unreadable_or_odd_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(apply_oom_score_adj(&dir.path().join("missing"), 800).is_err());
        let file = dir.path().join("oom_score_adj");
        std::fs::write(&file, "lots").unwrap();
        assert!(apply_oom_score_adj(&file, 800).is_err());
    }
}
