//! Append-only log files with size-based rotation, shared by the CLI
//! (`client.log`) and the daemon (`daemon.log`), and [`JobLog`], the
//! output sink of a gate run.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
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

/// Bytes of output a [`JobLog::Memory`] keeps (the most recent ones).
pub const MEMORY_LOG_CAP: usize = 256 * 1024;

/// Where the output of a gate run goes: its log file or, when the file
/// cannot be created (read-only `logs/` or state directory), a bounded
/// in-memory buffer that keeps the last [`MEMORY_LOG_CAP`] bytes for the
/// tail only. A gate is never refused because its log cannot be written.
#[derive(Debug)]
pub enum JobLog {
    /// The log file.
    File(File),
    /// The most recent output, in memory.
    Memory(VecDeque<u8>),
}

impl JobLog {
    /// Creates (truncates) `path`, creating its parent directory (`0700`)
    /// first. On failure the log falls back to memory and the error is
    /// returned as a one-line message for the caller to report once.
    #[must_use]
    pub fn create(path: &Path) -> (Self, Option<String>) {
        let opened = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or(Ok(()), |parent| {
                crate::dirs::create_private_dir(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))
            })
            .and_then(|()| {
                File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))
            });
        match opened {
            Ok(file) => (Self::File(file), None),
            Err(e) => (Self::Memory(VecDeque::new()), Some(e)),
        }
    }

    /// The in-memory output (`None` for a file log).
    #[must_use]
    pub fn memory(&self) -> Option<Vec<u8>> {
        match self {
            Self::File(_) => None,
            Self::Memory(buf) => Some(buf.iter().copied().collect()),
        }
    }
}

impl Write for JobLog {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.write(data),
            Self::Memory(buf) => {
                let keep = data.len().min(MEMORY_LOG_CAP);
                buf.extend(&data[data.len() - keep..]);
                let excess = buf.len().saturating_sub(MEMORY_LOG_CAP);
                buf.drain(..excess);
                Ok(data.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Memory(_) => Ok(()),
        }
    }
}

/// The last `n` lines of `bytes` (invalid UTF-8 replaced).
#[must_use]
pub fn last_lines(bytes: &[u8], n: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(n)..]
        .iter()
        .map(|l| (*l).to_owned())
        .collect()
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
    fn an_unwritable_log_falls_back_to_a_bounded_memory_buffer() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir(&logs).unwrap();
        std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o500)).unwrap();
        let (mut log, err) = JobLog::create(&logs.join("1-x.log"));
        std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o700)).unwrap();
        let Some(err) = err else {
            return; // running as root: permissions are not enforced
        };
        assert!(err.contains("Permission denied"), "{err}");
        log.write_all(b"first\n").unwrap();
        log.write_all(&vec![b'x'; MEMORY_LOG_CAP]).unwrap();
        log.write_all(b"\nlast line\n").unwrap();
        let mem = log.memory().unwrap();
        assert_eq!(mem.len(), MEMORY_LOG_CAP);
        assert_eq!(last_lines(&mem, 1), vec!["last line".to_owned()]);
        let (file, err) = JobLog::create(&dir.path().join("ok/2-y.log"));
        assert!(err.is_none() && file.memory().is_none());
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
