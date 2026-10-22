//! The in-process budget: the Phase 1 global `flock` semaphore shared by
//! every Sluice client of the user when the daemon is unavailable (one slot
//! per budget unit; a job takes `weight` slots, all or nothing).

use std::path::Path;
use std::thread;
use std::time::Duration;

use sluice_gate_core::semaphore::{Semaphore, SlotGuard};

/// Takes `weight` (clamped to `1..=budget`) slots of the semaphore under
/// `<scratch>/semaphore`, waiting with a backoff until they are all free.
/// Prints one waiting line on stderr naming `what` unless `quiet`.
///
/// # Errors
///
/// When the semaphore directory cannot be used.
pub fn acquire(
    scratch: &Path,
    budget: u32,
    weight: u32,
    what: &str,
    quiet: bool,
) -> Result<Vec<SlotGuard>, String> {
    let slots = budget.max(1);
    let needed = weight.clamp(1, slots) as usize;
    let semaphore = Semaphore::new(&scratch.join("semaphore"), slots).map_err(|e| e.to_string())?;
    let mut backoff = Duration::from_millis(25);
    let mut announced = quiet;
    loop {
        let mut guards = Vec::with_capacity(needed);
        while guards.len() < needed {
            match semaphore.try_acquire().map_err(|e| e.to_string())? {
                Some(guard) => guards.push(guard),
                None => break,
            }
        }
        if guards.len() == needed {
            return Ok(guards);
        }
        drop(guards);
        if !announced {
            eprintln!("sluice: waiting for {needed} of {slots} budget units for '{what}'");
            announced = true;
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(500));
    }
}
