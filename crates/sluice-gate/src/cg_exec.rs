//! `sluice cg-exec <leaf> -- <argv>`: the placement helper of the containment
//! design (section 2).
//!
//! Writes its own pid to `<leaf>/cgroup.procs` and then `exec`s `argv`, so the
//! command and everything it starts is in the leaf from its first instruction
//! (no race, like the `nice`/`ionice` wrapper). It fails open: when the write
//! is refused (cgroupfs read-only, leaf gone, no permission) the command still
//! runs, and the reason is logged at debug level (`SLUICE_LOG=debug`).

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

/// Moves this process into `leaf`, then replaces it with `argv`. Returns
/// only when the `exec` failed.
pub fn run(leaf: &Path, argv: &[OsString]) -> ExitCode {
    let Some((program, rest)) = argv.split_first() else {
        eprintln!("sluice cg-exec: no command given (usage: sluice cg-exec <leaf> -- <argv>)");
        return ExitCode::from(2);
    };
    let moved = write_pid(leaf, std::process::id());
    if let Err(e) = &moved {
        let env = sluice_gate_core::config::process_env();
        logging::init_cli(&env, "info");
        tracing::debug!("cg-exec: not moved into {}: {e}", leaf.display());
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
