//! What the clients do about the pull request of a train (ADR-0025): read
//! it (`queue status`, `queue reconcile`, MCP `queue_status`), open it
//! (`queue approve`) and close it (`queue cancel`). Every call runs with the
//! caller's own environment and `gh` login; nothing is stored or sent to the
//! daemon but the facts ([`PrFact`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::cache;
use super::pr::{AutoMerge, PrFact, PrObservation, PrSpec};
use super::{Budget, Forge, GitHub, call_env, locate_remote, unusable};
use crate::git::EnvMap;

/// Time one forge operation of the clients may take in all.
pub const BUDGET: Duration = Duration::from_secs(45);

/// The train a client looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainRef {
    /// Train id.
    pub train_id: String,
    /// Git common dir of the repository.
    pub repo: PathBuf,
    /// Remote of the target.
    pub remote: String,
    /// The landing commit `S`.
    pub merge_sha: String,
}

impl TrainRef {
    /// The branch the train is pushed to.
    #[must_use]
    pub fn branch(&self) -> String {
        branch_of(&self.train_id)
    }
}

/// `sluice/train/<id>`.
#[must_use]
pub fn branch_of(train_id: &str) -> String {
    format!("sluice/train/{train_id}")
}

/// A forge reached through `gh`, with the key its reads are cached under.
struct Reach {
    env: EnvMap,
    host: String,
    path: String,
}

impl Reach {
    fn key(&self, branch: &str) -> String {
        format!("{}/{}#{branch}", self.host, self.path)
    }

    fn github<'a>(&'a self, cwd: &'a Path) -> GitHub<'a> {
        GitHub::new(&self.env, cwd, &self.host, &self.path)
    }
}

fn reach(env: &EnvMap, t: &TrainRef, budget: &Budget) -> Result<Reach, String> {
    let (host, path) = locate_remote(env, &t.repo, &t.remote, budget).map_err(|p| match p {
        super::Protection::Unknown(why) => why,
        other => format!("{other:?}"),
    })?;
    if let Some(why) = unusable(env, &t.repo, &host, &path, budget) {
        return Err(why);
    }
    Ok(Reach {
        env: call_env(env),
        host,
        path,
    })
}

fn forge_unreachable(t: &TrainRef, reason: String) -> PrFact {
    PrFact::of(&t.train_id, PrObservation::Unreachable { reason })
}

/// Read-only: what the forge says about the pull request of `t`. Cached for
/// a few seconds. A forge that cannot be asked is an
/// [`PrObservation::Unreachable`] fact, never an error.
#[must_use]
pub fn observe(env: &EnvMap, t: &TrainRef) -> PrFact {
    observe_at(env, t, cache::now(), true)
}

/// Like [`observe`], without the short cache: what the forge says right now.
#[must_use]
pub fn observe_now(env: &EnvMap, t: &TrainRef) -> PrFact {
    observe_at(env, t, cache::now(), false)
}

fn observe_at(env: &EnvMap, t: &TrainRef, now: u64, cached: bool) -> PrFact {
    let budget = Budget::new(BUDGET);
    let reach = match reach(env, t, &budget) {
        Ok(r) => r,
        Err(why) => return forge_unreachable(t, why),
    };
    let key = reach.key(&t.branch());
    let hit = if cached {
        cache::load_pr(env, &key, now)
    } else {
        None
    };
    let found = match hit {
        Some(hit) => hit,
        None => match reach.github(&t.repo).pr_state(&t.branch(), &budget) {
            Ok(found) => {
                cache::store_pr(env, &key, found.as_ref(), now);
                found
            }
            Err(why) => return forge_unreachable(t, why),
        },
    };
    match found {
        Some(pr) => PrFact::from_pr(&t.train_id, &pr),
        None => PrFact::of(&t.train_id, PrObservation::Missing),
    }
}

/// Find-or-create the pull request of `t` with the caller's `gh`, and arm
/// auto-merge when `auto_merge` (never otherwise). The fact reports what
/// happened; a failure to open is a [`PrObservation::CreateFailed`] fact.
#[must_use]
pub fn open(env: &EnvMap, t: &TrainRef, spec: &PrSpec, auto_merge: bool) -> PrFact {
    let budget = Budget::new(BUDGET);
    let reach = match reach(env, t, &budget) {
        Ok(r) => r,
        Err(why) => return PrFact::of(&t.train_id, PrObservation::CreateFailed { reason: why }),
    };
    let github = reach.github(&t.repo);
    let (mut pr, created) = match github.ensure_pr(spec, &budget) {
        Ok(found) => found,
        Err(reason) => {
            return PrFact::of(&t.train_id, PrObservation::CreateFailed { reason });
        }
    };
    cache::forget_pr(env, &reach.key(&t.branch()));
    let mut armed = AutoMerge::NotRequested;
    if auto_merge && !pr.auto_merge && matches!(pr.state, PrObservation::Open { .. }) {
        armed = match github.arm_auto_merge(pr.number, &t.merge_sha, &budget) {
            Ok(()) => AutoMerge::Armed,
            Err(_) => AutoMerge::Failed,
        };
    } else if auto_merge && pr.auto_merge {
        armed = AutoMerge::Armed;
    }
    if armed == AutoMerge::Armed {
        pr.auto_merge = true;
    }
    PrFact {
        created,
        auto_merge: armed,
        ..PrFact::from_pr(&t.train_id, &pr)
    }
}

/// Closes pull request `number` of `t` with `comment`, with the caller's
/// `gh`. The caller says in `comment` that Sluice opened it.
///
/// # Errors
///
/// Why the forge could not be asked or refused.
pub fn close(env: &EnvMap, t: &TrainRef, number: u64, comment: &str) -> Result<(), String> {
    let budget = Budget::new(BUDGET);
    let reach = reach(env, t, &budget)?;
    let result = reach.github(&t.repo).close_pr(number, comment, &budget);
    cache::forget_pr(env, &reach.key(&t.branch()));
    result
}
