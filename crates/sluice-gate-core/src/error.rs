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

    /// The current directory is not inside a git repository.
    #[error("not a git repository: {0}")]
    NotARepository(PathBuf),

    /// `.sluice.toml` could not be found walking up from the given directory.
    #[error("no .sluice.toml found in {0} or its parents")]
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
