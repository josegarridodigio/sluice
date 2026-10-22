//! Load-aware admission from Linux Pressure Stall Information.
//!
//! Design: `kb/sluice/design/psi-admission.md`. The daemon samples
//! `<root>/cpu` and `<root>/memory` (normally `/proc/pressure`) every
//! `admission.psi_poll_interval` and feeds the sample to [`Admission`],
//! a pure state machine (no clocks: callers pass `now` in unix ms):
//!
//! - **Block** when either `some avg10` exceeds its threshold.
//! - **Hysteresis:** once blocking, resume only after two consecutive
//!   samples with both values below 80 % of their thresholds.
//! - **Starvation guard:** while blocking, a queue head that has waited
//!   longer than `admission.psi_max_wait` may be admitted anyway; at most one
//!   such override per `psi_max_wait` (see [`Admission::record_override`]).
//! - **Unavailable PSI:** the first failed read (both files missing or
//!   unreadable, e.g. a kernel booted with `psi=0`) disables PSI admission
//!   for the rest of the daemon's life, logged once; admission is then
//!   budget-only.

use std::path::Path;
use std::time::Duration;

use sluice_gate_core::config::AdmissionConfig;
use sluice_gate_proto::PsiInfo;

/// Default location of the PSI files.
pub const DEFAULT_ROOT: &str = "/proc/pressure";

/// Fraction of a threshold both values must stay below to resume admission.
const RESUME_FRACTION: f64 = 0.8;
/// Consecutive calm samples needed to resume admission.
const RESUME_SAMPLES: u8 = 2;

/// One reading of the `some avg10` values.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PsiSample {
    /// CPU `some avg10` percentage; `None` when missing or malformed.
    pub cpu_some_avg10: Option<f64>,
    /// Memory `some avg10` percentage; `None` when missing or malformed.
    pub memory_some_avg10: Option<f64>,
}

/// Extracts `some avg10` from the contents of one `/proc/pressure/*` file.
///
/// Tolerates missing `full` lines, extra fields and unknown lines; returns
/// `None` when there is no well-formed `some … avg10=<f>` entry.
pub fn parse_some_avg10(text: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("some") {
            return None;
        }
        fields
            .find_map(|f| f.strip_prefix("avg10="))
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
    })
}

/// Reads `<root>/cpu` and `<root>/memory`. Returns `None` when neither file
/// can be read; a file that is missing or malformed yields `None` for its
/// value only.
pub fn read_sample(root: &Path) -> Option<PsiSample> {
    let cpu = std::fs::read_to_string(root.join("cpu")).ok();
    let memory = std::fs::read_to_string(root.join("memory")).ok();
    if cpu.is_none() && memory.is_none() {
        return None;
    }
    Some(PsiSample {
        cpu_some_avg10: cpu.as_deref().and_then(parse_some_avg10),
        memory_some_avg10: memory.as_deref().and_then(parse_some_avg10),
    })
}

/// Mutable part of [`Admission`].
#[derive(Debug, Clone, Default)]
struct AdmissionState {
    last: PsiSample,
    blocking: bool,
    calm_samples: u8,
    last_observed_ms: Option<u64>,
    blocked_ms_total: u64,
    last_override_ms: Option<u64>,
}

/// PSI admission state machine.
#[derive(Debug, Clone)]
pub struct Admission {
    /// Whether PSI gating is active (config switch and PSI availability).
    pub enabled: bool,
    /// CPU `some avg10` threshold, percent.
    pub cpu_max: f64,
    /// Memory `some avg10` threshold, percent.
    pub memory_max: f64,
    /// Sampling period.
    pub poll_interval: Duration,
    /// Starvation guard threshold.
    pub max_wait: Duration,
    state: AdmissionState,
}

impl Admission {
    /// Builds the state machine from the `[admission]` table.
    pub fn new(config: &AdmissionConfig) -> Self {
        Self {
            enabled: config.psi_enabled,
            cpu_max: config.cpu_some_avg10_max,
            memory_max: config.memory_some_avg10_max,
            poll_interval: config.psi_poll_interval,
            max_wait: config.psi_max_wait,
            state: AdmissionState::default(),
        }
    }

    /// Feeds one sample taken at `now_ms`; returns whether admission is
    /// blocked afterwards. `None` (PSI unavailable) disables PSI admission
    /// permanently and is logged once.
    pub fn observe(&mut self, sample: Option<PsiSample>, now_ms: u64) -> bool {
        if self.state.blocking
            && let Some(prev) = self.state.last_observed_ms
        {
            self.state.blocked_ms_total += now_ms.saturating_sub(prev);
        }
        self.state.last_observed_ms = Some(now_ms);
        if !self.enabled {
            return false;
        }
        let Some(sample) = sample else {
            tracing::warn!(
                "pressure stall information is unavailable (missing /proc/pressure or a \
                 kernel booted with psi=0); admission is budget-only"
            );
            self.enabled = false;
            self.state.blocking = false;
            self.state.calm_samples = 0;
            self.state.last = PsiSample::default();
            return false;
        };
        self.state.last = sample;
        let above = |v: Option<f64>, max: f64| v.is_some_and(|v| v > max);
        let below = |v: Option<f64>, max: f64| v.is_none_or(|v| v < max * RESUME_FRACTION);
        let over = above(sample.cpu_some_avg10, self.cpu_max)
            || above(sample.memory_some_avg10, self.memory_max);
        if !self.state.blocking {
            if over {
                self.state.blocking = true;
                self.state.calm_samples = 0;
                tracing::info!(
                    cpu = ?sample.cpu_some_avg10,
                    memory = ?sample.memory_some_avg10,
                    "system pressure is high; new jobs wait"
                );
            }
        } else if below(sample.cpu_some_avg10, self.cpu_max)
            && below(sample.memory_some_avg10, self.memory_max)
        {
            self.state.calm_samples += 1;
            if self.state.calm_samples >= RESUME_SAMPLES {
                self.state.blocking = false;
                self.state.calm_samples = 0;
                tracing::info!(
                    cpu = ?sample.cpu_some_avg10,
                    memory = ?sample.memory_some_avg10,
                    "system pressure is back to normal; admission resumes"
                );
            }
        } else {
            self.state.calm_samples = 0;
        }
        self.state.blocking
    }

    /// Whether PSI currently blocks new jobs.
    pub fn blocking(&self) -> bool {
        self.enabled && self.state.blocking
    }

    /// The `budget.psi` object of `status`.
    pub fn info(&self) -> PsiInfo {
        if !self.enabled {
            return PsiInfo::default();
        }
        PsiInfo {
            enabled: true,
            cpu_some_avg10: self.state.last.cpu_some_avg10,
            memory_some_avg10: self.state.last.memory_some_avg10,
            blocking: self.state.blocking,
        }
    }

    /// Milliseconds spent blocking since the daemon started (up to the last
    /// sample).
    pub fn blocked_ms_total(&self) -> u64 {
        self.state.blocked_ms_total
    }

    /// Starvation guard: true when PSI blocks, the queue head (queued at
    /// `head_queued_at`) has waited longer than `max_wait`, and no override
    /// happened during the last `max_wait`.
    pub fn starvation_override(&self, head_queued_at: u64, now_ms: u64) -> bool {
        let max_wait = u64::try_from(self.max_wait.as_millis()).unwrap_or(u64::MAX);
        self.blocking()
            && now_ms.saturating_sub(head_queued_at) > max_wait
            && self
                .state
                .last_override_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= max_wait)
    }

    /// Records that the starvation guard admitted a job at `now_ms`.
    pub fn record_override(&mut self, now_ms: u64) {
        self.state.last_override_ms = Some(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CPU: &str = "some avg10=12.34 avg60=5.00 avg300=1.00 total=2574564237\n\
                       full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
    const MEM: &str = "some avg10=0.40 avg60=0.10 avg300=0.00 total=206207460\n\
                       full avg10=0.20 avg60=0.00 avg300=0.00 total=182593270\n";

    fn adm() -> Admission {
        Admission::new(&AdmissionConfig {
            psi_max_wait: Duration::from_secs(300),
            ..AdmissionConfig::default()
        })
    }

    #[allow(clippy::unnecessary_wraps)]
    fn s(cpu: f64, mem: f64) -> Option<PsiSample> {
        Some(PsiSample {
            cpu_some_avg10: Some(cpu),
            memory_some_avg10: Some(mem),
        })
    }

    #[test]
    fn parses_real_samples() {
        assert_eq!(parse_some_avg10(CPU), Some(12.34));
        assert_eq!(parse_some_avg10(MEM), Some(0.4));
        // Older kernels have no `full` line for cpu; extra fields are fine.
        assert_eq!(
            parse_some_avg10("some avg10=99.00 avg60=1 avg300=2 total=3 extra=4\n"),
            Some(99.0)
        );
        assert_eq!(
            parse_some_avg10("full avg10=1.00\nsome total=1 avg10=7.50"),
            Some(7.5)
        );
    }

    #[test]
    fn rejects_garbage() {
        for text in [
            "",
            "garbage",
            "full avg10=3.00 avg60=0 avg300=0 total=0",
            "some avg10=abc avg60=0",
            "some avg60=1.00 avg300=0 total=0",
            "some avg10=-1.0",
            "some avg10=NaN",
            "somewhat avg10=1.0",
        ] {
            assert_eq!(parse_some_avg10(text), None, "{text:?}");
        }
    }

    #[test]
    fn reads_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_sample(dir.path()), None);
        assert_eq!(read_sample(&dir.path().join("missing")), None);
        std::fs::write(dir.path().join("cpu"), CPU).unwrap();
        assert_eq!(
            read_sample(dir.path()),
            Some(PsiSample {
                cpu_some_avg10: Some(12.34),
                memory_some_avg10: None
            })
        );
        std::fs::write(dir.path().join("memory"), "nonsense").unwrap();
        assert_eq!(read_sample(dir.path()).unwrap().memory_some_avg10, None);
        std::fs::write(dir.path().join("memory"), MEM).unwrap();
        assert_eq!(
            read_sample(dir.path()).unwrap().memory_some_avg10,
            Some(0.4)
        );
    }

    #[test]
    fn hysteresis() {
        let mut a = adm();
        assert!(!a.observe(s(50.0, 10.0), 0));
        assert!(a.observe(s(61.0, 0.0), 1_000), "cpu over 60 blocks");
        // Between 80 % and 100 % of the threshold: still blocking.
        assert!(a.observe(s(55.0, 0.0), 2_000));
        // One calm sample is not enough.
        assert!(a.observe(s(10.0, 0.0), 3_000));
        // A sample above 80 % resets the count.
        assert!(a.observe(s(10.0, 17.0), 4_000));
        assert!(a.observe(s(10.0, 0.0), 5_000));
        assert!(!a.observe(s(10.0, 0.0), 6_000), "two calm samples resume");
        assert_eq!(a.blocked_ms_total(), 5_000);
        assert!(a.observe(s(0.0, 20.5), 7_000), "memory over 20 blocks");
        assert!(a.info().blocking);
        assert_eq!(a.info().memory_some_avg10, Some(20.5));
        assert!(a.observe(s(0.0, 0.0), 8_000));
        assert_eq!(a.blocked_ms_total(), 6_000);
        // Missing values count as calm.
        assert!(!a.observe(
            Some(PsiSample {
                cpu_some_avg10: None,
                memory_some_avg10: Some(1.0)
            }),
            9_000
        ));
        assert_eq!(a.blocked_ms_total(), 7_000);
    }

    #[test]
    fn unavailable_disables() {
        let mut a = adm();
        assert!(a.observe(s(99.0, 0.0), 0));
        assert!(!a.observe(None, 1_000));
        assert!(!a.enabled);
        assert_eq!(a.info(), PsiInfo::default());
        // Later samples are ignored.
        assert!(!a.observe(s(99.0, 99.0), 2_000));
        assert!(!a.blocking());
        assert_eq!(a.blocked_ms_total(), 1_000);

        let mut off = Admission::new(&AdmissionConfig {
            psi_enabled: false,
            ..AdmissionConfig::default()
        });
        assert!(!off.observe(s(99.0, 99.0), 0));
        assert!(!off.info().enabled);
    }

    #[test]
    fn starvation_guard() {
        let mut a = adm();
        assert!(!a.starvation_override(0, 400_000), "not blocking");
        a.observe(s(99.0, 0.0), 0);
        assert!(!a.starvation_override(0, 300_000), "exactly max_wait");
        assert!(a.starvation_override(0, 300_001));
        a.record_override(300_001);
        // The next head waited long too, but one override per max_wait.
        assert!(!a.starvation_override(1, 400_000));
        assert!(a.starvation_override(1, 600_001));
        a.observe(s(0.0, 0.0), 600_100);
        a.observe(s(0.0, 0.0), 600_200);
        assert!(!a.starvation_override(1, 900_000), "no longer blocking");
    }
}
