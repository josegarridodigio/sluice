//! Agent resource containment in cgroup v2 through the systemd user manager
//! (ADR-0017, `kb/sluice/design/containment.md`).
//!
//! - [`detect`]: what the machine allows (`Full`, `Partial`, `Off` with the
//!   reason `doctor` prints) and the memory pressure policy derived from the
//!   system's own `systemd-oomd` configuration.
//! - [`explain`]: the intervention log and the once-only notes to the agent.
//! - [`systemd`]: the only place that shells out to `busctl`, `systemctl` and
//!   `oomctl` (like [`crate::git`] for git; no D-Bus library).
//! - [`tree`]: the layout of `sluice.slice`, creation and pruning of session
//!   cgroups, weights, limits, placement of processes and usage readings.
//!
//! Every function returns a typed [`CgroupError`] and never panics; callers
//! fail open (the 0.4.1 behaviour stays when containment is not available).

use std::path::PathBuf;
use std::time::Duration;

pub mod detect;
pub mod explain;
pub mod systemd;
pub mod tree;

pub use detect::{
    Capabilities, Controllers, DoctorLine, Facts, FsKind, Level, Missing, OffReason, Outcome,
    Placement, PressurePolicy,
};
pub use tree::{Layout, Leaf, MemoryEvents, PlacedBy, PruneReport, Tree, TreeSettings, Usage};

/// Where the cgroup v2 hierarchy is mounted.
pub const CGROUP_MOUNT: &str = "/sys/fs/cgroup";

/// Longest session id kept by [`sanitize_session_id`].
pub const MAX_SESSION_ID: usize = 64;

/// Every failure of the containment code.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CgroupError {
    /// Filesystem failure (cgroupfs files and directories included).
    #[error("{context} ({path}): {source}")]
    Io {
        /// What was being attempted.
        context: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },

    /// An external tool is not installed or could not be spawned.
    #[error("{program} is not available: {source}")]
    ToolMissing {
        /// The program that was spawned.
        program: &'static str,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },

    /// An external tool exited with a non-zero status.
    #[error("{program} {args} failed with {status}: {stderr}")]
    Command {
        /// The program that was run.
        program: &'static str,
        /// Its arguments, joined for the message.
        args: String,
        /// Exit status description.
        status: String,
        /// First lines of stderr, trimmed.
        stderr: String,
    },

    /// An external tool did not finish in time and was killed.
    #[error("{program} {args} timed out after {timeout:?}")]
    Timeout {
        /// The program that was run.
        program: &'static str,
        /// Its arguments, joined for the message.
        args: String,
        /// The deadline that expired.
        timeout: Duration,
    },

    /// Output of a tool or a kernel file that could not be understood.
    #[error("cannot parse {what}: {text:?}")]
    Parse {
        /// What was being parsed.
        what: &'static str,
        /// The offending text (truncated).
        text: String,
    },

    /// An argument that cannot name a cgroup or unit.
    #[error("invalid {0}")]
    Invalid(String),
}

/// Result alias of the containment code.
pub type CgResult<T> = std::result::Result<T, CgroupError>;

impl CgroupError {
    /// Builds a [`CgroupError::Io`].
    pub fn io(context: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            context,
            path: path.into(),
            source,
        }
    }

    /// `true` for an `EACCES`/`EPERM` on a cgroup file: the case where
    /// `AttachProcessesToUnit` can still work.
    #[must_use]
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied)
    }

    pub(crate) fn parse(what: &'static str, text: &str) -> Self {
        Self::Parse {
            what,
            text: text.chars().take(200).collect(),
        }
    }
}

/// Sanitises an agent's session id to `[A-Za-z0-9_-]{1,64}`: other characters
/// become `_`, the result is cut to 64 characters. `None` when nothing is
/// left (empty or blank input).
#[must_use]
pub fn sanitize_session_id(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(MAX_SESSION_ID)
            .collect(),
    )
}

/// Directory name of a session cgroup: `s-<sanitised id>`, or `s-pid<pid>`
/// for agents without a session id.
#[must_use]
pub fn session_dir_name(session_id: Option<&str>, agent_pid: u32) -> String {
    match session_id.and_then(sanitize_session_id) {
        Some(id) => format!("s-{id}"),
        None => format!("s-pid{agent_pid}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_sanitised() {
        assert_eq!(
            sanitize_session_id("3f2a-91bc_ok").as_deref(),
            Some("3f2a-91bc_ok")
        );
        assert_eq!(sanitize_session_id("a/b c.d").as_deref(), Some("a_b_c_d"));
        assert_eq!(sanitize_session_id("../..").as_deref(), Some("_____"));
        assert_eq!(sanitize_session_id("  "), None);
        assert_eq!(sanitize_session_id(""), None);
        let long = "x".repeat(200);
        assert_eq!(sanitize_session_id(&long).unwrap().len(), MAX_SESSION_ID);
        assert_eq!(sanitize_session_id("ñandú").as_deref(), Some("_and_"));
    }

    #[test]
    fn session_dir_names() {
        assert_eq!(session_dir_name(Some("abc"), 5), "s-abc");
        assert_eq!(session_dir_name(None, 5), "s-pid5");
        assert_eq!(session_dir_name(Some(""), 77), "s-pid77");
    }
}
