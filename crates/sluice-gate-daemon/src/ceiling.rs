//! The kernel ceiling of every gate job (ADR-0027).
//!
//! Userspace cannot act once the machine thrashes: with swap full the kernel
//! keeps reclaiming file pages and every process, systemd-oomd and this
//! daemon included, stalls refaulting its own text. Only a limit written
//! before the shortage holds during it. So each sampler tick the daemon
//! writes every job leaf's `memory.max` from the headroom
//! `H = MemAvailable − floor`:
//!
//! - `H ≥ 0`: `anon + H / n` for each of the `n` running jobs. The ceilings
//!   add up to no more than what can be given away while keeping the floor
//!   free, so the jobs cannot push the machine below it even if the daemon
//!   stalls: the limits already written hold.
//! - `H < 0`: the job with the largest `anon` gets `anon − |H|`, below its
//!   usage (the kernel reclaims its page cache and, if that is not enough,
//!   kills the whole job, `memory.oom.group`); the others get `anon` plus
//!   [`SLACK`] (no room to grow until the machine recovers).
//!
//! [`ceilings`] is the pure decision; [`Writer`] performs the writes on its
//! own thread, because lowering `memory.max` below the usage is synchronous
//! (the kernel reclaims or kills before the write returns) and must never
//! block the sampler.
//!
//! ADR-0028 adds the same bound on swap ([`swap_ceilings`], written to
//! `memory.swap.max` by a second [`Writer`]): with `S = SwapFree − swap
//! floor`, each job may swap out `swap + S / n`; below the floor no job may
//! swap out more than it has. All six freezes of 2026-11-01 happened with
//! swap full: the jobs can no longer be the ones that fill it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// Room above its `anon` a job keeps when the machine is below the floor:
/// the error between the sampler's reading and the write.
pub const SLACK: u64 = 64 << 20;
/// No ceiling is ever written below this.
pub const MIN_CEILING: u64 = 64 << 20;
/// Ceilings are written at page granularity; a value within the same page
/// as the last one written is not written again.
const PAGE: u64 = 4096;

/// One running job and the RAM it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CeilingJob {
    /// Job id (also the name of its leaf under `jobs/`).
    pub id: String,
    /// `anon` of the leaf's `memory.stat`, bytes.
    pub anon: u64,
}

/// The ceiling of each job, in the order of `jobs`, for a machine with
/// `available` bytes of `MemAvailable` that must keep `floor` bytes free.
#[must_use]
pub fn ceilings(available: u64, floor: u64, jobs: &[CeilingJob]) -> Vec<(String, u64)> {
    let n = jobs.len() as u64;
    if n == 0 {
        return Vec::new();
    }
    let raw: Vec<u64> = if available >= floor {
        let share = (available - floor) / n;
        jobs.iter().map(|j| j.anon.saturating_add(share)).collect()
    } else {
        let deficit = floor - available;
        let victim = jobs
            .iter()
            .enumerate()
            .max_by_key(|(_, j)| j.anon)
            .map(|(i, _)| i);
        jobs.iter()
            .enumerate()
            .map(|(i, j)| {
                if Some(i) == victim {
                    j.anon.saturating_sub(deficit)
                } else {
                    j.anon.saturating_add(SLACK)
                }
            })
            .collect()
    };
    jobs.iter()
        .zip(raw)
        .map(|(j, c)| (j.id.clone(), c.max(MIN_CEILING) / PAGE * PAGE))
        .collect()
}

/// One running job and the swap it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapJob {
    /// Job id (also the name of its leaf under `jobs/`).
    pub id: String,
    /// `memory.swap.current` of the leaf, bytes.
    pub swap: u64,
}

/// The swap ceiling of each job, in the order of `jobs`, for a machine with
/// `free` bytes of `SwapFree` that must keep `floor` bytes of swap free.
/// Lowering `memory.swap.max` below the usage swaps nothing back in, so below
/// the floor every job keeps what it has and swaps out no more: a job at its
/// RAM ceiling then dies in its own cgroup instead of thrashing.
#[must_use]
pub fn swap_ceilings(free: u64, floor: u64, jobs: &[SwapJob]) -> Vec<(String, u64)> {
    let n = jobs.len() as u64;
    if n == 0 {
        return Vec::new();
    }
    let share = free.saturating_sub(floor) / n;
    jobs.iter()
        .map(|j| (j.id.clone(), j.swap.saturating_add(share) / PAGE * PAGE))
        .collect()
}

/// What the writer thread shares with the sampler.
#[derive(Debug, Default)]
struct Pending {
    /// The newest ceiling per leaf not yet written.
    values: HashMap<PathBuf, u64>,
    /// The last ceiling written per leaf.
    written: HashMap<PathBuf, u64>,
    /// The daemon is shutting down.
    stop: bool,
}

/// Writes ceilings on a dedicated thread, newest value per leaf only.
#[derive(Debug, Clone)]
pub struct Writer {
    shared: Arc<(Mutex<Pending>, Condvar)>,
}

impl Writer {
    /// Starts the writer thread of `memory.max`.
    #[must_use]
    pub fn spawn() -> Self {
        Self::spawn_for("memory.max")
    }

    /// Starts a writer thread of the leaf file `file` (`memory.max` or
    /// `memory.swap.max`).
    #[must_use]
    pub fn spawn_for(file: &'static str) -> Self {
        let shared = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        let _ = std::thread::Builder::new()
            .name("sluice-ceiling".into())
            .spawn(move || run(&worker, file));
        Self { shared }
    }

    /// Queues `value` for `leaf`'s file (`None` writes `max`),
    /// replacing any value still waiting; skipped when the leaf already has
    /// it.
    pub fn set(&self, leaf: &Path, value: Option<u64>) {
        let v = value.unwrap_or(u64::MAX);
        let (lock, cv) = &*self.shared;
        let mut p = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if p.written.get(leaf) == Some(&v) && !p.values.contains_key(leaf) {
            return;
        }
        p.values.insert(leaf.to_path_buf(), v);
        cv.notify_one();
    }

    /// Forgets every leaf but `live` (their jobs ended).
    pub fn retain(&self, live: &[PathBuf]) {
        let (lock, _) = &*self.shared;
        let mut p = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        p.values.retain(|leaf, _| live.contains(leaf));
        p.written.retain(|leaf, _| live.contains(leaf));
    }

    /// The last ceiling written to `leaf`, bytes (`None`: none, or `max`).
    #[must_use]
    pub fn written(&self, leaf: &Path) -> Option<u64> {
        let (lock, _) = &*self.shared;
        let p = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        p.written.get(leaf).copied().filter(|v| *v != u64::MAX)
    }

    /// Stops the writer thread after its current write.
    pub fn stop(&self) {
        let (lock, cv) = &*self.shared;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stop = true;
        cv.notify_all();
    }
}

/// The writer thread: takes one pending value at a time and writes it with
/// the lock released, so a write the kernel holds (reclaim, OOM) delays only
/// that leaf's next value.
fn run(shared: &(Mutex<Pending>, Condvar), file: &str) {
    let (lock, cv) = shared;
    loop {
        let (leaf, v) = {
            let mut p = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if p.stop {
                    return;
                }
                if let Some(leaf) = p.values.keys().next().cloned() {
                    let v = p.values.remove(&leaf).unwrap_or(u64::MAX);
                    break (leaf, v);
                }
                p = cv
                    .wait(p)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let text = if v == u64::MAX {
            "max".to_owned()
        } else {
            v.to_string()
        };
        // A leaf removed at job end is not an error: the job is gone.
        if std::fs::write(leaf.join(file), text).is_ok() {
            lock.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .written
                .insert(leaf, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;

    #[test]
    fn swap_ceilings_share_the_free_swap_above_the_floor() {
        let jobs = [
            SwapJob {
                id: "a".into(),
                swap: G,
            },
            SwapJob {
                id: "b".into(),
                swap: 0,
            },
        ];
        // 6 GiB free, 2 GiB floor: 2 GiB more for each.
        assert_eq!(
            swap_ceilings(6 * G, 2 * G, &jobs),
            [("a".to_owned(), 3 * G), ("b".to_owned(), 2 * G)]
        );
        // Below the floor nobody swaps out more than it holds.
        assert_eq!(
            swap_ceilings(G, 2 * G, &jobs),
            [("a".to_owned(), G), ("b".to_owned(), 0)]
        );
        assert_eq!(swap_ceilings(G, 0, &[]), Vec::new());
        // Page granularity.
        let odd = [SwapJob {
            id: "c".into(),
            swap: 4097,
        }];
        assert_eq!(swap_ceilings(0, 0, &odd), [("c".to_owned(), 4096)]);
    }

    fn job(id: &str, anon: u64) -> CeilingJob {
        CeilingJob {
            id: id.into(),
            anon,
        }
    }

    #[test]
    fn a_lone_job_may_grow_into_everything_above_the_floor() {
        // The recorded run: 13 GiB of anon with 3 GiB still available.
        let c = ceilings(3 * G, G + G / 2, &[job("a", 13 * G)]);
        assert_eq!(c, vec![("a".into(), 13 * G + G + G / 2)]);
    }

    #[test]
    fn the_headroom_is_shared_so_the_ceilings_never_add_up_past_the_floor() {
        let jobs = [job("a", 10 * G), job("b", G)];
        let c = ceilings(5 * G, G, &jobs);
        assert_eq!(c[0].1, 12 * G);
        assert_eq!(c[1].1, 3 * G);
        let given: u64 = c.iter().map(|x| x.1).sum::<u64>() - 11 * G;
        assert!(given <= 5 * G - G);
    }

    #[test]
    fn below_the_floor_the_largest_job_is_cut_and_the_others_are_held() {
        let jobs = [job("small", G), job("big", 12 * G)];
        let c = ceilings(G, 2 * G, &jobs);
        assert_eq!(c[0], ("small".into(), G + SLACK));
        assert_eq!(c[1], ("big".into(), 11 * G));
    }

    #[test]
    fn a_ceiling_never_goes_below_the_minimum_and_is_page_aligned() {
        let c = ceilings(0, 4 * G, &[job("a", 100 << 20)]);
        assert_eq!(c[0].1, MIN_CEILING);
        let c = ceilings(G + 123, 0, &[job("a", 7)]);
        assert_eq!(c[0].1 % PAGE, 0);
        assert!(c[0].1 <= G + 130);
    }

    #[test]
    fn no_job_no_ceiling() {
        assert_eq!(ceilings(G, G, &[]), Vec::new());
    }

    #[test]
    fn the_writer_writes_the_newest_value_and_skips_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = dir.path().join("j1");
        std::fs::create_dir(&leaf).unwrap();
        let w = Writer::spawn();
        w.set(&leaf, Some(5 * G));
        let wait = |want: &str| {
            for _ in 0..400 {
                if std::fs::read_to_string(leaf.join("memory.max"))
                    .ok()
                    .as_deref()
                    == Some(want)
                {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("writer never wrote {want}");
        };
        wait(&(5 * G).to_string());
        assert_eq!(w.written(&leaf), Some(5 * G));
        w.set(&leaf, None);
        wait("max");
        assert_eq!(w.written(&leaf), None);
        // A leaf that is gone is forgotten, not retried.
        std::fs::remove_dir_all(&leaf).unwrap();
        w.set(&leaf, Some(G));
        w.retain(&[]);
        assert_eq!(w.written(&leaf), None);
        w.stop();
    }
}
