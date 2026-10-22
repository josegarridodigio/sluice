//! Cache mode selection from `SLUICE_DISABLE`, `SLUICE_CACHE` and `CI`.

use crate::config::EnvMap;

/// How the cache behaves for this invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheMode {
    /// Normal operation: read and write the cache.
    Enabled,
    /// The cache is off but gates still run through the queue.
    Disabled {
        /// Why the cache is off, for display to the user.
        reason: String,
    },
    /// Sluice is bypassed entirely: no cache, no queue.
    Bypass,
}

/// Whether `value` means "on": `1`, `true`, `yes` or `on`, case-insensitive.
pub fn is_truthy(value: &str) -> bool {
    let v = value.trim();
    ["1", "true", "yes", "on"]
        .iter()
        .any(|t| v.eq_ignore_ascii_case(t))
}

fn is_falsy(value: &str) -> bool {
    let v = value.trim();
    ["0", "false", "no", "off"]
        .iter()
        .any(|t| v.eq_ignore_ascii_case(t))
}

/// Resolve the cache mode from the environment.
///
/// Precedence: a truthy `SLUICE_DISABLE` bypasses everything; a falsy
/// `SLUICE_CACHE` disables the cache; a truthy `SLUICE_CACHE` enables it even
/// when `CI` is set; otherwise a `CI` value that is not falsy disables it.
pub fn cache_mode(env: &EnvMap) -> CacheMode {
    if env.get("SLUICE_DISABLE").is_some_and(|v| is_truthy(v)) {
        return CacheMode::Bypass;
    }
    if let Some(v) = env.get("SLUICE_CACHE") {
        if is_falsy(v) {
            return CacheMode::Disabled {
                reason: "SLUICE_CACHE=0".to_owned(),
            };
        }
        if is_truthy(v) {
            return CacheMode::Enabled;
        }
    }
    if env.get("CI").is_some_and(|v| !is_falsy(v)) {
        return CacheMode::Disabled {
            reason: "CI is set".to_owned(),
        };
    }
    CacheMode::Enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn disabled(reason: &str) -> CacheMode {
        CacheMode::Disabled {
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn truthy_values() {
        for v in ["1", "true", "TRUE", "Yes", "on", "ON"] {
            assert!(is_truthy(v), "{v}");
        }
        for v in ["", "0", "false", "no", "off", "2", "maybe"] {
            assert!(!is_truthy(v), "{v}");
        }
    }

    #[test]
    fn empty_env_is_enabled() {
        assert_eq!(cache_mode(&env(&[])), CacheMode::Enabled);
    }

    #[test]
    fn disable_bypasses_everything() {
        assert_eq!(
            cache_mode(&env(&[("SLUICE_DISABLE", "1"), ("SLUICE_CACHE", "1")])),
            CacheMode::Bypass
        );
        assert_eq!(
            cache_mode(&env(&[("SLUICE_DISABLE", "true"), ("CI", "1")])),
            CacheMode::Bypass
        );
    }

    #[test]
    fn falsy_disable_is_ignored() {
        assert_eq!(
            cache_mode(&env(&[("SLUICE_DISABLE", "0")])),
            CacheMode::Enabled
        );
    }

    #[test]
    fn cache_off_values() {
        for v in ["0", "false", "No", "OFF"] {
            assert_eq!(
                cache_mode(&env(&[("SLUICE_CACHE", v)])),
                disabled("SLUICE_CACHE=0"),
                "{v}"
            );
        }
    }

    #[test]
    fn cache_on_overrides_ci() {
        assert_eq!(
            cache_mode(&env(&[("SLUICE_CACHE", "1"), ("CI", "true")])),
            CacheMode::Enabled
        );
    }

    #[test]
    fn cache_off_wins_over_ci_reason() {
        assert_eq!(
            cache_mode(&env(&[("SLUICE_CACHE", "0"), ("CI", "true")])),
            disabled("SLUICE_CACHE=0")
        );
    }

    #[test]
    fn ci_disables_cache() {
        assert_eq!(cache_mode(&env(&[("CI", "true")])), disabled("CI is set"));
        assert_eq!(cache_mode(&env(&[("CI", "")])), disabled("CI is set"));
    }

    #[test]
    fn ci_falsy_keeps_cache() {
        assert_eq!(cache_mode(&env(&[("CI", "false")])), CacheMode::Enabled);
        assert_eq!(cache_mode(&env(&[("CI", "0")])), CacheMode::Enabled);
    }

    #[test]
    fn unrecognised_cache_value_falls_through_to_ci() {
        assert_eq!(
            cache_mode(&env(&[("SLUICE_CACHE", "maybe"), ("CI", "1")])),
            disabled("CI is set")
        );
        assert_eq!(
            cache_mode(&env(&[("SLUICE_CACHE", "maybe")])),
            CacheMode::Enabled
        );
    }
}
