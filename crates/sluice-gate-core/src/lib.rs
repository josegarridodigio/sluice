//! Core library for Sluice.
//!
//! This crate is synchronous and dependency-light on purpose: the cache-hit
//! path of `sluice run` must stay fast and must never need the daemon.
//!
//! Modules:
//! - [`config`]: `.sluice.toml` (repository gates) and the user config.
//! - [`cpus`]: CPUs available to the process (affinity mask, cgroup limit).
//! - [`git`]: the only place that spawns the `git` binary.
//! - [`tree`]: working-tree snapshot from a private index and object dir.
//! - [`key`]: cache-key derivation.
//! - [`scope`]: path-scoped keys (pattern matching and the scope hash).
//! - [`marker`]: markers and miss records under the git common dir.
//! - [`lookup`]: the shared cache-hit check (resolve, lookup, run material).
//! - [`agent`]: "is this hook invoked by a coding agent?" detection.
//! - [`semaphore`]: `flock`-based counting semaphore (Phase 1 global guard).
//! - [`mode`]: `CI` / `SLUICE_CACHE` / `SLUICE_DISABLE` handling.
//! - [`dirs`]: state, runtime and socket paths, job ids.
//! - [`which`]: `PATH` resolution that skips Sluice's own shims.
//! - [`heavy`]: the heavy-command list and package-manager equivalences.
//! - [`pkgscripts`]: `package.json` scripts as heavy-command sources.

pub mod agent;
pub mod config;
pub mod cpus;
pub mod dirs;
pub mod error;
pub mod git;
pub mod heavy;
pub mod key;
pub mod logfile;
pub mod lookup;
pub mod marker;
pub mod mode;
pub mod pkgscripts;
pub mod scope;
pub mod semaphore;
pub mod tree;
pub mod which;

pub use error::{Error, Result};

/// Version of the on-disk cache format. Bumping it invalidates every marker.
pub const CACHE_FORMAT_VERSION: u32 = 1;

/// Environment variable carrying the job id on every process Sluice runs
/// (gates, `sluice exec` commands, in-process fallbacks). A nested
/// `sluice exec` or `sluice run` that sees it runs directly, unqueued: the
/// parent job's weight already covers it.
pub const JOB_ENV_VAR: &str = "SLUICE_JOB";

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
