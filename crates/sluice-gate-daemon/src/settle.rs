//! Admission settles after a memory kill (ADR-0026 decision 4).
//!
//! Right after the kernel or systemd-oomd kills a gate job, the memory
//! pressure averages of the gates' scope still show the dead job for several
//! seconds. A job started then is oomd's only candidate in that scope
//! (measured on a second machine: two type checks killed 3-4 s after a kernel
//! kill). So after a kill nothing starts until [`SETTLE_MIN_MS`] has passed and
//! the scope's memory `some avg10` is below `[admission]
//! memory_some_avg10_low`. The PSI starvation guard does not override it.

/// The shortest settling time after a memory kill, milliseconds.
pub const SETTLE_MIN_MS: u64 = 10_000;

/// The settling state: when the latest memory kill was seen.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Settle {
    since: Option<u64>,
}

/// What [`Settle::check`] decided at one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No kill to settle from: admission as usual.
    Open,
    /// Admission waits.
    Holds,
    /// The settling ended at this pass (log it once).
    Resumed,
}

impl Settle {
    /// A gate job was killed for lack of memory at `now_ms`; a later kill
    /// restarts the settling.
    pub fn kill(&mut self, now_ms: u64) {
        self.since = Some(self.since.map_or(now_ms, |t| t.max(now_ms)));
    }

    /// Whether admission must wait at `now_ms`, given the memory `some
    /// avg10` of the gates' scope (`None` when unreadable: the time rule
    /// alone decides) and the configured low threshold.
    pub fn check(&mut self, now_ms: u64, some_avg10: Option<f64>, low: f64) -> Verdict {
        let Some(at) = self.since else {
            return Verdict::Open;
        };
        if now_ms.saturating_sub(at) < SETTLE_MIN_MS || some_avg10.is_some_and(|p| p >= low) {
            return Verdict::Holds;
        }
        self.since = None;
        Verdict::Resumed
    }

    /// When the settling started, if one is in progress.
    pub fn since(&self) -> Option<u64> {
        self.since
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_settles_without_a_kill() {
        let mut s = Settle::default();
        assert_eq!(s.check(5, Some(90.0), 5.0), Verdict::Open);
    }

    #[test]
    fn a_kill_holds_admission_for_ten_seconds_then_until_the_pressure_is_low() {
        let mut s = Settle::default();
        s.kill(1_000);
        // Low pressure at once: the time rule still holds.
        assert_eq!(s.check(4_000, Some(0.0), 5.0), Verdict::Holds);
        // Ten seconds later the scope still shows the dead job.
        assert_eq!(s.check(11_000, Some(41.4), 5.0), Verdict::Holds);
        assert_eq!(s.since(), Some(1_000));
        // The average has decayed: admission resumes, once.
        assert_eq!(s.check(15_000, Some(4.9), 5.0), Verdict::Resumed);
        assert_eq!(s.check(15_100, Some(4.9), 5.0), Verdict::Open);
    }

    #[test]
    fn an_unreadable_pressure_leaves_the_time_rule_alone() {
        let mut s = Settle::default();
        s.kill(0);
        assert_eq!(s.check(9_999, None, 5.0), Verdict::Holds);
        assert_eq!(s.check(10_000, None, 5.0), Verdict::Resumed);
    }

    #[test]
    fn a_later_kill_restarts_the_settling_and_an_older_one_does_not() {
        let mut s = Settle::default();
        s.kill(10_000);
        s.kill(2_000);
        assert_eq!(s.since(), Some(10_000));
        assert_eq!(s.check(19_000, Some(0.0), 5.0), Verdict::Holds);
        s.kill(19_500);
        assert_eq!(s.check(25_000, Some(0.0), 5.0), Verdict::Holds);
        assert_eq!(s.check(29_500, Some(0.0), 5.0), Verdict::Resumed);
    }
}
