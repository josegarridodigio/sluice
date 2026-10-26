//! Explaining interventions to the agent (design section 5).
//!
//! The daemon appends every intervention it records (a kill by the kernel OOM
//! killer or by `systemd-oomd` in a Sluice cgroup) to a small JSON lines file
//! next to its database ([`log_path`]). The MCP `verify` and `status` results
//! and the guard's context note read it (the guard never contacts the daemon)
//! and tell the agent, once per intervention, that its own session was
//! affected ([`take_note`]). What was already told is remembered per session
//! in `interventions-seen/<session>` under the state directory, so every hook
//! and MCP process shares it. Everything fails quiet: a missing or unreadable
//! file simply means there is nothing to say.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::EnvMap;
use crate::dirs;

use super::sanitize_session_id;

/// Name of the intervention log inside the state directory.
pub const LOG_FILE: &str = "interventions.jsonl";
/// Directory (inside the state directory) of the per-session delivery marks.
pub const SEEN_DIR: &str = "interventions-seen";
/// Environment variable exported by the session hook: the `tools` leaf.
pub const LEAF_VAR: &str = "SLUICE_SESSION_LEAF";
/// Interventions older than this are not told to the agent any more.
pub const MAX_AGE: Duration = Duration::from_hours(6);
/// The log is rewritten to its newest lines when it grows past this size.
const LOG_TRIM_BYTES: u64 = 64 * 1024;
/// Lines kept when the log is trimmed.
const LOG_KEEP_LINES: usize = 200;
/// Delivery marks kept per session.
const SEEN_KEEP: usize = 100;

/// One intervention, as stored in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Event {
    /// When it happened (RFC 3339, UTC).
    pub at: String,
    /// Session id (the `s-<id>` directory without its prefix).
    pub session: Option<String>,
    /// Cgroup path inside the Sluice scope (`s-abc/tools`).
    pub leaf: String,
    /// `oom_kill`, `oom_group_kill`, `oomd_memory_pressure`, `oomd_swap` or
    /// `sluice_memory_guard` (Sluice's own guard cancelled a gate job).
    pub cause: String,
    /// Human-readable detail, when known.
    pub detail: Option<String>,
}

impl Event {
    /// The identity used to tell an intervention only once.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.at, self.leaf, self.cause)
    }
}

/// Path of the intervention log.
#[must_use]
pub fn log_path(env: &EnvMap) -> PathBuf {
    dirs::state_dir(env).join(LOG_FILE)
}

/// Appends an event to the log at `path` (best effort; creates the file).
pub fn append_event(path: &Path, event: &Event) {
    let Ok(line) = serde_json::to_string(event) else {
        return;
    };
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(f, "{line}")
    };
    if write().is_err() {
        return;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > LOG_TRIM_BYTES) {
        let events = read_events(path);
        let keep = events.len().saturating_sub(LOG_KEEP_LINES);
        let text: String = events[keep..]
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .map(|l| l + "\n")
            .collect();
        let _ = fs::write(path, text);
    }
}

/// Reads the log (oldest first); unreadable lines are skipped.
#[must_use]
pub fn read_events(path: &Path) -> Vec<Event> {
    fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str::<Event>(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The session id in a cgroup path: the first component named `s-<id>`.
#[must_use]
pub fn session_in_path(path: &str) -> Option<String> {
    path.split('/')
        .find_map(|c| c.strip_prefix("s-"))
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// The caller's session: the leaf the hook exported (`SLUICE_SESSION_LEAF`),
/// else the agent's session id when the caller knows it (`hook_session_id`,
/// sanitised the way the hook names the cgroup), else the session named by
/// the caller's own cgroup (`own_cgroup`, the `/proc/self/cgroup` path).
#[must_use]
pub fn caller_session(
    env: &EnvMap,
    own_cgroup: Option<&str>,
    hook_session_id: Option<&str>,
) -> Option<String> {
    env.get(LEAF_VAR)
        .and_then(|leaf| session_in_path(leaf))
        .or_else(|| hook_session_id.and_then(sanitize_session_id))
        .or_else(|| own_cgroup.and_then(session_in_path))
}

/// The calling process's own cgroup path (`/proc/self/cgroup`).
#[must_use]
pub fn own_cgroup() -> Option<String> {
    fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|t| super::detect::parse_proc_cgroup(&t))
}

/// Friendly name of a cause.
#[must_use]
pub fn cause_text(cause: &str) -> &str {
    match cause {
        "oom_kill" => "kernel OOM killer",
        "oom_group_kill" => "kernel OOM killer, whole group",
        "oomd_memory_pressure" => "systemd-oomd, memory pressure",
        "oomd_swap" => "systemd-oomd, swap use",
        "sluice_memory_guard" => "Sluice memory guard, gate cancelled for lack of memory",
        other => other,
    }
}

/// The sentence told to the agent about one intervention.
#[must_use]
pub fn note_for(event: &Event) -> String {
    let at = event.at.replace('T', " ").trim_end_matches('Z').to_owned() + " UTC";
    format!(
        "Sluice stopped {} in your session at {at}: memory pressure on the machine ({}); the \
         machine was protected. Re-run the command; if it is heavy, run it through \
         `verify`/the queue.",
        event.leaf,
        cause_text(&event.cause)
    )
}

fn seen_file(env: &EnvMap, session: &str) -> Option<PathBuf> {
    let safe = sanitize_session_id(session)?;
    Some(dirs::state_dir(env).join(SEEN_DIR).join(safe))
}

fn recent(at: &str, now: SystemTime) -> bool {
    humantime::parse_rfc3339(at)
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .is_none_or(|age| age <= MAX_AGE)
}

/// The notes still to tell the session `session` about `events`, marking them
/// as told. Events of other sessions, older than [`MAX_AGE`] or already told
/// give nothing.
pub fn take_notes(env: &EnvMap, session: &str, events: &[Event]) -> Vec<String> {
    take_notes_at(env, session, events, SystemTime::now())
}

/// [`take_notes`] with an explicit clock (tests).
pub fn take_notes_at(
    env: &EnvMap,
    session: &str,
    events: &[Event],
    now: SystemTime,
) -> Vec<String> {
    let Some(file) = seen_file(env, session) else {
        return Vec::new();
    };
    let known = sanitize_session_id(session);
    let told: Vec<String> = fs::read_to_string(&file)
        .map(|t| t.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    let told_set: BTreeSet<&str> = told.iter().map(String::as_str).collect();
    let mut fresh: Vec<&Event> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for e in events {
        let mine = e.session.as_deref().and_then(sanitize_session_id) == known;
        let key = e.key();
        if mine && recent(&e.at, now) && !told_set.contains(key.as_str()) && !keys.contains(&key) {
            keys.push(key);
            fresh.push(e);
        }
    }
    if fresh.is_empty() {
        return Vec::new();
    }
    let mut all = told.clone();
    all.extend(keys);
    let skip = all.len().saturating_sub(SEEN_KEEP);
    let text = all[skip..].join("\n") + "\n";
    if let Some(dir) = file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::write(&file, text).is_err() {
        // Cannot remember: better silent than repeating itself forever.
        return Vec::new();
    }
    fresh.into_iter().map(note_for).collect()
}

/// The note (one line per new intervention) for the calling session, or
/// `None` when it has none. `events` come from the daemon's status reply or
/// from [`read_events`].
pub fn take_note(
    env: &EnvMap,
    events: &[Event],
    own_cgroup: Option<&str>,
    hook_session_id: Option<&str>,
) -> Option<String> {
    if events.is_empty() {
        return None;
    }
    let session = caller_session(env, own_cgroup, hook_session_id)?;
    let notes = take_notes(env, &session, events);
    (!notes.is_empty()).then(|| notes.join("\n"))
}

#[cfg(test)]
#[allow(clippy::assert_is_empty)]
mod tests {
    use super::*;

    fn env(dir: &Path) -> EnvMap {
        let mut e = EnvMap::new();
        e.insert("XDG_STATE_HOME".into(), dir.display().to_string());
        e
    }

    fn ev(session: &str, at: &str, cause: &str) -> Event {
        Event {
            at: at.into(),
            session: Some(session.into()),
            leaf: format!("s-{session}/tools"),
            cause: cause.into(),
            detail: None,
        }
    }

    fn now() -> SystemTime {
        humantime::parse_rfc3339("2026-10-24T12:00:00Z").unwrap()
    }

    #[test]
    fn every_known_cause_has_friendly_text() {
        for cause in [
            "oom_kill",
            "oom_group_kill",
            "oomd_memory_pressure",
            "oomd_swap",
            "sluice_memory_guard",
        ] {
            assert_ne!(cause_text(cause), cause, "{cause}");
        }
        assert_eq!(cause_text("something_new"), "something_new");
        assert!(
            note_for(&ev("a", "2026-10-24T11:59:00Z", "sluice_memory_guard"))
                .contains("Sluice memory guard")
        );
    }

    #[test]
    fn sessions_come_from_the_leaf_the_cgroup_or_the_hook() {
        let mut e = EnvMap::new();
        e.insert(
            LEAF_VAR.into(),
            "/sys/fs/cgroup/u/sluice.slice/sluice-agents.scope/s-abc/tools".into(),
        );
        assert_eq!(caller_session(&e, None, None).as_deref(), Some("abc"));
        let none = EnvMap::new();
        assert_eq!(
            caller_session(&none, Some("/x/sluice-agents.scope/s-def/ui"), None).as_deref(),
            Some("def")
        );
        assert_eq!(
            caller_session(&none, Some("/user.slice/app.scope"), Some("a/b")).as_deref(),
            Some("a_b")
        );
        assert_eq!(caller_session(&none, None, None), None);
    }

    #[test]
    fn a_note_is_told_once_and_only_to_its_session() {
        let dir = tempfile::tempdir().unwrap();
        let e = env(dir.path());
        let events = vec![
            ev("abc", "2026-10-24T10:02:11Z", "oomd_memory_pressure"),
            ev("other", "2026-10-24T10:03:00Z", "oom_kill"),
        ];
        let first = take_notes_at(&e, "abc", &events, now());
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(
            first[0].starts_with(
                "Sluice stopped s-abc/tools in your session at 2026-10-24 10:02:11 UTC: memory \
                 pressure on the machine (systemd-oomd, memory pressure); the machine was \
                 protected."
            ),
            "{}",
            first[0]
        );
        assert!(first[0].ends_with("run it through `verify`/the queue."));
        assert!(take_notes_at(&e, "abc", &events, now()).is_empty());
        // A new one is told, the old one is not repeated.
        let mut more = events.clone();
        more.push(ev("abc", "2026-10-24T11:00:00Z", "oom_kill"));
        let second = take_notes_at(&e, "abc", &more, now());
        assert_eq!(second.len(), 1);
        assert!(second[0].contains("kernel OOM killer"));
        // The other session still has its own.
        assert_eq!(take_notes_at(&e, "other", &more, now()).len(), 1);
    }

    #[test]
    fn old_interventions_are_not_told() {
        let dir = tempfile::tempdir().unwrap();
        let e = env(dir.path());
        let old = vec![ev("abc", "2026-10-23T10:00:00Z", "oom_kill")];
        assert!(take_notes_at(&e, "abc", &old, now()).is_empty());
    }

    #[test]
    fn the_log_round_trips_and_is_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/interventions.jsonl");
        assert!(read_events(&path).is_empty());
        let one = ev("abc", "2026-10-24T10:02:11Z", "oom_kill");
        append_event(&path, &one);
        assert_eq!(read_events(&path), vec![one.clone()]);
        std::fs::write(&path, "garbage\n").unwrap();
        append_event(&path, &one);
        assert_eq!(read_events(&path), vec![one]);
        for i in 0..2000 {
            let mut big = ev("abc", "2026-10-24T10:02:11Z", "oom_kill");
            big.detail = Some(format!("{i} {}", "x".repeat(60)));
            append_event(&path, &big);
        }
        let kept = read_events(&path);
        assert!(kept.len() <= LOG_KEEP_LINES + 200, "{}", kept.len());
        assert!(
            kept.last()
                .unwrap()
                .detail
                .as_deref()
                .unwrap()
                .starts_with("1999 ")
        );
    }
}
