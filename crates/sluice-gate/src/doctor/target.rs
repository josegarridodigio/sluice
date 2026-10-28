//! The `target-protection` check of `sluice doctor` (ADR-0025): does the
//! queue's target accept direct pushes? Probed with the user's own `gh`
//! login (never the daemon's), advisory only.

use sluice_gate_core::config::{EnvMap, Land, RepoConfig, UserConfig};
use sluice_gate_core::forge::{self, Protection};
use sluice_gate_core::git::{self, RepoPaths};
use sluice_gate_proto::{QueueStatusReport, QueueTrainState};

use super::{Check, Status, check};

/// Name of the check.
const NAME: &str = "target-protection";

/// The check for `label` (`origin/main`) from a probe result.
///
/// With `land = "push"`, `warn` for a protection that stops a direct push
/// (the queue cannot land there; `land = "pull-request"` is the way). With
/// `land = "pull-request"` the protection is what the queue expects: `ok`.
/// An unknown result is
/// `ok`: nothing is wrong with the installation (no `gh`, no login, no
/// network, a forge Sluice cannot query), many users never use the queue,
/// and the enqueue warns by itself.
pub fn protection_check(label: &str, probe: &Protection, land: Land) -> Check {
    if land == Land::PullRequest {
        let detail = match probe {
            Protection::Open => {
                format!("{label} accepts direct pushes; trains land through a pull request anyway")
            }
            Protection::PullRequestRequired(e) | Protection::Protected(e) => {
                format!("{label} is protected ({e}); trains land through a pull request")
            }
            Protection::Unknown(r) => format!(
                "{label}: trains land through a pull request (could not check the target: {r})"
            ),
        };
        return check(NAME, Status::Ok, detail);
    }
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
                "{label} requires a pull request ({e}): the queue cannot land on this target by \
                 push; set [queue] land = \"pull-request\" (see docs/merge-queue.md)"
            ),
        ),
        Protection::Protected(e) => check(
            NAME,
            Status::Warn,
            format!(
                "{label} is protected ({e}): a direct push may be refused and then the queue \
                 cannot land on this target by push; set [queue] land = \"pull-request\" (see \
                 docs/merge-queue.md)"
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
    // The checked-out configuration (advisory): the daemon reads the target
    // commit's when it builds a train.
    let user = UserConfig::load(env).unwrap_or_default();
    let land = Land::effective(cfg.queue.land, user.queue.land);
    Some(protection_check(
        &format!("{remote}/{branch} (as checked at an enqueue {ago} min ago)"),
        &probe,
        land,
    ))
}

/// `queue-pr-branches`: the `sluice/train/*` branches the daemon's store knows
/// about, from its status (no network): trains waiting for their pull
/// request, and branches of dissolved trains that were not deleted (a closed
/// pull request, a cancel whose cleanup failed). `None` when the store knows
/// none.
pub fn pr_branches_check(report: &QueueStatusReport) -> Option<Check> {
    let waiting = report
        .trains
        .iter()
        .filter(|t| t.state == QueueTrainState::AwaitingMerge)
        .count();
    let leftover: Vec<String> = report
        .trains
        .iter()
        .filter(|t| t.state == QueueTrainState::Dissolved)
        .filter_map(|t| {
            let pr = t.pr.as_ref().filter(|p| !p.branch_deleted)?;
            let place = if t.remote.is_empty() {
                String::new()
            } else {
                format!(" on {}", t.remote)
            };
            Some(format!("{}{place}", pr.branch))
        })
        .collect();
    if leftover.is_empty() && waiting == 0 {
        return None;
    }
    if leftover.is_empty() {
        return Some(check(
            "queue-pr-branches",
            Status::Ok,
            format!("{waiting} train(s) waiting for their pull request to be merged"),
        ));
    }
    Some(check(
        "queue-pr-branches",
        Status::Warn,
        format!(
            "branches left behind by dissolved pull-request trains: {}; delete each with \
             'git push <remote> --delete <branch>' once its pull request is closed",
            leftover.join(", ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_is_ok_and_protections_warn() {
        let open = protection_check("origin/main", &Protection::Open, Land::Push);
        assert_eq!(open.status, Status::Ok);
        let pr = protection_check(
            "origin/main",
            &Protection::PullRequestRequired("a ruleset requires a pull request".into()),
            Land::Push,
        );
        assert_eq!(pr.status, Status::Warn);
        assert!(pr.detail.contains("cannot land"), "{}", pr.detail);
        let p = protection_check(
            "origin/main",
            &Protection::Protected("status checks".into()),
            Land::Push,
        );
        assert_eq!(p.status, Status::Warn);
        assert_eq!(pr.name, "target-protection");
    }

    #[test]
    fn unknown_says_it_could_not_check_and_stays_ok() {
        let c = protection_check(
            "origin/main",
            &Protection::Unknown("gh is not installed".into()),
            Land::Push,
        );
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("could not check"), "{}", c.detail);
        assert!(c.detail.contains("gh is not installed"), "{}", c.detail);
    }

    #[test]
    fn a_pull_request_landing_expects_the_protection() {
        let pr = protection_check(
            "origin/main",
            &Protection::PullRequestRequired("a ruleset requires a pull request".into()),
            Land::PullRequest,
        );
        assert_eq!(pr.status, Status::Ok);
        assert!(
            pr.detail.contains("through a pull request"),
            "{}",
            pr.detail
        );
    }

    fn train(state: QueueTrainState, branch_deleted: bool) -> sluice_gate_proto::QueueTrainView {
        sluice_gate_proto::QueueTrainView {
            train_id: "t-1".into(),
            remote: "origin".into(),
            state,
            pr: Some(sluice_gate_proto::PullRequestView {
                branch: "sluice/train/t-1".into(),
                branch_deleted,
                ..sluice_gate_proto::PullRequestView::default()
            }),
            ..sluice_gate_proto::QueueTrainView::default()
        }
    }

    #[test]
    fn leftover_branches_are_listed_from_the_store_without_the_network() {
        let none = QueueStatusReport::default();
        assert!(pr_branches_check(&none).is_none());
        let waiting = QueueStatusReport {
            trains: vec![train(QueueTrainState::AwaitingMerge, false)],
            ..QueueStatusReport::default()
        };
        let c = pr_branches_check(&waiting).unwrap();
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("1 train(s) waiting"), "{}", c.detail);
        let left = QueueStatusReport {
            trains: vec![
                train(QueueTrainState::Dissolved, false),
                train(QueueTrainState::Dissolved, true),
            ],
            ..QueueStatusReport::default()
        };
        let c = pr_branches_check(&left).unwrap();
        assert_eq!(c.status, Status::Warn);
        assert!(
            c.detail.contains("sluice/train/t-1 on origin"),
            "{}",
            c.detail
        );
        assert_eq!(c.detail.matches("sluice/train/t-1").count(), 1);
    }
}
