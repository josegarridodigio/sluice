//! The `target-protection` check of `sluice doctor` (ADR-0025): does the
//! queue's target accept direct pushes? Probed with the user's own `gh`
//! login (never the daemon's), advisory only.

use sluice_gate_core::config::{EnvMap, RepoConfig};
use sluice_gate_core::forge::{self, Protection};
use sluice_gate_core::git::{self, RepoPaths};

use super::{Check, Status, check};

/// Name of the check.
const NAME: &str = "target-protection";

/// The check for `label` (`origin/main`) from a probe result.
///
/// `warn` for a protection that stops a direct push (`land` is only `push`
/// in this release, so the queue cannot land there). An unknown result is
/// `ok`: nothing is wrong with the installation (no `gh`, no login, no
/// network, a forge Sluice cannot query), many users never use the queue,
/// and the enqueue warns by itself.
pub fn protection_check(label: &str, probe: &Protection) -> Check {
    match probe {
        Protection::Open => check(
            NAME,
            Status::Ok,
            format!("{label} accepts direct pushes (no protection found)"),
        ),
        Protection::PullRequestRequired(e) => check(
            NAME,
            Status::Warn,
            format!(
                "{label} requires a pull request ({e}): the queue cannot land on this target; \
                 see docs/merge-queue.md"
            ),
        ),
        Protection::Protected(e) => check(
            NAME,
            Status::Warn,
            format!(
                "{label} is protected ({e}): a direct push may be refused and then the queue \
                 cannot land on this target; see docs/merge-queue.md"
            ),
        ),
        Protection::Unknown(r) => check(
            NAME,
            Status::Ok,
            format!("could not check whether {label} accepts direct pushes: {r}"),
        ),
    }
}

/// The remote and target branch of `[queue]`: the configured target, else
/// the remote's HEAD as already known locally (no network).
fn queue_target(env: &EnvMap, paths: &RepoPaths, cfg: &RepoConfig) -> Option<(String, String)> {
    let remote = cfg.queue.remote.clone();
    git::run(
        &["remote", "get-url", "--push", "--", &remote],
        &paths.toplevel,
        env,
        true,
    )
    .ok()?;
    let branch = match &cfg.queue.target {
        Some(t) => t.clone(),
        None => remote_head(env, paths, &remote)?,
    };
    Some((remote, branch))
}

/// The branch `remote`'s HEAD points at, as already known locally.
fn remote_head(env: &EnvMap, paths: &RepoPaths, remote: &str) -> Option<String> {
    let sym = format!("refs/remotes/{remote}/HEAD");
    let full = git::run(
        &["symbolic-ref", "--quiet", "--", &sym],
        &paths.toplevel,
        env,
        true,
    )
    .ok()?;
    Some(
        full.strip_prefix(&format!("refs/remotes/{remote}/"))?
            .to_owned(),
    )
}

/// The check for this repository's configured queue target, from the last
/// verdict an enqueue stored (doctor never asks the forge itself); none when
/// the repository has no such remote or target or nothing was checked yet.
pub fn check_target(env: &EnvMap, paths: &RepoPaths, cfg: &RepoConfig) -> Option<Check> {
    let probe_env = git::probe_env(env, Some(paths));
    let (remote, branch) = queue_target(&probe_env, paths, cfg)?;
    let (probe, at) = forge::last_known(env, &paths.toplevel, &remote, &branch)?;
    let ago = forge::cache::now().saturating_sub(at) / 60;
    Some(protection_check(
        &format!("{remote}/{branch} (as checked at an enqueue {ago} min ago)"),
        &probe,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_is_ok_and_protections_warn() {
        let open = protection_check("origin/main", &Protection::Open);
        assert_eq!(open.status, Status::Ok);
        let pr = protection_check(
            "origin/main",
            &Protection::PullRequestRequired("a ruleset requires a pull request".into()),
        );
        assert_eq!(pr.status, Status::Warn);
        assert!(pr.detail.contains("cannot land"), "{}", pr.detail);
        let p = protection_check(
            "origin/main",
            &Protection::Protected("status checks".into()),
        );
        assert_eq!(p.status, Status::Warn);
        assert_eq!(pr.name, "target-protection");
    }

    #[test]
    fn unknown_says_it_could_not_check_and_stays_ok() {
        let c = protection_check(
            "origin/main",
            &Protection::Unknown("gh is not installed".into()),
        );
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("could not check"), "{}", c.detail);
        assert!(c.detail.contains("gh is not installed"), "{}", c.detail);
    }
}
