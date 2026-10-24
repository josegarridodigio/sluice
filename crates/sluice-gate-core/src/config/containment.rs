//! The `[containment]` table of the user config.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use super::config_err;
use crate::error::Result;

/// Highest cgroup v2 `cpu.weight`.
pub const MAX_CPU_WEIGHT: u32 = 10_000;

/// The value of `memory_pressure_limit` and `memory_pressure_duration` that
/// derives the setting from the system's own `systemd-oomd` policy.
pub const AUTO: &str = "auto";

/// How the agents scope handles memory pressure (`[containment] memory_policy`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryPolicy {
    /// `systemd-oomd` acts inside the agents scope first, where it is
    /// active and systemd is new enough; accounting only elsewhere.
    #[default]
    Oomd,
    /// Accounting only.
    None,
}

/// The `[containment]` table (ADR-0017, design section 7): agent resource
/// containment in cgroups through the systemd user manager.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ContainmentConfig {
    /// `false` restores the 0.4.1 behaviour (no cgroups).
    pub enabled: bool,
    /// `cpu.weight` of `sluice.slice`.
    pub slice_cpu_weight: u32,
    /// `cpu.weight` of the daemon scope (daemon and gate jobs).
    pub gates_cpu_weight: u32,
    /// `cpu.weight` of the agents scope.
    pub agents_cpu_weight: u32,
    /// `cpu.weight` of a session's `ui` leaf.
    pub ui_cpu_weight: u32,
    /// `cpu.weight` of a session's `tools` leaf.
    pub tools_cpu_weight: u32,
    /// `nice` applied to shells that cannot be moved to `tools`.
    pub sandbox_nice: u32,
    /// Optional CPU ceiling of the agents scope, a percentage of all CPUs
    /// such as `"80%"`; empty means none.
    pub cpu_ceiling: String,
    /// `pids.max` of a session.
    pub pids_max: u32,
    /// Memory pressure policy of the agents scope.
    pub memory_policy: MemoryPolicy,
    /// `ManagedOOMMemoryPressureLimit` of the agents scope: `"auto"`
    /// (derived from the policy watching the user manager) or `"1%"` to `"99%"`.
    pub memory_pressure_limit: String,
    /// `ManagedOOMMemoryPressureDurationSec` of the agents scope: `"auto"`
    /// or a duration such as `"5s"`.
    pub memory_pressure_duration: String,
    /// Opt-in `memory.high` per session, a size such as `"8G"`; empty means none.
    pub memory_high: String,
    /// Opt-in `memory.max` per session; empty means none.
    pub memory_max: String,
}

impl Default for ContainmentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            slice_cpu_weight: 50,
            gates_cpu_weight: 100,
            agents_cpu_weight: 30,
            ui_cpu_weight: 400,
            tools_cpu_weight: 25,
            sandbox_nice: 19,
            cpu_ceiling: String::new(),
            pids_max: 4096,
            memory_policy: MemoryPolicy::Oomd,
            memory_pressure_limit: AUTO.to_owned(),
            memory_pressure_duration: AUTO.to_owned(),
            memory_high: String::new(),
            memory_max: String::new(),
        }
    }
}

/// Parses `"30%"` (the `%` is optional) into a whole percentage.
#[must_use]
pub fn parse_percent(text: &str) -> Option<u8> {
    let t = text.trim();
    let digits = t.strip_suffix('%').unwrap_or(t);
    digits.trim().parse::<u8>().ok().filter(|p| *p <= 100)
}

/// Parses a size such as `8G`, `512M`, `64K` or `1048576` into bytes
/// (binary multiples, like systemd; an optional trailing `B` or `iB` is ignored).
#[must_use]
pub fn parse_size_bytes(text: &str) -> Option<u64> {
    let t = text.trim();
    let t = t
        .strip_suffix("iB")
        .or_else(|| t.strip_suffix('B'))
        .unwrap_or(t);
    let (digits, shift) = match t.chars().last()? {
        'K' | 'k' => (&t[..t.len() - 1], 10),
        'M' | 'm' => (&t[..t.len() - 1], 20),
        'G' | 'g' => (&t[..t.len() - 1], 30),
        'T' | 't' => (&t[..t.len() - 1], 40),
        _ => (t, 0),
    };
    digits
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1u64 << shift)
        .filter(|b| *b > 0)
}

impl ContainmentConfig {
    /// Explicit `memory_pressure_limit` percentage; `None` for `"auto"`.
    #[must_use]
    pub fn pressure_limit_override(&self) -> Option<u8> {
        parse_percent(&self.memory_pressure_limit).filter(|_| self.memory_pressure_limit != AUTO)
    }

    /// Explicit `memory_pressure_duration`; `None` for `"auto"`.
    #[must_use]
    pub fn pressure_duration_override(&self) -> Option<Duration> {
        if self.memory_pressure_duration == AUTO {
            return None;
        }
        humantime::parse_duration(self.memory_pressure_duration.trim()).ok()
    }

    /// `cpu_ceiling` as a percentage of all CPUs, `None` when unset.
    #[must_use]
    pub fn cpu_ceiling_percent(&self) -> Option<u8> {
        parse_percent(&self.cpu_ceiling).filter(|_| !self.cpu_ceiling.trim().is_empty())
    }

    /// `memory_high` in bytes, `None` when unset.
    #[must_use]
    pub fn memory_high_bytes(&self) -> Option<u64> {
        parse_size_bytes(&self.memory_high)
    }

    /// `memory_max` in bytes, `None` when unset.
    #[must_use]
    pub fn memory_max_bytes(&self) -> Option<u64> {
        parse_size_bytes(&self.memory_max)
    }

    pub(super) fn validate(&self, path: &Path) -> Result<()> {
        for (name, w) in [
            ("slice_cpu_weight", self.slice_cpu_weight),
            ("gates_cpu_weight", self.gates_cpu_weight),
            ("agents_cpu_weight", self.agents_cpu_weight),
            ("ui_cpu_weight", self.ui_cpu_weight),
            ("tools_cpu_weight", self.tools_cpu_weight),
        ] {
            if !(1..=MAX_CPU_WEIGHT).contains(&w) {
                return Err(config_err(
                    path,
                    format!("containment.{name} must be in 1..={MAX_CPU_WEIGHT}, got {w}"),
                ));
            }
        }
        if self.sandbox_nice > 19 {
            return Err(config_err(
                path,
                "containment.sandbox_nice must be in 0..=19",
            ));
        }
        if self.pids_max < 1 {
            return Err(config_err(path, "containment.pids_max must be >= 1"));
        }
        if !self.cpu_ceiling.trim().is_empty()
            && !parse_percent(&self.cpu_ceiling).is_some_and(|p| (1..=100).contains(&p))
        {
            return Err(config_err(
                path,
                format!(
                    "containment.cpu_ceiling {:?} must be empty or a percentage such as \"80%\" (1% to 100%)",
                    self.cpu_ceiling
                ),
            ));
        }
        if self.memory_pressure_limit != AUTO
            && !parse_percent(&self.memory_pressure_limit).is_some_and(|p| (1..=99).contains(&p))
        {
            return Err(config_err(
                path,
                format!(
                    "containment.memory_pressure_limit {:?} must be \"auto\" or a percentage from \"1%\" to \"99%\"",
                    self.memory_pressure_limit
                ),
            ));
        }
        if self.memory_pressure_duration != AUTO
            && self
                .pressure_duration_override()
                .is_none_or(|d| d.is_zero())
        {
            return Err(config_err(
                path,
                format!(
                    "containment.memory_pressure_duration {:?} must be \"auto\" or a duration greater than zero such as \"5s\"",
                    self.memory_pressure_duration
                ),
            ));
        }
        for (name, value) in [
            ("memory_high", &self.memory_high),
            ("memory_max", &self.memory_max),
        ] {
            if !value.trim().is_empty() && parse_size_bytes(value).is_none() {
                return Err(config_err(
                    path,
                    format!(
                        "containment.{name} {value:?} must be empty or a size such as \"8G\" or \"512M\""
                    ),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Error, UserConfig};
    use super::*;

    fn parse(text: &str) -> Result<UserConfig> {
        UserConfig::parse(text, Path::new("/u.toml"))
    }

    fn err(text: &str) -> String {
        match parse(text) {
            Err(Error::Config { message, .. }) => message,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn absent_table_is_the_defaults() {
        let c = parse("version = 1").unwrap();
        assert_eq!(c.containment, ContainmentConfig::default());
        let d = ContainmentConfig::default();
        assert!(d.enabled);
        assert_eq!(
            (d.slice_cpu_weight, d.gates_cpu_weight, d.agents_cpu_weight),
            (50, 100, 30)
        );
        assert_eq!((d.ui_cpu_weight, d.tools_cpu_weight), (400, 25));
        assert_eq!((d.sandbox_nice, d.pids_max), (19, 4096));
        assert_eq!(d.memory_policy, MemoryPolicy::Oomd);
        assert_eq!(d.pressure_limit_override(), None);
        assert_eq!(d.pressure_duration_override(), None);
        assert_eq!(d.cpu_ceiling_percent(), None);
        assert_eq!(d.memory_high_bytes(), None);
        assert_eq!(d.memory_max_bytes(), None);
    }

    #[test]
    fn full_table_parses() {
        let c = parse(
            "version = 1\n[containment]\nenabled = false\nslice_cpu_weight = 10\n\
             gates_cpu_weight = 10000\nagents_cpu_weight = 1\nui_cpu_weight = 200\n\
             tools_cpu_weight = 50\nsandbox_nice = 10\ncpu_ceiling = \"80%\"\npids_max = 100\n\
             memory_policy = \"none\"\nmemory_pressure_limit = \"25%\"\n\
             memory_pressure_duration = \"3s\"\nmemory_high = \"8G\"\nmemory_max = \"512M\"\n",
        )
        .unwrap()
        .containment;
        assert!(!c.enabled);
        assert_eq!(c.gates_cpu_weight, 10_000);
        assert_eq!(c.memory_policy, MemoryPolicy::None);
        assert_eq!(c.cpu_ceiling_percent(), Some(80));
        assert_eq!(c.pressure_limit_override(), Some(25));
        assert_eq!(c.pressure_duration_override(), Some(Duration::from_secs(3)));
        assert_eq!(c.memory_high_bytes(), Some(8 << 30));
        assert_eq!(c.memory_max_bytes(), Some(512 << 20));
    }

    #[test]
    fn validation_errors() {
        let t = |body: &str| err(&format!("version = 1\n[containment]\n{body}\n"));
        assert!(t("ui_cpu_weight = 0").contains("ui_cpu_weight"));
        assert!(t("tools_cpu_weight = 10001").contains("tools_cpu_weight"));
        assert!(t("sandbox_nice = 20").contains("sandbox_nice"));
        assert!(t("pids_max = 0").contains("pids_max"));
        assert!(t("cpu_ceiling = \"0%\"").contains("cpu_ceiling"));
        assert!(t("cpu_ceiling = \"abc\"").contains("cpu_ceiling"));
        assert!(t("memory_pressure_limit = \"100%\"").contains("memory_pressure_limit"));
        assert!(t("memory_pressure_limit = \"0%\"").contains("memory_pressure_limit"));
        assert!(t("memory_pressure_duration = \"0s\"").contains("memory_pressure_duration"));
        assert!(t("memory_pressure_duration = \"soon\"").contains("memory_pressure_duration"));
        assert!(t("memory_high = \"lots\"").contains("memory_high"));
        assert!(t("memory_max = \"0\"").contains("memory_max"));
        assert!(t("memory_policy = \"kill\"").contains("memory_policy"));
        assert!(t("bogus = 1").contains("unknown field"));
    }

    #[test]
    fn size_and_percent_helpers() {
        assert_eq!(parse_size_bytes("64K"), Some(65_536));
        assert_eq!(parse_size_bytes("1GiB"), Some(1 << 30));
        assert_eq!(parse_size_bytes("1048576"), Some(1_048_576));
        assert_eq!(parse_size_bytes(""), None);
        assert_eq!(parse_size_bytes("G"), None);
        assert_eq!(parse_percent("30%"), Some(30));
        assert_eq!(parse_percent("30"), Some(30));
        assert_eq!(parse_percent("101%"), None);
        assert_eq!(parse_percent("auto"), None);
    }
}
