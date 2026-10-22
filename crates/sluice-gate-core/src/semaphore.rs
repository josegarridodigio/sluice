//! `flock`-based counting semaphore.

use std::fs::{DirBuilder, File, OpenOptions};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};

use crate::{Error, Result};

/// A counting semaphore backed by `slot-<n>.lock` files in a directory.
///
/// Locks are advisory `flock`s, so they are released by the kernel when the
/// holder exits, even after a crash.
#[derive(Debug, Clone)]
pub struct Semaphore {
    dir: PathBuf,
    slots: u32,
}

/// A held slot; the lock is released on drop.
#[derive(Debug)]
pub struct SlotGuard {
    _lock: Flock<File>,
    /// Index of the slot held.
    pub slot: u32,
}

impl Semaphore {
    /// Create the lock directory (mode `0700`) and the semaphore.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `slots` is zero, [`Error::Io`] when the directory
    /// cannot be created.
    pub fn new(dir: &Path, slots: u32) -> Result<Self> {
        if slots == 0 {
            return Err(Error::Config {
                path: dir.to_path_buf(),
                message: "slots must be >= 1".to_owned(),
            });
        }
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| Error::io("creating semaphore directory", dir, e))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            slots,
        })
    }

    /// Try every slot once without blocking.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when a lock file cannot be opened or locked.
    pub fn try_acquire(&self) -> Result<Option<SlotGuard>> {
        for slot in 0..self.slots {
            let path = self.dir.join(format!("slot-{slot}.lock"));
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .map_err(|e| Error::io("opening semaphore slot", &path, e))?;
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(lock) => return Ok(Some(SlotGuard { _lock: lock, slot })),
                Err((_, Errno::EWOULDBLOCK)) => {}
                Err((_, errno)) => {
                    return Err(Error::io("locking semaphore slot", &path, errno.into()));
                }
            }
        }
        Ok(None)
    }

    /// Wait for a slot, polling with a backoff from 25 ms up to 500 ms.
    ///
    /// `None` waits forever.
    ///
    /// # Errors
    ///
    /// [`Error::SemaphoreTimeout`] when `timeout` elapses first, or any error
    /// from [`Semaphore::try_acquire`].
    pub fn acquire(&self, timeout: Option<Duration>) -> Result<SlotGuard> {
        let start = Instant::now();
        let mut delay = Duration::from_millis(25);
        loop {
            if let Some(guard) = self.try_acquire()? {
                return Ok(guard);
            }
            let sleep = match timeout {
                Some(limit) => {
                    let elapsed = start.elapsed();
                    if elapsed >= limit {
                        return Err(Error::SemaphoreTimeout(limit));
                    }
                    delay.min(limit.saturating_sub(elapsed))
                }
                None => delay,
            };
            std::thread::sleep(sleep);
            delay = (delay * 2).min(Duration::from_millis(500));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_slots_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Semaphore::new(dir.path(), 0),
            Err(Error::Config { .. })
        ));
    }

    #[test]
    fn limits_concurrent_holders() {
        let dir = tempfile::tempdir().unwrap();
        let sem = Semaphore::new(&dir.path().join("sem"), 2).unwrap();
        let a = sem.try_acquire().unwrap().unwrap();
        let b = sem.try_acquire().unwrap().unwrap();
        assert_ne!(a.slot, b.slot);
        assert!(sem.try_acquire().unwrap().is_none());
        drop(a);
        assert!(sem.try_acquire().unwrap().is_some());
    }

    #[test]
    fn acquire_times_out_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let sem = Semaphore::new(dir.path(), 1).unwrap();
        let _held = sem.acquire(None).unwrap();
        let start = Instant::now();
        let err = sem.acquire(Some(Duration::from_millis(200))).unwrap_err();
        assert!(matches!(err, Error::SemaphoreTimeout(_)));
        assert!(start.elapsed() >= Duration::from_millis(200));
    }

    #[test]
    fn acquire_succeeds_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let sem = Semaphore::new(dir.path(), 1).unwrap();
        let held = sem.acquire(None).unwrap();
        let sem2 = sem.clone();
        let t =
            std::thread::spawn(move || sem2.acquire(Some(Duration::from_secs(5))).map(|g| g.slot));
        std::thread::sleep(Duration::from_millis(100));
        drop(held);
        assert_eq!(t.join().unwrap().unwrap(), 0);
    }

    #[test]
    fn lock_held_by_child_process_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let sem = Semaphore::new(dir.path(), 1).unwrap();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec 9>slot-0.lock; flock -x 9; echo locked; exec sleep 5")
            .current_dir(dir.path())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Wait until the child reports it holds the lock.
        let mut line = String::new();
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        std::io::BufRead::read_line(&mut out, &mut line).unwrap();
        assert_eq!(line.trim(), "locked");
        assert!(sem.try_acquire().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(sem.acquire(Some(Duration::from_secs(2))).is_ok());
    }
}
