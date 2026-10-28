//! Error type shared by every module of the core crate.

use std::path::PathBuf;

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Every failure the core crate can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// `git` is not installed or could not be spawned.
    #[error("git is not available in PATH: {0}")]
    GitMissing(#[source] std::io::Error),

    /// A git command exited with a non-zero status.
    #[error("git {args:?} failed with {status}: {stderr}")]
    GitCommand {
        /// Arguments passed to git.
        args: Vec<String>,
        /// Exit status description.
        status: String,
        /// First lines of stderr, trimmed.
        stderr: String,
    },

    /// A git command with a deadline (network calls of the merge queue) did
    /// not finish in time; its process group was terminated.
    #[error("git {} timed out after {}; its process group was killed", args.join(" "), humantime::format_duration(*timeout))]
    GitTimeout {
        /// Arguments passed to git.
        args: Vec<String>,
        /// The deadline that expired.
        timeout: std::time::Duration,
    },

    /// The current directory is not inside a git repository.
    #[error("not a git repository: {0}")]
    NotARepository(PathBuf),

    /// The installed git is older than Sluice needs.
    #[error("git {found} is too old: Sluice needs git {required} or newer")]
    GitTooOld {
        /// Version string reported by `git --version`.
        found: String,
        /// Minimum supported version.
        required: &'static str,
    },

    /// `.sluice.toml` is missing from the worktree root.
    #[error(
        "no .sluice.toml found in {0} (run 'sluice init' there; without one, gates come from \
         hooks Sluice can run, see 'sluice hooks status')"
    )]
    ConfigNotFound(PathBuf),

    /// A configuration file could not be parsed or failed validation.
    #[error("invalid configuration in {path}: {message}")]
    Config {
        /// File that failed.
        path: PathBuf,
        /// Human-readable reason.
        message: String,
    },

    /// A gate name is not defined in `.sluice.toml`.
    #[error("unknown gate '{0}'")]
    UnknownGate(String),

    /// Filesystem failure with the path that caused it.
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

    /// JSON (de)serialisation failure for markers and miss records.
    #[error("invalid marker data in {path}: {source}")]
    MarkerFormat {
        /// File involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: serde_json::Error,
    },

    /// A `key_inputs` command failed.
    #[error("key input command failed: {cmd}: {reason}")]
    KeyInput {
        /// The command as written in the config.
        cmd: String,
        /// Why it failed.
        reason: String,
    },

    /// The daemon socket path cannot be used: too long for `sun_path`, or
    /// its directory is unsafe (see [`crate::dirs::socket_location`]).
    #[error("unusable daemon socket path {path}: {problem}")]
    SocketPath {
        /// The socket path (or its directory) that was refused.
        path: PathBuf,
        /// What is wrong with it and how to fix it.
        problem: String,
    },

    /// The semaphore could not be acquired within the allowed time.
    #[error("could not acquire a concurrency slot within {0:?}")]
    SemaphoreTimeout(std::time::Duration),
}

impl Error {
    /// Build an [`Error::Io`] with context.
    pub fn io(context: &'static str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            context,
            path: path.into(),
            source,
        }
    }
}
