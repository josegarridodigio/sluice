//! A short cache of probe results, so repeated enqueues do not repeat the
//! API calls. One JSON file in the user's cache directory; it holds
//! verdicts and short evidence strings, never credentials.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::Protection;
use crate::git::EnvMap;

/// How long a verdict is reused by a probe.
pub const TTL_SECS: u64 = 300;

/// How long a verdict is kept for `sluice doctor`, which reads it without
/// probing (30 days).
pub const KEEP_SECS: u64 = 30 * 24 * 3600;

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    at: u64,
    result: Protection,
}

fn path(env: &EnvMap) -> PathBuf {
    crate::dirs::cache_dir(env).join("forge-probe.json")
}

/// Seconds since the epoch.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn read(env: &EnvMap) -> File {
    std::fs::read_to_string(path(env))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// The cached verdict for `key`, when younger than [`TTL_SECS`].
#[must_use]
pub fn load(env: &EnvMap, key: &str, now: u64) -> Option<Protection> {
    let file = read(env);
    let e = file.entries.get(key)?;
    (now.saturating_sub(e.at) < TTL_SECS).then(|| e.result.clone())
}

/// The latest verdict stored for `key`, whatever its age, with when it was
/// stored (seconds since the epoch). Reads the file only.
#[must_use]
pub fn last(env: &EnvMap, key: &str) -> Option<(Protection, u64)> {
    let file = read(env);
    file.entries.get(key).map(|e| (e.result.clone(), e.at))
}

/// Stores a definitive verdict (best effort; a cache that cannot be written
/// is just absent). Entries older than [`KEEP_SECS`] are dropped.
pub fn store(env: &EnvMap, key: &str, result: &Protection, now: u64) {
    if matches!(result, Protection::Unknown(_)) {
        return;
    }
    let mut file = read(env);
    file.entries
        .retain(|_, e| now.saturating_sub(e.at) < KEEP_SECS);
    file.entries.insert(
        key.to_owned(),
        Entry {
            at: now,
            result: result.clone(),
        },
    );
    let target = path(env);
    let Some(dir) = target.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!("forge-probe.{}.tmp", std::process::id()));
    let Ok(text) = serde_json::to_string(&file) else {
        return;
    };
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}
