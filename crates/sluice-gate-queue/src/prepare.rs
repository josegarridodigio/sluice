//! `[queue] prepare` (design section 5, step 4b): the command that makes a
//! fresh merge worktree able to run the gates (install the dependencies git
//! does not carry), the job that runs it through the daemon's scheduler, and
//! the base probe that attributes a failure.
//!
//! A prepare failure is somebody's fault only when the base prepares: the
//! runner prepares the base alone in `<state>/merge/<train>-base` and
//! remembers the answer per `(repository, base commit, command)` for the
//! daemon's life.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;
use sluice_gate_core::config::{RepoConfig, Weight};
use sluice_gate_daemon::server::RunSubscription;
use sluice_gate_proto::{
    Caller, GateSpec, OutcomeStatus, RepoInfo, RunRequest, ServerMessage, TrainPrepare,
};

use crate::git::{Git, worktree_git_dir};
use crate::runner::{Core, blocking};
use crate::store::{self, TrainRow};

/// Name of the prepare job in the daemon (gate names match
/// `[a-z0-9][a-z0-9_-]*`, so no gate can collide with it).
pub(crate) const JOB_NAME: &str = "queue:prepare";

/// The base's `[queue] prepare*` settings.
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    /// The command, run with `sh -c`.
    pub(crate) cmd: String,
    /// Its time limit (never zero).
    pub(crate) timeout: Duration,
    /// Its weight, like a gate's.
    pub(crate) weight: Weight,
}

impl Spec {
    /// `None` when the base has no `prepare`.
    pub(crate) fn from_config(config: Option<&RepoConfig>) -> Option<Self> {
        let q = &config?.queue;
        let cmd = q.prepare.as_deref()?.trim();
        (!cmd.is_empty()).then(|| Self {
            cmd: cmd.to_owned(),
            timeout: q.prepare_timeout.max(Duration::from_millis(1)),
            weight: q.prepare_weight,
        })
    }
}

/// How one prepare run ended.
#[derive(Debug, Clone, Default)]
pub(crate) struct Run {
    /// `pass`, `fail`, `timeout`, `killed` or `error`.
    pub(crate) status: &'static str,
    /// The daemon's job.
    pub(crate) job_id: Option<String>,
    /// Run time in milliseconds.
    pub(crate) duration_ms: Option<u64>,
    /// The job log.
    pub(crate) log_path: Option<String>,
    /// Exit code, when the process exited.
    pub(crate) exit_code: Option<i32>,
    /// What the daemon said about the run.
    pub(crate) message: String,
    /// The daemon's `killed_by` (`memory` when the run was killed for lack of
    /// memory; the message then says how much it used).
    pub(crate) killed_by: Option<String>,
}

impl Run {
    /// The view stored on the train with `status` (`dirty` for a pass that
    /// left the tree changed).
    pub(crate) fn view(&self, status: &str) -> TrainPrepare {
        TrainPrepare {
            status: status.to_owned(),
            job_id: self.job_id.clone(),
            duration_ms: self.duration_ms,
            log_path: self.log_path.clone(),
        }
    }

    /// The job `detail` of a failure caused by this run.
    pub(crate) fn detail(&self) -> serde_json::Value {
        let mut detail = json!({
            "job_id": self.job_id,
            "status": self.status,
            "exit_code": self.exit_code,
            "log_path": self.log_path,
            "message": self.message,
        });
        if let (Some(by), Some(map)) = (&self.killed_by, detail.as_object_mut()) {
            map.insert("killed_by".to_owned(), json!(by));
        }
        detail
    }

    /// One line saying why the run failed.
    pub(crate) fn error_text(&self) -> String {
        if self.message.is_empty() {
            match self.exit_code {
                Some(c) => format!("prepare {} (exit code {c})", self.status),
                None => format!("prepare {}", self.status),
            }
        } else {
            format!("prepare {}: {}", self.status, self.message)
        }
    }
}

/// What waiting for a prepare run ended with.
#[derive(Debug)]
pub(crate) enum End {
    /// The job finished.
    Done(Run),
    /// A member of the train was cancelled: the run was killed.
    Cancelled,
}

/// The answer of a base probe.
#[derive(Debug, Clone)]
pub(crate) struct Probe {
    /// The probe's run.
    pub(crate) run: Run,
    /// Files the pass left that the base's tree does not have.
    pub(crate) dirty: Vec<String>,
}

impl Probe {
    /// The base prepares cleanly.
    pub(crate) fn passed(&self) -> bool {
        self.run.status == "pass" && self.dirty.is_empty()
    }
}

/// `<state>/merge/<train>-base`, the probe worktree of `t`.
pub(crate) fn probe_path(t: &TrainRow) -> PathBuf {
    t.worktree.with_file_name(format!("{}-base", t.id))
}

/// Submits the prepare command to run in the worktree `cwd`. The run is
/// killed when the subscription is dropped (the worktree is about to go).
pub(crate) fn submit(
    core: &Core,
    train: &TrainRow,
    spec: &Spec,
    cwd: &Path,
    git_dir: &Path,
) -> RunSubscription {
    let req = RunRequest {
        repo: RepoInfo {
            toplevel: cwd.to_path_buf(),
            common_dir: train.repo.clone(),
            git_dir: git_dir.to_path_buf(),
        },
        gate: GateSpec {
            name: JOB_NAME.to_owned(),
            cmd: spec.cmd.clone(),
            weight: spec
                .weight
                .ceiling(sluice_gate_core::cpus::default_budget()),
            weight_auto: spec.weight.is_auto(),
            timeout_ms: u64::try_from(spec.timeout.as_millis())
                .unwrap_or(u64::MAX)
                .max(1),
            env: BTreeMap::new(),
            cache: false,
            stall_timeout_ms: None,
        },
        key: None,
        tree: None,
        committed_tree: None,
        cmd: spec.cmd.clone(),
        extra: Vec::new(),
        cwd: cwd.to_path_buf(),
        child_env: core.tool_env.clone(),
        probe_env: core.git.env().clone(),
        caller: Caller::Human,
        session: Some(format!("queue:{}", train.id)),
        client_pid: std::process::id(),
        hook: None,
        no_marker: true,
        requested_at: None,
    };
    core.handle.submit_run_owned(req)
}

/// Waits for the run; stops (dropping `sub`, which kills the job) when a
/// member of the train was cancelled. Cancel-safe: dropping the future kills
/// the job too (daemon shutdown).
#[allow(
    clippy::needless_pass_by_value,
    reason = "dropping the subscription on return is what kills the job"
)]
pub(crate) async fn await_run(core: &Core, train: &TrainRow, mut sub: RunSubscription) -> End {
    let tick = core
        .handle
        .config()
        .queue
        .tick
        .max(Duration::from_millis(50));
    // One interval for the whole wait: a `sleep` recreated per iteration would
    // be starved by a steady stream of `progress` messages.
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    let mut job_id = (!sub.job_id().is_empty()).then(|| sub.job_id().to_owned());
    loop {
        tokio::select! {
            msg = sub.recv() => match msg {
                Some(ServerMessage::Done { job_id: id, outcome, .. }) => {
                    let ok = matches!(outcome.status, OutcomeStatus::Pass | OutcomeStatus::Cached);
                    let status = match outcome.status {
                        OutcomeStatus::Pass | OutcomeStatus::Cached => "pass",
                        OutcomeStatus::Fail => "fail",
                        OutcomeStatus::Timeout => "timeout",
                        OutcomeStatus::Killed => "killed",
                        OutcomeStatus::Error | OutcomeStatus::Unknown => "error",
                    };
                    let killed_by = outcome.killed_by.clone().filter(|_| outcome.killed_for_memory());
                    return End::Done(Run {
                        status,
                        job_id: Some(id),
                        duration_ms: Some(outcome.duration_ms),
                        log_path: outcome.log_path.map(|p| p.display().to_string()),
                        exit_code: outcome.exit_code,
                        killed_by,
                        message: if ok { String::new() } else { outcome.message },
                    });
                }
                Some(ServerMessage::Error { message, .. }) => {
                    return End::Done(Run {
                        status: "error",
                        job_id,
                        message,
                        ..Run::default()
                    });
                }
                Some(_) => {
                    if job_id.is_none() && !sub.job_id().is_empty() {
                        job_id = Some(sub.job_id().to_owned());
                    }
                }
                None => {
                    return End::Done(Run {
                        status: "error",
                        job_id,
                        message: "the prepare run ended without a result".to_owned(),
                        ..Run::default()
                    });
                }
            },
            _ = ticker.tick() => {
                let jobs = core.read(|tx| store::train_jobs(tx, train)).unwrap_or_default();
                if jobs.iter().any(|j| j.state == sluice_gate_proto::QueueJobState::Cancelled) {
                    return End::Cancelled;
                }
            }
        }
    }
}

/// `git status --porcelain` of `wt` (untracked files included, ignored ones
/// not): the paths of whatever differs from the commit. Blocking.
pub(crate) fn dirty_paths(git: &Git, wt: &Path) -> Result<Vec<String>> {
    git.status_paths(wt, true)
}

/// Creates the locked probe worktree at the train's base; returns its
/// per-worktree git dir. Blocking.
fn add_probe_worktree(git: &Git, t: &TrainRow, path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        sluice_gate_core::dirs::create_private_dir(parent)?;
    }
    if path.exists() {
        git.remove_worktree(&t.repo, path);
    }
    let p = path.to_string_lossy().into_owned();
    git.repo(&t.repo, &["worktree", "add", "--detach", &p, &t.base_sha])?;
    git.repo(
        &t.repo,
        &["worktree", "lock", "--reason", "sluice merge queue", &p],
    )?;
    worktree_git_dir(git, path)
}

/// Prepares the train's base alone, in `<state>/merge/<train>-base`, and
/// says whether it works: the same command, the same limits, the same
/// cleanliness rule as the train's own run. The answer is never remembered:
/// a pass would blame the members for a later environment failure (registry
/// down, timeout, memory kill) on the same base. `None` when a member was cancelled meanwhile (the probe
/// worktree is gone either way).
///
/// # Errors
///
/// The probe worktree could not be created.
pub(crate) async fn probe_base(
    core: &Arc<Core>,
    train: &TrainRow,
    spec: &Spec,
) -> Result<Option<Arc<Probe>>> {
    let path = probe_path(train);
    let (git, t, p2) = (core.git.clone(), train.clone(), path.clone());
    let added = blocking(move || add_probe_worktree(&git, &t, &p2)).await?;
    let git_dir = match added {
        Ok(d) => d,
        Err(e) => {
            remove(core, train, &path).await;
            return Err(anyhow!("cannot create the probe worktree: {e:#}"));
        }
    };
    let sub = submit(core, train, spec, &path, &git_dir);
    let end = await_run(core, train, sub).await;
    let probe = match end {
        End::Cancelled => None,
        End::Done(run) => {
            let dirty = if run.status == "pass" {
                let (git, wt) = (core.git.clone(), path.clone());
                blocking(move || dirty_paths(&git, &wt))
                    .await?
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            Some(Arc::new(Probe { run, dirty }))
        }
    };
    remove(core, train, &path).await;
    Ok(probe)
}

/// Removes the probe worktree (a failure to is left to the sweep).
async fn remove(core: &Core, train: &TrainRow, path: &Path) {
    let (git, repo, wt) = (core.git.clone(), train.repo.clone(), path.to_path_buf());
    let _ = blocking(move || git.remove_worktree(&repo, &wt)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_blank_prepare_gives_no_spec() {
        let parse = |text: &str| RepoConfig::parse(text, Path::new(".sluice.toml")).unwrap();
        assert!(Spec::from_config(None).is_none());
        assert!(Spec::from_config(Some(&parse("version = 1"))).is_none());
        let c = parse("version = 1\n[queue]\nprepare = \" make deps \"\nprepare_timeout = \"2m\"");
        let spec = Spec::from_config(Some(&c)).unwrap();
        assert_eq!(spec.cmd, "make deps");
        assert_eq!(spec.timeout, Duration::from_mins(2));
        assert!(spec.weight.is_auto());
    }

    #[test]
    fn a_memory_kill_travels_in_the_detail_and_the_error_text() {
        let run = Run {
            status: "killed",
            message: "queue:prepare was killed for lack of memory after 3.0s".to_owned(),
            killed_by: Some("memory".to_owned()),
            ..Run::default()
        };
        assert_eq!(run.detail()["killed_by"], "memory");
        assert!(run.error_text().contains("killed for lack of memory"));
        // Other failures do not grow the key.
        let plain = Run {
            status: "fail",
            ..Run::default()
        };
        assert!(plain.detail().get("killed_by").is_none());
    }

    #[test]
    fn a_run_explains_itself() {
        let run = Run {
            status: "fail",
            exit_code: Some(3),
            ..Run::default()
        };
        assert_eq!(run.error_text(), "prepare fail (exit code 3)");
        let run = Run {
            status: "timeout",
            message: "timed out after 10m".to_owned(),
            ..Run::default()
        };
        assert_eq!(run.error_text(), "prepare timeout: timed out after 10m");
        assert_eq!(run.view("dirty").status, "dirty");
    }
}
