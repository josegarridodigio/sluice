//! Append-only log files with size-based rotation, shared by the CLI
//! (`client.log`) and the daemon (`daemon.log`).

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// File name prefix of the MCP server logs (`mcp-<pid>.log`).
pub const MCP_LOG_PREFIX: &str = "mcp-";
/// File name of the CLI invocation log.
pub const CLIENT_LOG: &str = "client.log";

/// True for `<job-id>.log` file names (job ids start with a digit: the Unix
/// time in milliseconds), as opposed to `client.log` and `mcp-<pid>.log`.
#[must_use]
pub fn is_job_log_name(name: &str) -> bool {
    name.strip_suffix(".log")
        .is_some_and(|stem| stem.starts_with(|c: char| c.is_ascii_digit()))
}

/// True for `mcp-<pid>.log` file names.
#[must_use]
pub fn is_mcp_log_name(name: &str) -> bool {
    name.strip_prefix(MCP_LOG_PREFIX)
        .and_then(|rest| rest.strip_suffix(".log"))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

/// Name of file `n` in the rotation chain of `path`: `n == 0` is `path`
/// itself, otherwise `path.<n>`.
fn generation(path: &Path, n: usize) -> PathBuf {
    if n == 0 {
        return path.to_path_buf();
    }
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Shifts `path` -> `path.1` -> ... -> `path.<keep>`; the oldest generation is
/// dropped. A missing `path` is not an error.
///
/// # Errors
///
/// The I/O error of the failing rename (other than "not found").
pub fn rotate(path: &Path, keep: usize) -> io::Result<()> {
    let keep = keep.max(1);
    for n in (0..keep).rev() {
        match std::fs::rename(generation(path, n), generation(path, n + 1)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Rotates `path` (keeping `keep` old generations) when it is larger than
/// `max_bytes`. Returns whether a rotation happened. `max_bytes == 0`
/// disables the rotation.
///
/// # Errors
///
/// The I/O error of the failing rename.
pub fn rotate_if_larger(path: &Path, max_bytes: u64, keep: usize) -> io::Result<bool> {
    if max_bytes == 0 {
        return Ok(false);
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > max_bytes => rotate(path, keep).map(|()| true),
        _ => Ok(false),
    }
}

/// Appends `line` (a newline is added) with a single `write` on an
/// `O_APPEND` descriptor, so concurrent writers never interleave inside a
/// line. The parent directory is created (`0700`) when missing.
///
/// # Errors
///
/// The I/O error of opening or writing the file.
pub fn append_line(path: &Path, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        crate::dirs::create_private_dir(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    file.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_the_requested_generations() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("x.log");
        for round in 0..5 {
            std::fs::write(&log, format!("gen{round}")).unwrap();
            rotate(&log, 3).unwrap();
        }
        assert!(!log.exists());
        let read = |n| std::fs::read_to_string(generation(&log, n)).unwrap();
        assert_eq!(
            (read(1), read(2), read(3)),
            ("gen4".into(), "gen3".into(), "gen2".into())
        );
        assert!(!generation(&log, 4).exists());
    }

    #[test]
    fn log_name_classes() {
        assert!(is_job_log_name("1761234-42-abc123.log"));
        assert!(!is_job_log_name("client.log") && !is_job_log_name("mcp-42.log"));
        assert!(is_mcp_log_name("mcp-42.log") && !is_mcp_log_name("mcp-.log"));
        assert!(!is_mcp_log_name("client.log"));
    }

    #[test]
    fn rotates_only_above_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("x.log");
        assert!(!rotate_if_larger(&log, 10, 1).unwrap());
        std::fs::write(&log, "0123456789").unwrap();
        assert!(!rotate_if_larger(&log, 10, 1).unwrap());
        append_line(&log, "x").unwrap();
        assert!(!rotate_if_larger(&log, 0, 1).unwrap());
        assert!(rotate_if_larger(&log, 10, 1).unwrap());
        assert!(generation(&log, 1).exists() && !log.exists());
    }

    #[test]
    fn append_line_adds_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("sub/x.log");
        append_line(&log, "a").unwrap();
        append_line(&log, "b").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "a\nb\n");
    }
}
