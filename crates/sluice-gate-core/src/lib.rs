//! Core library for Sluice.
//!
//! This crate is synchronous and dependency-light on purpose: the cache-hit
//! path of `sluice run` must stay fast and must never need the daemon.
//! It will hold gate configuration (`.sluice.toml`), git tree hashing,
//! cache-key derivation, marker files and agent detection.

/// Version of the on-disk cache format. Bumping it invalidates every marker.
pub const CACHE_FORMAT_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::CACHE_FORMAT_VERSION;

    #[test]
    fn cache_format_version_is_one_in_phase_zero() {
        assert_eq!(CACHE_FORMAT_VERSION, 1);
    }
}
