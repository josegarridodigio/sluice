//! Telling a gate that was killed for lack of memory from one that failed
//! (ADR-0019 piece 4).
//!
//! The kernel OOM killer and `systemd-oomd` end a job with `SIGKILL`: the
//! leader shows up as `Signaled(9)` or, when a shell supervises it, as
//! `Exited(137)`, exactly like a gate that crashed or was killed by hand. The
//! only evidence is outside the process: the OOM counters of the job's cgroup
//! (`memory.events`, read while the leaf still exists) or a `systemd-oomd`
//! line in the journal naming that leaf. [`memory_kill_message`] is the pure
//! rule over that evidence; nothing here is ever inferred from the exit code
//! alone.

use std::path::Path;
use std::time::Duration;

use crate::executor::ExecStatus;

/// The `killed_by` value of a job killed for lack of memory.
pub const KILLED_BY_MEMORY: &str = sluice_gate_proto::KILLED_BY_MEMORY;

/// Exit status of a shell whose child died of `SIGKILL` (128 + 9).
const EXIT_SIGKILL: i32 = 137;
/// `SIGKILL`.
const SIGKILL: i32 = 9;

/// What was observed at the end of a job.
#[derive(Debug, Clone, Copy)]
pub struct MemoryEvidence<'a> {
    /// Name of the gate (or `exec '<label>'`) for the message.
    pub gate: &'a str,
    /// How the leader ended.
    pub status: ExecStatus,
    /// `oom_kill` of the job's leaf at its end (the leaf is created for the
    /// job, so this is also the growth since it started).
    pub oom_kill: u64,
    /// `oom_group_kill` of the job's leaf at its end.
    pub oom_group_kill: u64,
    /// A `systemd-oomd` line names the job's leaf.
    pub journal_hit: bool,
    /// `memory.peak` of the leaf (page cache included).
    pub peak_bytes: Option<u64>,
    /// `MemAvailable` when the job ended.
    pub available_bytes: Option<u64>,
    /// Time from spawn to exit.
    pub duration: Duration,
}

/// The leader ended in a way an OOM kill produces: any signal, or exit 137.
#[must_use]
pub fn ended_by_signal(status: ExecStatus) -> bool {
    matches!(
        status,
        ExecStatus::Signaled(_) | ExecStatus::Exited(EXIT_SIGKILL)
    )
}

/// The ending an oomd kill produces when no kernel counter moved: `SIGKILL`
/// of the leader or the shell's 137.
#[must_use]
pub fn sigkill_shaped(status: ExecStatus) -> bool {
    matches!(
        status,
        ExecStatus::Signaled(SIGKILL) | ExecStatus::Exited(EXIT_SIGKILL)
    )
}

/// Whether the journal is worth asking: the leader ended by `SIGKILL` and no
/// kernel counter explains it.
#[must_use]
pub fn needs_journal(status: ExecStatus, oom_kill: u64, oom_group_kill: u64) -> bool {
    sigkill_shaped(status) && oom_kill == 0 && oom_group_kill == 0
}

/// The message of a job killed for lack of memory, or `None` when the
/// evidence does not say so.
///
/// The rule: the leader ended by a signal or exit 137 (never `Timeout` or
/// `Killed`, which are Sluice's own cancels, and never a normal exit), and
/// either the leaf's `oom_kill` or `oom_group_kill` is above zero, or the
/// ending is `SIGKILL`-shaped (`Signaled(9)` or `Exited(137)`) and a
/// `systemd-oomd` line names the leaf.
#[must_use]
pub fn memory_kill_message(e: &MemoryEvidence<'_>) -> Option<String> {
    if !ended_by_signal(e.status) {
        return None;
    }
    let counters = e.oom_kill > 0 || e.oom_group_kill > 0;
    let explained = counters || (sigkill_shaped(e.status) && e.journal_hit);
    if !explained {
        return None;
    }
    let mut facts = Vec::new();
    if let Some(peak) = e.peak_bytes {
        facts.push(format!("job peak {}", format_bytes(peak)));
    }
    if let Some(free) = e.available_bytes {
        facts.push(format!("{} available at the end", format_bytes(free)));
    }
    let detail = if facts.is_empty() {
        String::new()
    } else {
        format!(" ({})", facts.join("; "))
    };
    Some(format!(
        "{} was killed for lack of memory after {:.1}s{detail}; running it again unchanged is \
         likely to end the same way: lower its concurrency or free memory first",
        e.gate,
        e.duration.as_secs_f64()
    ))
}

/// `18.1 GiB`, `512 MiB` or `4 KiB`-style text for a byte count.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= GIB {
        format!("{}.{} GiB", bytes / GIB, (bytes % GIB) * 10 / GIB)
    } else if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{} KiB", bytes.div_ceil(1024))
    }
}

/// `MemAvailable` of `<proc_root>/meminfo`, in bytes.
#[must_use]
pub fn mem_available(proc_root: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(proc_root.join("meminfo")).ok()?;
    let kib: u64 = text
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    kib.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(status: ExecStatus) -> MemoryEvidence<'static> {
        MemoryEvidence {
            gate: "test",
            status,
            oom_kill: 0,
            oom_group_kill: 0,
            journal_hit: false,
            peak_bytes: Some(19_437_000_000),
            available_bytes: Some(1_500_000_000),
            duration: Duration::from_millis(41_200),
        }
    }

    #[test]
    fn a_counter_that_grew_after_a_kill_ending_is_a_memory_kill() {
        for status in [ExecStatus::Exited(137), ExecStatus::Signaled(9)] {
            let e = MemoryEvidence {
                oom_kill: 1,
                ..evidence(status)
            };
            let m = memory_kill_message(&e).unwrap();
            assert!(
                m.starts_with("test was killed for lack of memory after 41.2s"),
                "{m}"
            );
            assert!(m.contains("job peak 18.1 GiB"), "{m}");
            assert!(m.contains("1.3 GiB available at the end"), "{m}");
        }
        // The group counter alone is enough, and any signal counts when a
        // counter grew (a worker took the leader down).
        let e = MemoryEvidence {
            oom_group_kill: 1,
            ..evidence(ExecStatus::Signaled(15))
        };
        assert!(memory_kill_message(&e).is_some());
    }

    #[test]
    fn the_exit_code_alone_is_never_enough() {
        assert_eq!(
            memory_kill_message(&evidence(ExecStatus::Exited(137))),
            None
        );
        assert_eq!(
            memory_kill_message(&evidence(ExecStatus::Signaled(9))),
            None
        );
    }

    #[test]
    fn a_journal_line_counts_only_for_a_sigkill_shaped_ending() {
        for status in [ExecStatus::Exited(137), ExecStatus::Signaled(9)] {
            let e = MemoryEvidence {
                journal_hit: true,
                ..evidence(status)
            };
            assert!(memory_kill_message(&e).is_some());
        }
        for status in [ExecStatus::Signaled(15), ExecStatus::Exited(1)] {
            let e = MemoryEvidence {
                journal_hit: true,
                ..evidence(status)
            };
            assert_eq!(memory_kill_message(&e), None, "{status:?}");
        }
    }

    #[test]
    fn sluice_own_cancels_timeouts_and_normal_exits_are_never_memory_kills() {
        for status in [
            ExecStatus::Killed,
            ExecStatus::Timeout,
            ExecStatus::Exited(0),
            ExecStatus::Exited(1),
        ] {
            let e = MemoryEvidence {
                oom_kill: 3,
                oom_group_kill: 1,
                journal_hit: true,
                ..evidence(status)
            };
            assert_eq!(memory_kill_message(&e), None, "{status:?}");
        }
    }

    #[test]
    fn the_message_leaves_out_what_is_unknown() {
        let e = MemoryEvidence {
            oom_kill: 1,
            peak_bytes: None,
            available_bytes: None,
            ..evidence(ExecStatus::Signaled(9))
        };
        let m = memory_kill_message(&e).unwrap();
        assert!(
            m.starts_with("test was killed for lack of memory after 41.2s;"),
            "{m}"
        );
        let only_peak = MemoryEvidence {
            available_bytes: None,
            ..e
        };
        let m = memory_kill_message(&MemoryEvidence {
            peak_bytes: Some(3 << 30),
            ..only_peak
        })
        .unwrap();
        assert!(m.contains("(job peak 3.0 GiB);"), "{m}");
    }

    #[test]
    fn the_journal_is_asked_only_when_nothing_else_explains_a_sigkill() {
        assert!(needs_journal(ExecStatus::Signaled(9), 0, 0));
        assert!(needs_journal(ExecStatus::Exited(137), 0, 0));
        assert!(!needs_journal(ExecStatus::Signaled(9), 1, 0));
        assert!(!needs_journal(ExecStatus::Signaled(15), 0, 0));
        assert!(!needs_journal(ExecStatus::Exited(1), 0, 0));
        assert!(!needs_journal(ExecStatus::Killed, 0, 0));
        assert!(!needs_journal(ExecStatus::Timeout, 0, 0));
    }

    #[test]
    fn byte_counts_and_meminfo() {
        assert_eq!(format_bytes(0), "0 KiB");
        assert_eq!(format_bytes(300 << 20), "300 MiB");
        assert_eq!(format_bytes(18 << 30), "18.0 GiB");
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(mem_available(dir.path()), None);
        std::fs::write(
            dir.path().join("meminfo"),
            "MemTotal:       31457280 kB\nMemFree:  100 kB\nMemAvailable:    1048576 kB\n",
        )
        .unwrap();
        assert_eq!(mem_available(dir.path()), Some(1 << 30));
    }
}
