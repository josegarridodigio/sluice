//! Forges (GitHub first): what the merge queue's clients can ask the
//! user's own forge CLI, in the caller's real environment (never the
//! daemon's). ADR-0025.
//!
//! [`probe_target`] answers "does this target accept direct pushes?" as a
//! [`Protection`]; it is advisory (a warning, never a refusal) and never an
//! error: whatever cannot be checked is [`Protection::Unknown`] with the
//! reason. [`Forge`] is the interface a later wave grows with pull-request
//! calls (`ensure_pr`, `pr_state`); GitLab and others plug in behind it.

pub mod cache;
mod exec;
mod github;
mod remote;

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use exec::Budget;
pub use github::{GitHub, Reply, decide, parse_reply};
pub use remote::{RemoteUrl, hostname_of, parse as parse_remote};

use crate::git::{self, EnvMap};

/// Time the whole probe may take (identify the remote, up to three API
/// calls).
pub const PROBE_BUDGET: Duration = Duration::from_secs(8);

/// Whether a target accepts a direct push, as far as the forge says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "kebab-case")]
pub enum Protection {
    /// No protection found.
    Open,
    /// Changes must go through a pull request (the evidence says how).
    PullRequestRequired(String),
    /// Protected, but not shown to require a pull request (status checks,
    /// restrictions, unreadable rules): a direct push may be refused.
    Protected(String),
    /// Could not be checked, and why.
    Unknown(String),
}

/// What a forge can be asked. The pull-request calls come with the landing
/// wave.
pub trait Forge {
    /// Display name (`GitHub`).
    fn name(&self) -> &'static str;

    /// Read-only probe of the protection of `target` (a short branch name).
    fn probe_protection(&self, target: &str, budget: &Budget) -> Protection;
}

/// Identifies the forge of `remote` (a remote name of the repository at
/// `cwd`) and probes `target` there, using the cache. `env` is the
/// caller's real environment.
#[must_use]
pub fn probe_target(env: &EnvMap, cwd: &Path, remote: &str, target: &str) -> Protection {
    probe_target_at(env, cwd, remote, target, cache::now())
}

/// The last verdict an enqueue stored for `target` on `remote`, and when
/// (seconds since the epoch). Never asks the forge: `sluice doctor` uses it
/// so that it stays offline.
#[must_use]
pub fn last_known(
    env: &EnvMap,
    cwd: &Path,
    remote: &str,
    target: &str,
) -> Option<(Protection, u64)> {
    let (key, _, _) = target_key(env, cwd, remote, target, &Budget::new(PROBE_BUDGET)).ok()?;
    cache::last(env, &key)
}

/// The cache key of `target` on `remote`, with the forge host and the
/// repository path, or why the target cannot be probed.
fn target_key(
    env: &EnvMap,
    cwd: &Path,
    remote: &str,
    target: &str,
    budget: &Budget,
) -> Result<(String, String, String), Protection> {
    if remote.is_empty() {
        return Err(Protection::Unknown("the target is a local branch".into()));
    }
    let probe_env = git::probe_env(env, None);
    let url = git::run(
        &["remote", "get-url", "--push", "--", remote],
        cwd,
        &probe_env,
        true,
    )
    .map_err(|e| Protection::Unknown(format!("cannot read the URL of {remote}: {e}")))?;
    let Some(parsed) = remote::parse(&url) else {
        return Err(Protection::Unknown(format!(
            "{remote} is not a network remote"
        )));
    };
    let host = if parsed.ssh {
        remote::real_host(&parsed, env, cwd, budget)
    } else {
        parsed.host.clone()
    };
    let key = format!("{host}/{}#{target}", parsed.path);
    Ok((key, host, parsed.path))
}

fn probe_target_at(env: &EnvMap, cwd: &Path, remote: &str, target: &str, now: u64) -> Protection {
    let budget = Budget::new(PROBE_BUDGET);
    let (key, host, path) = match target_key(env, cwd, remote, target, &budget) {
        Ok(k) => k,
        Err(unknown) => return unknown,
    };
    if let Some(hit) = cache::load(env, &key, now) {
        return hit;
    }
    let result = probe_forge(env, cwd, &host, &path, target, &budget);
    cache::store(env, &key, &result, now);
    result
}

fn probe_forge(
    env: &EnvMap,
    cwd: &Path,
    host: &str,
    path: &str,
    target: &str,
    budget: &Budget,
) -> Protection {
    if matches!(host, "gitlab.com" | "codeberg.org" | "bitbucket.org") {
        return Protection::Unknown(format!(
            "{host} cannot be queried yet (only GitHub can); a refused push is still learned"
        ));
    }
    if host != "github.com" && !GitHub::logged_in(env, cwd, host, budget) {
        return Protection::Unknown(format!(
            "{host} is not a forge Sluice can query (no gh login for it)"
        ));
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() != 2 || segments.contains(&"") {
        return Protection::Unknown(format!("cannot tell owner/repo from the path '{path}'"));
    }
    let mut call_env = env.clone();
    for (k, v) in [
        ("GH_PROMPT_DISABLED", "1"),
        ("GH_NO_UPDATE_NOTIFIER", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
    ] {
        call_env.insert(k.to_owned(), v.to_owned());
    }
    GitHub::new(&call_env, cwd, host, path).probe_protection(target, budget)
}

/// Where the user is told the queue's limits for such a target.
const DOCS: &str = "see docs/merge-queue.md";

/// The warnings an enqueue to `target` (`origin/main`) shows: the probe's
/// verdict and, when a push to it was refused before, that fact
/// (`learned` = date and evidence). Empty for an open target.
#[must_use]
pub fn warnings(target: &str, probe: &Protection, learned: Option<(&str, &str)>) -> Vec<String> {
    let mut out = Vec::new();
    // The probe names the protection when it can; the learned fact speaks
    // when it cannot (open or unknown).
    if let (Some((since, evidence)), Protection::Open | Protection::Unknown(_)) = (learned, probe) {
        out.push(format!(
            "{target} refused a direct push on {since} ({evidence}); the queue cannot land on this \
             target; {DOCS}"
        ));
    }
    match probe {
        Protection::Open => {}
        Protection::PullRequestRequired(e) => out.push(format!(
            "{target} requires a pull request ({e}); the queue cannot land on this target; {DOCS}"
        )),
        Protection::Protected(e) => out.push(format!(
            "{target} is protected ({e}); a direct push may be refused, and then the queue cannot \
             land on this target; {DOCS}"
        )),
        Protection::Unknown(r) => out.push(format!("could not check {target}: {r}")),
    }
    out
}

/// The warnings for an enqueue to `remote`/`target` of the repository at
/// `cwd`: probes the forge with the caller's `env` and adds what the queue
/// learned (`learned` = RFC 3339 time and evidence of an earlier refusal).
/// A local target (empty `remote`) has no forge and no warning.
#[must_use]
pub fn enqueue_warnings(
    env: &EnvMap,
    cwd: &Path,
    remote: &str,
    target: &str,
    learned: Option<(&str, &str)>,
) -> Vec<String> {
    if remote.is_empty() {
        return Vec::new();
    }
    let probe = probe_target(env, cwd, remote, target);
    let day = learned.map(|(since, e)| (since.split('T').next().unwrap_or(since), e));
    warnings(&format!("{remote}/{target}"), &probe, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(s: &str) -> Protection {
        Protection::PullRequestRequired(s.to_owned())
    }

    #[test]
    fn an_open_target_has_no_warning() {
        assert_eq!(
            warnings("origin/main", &Protection::Open, None),
            Vec::<String>::new()
        );
    }

    #[test]
    fn protections_name_themselves_and_say_the_queue_cannot_land() {
        let w = warnings(
            "origin/main",
            &some("a ruleset requires a pull request"),
            None,
        );
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("origin/main requires a pull request"),
            "{w:?}"
        );
        assert!(
            w[0].contains("the queue cannot land on this target; see docs/merge-queue.md"),
            "{w:?}"
        );
        let w = warnings(
            "origin/main",
            &Protection::Protected("status checks".into()),
            None,
        );
        assert!(w[0].contains("is protected (status checks)"), "{w:?}");
        assert!(w[0].contains("cannot land"), "{w:?}");
    }

    #[test]
    fn unknown_says_it_could_not_check() {
        let w = warnings(
            "origin/main",
            &Protection::Unknown("gh is not installed".into()),
            None,
        );
        assert_eq!(w, ["could not check origin/main: gh is not installed"]);
    }

    #[test]
    fn a_learned_fact_speaks_when_the_probe_cannot() {
        let learned = Some(("2026-10-28", "GH006: Protected branch update failed"));
        let w = warnings("origin/main", &Protection::Unknown("no gh".into()), learned);
        assert_eq!(w.len(), 2);
        assert!(
            w[0].contains("refused a direct push on 2026-10-28"),
            "{w:?}"
        );
        assert!(w[0].contains("GH006"), "{w:?}");
        let w = warnings("origin/main", &Protection::Open, learned);
        assert_eq!(w.len(), 1);
        // The probe's own protection is not said twice.
        let w = warnings("origin/main", &some("rule"), learned);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("requires a pull request"), "{w:?}");
    }

    #[test]
    fn a_local_target_has_no_forge_and_no_warning() {
        let env = EnvMap::new();
        assert_eq!(
            enqueue_warnings(&env, Path::new("/"), "", "main", None),
            Vec::<String>::new()
        );
    }

    #[test]
    fn protection_survives_the_cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = EnvMap::new();
        env.insert("XDG_CACHE_HOME".into(), dir.path().display().to_string());
        let key = "github.com/acme/widgets#main";
        assert_eq!(cache::load(&env, key, 1_000), None);
        cache::store(&env, key, &some("rule"), 1_000);
        assert_eq!(
            cache::load(&env, key, 1_000 + cache::TTL_SECS - 1),
            Some(some("rule"))
        );
        assert_eq!(cache::load(&env, key, 1_000 + cache::TTL_SECS), None);
        // Unknown is never cached.
        cache::store(&env, "k2", &Protection::Unknown("x".into()), 1_000);
        assert_eq!(cache::load(&env, "k2", 1_000), None);
    }

    #[test]
    fn doctor_reads_a_verdict_past_the_probe_reuse_until_it_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = EnvMap::new();
        env.insert("XDG_CACHE_HOME".into(), dir.path().display().to_string());
        let key = "github.com/acme/widgets#main";
        cache::store(&env, key, &some("rule"), 1_000);
        // Past the probe's reuse, still there for doctor, with its time.
        assert_eq!(cache::load(&env, key, 1_000 + cache::TTL_SECS), None);
        assert_eq!(cache::last(&env, key), Some((some("rule"), 1_000)));
        // A later store drops what is older than the keep window.
        cache::store(&env, "other", &some("x"), 1_000 + cache::KEEP_SECS);
        assert_eq!(cache::last(&env, key), None);
    }
}
