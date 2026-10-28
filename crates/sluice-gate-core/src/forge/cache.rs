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

/// How long a pull request read is reused (`queue status` polls).
pub const PR_TTL_SECS: u64 = 20;

#[derive(Debug, Default, Serialize, Deserialize)]
struct PrFile {
    entries: BTreeMap<String, PrEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PrEntry {
    at: u64,
    pr: Option<super::PullRequest>,
}

fn pr_path(env: &EnvMap) -> PathBuf {
    crate::dirs::cache_dir(env).join("forge-pr.json")
}

fn pr_read(env: &EnvMap) -> PrFile {
    std::fs::read_to_string(pr_path(env))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// The pull request read for `key` (`host/owner/repo#branch`) less than
/// [`PR_TTL_SECS`] ago: `Some(None)` is "there was none".
#[must_use]
pub fn load_pr(env: &EnvMap, key: &str, now: u64) -> Option<Option<super::PullRequest>> {
    let file = pr_read(env);
    let e = file.entries.get(key)?;
    (now.saturating_sub(e.at) < PR_TTL_SECS).then(|| e.pr.clone())
}

/// Stores a definitive read (best effort); stale entries are dropped.
pub fn store_pr(env: &EnvMap, key: &str, pr: Option<&super::PullRequest>, now: u64) {
    let mut file = pr_read(env);
    file.entries
        .retain(|_, e| now.saturating_sub(e.at) < PR_TTL_SECS * 30);
    file.entries.insert(
        key.to_owned(),
        PrEntry {
            at: now,
            pr: pr.cloned(),
        },
    );
    store_pr_file(env, &file);
}

/// Forgets a read (after the caller changed the pull request).
pub fn forget_pr(env: &EnvMap, key: &str) {
    let mut file = pr_read(env);
    if file.entries.remove(key).is_some() {
        store_pr_file(env, &file);
    }
}

fn store_pr_file(env: &EnvMap, file: &PrFile) {
    let target = pr_path(env);
    let Some(dir) = target.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!("forge-pr.{}.tmp", std::process::id()));
    let Ok(text) = serde_json::to_string(file) else {
        return;
    };
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}
