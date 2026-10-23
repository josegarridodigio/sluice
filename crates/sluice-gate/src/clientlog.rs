//! `<state>/logs/client.log`: one line per `sluice run` invocation.
//!
//! Format (one line, space separated, `cwd` last because it may contain
//! spaces):
//!
//! ```text
//! 2026-10-22T10:00:00Z pid=4242 caller=agent hook=pre-commit gate=test tree=0123456789ab decision=hit exit=0 ms=12 cwd=/home/u/repo
//! ```
//!
//! `hook` and `tree` are `-` when there is none. When the cache was off for
//! the invocation, ` reason=<code>` follows `tree` (`skip-worktree`,
//! `unmerged`, `submodules`, `git-error`, `disabled`, ...); it is absent
//! otherwise. `decision` is one of
//! `hit`, `miss-run`, `miss-fail-fast`, `coalesced`, `in-process`, `nested`,
//! `disabled`, `error`. The file is appended with one `O_APPEND` write per
//! line, so concurrent invocations never interleave inside a line; above
//! 5 MiB it is rotated to `client.log.1` (one generation is kept). Writing
//! never fails the command: callers log the error at debug level and go on.

use std::io;
use std::path::Path;

use sluice_gate_core::git::EnvMap;
use sluice_gate_core::logfile::{self, CLIENT_LOG};

use crate::dirs;

/// Size above which `client.log` is rotated.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Test-only override of [`MAX_BYTES`] (a byte count), used by the
/// rotation test to avoid writing 5 MiB. Not part of the user interface.
pub const TEST_CAP_ENV: &str = "SLUICE_TEST_CLIENT_LOG_CAP";

/// One invocation of `sluice run`.
#[derive(Debug)]
pub struct Entry<'a> {
    /// Process id of the CLI.
    pub pid: u32,
    /// `agent` or `human`.
    pub caller: &'a str,
    /// Hook that invoked Sluice, if any.
    pub hook: Option<&'a str>,
    /// Gate name.
    pub gate: &'a str,
    /// Working-tree hash, when known (shortened here).
    pub tree: Option<&'a str>,
    /// Short code of why the cache was off (`skip-worktree`, ...), if it was.
    pub reason: Option<&'a str>,
    /// Decision keyword.
    pub decision: &'a str,
    /// Exit code of the invocation.
    pub exit: u8,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u128,
    /// Working directory.
    pub cwd: &'a Path,
}

/// Renders the log line (without the trailing newline).
#[must_use]
pub fn format_line(timestamp: &str, e: &Entry<'_>) -> String {
    let clean = |s: &str| s.replace(char::is_whitespace, "_");
    let tree = e
        .tree
        .map_or_else(|| "-".to_owned(), |t| t.chars().take(12).collect());
    let reason = e
        .reason
        .map_or_else(String::new, |r| format!(" reason={}", clean(r)));
    format!(
        "{timestamp} pid={} caller={} hook={} gate={} tree={tree}{reason} decision={} exit={} ms={} cwd={}",
        e.pid,
        e.caller,
        e.hook.map_or_else(|| "-".to_owned(), clean),
        clean(e.gate),
        e.decision,
        e.exit,
        e.duration_ms,
        e.cwd.display().to_string().replace('\n', " "),
    )
}

/// Appends the entry to `<state>/logs/client.log`, rotating first when the
/// file exceeds the cap.
///
/// # Errors
///
/// The I/O error of rotating or appending; callers ignore it.
pub fn record(env: &EnvMap, e: &Entry<'_>) -> io::Result<()> {
    let path = dirs::logs_dir(env).join(CLIENT_LOG);
    let cap = env
        .get(TEST_CAP_ENV)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(MAX_BYTES);
    logfile::rotate_if_larger(&path, cap, 1)?;
    logfile::append_line(
        &path,
        &format_line(&sluice_gate_core::dirs::now_rfc3339(), e),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_has_every_field() {
        let e = Entry {
            pid: 7,
            caller: "agent",
            hook: Some("pre-commit"),
            gate: "test",
            tree: Some("0123456789abcdef0123"),
            reason: None,
            decision: "hit",
            exit: 0,
            duration_ms: 12,
            cwd: Path::new("/a b/c"),
        };
        assert_eq!(
            format_line("T", &e),
            "T pid=7 caller=agent hook=pre-commit gate=test tree=0123456789ab decision=hit exit=0 ms=12 cwd=/a b/c"
        );
        let e = Entry {
            hook: None,
            tree: None,
            ..e
        };
        assert!(!format_line("T", &e).contains("reason="));
        assert!(format_line("T", &e).contains("hook=- gate=test tree=- decision=hit"));
        let e = Entry {
            reason: Some("skip-worktree"),
            ..e
        };
        assert!(
            format_line("T", &e)
                .contains("hook=- gate=test tree=- reason=skip-worktree decision=hit")
        );
    }
}
