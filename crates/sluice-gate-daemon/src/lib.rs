//! The Sluice daemon: one per user, listening on a Unix socket.
//!
//! Modules:
//! - [`store`]: SQLite persistence (jobs, lease, events, totals).
//! - [`scheduler`]: pure admission logic (budget, backfill, reservation, coalescing).
//! - [`psi`]: pressure stall information sampling and the PSI admission gate.
//! - [`executor`]: runs one gate in a process group with log capture and timeout.
//! - [`reaper`]: companion process that kills the gate groups when the daemon dies.
//! - [`server`]: socket listener, protocol handling, subscribers, lifecycle.
//!
//! Design: `kb/sluice/design/executor.md`, `scheduler.md`, `protocol.md`,
//! `psi-admission.md`.

pub mod executor;
mod meta;
pub mod psi;
pub mod reaper;
pub mod scheduler;
pub mod server;
pub mod store;

/// Errors of the daemon crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Core library failure.
    #[error(transparent)]
    Core(#[from] sluice_gate_core::Error),
    /// SQLite failure.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The database is busy; the caller may retry.
    #[error("database busy, retry later")]
    Busy,
    /// Schema version mismatch.
    #[error(
        "database {path} has schema version {found} (written by {writer}), this daemon needs \
         {expected}; restore a backup or delete the file (job history and queue state are \
         lost, cache markers live in the repository and are not affected)"
    )]
    Schema {
        /// Release that last opened the database (`sluice X.Y.Z`).
        writer: String,
        /// Database file.
        path: std::path::PathBuf,
        /// Version found.
        found: u32,
        /// Version required.
        expected: u32,
    },
    /// Another daemon holds the lock.
    #[error("another daemon is already running (lock {0})")]
    AlreadyRunning(std::path::PathBuf),
    /// I/O failure with context.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Protocol violation by a client.
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Result alias for the daemon crate.
pub type Result<T> = std::result::Result<T, Error>;
