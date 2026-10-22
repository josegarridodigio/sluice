//! Core library for Sluice.
//!
//! This crate is synchronous and dependency-light on purpose: the cache-hit
//! path of `sluice run` must stay fast and must never need the daemon.
//!
//! Modules:
//! - [`config`]: `.sluice.toml` (repository gates) and the user config.
//! - [`git`]: the only place that spawns the `git` binary.
//! - [`tree`]: working-tree snapshot from a private index and object dir.
//! - [`key`]: cache-key derivation.
//! - [`marker`]: markers and miss records under the git common dir.
//! - [`agent`]: "is this hook invoked by a coding agent?" detection.
//! - [`semaphore`]: `flock`-based counting semaphore (Phase 1 global guard).
//! - [`mode`]: `CI` / `SLUICE_CACHE` / `SLUICE_DISABLE` handling.

pub mod agent;
pub mod config;
pub mod error;
pub mod git;
pub mod key;
pub mod marker;
pub mod mode;
pub mod semaphore;
pub mod tree;

pub use error::{Error, Result};

/// Version of the on-disk cache format. Bumping it invalidates every marker.
pub const CACHE_FORMAT_VERSION: u32 = 1;

/// Version of the Sluice crates, embedded in markers.
pub const SLUICE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::CACHE_FORMAT_VERSION;

    #[test]
    fn cache_format_version_is_one_in_phase_one() {
        assert_eq!(CACHE_FORMAT_VERSION, 1);
    }
}
