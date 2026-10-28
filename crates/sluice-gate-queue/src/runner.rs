//! The runner (design section 5): one tokio task per daemon, woken by a
//! [`Notify`] (enqueue, retry, approve, cancel) and by the `[queue] tick`.
//! Preparing a group's base (network), building a train, watching a ready
//! train (network) and pushing run as child tasks, each bounded by a
//! deadline, so that one stalled remote or one long gate run never delays
//! other repositories, approvals or the watch of other trains.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use rusqlite::Transaction;
use serde_json::json;
use sluice_gate_core::config::{Gate, REPO_CONFIG_FILE, RepoConfig};
use sluice_gate_core::env::LOGIN_ENV_DEADLINE;
use sluice_gate_core::git::RepoPaths;
use sluice_gate_core::key::CacheKey;
use sluice_gate_core::lookup;
use sluice_gate_core::marker::{Lookup, MarkerStore};
use sluice_gate_core::tree::{self, SnapshotFull, StagedEntries, TreeHash};
use sluice_gate_daemon::server::{DaemonHandle, RunSubscription, StoreError};
use sluice_gate_proto::{
    Caller, GateSpec, OutcomeStatus, QueueJobState as J, QueueTrainState as T, RepoInfo,
    RunRequest, ServerMessage, TrainGate, TrainPrepare,
};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::git::{EnvMap, Git, worktree_git_dir};
use crate::hookcheck;
use crate::model::{TrainHashInput, train_hash};
use crate::prepare;
use crate::push::{self, PushEnd, PushVerdict};
use crate::store::{
    self, Actor, JobEdit, JobRow, MarkerRow, NewTrain, TrainEdit, TrainRow, TrainSettings, Writer,
};
use crate::target::endpoint;
use crate::toolenv::{self, ToolEnvSource};

/// Shared state of the queue inside one daemon.
#[derive(Debug)]
pub struct Core {
    /// The daemon.
    pub handle: DaemonHandle,
    /// Git runner (daemon env without `GIT_*`).
    pub git: Git,
    /// The daemon's sanitised env (the base of `git`; the tool environment is
    /// built on it).
    pub child_env: EnvMap,
    /// Environment of train gates, `prepare`, base probe and `key_inputs`
    /// (ADR-0020): resolved once at start, never the enqueuer's.
    pub tool_env: EnvMap,
    /// Where `tool_env` came from (status, doctor).
    pub tool_env_source: ToolEnvSource,
    /// Wakes the runner.
    pub notify: Notify,
    busy: Mutex<HashSet<String>>,
    /// Last watch of each ready train (one watch per tick).
    watched: Mutex<HashMap<String, Instant>>,
    /// Scratch root for tree snapshots.
    pub scratch: PathBuf,
    /// `<state>/merge`.
    pub merge_dir: PathBuf,
    /// Program spawned for the push instead of `git` (tests only).
    pub push_program: Option<PathBuf>,
}

/// `config_source` of a train built from its member's own `.sluice.toml`
/// (`--trust-member-config`, ADR-0018 section 6).
pub const CONFIG_SOURCE_MEMBER: &str = "member";

/// True when the train runs the configuration of its member's branch, not the
/// base's.
pub(crate) fn is_trusted(t: &TrainRow) -> bool {
    t.config_source.as_deref() == Some(CONFIG_SOURCE_MEMBER)
}

/// What a claim for a trusted job carries: the job (always alone in its
/// train) and the hash of the config blob its train was built from.
#[derive(Debug, Clone)]
struct Trust {
    job: String,
    config_hash: String,
}

/// A merge-queue error message for a request or a log line.
pub(crate) fn store_msg(e: &StoreError) -> String {
    store::refusal(e).map_or_else(|| e.to_string(), ToString::to_string)
}

impl Core {
    /// Builds the core from the daemon handle; `base_env` replaces the
    /// daemon's sanitised env (tests).
    ///
    /// # Errors
    ///
    /// No usable scratch directory.
    pub fn new(handle: DaemonHandle, base_env: Option<EnvMap>) -> Result<Self> {
        Self::with_push_program(handle, base_env, None, None)
    }

    /// Like [`Core::new`], spawning `push_program` instead of `git` for the
    /// push (tests inject a spawn failure this way) and resolving the train
    /// tool environment through `login_shell` instead of the user's (tests).
    ///
    /// # Errors
    ///
    /// No usable scratch directory.
    pub fn with_push_program(
        handle: DaemonHandle,
        base_env: Option<EnvMap>,
        push_program: Option<PathBuf>,
        login_shell: Option<&Path>,
    ) -> Result<Self> {
        let test_env = base_env.is_some();
        let child_env = base_env.unwrap_or_else(|| handle.sanitised_env());
        // Blocks for up to the login-shell deadline: the extension builds the
        // core off the async threads, before the runner starts.
        let tools = toolenv::resolve(
            &handle.config().queue,
            &toolenv::Inputs {
                daemon_env: &child_env,
                test_env,
                login_shell,
                detection: &handle.config().agent_detection,
                deadline: LOGIN_ENV_DEADLINE,
            },
        );
        let git = Git::new(&child_env);
        let scratch = tree::scratch_root(&child_env).context("scratch directory")?;
        let merge_dir = handle.state_dir().join("merge");
        Ok(Self {
            handle,
            git,
            child_env,
            tool_env: tools.env,
            tool_env_source: tools.source,
            notify: Notify::new(),
            busy: Mutex::new(HashSet::new()),
            watched: Mutex::new(HashMap::new()),
            scratch,
            merge_dir,
            push_program,
        })
    }

    /// True when the ready train `train` was not watched during the last
    /// `period`; records the watch.
    fn due_for_watch(&self, train: &str, period: Duration) -> bool {
        let mut w = self
            .watched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if w.get(train)
            .is_some_and(|at| now.duration_since(*at) < period)
        {
            return false;
        }
        w.insert(train.to_owned(), now);
        true
    }

    /// Forgets watches of trains that are no longer ready.
    fn forget_watches(&self, ready: &HashSet<&str>) {
        self.watched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|k, _| ready.contains(k.as_str()));
    }

    fn busy(&self) -> MutexGuard<'_, HashSet<String>> {
        self.busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// True while a build or push task owns the train.
    pub fn is_busy(&self, train: &str) -> bool {
        self.busy().contains(train)
    }

    fn claim_busy(&self, train: &str) -> bool {
        self.busy().insert(train.to_owned())
    }

    fn release_busy(&self, train: &str) {
        self.busy().remove(train);
        self.notify.notify_one();
    }

    /// Read-only access in one transaction.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn read<R>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
    ) -> Result<R, StoreError> {
        self.handle.store_tx(f)
    }

    /// A fenced write in one transaction.
    ///
    /// # Errors
    ///
    /// Store errors, refusals included.
    pub fn write<R>(
        &self,
        actor: Actor,
        f: impl FnOnce(&Writer<'_, '_>) -> rusqlite::Result<R>,
    ) -> Result<R, StoreError> {
        let fence = self.handle.fence();
        self.handle.store_tx(|tx| {
            let w = Writer::new(tx, &fence, actor)?;
            f(&w)
        })
    }

    /// A fenced write committed with `synchronous = FULL` (push marker).
    ///
    /// # Errors
    ///
    /// Store errors, refusals included.
    pub fn write_durable<R>(
        &self,
        actor: Actor,
        f: impl FnOnce(&Writer<'_, '_>) -> rusqlite::Result<R>,
    ) -> Result<R, StoreError> {
        let fence = self.handle.fence();
        self.handle.store_tx_durable(|tx| {
            let w = Writer::new(tx, &fence, actor)?;
            f(&w)
        })
    }

    /// Git with a client's allowlisted env on top.
    pub fn git_for(&self, client: &EnvMap) -> Git {
        self.git.with_client_env(client)
    }
}

/// Runs blocking work (git) off the async threads.
pub(crate) async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
) -> Result<R> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow!("blocking task failed: {e}"))
}

/// The runner loop: returns when the daemon stops.
pub async fn run(core: Arc<Core>) {
    let mut shutdown = core.handle.shutdown();
    let tick = core
        .handle
        .config()
        .queue
        .tick
        .max(Duration::from_millis(50));
    let enabled = core.handle.config().queue.enabled;
    let mut tasks: JoinSet<()> = JoinSet::new();
    // Daemon start already pruned and swept (`on_start`).
    let mut last_maintenance = Instant::now();
    loop {
        if *shutdown.borrow() {
            break;
        }
        if enabled && core.handle.lease_held() && last_maintenance.elapsed() >= MAINTENANCE_PERIOD {
            last_maintenance = Instant::now();
            let c = Arc::clone(&core);
            if let Err(e) = blocking(move || maintenance(&c)).await {
                tracing::warn!("merge queue maintenance failed: {e:#}");
            }
        }
        if enabled
            && core.handle.lease_held()
            && let Err(e) = step(&core, &mut tasks).await
        {
            tracing::warn!("merge queue step failed: {e:#}");
        }
        while tasks.try_join_next().is_some() {}
        tokio::select! {
            () = core.notify.notified() => {}
            () = tokio::time::sleep(tick) => {}
            _ = shutdown.changed() => {}
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

/// Period of the retention prune and of the merge-dir sweep (also run at
/// daemon start).
pub const MAINTENANCE_PERIOD: Duration = Duration::from_secs(3600);

/// Finished trains changed within this window get their merge worktree
/// removed by every step; older leftovers are the sweep's.
const HOUSEKEEPING_WINDOW_MS: u64 = 3_600_000;

/// Retention (user config `[queue] retention`) and the sweep of
/// `<state>/merge/` leftovers. Blocking.
pub fn maintenance(core: &Core) {
    let retention = core.handle.config().queue.retention;
    let keep_ms = u64::try_from(retention.as_millis()).unwrap_or(u64::MAX);
    let cutoff = sluice_gate_core::dirs::now_ms().saturating_sub(keep_ms);
    match core.write(Actor::Runner, |w| w.prune(cutoff)) {
        Ok(p) if p != store::Pruned::default() => tracing::info!(
            jobs = p.jobs,
            trains = p.trains,
            markers = p.markers,
            events = p.events,
            "merge queue retention: pruned finished rows"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!("merge queue retention failed: {}", store_msg(&e)),
    }
    sweep_merge_dir(core);
}

/// Removes `<state>/merge/*` leftovers not owned by a live train. Blocking.
pub fn sweep_merge_dir(core: &Core) {
    let Ok(entries) = std::fs::read_dir(&core.merge_dir) else {
        return;
    };
    let trains = core.read(|tx| store::trains(tx, None)).unwrap_or_default();
    for e in entries.flatten() {
        let path = e.path();
        // A train's worktree or its base probe (`<train>-base`).
        let owner = trains
            .iter()
            .find(|t| t.worktree == path || prepare::probe_path(t) == path);
        match owner {
            Some(t) if crate::model::train_is_active(t.state) || core.is_busy(&t.id) => {}
            Some(t) => core.git.remove_worktree(&t.repo, &path),
            None => {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

async fn step(core: &Arc<Core>, tasks: &mut JoinSet<()>) -> Result<()> {
    let since = sluice_gate_core::dirs::now_ms().saturating_sub(HOUSEKEEPING_WINDOW_MS);
    let (trains, finished) = core
        .read(|tx| {
            Ok((
                store::live_trains(tx, None)?,
                store::finished_trains_since(tx, since)?,
            ))
        })
        .map_err(|e| anyhow!(store_msg(&e)))?;
    // Housekeeping: worktrees of recently finished trains.
    for t in &finished {
        for wt in [t.worktree.clone(), prepare::probe_path(t)] {
            if wt.exists() && !core.is_busy(&t.id) {
                let (git, repo) = (core.git.clone(), t.repo.clone());
                blocking(move || git.remove_worktree(&repo, &wt)).await?;
            }
        }
    }
    let ready: HashSet<&str> = trains
        .iter()
        .filter(|t| t.state == T::Ready)
        .map(|t| t.id.as_str())
        .collect();
    core.forget_watches(&ready);
    // Approved trains: push. Ready trains: cheap re-check, at most once per
    // tick. Both run as child tasks (network calls with a deadline).
    let tick = core
        .handle
        .config()
        .queue
        .tick
        .max(Duration::from_millis(50));
    for t in trains
        .iter()
        .filter(|t| matches!(t.state, T::Ready | T::Approved))
    {
        if t.state == T::Ready && !core.due_for_watch(&t.id, tick) {
            continue;
        }
        if !core.claim_busy(&t.id) {
            continue;
        }
        let (c, t2) = (Arc::clone(core), t.clone());
        if t.state == T::Approved {
            tasks.spawn(async move {
                if let Err(e) = push_train(&c, &t2.id).await {
                    tracing::warn!(train = %t2.id, "push failed: {e:#}");
                }
                c.release_busy(&t2.id);
            });
        } else {
            tasks.spawn(async move {
                watch_ready(&c, &t2).await;
                c.release_busy(&t2.id);
            });
        }
    }
    claim_next(core, &trains, tasks)
}

/// Ready train: dissolve when a member was cancelled or the target moved.
/// The target is read at the push URL bound to the train; when that URL
/// changed the train is left alone (the pre-push check dissolves it with
/// `endpoint_changed` if it is approved).
async fn watch_ready(core: &Arc<Core>, t: &TrainRow) {
    let Ok(jobs) = core.read(|tx| store::train_jobs(tx, t)) else {
        return;
    };
    let ready: Vec<&JobRow> = jobs.iter().filter(|j| j.state == J::Ready).collect();
    let lost_member = ready.is_empty() || jobs.iter().any(|j| j.state == J::Cancelled);
    let note = if lost_member {
        Some("member cancelled".to_owned())
    } else {
        let env = ready.first().map(|j| j.env.clone()).unwrap_or_default();
        let git = core
            .git_for(&env)
            .with_network_timeout(t.settings.network_timeout());
        let t2 = t.clone();
        let current = blocking(move || {
            let url = bound_url(&git, &t2)?;
            target_at(&git, &t2, &url)
        })
        .await;
        match current {
            Ok(Ok(cur)) if cur.as_deref() != Some(t.base_sha.as_str()) => {
                Some("target moved".to_owned())
            }
            Ok(Err(e)) => {
                tracing::debug!(train = %t.id, "ready watch skipped: {e:#}");
                None
            }
            _ => None,
        }
    };
    if let Some(note) = note {
        tracing::info!(train = %t.id, "dissolving ready train: {note}");
        if let Err(e) = core.write(Actor::Runner, |w| w.dissolve(t, t.state, &note, false)) {
            tracing::debug!(train = %t.id, "dissolve refused: {}", store_msg(&e));
        }
        core.notify.notify_one();
    }
}

/// The push URL of the train's remote when it still has the endpoint the
/// train is bound to (`""` for a local target).
///
/// # Errors
///
/// `endpoint_changed`, or the remote cannot be resolved.
fn bound_url(git: &Git, t: &TrainRow) -> Result<String> {
    let (url, hash) = endpoint(git, &t.repo, &t.remote)?;
    if hash != t.endpoint_hash {
        anyhow::bail!("endpoint_changed");
    }
    Ok(url)
}

/// Current SHA of the target: `ls-remote <push url>` (with the train's
/// deadline) for a remote, the local ref otherwise.
fn target_at(git: &Git, t: &TrainRow, url: &str) -> Result<Option<String>> {
    let head = format!("refs/heads/{}", t.target);
    if t.remote.is_empty() {
        return git.ref_sha(&t.repo, &head);
    }
    Ok(git.ls_remote(&t.repo, url, &[&head])?.get(&head).cloned())
}

/// Busy key of a (repo, remote, target) group while its base is prepared.
fn group_key(group: &(PathBuf, String, String)) -> String {
    format!(
        "group:{}\u{0}{}\u{0}{}",
        group.0.display(),
        group.1,
        group.2
    )
}

/// For every claimable (repo, remote, target), oldest first, spawns a task
/// that prepares the base and trusted config (step 2, network with a
/// deadline), claims up to `max_train` jobs and builds the train. Groups
/// never wait for each other.
fn claim_next(core: &Arc<Core>, live: &[TrainRow], tasks: &mut JoinSet<()>) -> Result<()> {
    let queued = core
        .read(|tx| store::jobs_in_state(tx, J::Queued))
        .map_err(|e| anyhow!(store_msg(&e)))?;
    let mut seen: HashSet<(PathBuf, String, String)> = HashSet::new();
    for first in &queued {
        let group = (
            first.repo.clone(),
            first.remote.clone(),
            first.target.clone(),
        );
        if !seen.insert(group.clone()) {
            continue;
        }
        let occupied = live.iter().any(|t| {
            crate::model::train_is_active(t.state)
                && t.repo == group.0
                && t.remote == group.1
                && t.target == group.2
        });
        if occupied {
            continue;
        }
        // A base that failed is retried at most once per tick, whatever
        // wakes the runner, so the attempts before `failed` span real time.
        if is_base_failure(first.reason.as_deref())
            && sluice_gate_core::dirs::now_ms().saturating_sub(first.updated_at) < tick_ms(core)
        {
            continue;
        }
        let key = group_key(&group);
        if !core.claim_busy(&key) {
            continue;
        }
        let ids: Vec<String> = queued
            .iter()
            .filter(|j| j.repo == group.0 && j.remote == group.1 && j.target == group.2)
            .map(|j| j.id.clone())
            .collect();
        let (c, first) = (Arc::clone(core), first.clone());
        tasks.spawn(async move {
            let claimed = prepare_and_claim(&c, &first, &group, &ids).await;
            c.release_busy(&key);
            if let Some((train, config)) = claimed
                && c.claim_busy(&train.id)
            {
                let id = train.id.clone();
                build(&c, train, config).await;
                c.release_busy(&id);
            }
        });
    }
    Ok(())
}

/// The `[queue] network_timeout` for preparing a group's base, read from a
/// trusted commit of the target (Sluice's last fetched base, else the
/// remote-tracking branch), never from a member; the default otherwise.
fn trusted_network_timeout(git: &Git, j: &JobRow) -> Duration {
    let default = sluice_gate_core::config::DEFAULT_NETWORK_TIMEOUT;
    if j.remote.is_empty() {
        return default;
    }
    for r in [
        format!("refs/sluice/base/{}/{}", j.remote, j.target),
        format!("refs/remotes/{}/{}", j.remote, j.target),
    ] {
        if let Ok(Some(sha)) = git.ref_sha(&j.repo, &r) {
            return match read_config(git, &j.repo, &sha) {
                Ok(Some(c)) => c.queue.network_timeout,
                _ => default,
            };
        }
    }
    default
}

/// The configuration of a trusted job's own branch; `None` when it cannot be
/// used (an invalid file fails the job with `member_config_invalid`).
async fn member_config(core: &Arc<Core>, first: &JobRow) -> Option<crate::trust::MemberConfig> {
    let (git, j) = (core.git.clone(), first.clone());
    match blocking(move || crate::trust::read_member_config(&git, &j.repo, &j.head_sha)).await {
        Ok(Ok(m)) => Some(m),
        Ok(Err(e)) => {
            tracing::warn!(job = %first.id, "member config invalid: {e:#}");
            let detail = json!({ "error": format!("{e:#}") });
            let failed = core.write(Actor::Runner, |w| {
                w.job_to(
                    &first.id,
                    J::Queued,
                    J::Failed,
                    &JobEdit {
                        reason: Some(Some("member_config_invalid".into())),
                        detail: Some(Some(detail)),
                        note: Some("member_config_invalid".into()),
                        ..JobEdit::default()
                    },
                )
            });
            if let Err(err) = failed {
                tracing::debug!("recording the failure refused: {}", store_msg(&err));
            }
            None
        }
        Err(e) => {
            tracing::warn!(job = %first.id, "reading the member config failed: {e:#}");
            None
        }
    }
}

/// Step 2 for one group, then step 1's claim. `None` when nothing was
/// claimed (the failure is recorded on the queued jobs).
async fn prepare_and_claim(
    core: &Arc<Core>,
    first: &JobRow,
    group: &(PathBuf, String, String),
    ids: &[String],
) -> Option<(TrainRow, Option<RepoConfig>)> {
    // A trusted job's train runs its own branch's configuration: read and
    // validated first, a broken file fails the job (no retry loop).
    let member = if first.trust_config {
        Some(member_config(core, first).await?)
    } else {
        None
    };
    let trusted = member.is_some();
    let (git, j) = (core.git_for(&first.env), first.clone());
    let prepared = blocking(move || {
        let timeout = trusted_network_timeout(&git, &j);
        prepare(&git.with_network_timeout(timeout), &j, !trusted)
    })
    .await;
    let (base_sha, endpoint_hash, base_config) = match prepared {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::warn!(job = %first.id, "cannot prepare a train for {}: {e}", first.target);
            if let Err(err) = core.write(Actor::Runner, |w| record_unprepared(w, ids, &e)) {
                tracing::debug!("recording the failure refused: {}", store_msg(&err));
            }
            return None;
        }
        Err(e) => {
            tracing::warn!(job = %first.id, "prepare task failed: {e:#}");
            return None;
        }
    };
    let id = train_id();
    // The base's gates and `[queue]`, or the member's for a trusted job
    // (`remote` and `target` stay as resolved at enqueue).
    let (config, trust) = match member {
        Some(m) => (
            m.config,
            Some(Trust {
                job: first.id.clone(),
                config_hash: m.hash,
            }),
        ),
        None => (base_config, None),
    };
    let queue = config.as_ref().map(|c| c.queue.clone()).unwrap_or_default();
    let settings = TrainSettings {
        push_timeout_ms: u64::try_from(queue.push_timeout.as_millis()).unwrap_or(u64::MAX),
        audit_ref: queue.audit_ref,
        network_timeout_ms: u64::try_from(queue.network_timeout.as_millis()).unwrap_or(u64::MAX),
    };
    let max = if trust.is_some() {
        1
    } else {
        queue.max_train.max(1) as usize
    };
    let worktree = core.merge_dir.join(&id);
    let claimed = core.write(Actor::Runner, |w| {
        claim(
            w,
            group,
            max,
            &id,
            &base_sha,
            &endpoint_hash,
            &worktree,
            settings,
            trust.as_ref(),
        )
    });
    match claimed {
        Ok(Some(t)) => {
            tracing::info!(train = %t.id, members = ?t.members, base = %base_sha, "train claimed");
            Some((t, config))
        }
        Ok(None) => None,
        Err(e) => {
            tracing::debug!("claim refused: {}", store_msg(&e));
            None
        }
    }
}

/// `t-<unix-ms>-<6 hex>`.
fn train_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "t-{}-{:06x}",
        now.as_millis(),
        (now.subsec_nanos() ^ std::process::id()) & 0xff_ffff
    )
}

type Prepared = (String, String, Option<RepoConfig>);

/// Consecutive failed attempts to prepare a base after which the waiting
/// jobs become `failed` (design section 7).
pub const MAX_PREPARE_FAILURES: u32 = 3;

/// Why step 2 could not produce a base for a group of queued jobs.
#[derive(Debug)]
pub enum PrepareError {
    /// The fetch (or the local branch, or the endpoint) failed: reason
    /// `base_unavailable`.
    Unavailable(String),
    /// The base commit's `.sluice.toml` does not parse: reason
    /// `base_config_invalid`.
    ConfigInvalid(String),
}

impl PrepareError {
    /// The job `reason` recorded for this failure.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "base_unavailable",
            Self::ConfigInvalid(_) => "base_config_invalid",
        }
    }

    /// The git or config error, as recorded in the job `detail`.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(m) | Self::ConfigInvalid(m) => m,
        }
    }
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason(), self.message())
    }
}

/// Step 2: base SHA (for a remote: fetched from the push URL, with the
/// deadline of `git`), endpoint hash, config of the base (`read_config`: not
/// for a trusted job, whose train does not use the base's config, so a base
/// whose config is broken does not stand in its way).
fn prepare(git: &Git, j: &JobRow, read_config_of_base: bool) -> Result<Prepared, PrepareError> {
    let unavailable = |e: anyhow::Error| PrepareError::Unavailable(format!("{e:#}"));
    let (url, endpoint) = endpoint(git, &j.repo, &j.remote).map_err(unavailable)?;
    let base_sha = if j.remote.is_empty() {
        git.ref_sha(&j.repo, &format!("refs/heads/{}", j.target))
            .map_err(unavailable)?
            .ok_or_else(|| {
                PrepareError::Unavailable(format!("local branch {} does not exist", j.target))
            })?
    } else {
        let base_ref = format!("refs/sluice/base/{}/{}", j.remote, j.target);
        let spec = format!("+refs/heads/{}:{base_ref}", j.target);
        git.fetch(&j.repo, &url, &spec).map_err(unavailable)?;
        git.ref_sha(&j.repo, &base_ref)
            .map_err(unavailable)?
            .ok_or_else(|| PrepareError::Unavailable(format!("fetch did not produce {base_ref}")))?
    };
    let config = if read_config_of_base {
        read_config(git, &j.repo, &base_sha)
            .map_err(|e| PrepareError::ConfigInvalid(format!("{e:#}")))?
    } else {
        None
    };
    Ok((base_sha, endpoint, config))
}

/// True for the reasons a queued job carries while no train can be built for
/// it for a reason that is nobody's fault: the base cannot be prepared, or
/// the key inputs fail in the merge worktree.
fn is_base_failure(reason: Option<&str>) -> bool {
    matches!(
        reason,
        Some("base_unavailable" | "base_config_invalid" | "key_input_failed" | "prepare_failed")
    )
}

/// The user config's `[queue] tick` in milliseconds.
fn tick_ms(core: &Core) -> u64 {
    u64::try_from(core.handle.config().queue.tick.as_millis()).unwrap_or(u64::MAX)
}

/// Records a failed step 2 on every queued job of the group (they stay
/// `queued` with the reason); after [`MAX_PREPARE_FAILURES`] consecutive
/// failures a job becomes `failed` with that reason.
fn record_unprepared(w: &Writer<'_, '_>, ids: &[String], e: &PrepareError) -> rusqlite::Result<()> {
    for id in ids {
        record_queued_failure(w, id, e.reason(), e.message())?;
    }
    Ok(())
}

/// Records `reason` on the queued job `id`; the third consecutive failure
/// makes it `failed`.
fn record_queued_failure(
    w: &Writer<'_, '_>,
    id: &str,
    reason: &str,
    error: &str,
) -> rusqlite::Result<()> {
    record_queued_failure_with(w, id, reason, error, &serde_json::Value::Null)
}

/// [`record_queued_failure`] with more keys in the recorded detail.
fn record_queued_failure_with(
    w: &Writer<'_, '_>,
    id: &str,
    reason: &str,
    error: &str,
    extra: &serde_json::Value,
) -> rusqlite::Result<()> {
    // `None`: claimed or cancelled meanwhile, nothing to record.
    let Some(failures) = w.note_queued_failure_with(id, reason, error, extra)? else {
        return Ok(());
    };
    if failures >= MAX_PREPARE_FAILURES {
        w.job_to(
            id,
            J::Queued,
            J::Failed,
            &JobEdit {
                note: Some(format!("{reason} after {failures} attempts")),
                ..JobEdit::default()
            },
        )?;
    }
    Ok(())
}

/// `git show <rev>:.sluice.toml` parsed with `RepoConfig::parse`; `None`
/// when the base has no config.
///
/// # Errors
///
/// An invalid config.
pub fn read_config(git: &Git, common: &Path, rev: &str) -> Result<Option<RepoConfig>> {
    let spec = format!("{rev}:{REPO_CONFIG_FILE}");
    let out = git.repo_out(common, &["show", &spec])?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let path = PathBuf::from(format!("{rev}:{REPO_CONFIG_FILE}"));
    Ok(Some(
        RepoConfig::parse(&text, &path).context("invalid .sluice.toml on the base")?,
    ))
}

/// Step 1, in one transaction: re-reads the queued jobs of the group, takes
/// up to `max` of them (a `solo` or trusted job alone, and a batch stops
/// before one), creates the train, moves them to `merging`. `trust` is the
/// trusted job whose config the caller read: when the first job of the group
/// is not exactly that one (or is trusted without `trust`), nothing is
/// claimed, because the settings passed in belong to another train.
#[allow(clippy::too_many_arguments)]
fn claim(
    w: &Writer<'_, '_>,
    group: &(PathBuf, String, String),
    max: usize,
    id: &str,
    base_sha: &str,
    endpoint: &str,
    worktree: &Path,
    settings: TrainSettings,
    trust: Option<&Trust>,
) -> rusqlite::Result<Option<TrainRow>> {
    let tx = w.tx();
    let busy = store::trains(tx, Some(&group.0))?.into_iter().any(|t| {
        crate::model::train_is_active(t.state) && t.remote == group.1 && t.target == group.2
    });
    if busy {
        return Ok(None);
    }
    let mut members = Vec::new();
    // Jobs whose last train failed on its key inputs or its prepare keep that
    // reason and its failure count through the claim: the count spans the
    // attempts.
    let mut counting: HashSet<String> = HashSet::new();
    for j in store::jobs_in_state(tx, J::Queued)? {
        if j.repo != group.0 || j.remote != group.1 || j.target != group.2 {
            continue;
        }
        if matches!(
            j.reason.as_deref(),
            Some("key_input_failed" | "prepare_failed")
        ) {
            counting.insert(j.id.clone());
        }
        if members.is_empty() {
            let actual = j.trust_config.then_some(j.id.as_str());
            if actual != trust.map(|t| t.job.as_str()) {
                return Ok(None);
            }
        }
        if j.trust_config {
            if members.is_empty() {
                members.push(j.id);
            }
            break;
        }
        if j.solo {
            if members.is_empty() {
                members.push(j.id);
            }
            break;
        }
        members.push(j.id);
        if members.len() >= max {
            break;
        }
    }
    if members.is_empty() {
        return Ok(None);
    }
    w.insert_train(&NewTrain {
        id: id.to_owned(),
        repo: group.0.clone(),
        remote: group.1.clone(),
        target: group.2.clone(),
        endpoint_hash: endpoint.to_owned(),
        base_sha: base_sha.to_owned(),
        worktree: worktree.to_path_buf(),
        members: members.clone(),
        settings,
        config_source: trust.map(|_| CONFIG_SOURCE_MEMBER.to_owned()),
        config_hash: trust.map(|t| t.config_hash.clone()),
    })?;
    for m in &members {
        let keep = counting.contains(m);
        w.job_to(
            m,
            J::Queued,
            J::Merging,
            &JobEdit {
                train_id: Some(Some(id.to_owned())),
                reason: (!keep).then_some(None),
                detail: (!keep).then_some(None),
                ..JobEdit::default()
            },
        )?;
    }
    store::train(tx, id)
}

// ---------------------------------------------------------------------------
// Build: worktree, merge, key, gates, outcome (steps 3–7)
// ---------------------------------------------------------------------------

/// Result of steps 3–4.
#[derive(Debug, Default)]
pub(crate) struct Merged {
    /// `S`.
    pub(crate) s: String,
    /// `T`.
    pub(crate) t: String,
    /// Members that merged cleanly, with their `config_changed` flag.
    pub(crate) survivors: Vec<(String, bool)>,
    /// Members that did not: (job, reason, paths).
    pub(crate) conflicts: Vec<(String, &'static str, Vec<String>)>,
}

async fn build(core: &Arc<Core>, train: TrainRow, config: Option<RepoConfig>) {
    let id = train.id.clone();
    if let Err(e) = build_inner(core, &train, config).await {
        tracing::warn!(train = %id, "train failed: {e:#}");
        let msg = format!("{e:#}");
        let r = core.write(Actor::Runner, |w| {
            let Some(t) = store::train(w.tx(), &id)? else {
                return Ok(());
            };
            if !matches!(t.state, T::Building | T::Verifying) {
                return Ok(());
            }
            for j in store::train_jobs(w.tx(), &t)? {
                if matches!(j.state, J::Merging | J::Verifying) {
                    w.job_to(
                        &j.id,
                        j.state,
                        J::Failed,
                        &JobEdit {
                            reason: Some(Some("internal".into())),
                            detail: Some(Some(json!({"error": msg}))),
                            ..JobEdit::default()
                        },
                    )?;
                }
            }
            w.dissolve(&t, t.state, "internal error", false)
        });
        if let Err(e) = r {
            tracing::warn!(train = %id, "cannot record the failure: {}", store_msg(&e));
        }
    }
    let t2 = core.read(|tx| store::train(tx, &id)).ok().flatten();
    if let Some(t) = t2.filter(|t| t.state == T::Dissolved) {
        let (git, repo, wt) = (core.git.clone(), t.repo.clone(), t.worktree.clone());
        let _ = blocking(move || git.remove_worktree(&repo, &wt)).await;
    }
}

#[allow(clippy::too_many_lines, reason = "steps 4-7 of the design, in order")]
async fn build_inner(core: &Arc<Core>, train: &TrainRow, config: Option<RepoConfig>) -> Result<()> {
    let jobs = core
        .read(|tx| store::train_jobs(tx, train))
        .map_err(|e| anyhow!(store_msg(&e)))?;
    let members: Vec<(String, String, String)> = jobs
        .iter()
        .filter(|j| j.state == J::Merging)
        .map(|j| (j.id.clone(), j.branch.clone(), j.head_sha.clone()))
        .collect();
    let (git, t) = (core.git.clone(), train.clone());
    let merged = blocking(move || merge_members(&git, &t, &members)).await??;

    // Record merge results.
    let id = train.id.clone();
    let survivors = merged.survivors.clone();
    let done = core
        .write(Actor::Runner, |w| {
            // A member cancelled while merging: drop the train, re-queue the rest.
            let t = store::train(w.tx(), &id)?
                .ok_or_else(|| store::refuse(store::Refusal::NotFound(id.clone())))?;
            if store::train_jobs(w.tx(), &t)?
                .iter()
                .any(|j| j.state == J::Cancelled)
            {
                w.dissolve(&t, T::Building, "member cancelled", false)?;
                return Ok(true);
            }
            for (job, reason, paths) in &merged.conflicts {
                w.job_to(
                    job,
                    J::Merging,
                    J::Conflict,
                    &JobEdit {
                        reason: Some(Some((*reason).to_owned())),
                        detail: Some(Some(json!({ "paths": paths }))),
                        note: Some((*reason).to_owned()),
                        ..JobEdit::default()
                    },
                )?;
            }
            if survivors.is_empty() {
                w.dissolve(&t, T::Building, "no member merged", false)?;
                return Ok(true);
            }
            for (job, config_changed) in &survivors {
                // Keeps the key-input failure count a claim carried over.
                let existing = store::job(w.tx(), job)?.and_then(|j| j.detail);
                let detail = if *config_changed {
                    Some(merge_detail(
                        existing.as_ref(),
                        &json!({ "config_changed": true }),
                    ))
                } else {
                    existing
                };
                w.job_to(
                    job,
                    J::Merging,
                    J::Verifying,
                    &JobEdit {
                        detail: Some(detail),
                        ..JobEdit::default()
                    },
                )?;
            }
            w.train_to(
                &id,
                T::Building,
                T::Verifying,
                &TrainEdit {
                    merge_sha: Some(merged.s.clone()),
                    merge_tree: Some(merged.t.clone()),
                    ..TrainEdit::default()
                },
            )?;
            Ok(false)
        })
        .map_err(|e| anyhow!(store_msg(&e)))?;
    if done {
        return Ok(());
    }

    // Step 5: the clean merge worktree must snapshot to T.
    let wt_git = {
        let (git, wt) = (core.git.clone(), train.worktree.clone());
        blocking(move || worktree_git_dir(&git, &wt)).await??
    };
    let paths = RepoPaths {
        toplevel: train.worktree.clone(),
        git_dir: wt_git.clone(),
        common_dir: train.repo.clone(),
        index_file: wt_git.join("index"),
        objects_dir: train.repo.join("objects"),
    };
    // The staged entries of the merge worktree feed path-scoped keys
    // (design path-scoped-keys.md, section 7): no extra git spawn.
    let entries = match snapshot_of(core, &paths, &merged.t).await? {
        Ok(entries) => entries,
        Err(problem) => {
            return fail_survivors(
                core,
                train,
                "tree_mismatch",
                &json!({ "error": problem.why() }),
            )
            .map_err(|e| anyhow!(store_msg(&e)));
        }
    };

    // What the push's hook will read must not ask for gates this train skips.
    if reject_hook_gates(core, train, &merged, config.as_ref()).await? {
        return Ok(());
    }

    // Step 5–6: prepare, keys, markers, runs.
    let gates = select_gates(config.as_ref());
    let tree_hash = sluice_gate_core::tree::TreeHash::try_from(merged.t.clone())
        .map_err(|e| anyhow!("invalid tree {}: {e}", merged.t))?;
    let mut derived = derive_views(core, train, &gates, &tree_hash, &entries).await?;
    if let Some(spec) = prepare::Spec::from_config(config.as_ref()) {
        // Fast path: an already verified tree costs nothing, not even the
        // installation of its dependencies.
        let all_hit = matches!(&derived, Ok((_, to_run)) if to_run.is_empty());
        if !all_hit {
            let step = prepare_train(core, train, &spec, &merged, &paths).await?;
            let Some(entries) = step else {
                return Ok(());
            };
            // The keys may need what the prepare installed.
            derived = derive_views(core, train, &gates, &tree_hash, &entries).await?;
        }
    }
    let (mut views, to_run) = match derived {
        Ok(d) => d,
        Err((gate, error)) => return key_input_failed(core, train, &gate, &error),
    };
    // The keys derived: a reason left by an earlier failed attempt is stale.
    if jobs.iter().any(|j| {
        matches!(
            j.reason.as_deref(),
            Some("key_input_failed" | "prepare_failed")
        )
    }) {
        let tid = train.id.clone();
        core.write(Actor::Runner, |w| clear_key_failures(w, &tid))
            .map_err(|e| anyhow!(store_msg(&e)))?;
    }
    let ran: Vec<String> = to_run.iter().map(|(_, n, _, _)| n.clone()).collect();
    let mut failure: Option<Failure> = None;
    if !to_run.is_empty() {
        let mut subs: Vec<(usize, RunSubscription)> = Vec::new();
        for (idx, name, gate, key) in to_run {
            let req = RunRequest {
                repo: RepoInfo {
                    toplevel: train.worktree.clone(),
                    common_dir: train.repo.clone(),
                    git_dir: wt_git.clone(),
                },
                gate: GateSpec {
                    name: name.clone(),
                    cmd: gate.cmd.clone(),
                    // The daemon resolves "auto" against its own budget.
                    weight: gate
                        .weight
                        .ceiling(sluice_gate_core::cpus::default_budget()),
                    weight_auto: gate.weight.is_auto(),
                    timeout_ms: u64::try_from(gate.timeout.as_millis()).unwrap_or(u64::MAX),
                    env: gate.env.clone(),
                    cache: gate.cache,
                    stall_timeout_ms: gate.stall_timeout.to_wire(),
                },
                key,
                tree: Some(tree_hash.clone()),
                committed_tree: Some(tree_hash.clone()),
                cmd: gate.cmd.trim().to_owned(),
                extra: Vec::new(),
                cwd: train.worktree.clone(),
                child_env: core.tool_env.clone(),
                probe_env: core.git.env().clone(),
                caller: Caller::Human,
                session: Some(format!("queue:{}", train.id)),
                client_pid: std::process::id(),
                hook: None,
                no_marker: false,
                requested_at: None,
            };
            // Killed when the train drops it, whatever `last_subscriber_gone`
            // says: the worktree it runs in is about to be removed.
            let sub = core.handle.submit_run_owned(req);
            views[idx].job_id = (!sub.job_id().is_empty()).then(|| sub.job_id().to_owned());
            "running".clone_into(&mut views[idx].status);
            subs.push((idx, sub));
        }
        let v2 = views.clone();
        let _ = core.write(Actor::Runner, |w| {
            w.train_to(
                &train.id,
                T::Verifying,
                T::Verifying,
                &TrainEdit {
                    gates: Some(v2),
                    ..TrainEdit::default()
                },
            )
        });
        failure = await_gates(core, train, subs, &mut views).await;
        if failure.is_none() {
            failure = modified_tree(core, train, &ran).await?;
        }
    }

    // Step 7: outcome, decided inside the write transaction so that a
    // cancel landing between the gates' end and this write cannot turn into
    // an internal failure of the other members.
    let id = train.id.clone();
    let outcome = core
        .write(Actor::Runner, |w| {
            finish_train(w, &id, &merged, &views, failure.as_ref())
        })
        .map_err(|e| anyhow!(store_msg(&e)))?;
    tracing::info!(train = %id, ?outcome, "train verified");
    Ok(())
}

/// Build-time check of the merged `.sluice.toml` (what the push's `pre-push`
/// hook will read): it must not ask for gates the train does not verify.
/// Members that changed the configuration fail (`hook_gate_unverified`, or
/// `member_config_invalid` when it does not parse); the others are requeued,
/// not split. True when the train ended here.
async fn reject_hook_gates(
    core: &Arc<Core>,
    train: &TrainRow,
    merged: &Merged,
    config: Option<&RepoConfig>,
) -> Result<bool> {
    let wt = train.worktree.clone();
    let Some(text) = blocking(move || hookcheck::read_merged(&wt)).await?? else {
        return Ok(false);
    };
    let verified: Vec<String> = select_gates(config).into_iter().map(|(n, _)| n).collect();
    let Some(problem) = hookcheck::check(&text, &verified) else {
        return Ok(false);
    };
    let changed: HashSet<&str> = merged
        .survivors
        .iter()
        .filter(|(_, c)| *c)
        .map(|(j, _)| j.as_str())
        .collect();
    if changed.is_empty() {
        // No member touched the configuration: nobody to blame.
        tracing::warn!(train = %train.id, "merged config problem with no member changing it: {problem:?}");
        return Ok(false);
    }
    let (reason, what) = match &problem {
        hookcheck::Problem::Invalid(_) => (
            "member_config_invalid",
            "the merged .sluice.toml does not parse".to_owned(),
        ),
        hookcheck::Problem::Unverified(names) => (
            "hook_gate_unverified",
            format!(
                "the merged .sluice.toml asks the pre-push hook for gates the train does not verify: {}",
                names.join(", ")
            ),
        ),
    };
    let id = train.id.clone();
    core.write(Actor::Runner, |w| {
        let t = store::train(w.tx(), &id)?
            .ok_or_else(|| store::refuse(store::Refusal::NotFound(id.clone())))?;
        let jobs = store::train_jobs(w.tx(), &t)?;
        let alive: Vec<&JobRow> = jobs.iter().filter(|j| j.state == J::Verifying).collect();
        if alive.len() != merged.survivors.len() {
            return w.dissolve(&t, T::Verifying, "member cancelled", false);
        }
        for j in alive.iter().filter(|j| changed.contains(j.id.as_str())) {
            let detail = match &problem {
                hookcheck::Problem::Invalid(error) => json!({ "error": error }),
                hookcheck::Problem::Unverified(names) => json!({
                    "gates": names,
                    "hint": format!(
                        "land the configuration change first, or a human can run \
                         `sluice queue retry {} --trust-member-config`",
                        j.id
                    ),
                }),
            };
            w.job_to(
                &j.id,
                J::Verifying,
                J::Failed,
                &JobEdit {
                    reason: Some(Some(reason.into())),
                    detail: Some(Some(merge_detail(j.detail.as_ref(), &detail))),
                    note: Some(what.clone()),
                    ..JobEdit::default()
                },
            )?;
        }
        // The members that did not change the configuration go back to the queue.
        w.dissolve(&t, T::Verifying, &what, false)
    })
    .map_err(|e| anyhow!(store_msg(&e)))?;
    tracing::info!(train = %id, reason, "{what}");
    Ok(true)
}

/// Why the merge worktree does not snapshot to `T`.
#[derive(Debug)]
enum SnapshotProblem {
    /// It snapshots to another tree (`root`).
    Differs(String),
    /// The snapshot could not be taken.
    Unknown(String),
}

impl SnapshotProblem {
    fn why(&self) -> String {
        match self {
            Self::Differs(why) | Self::Unknown(why) => why.clone(),
        }
    }
}

/// The staged entries of the merge worktree when it snapshots to `t`.
async fn snapshot_of(
    core: &Arc<Core>,
    paths: &RepoPaths,
    t: &str,
) -> Result<Result<Arc<StagedEntries>, SnapshotProblem>> {
    let (p2, env, scratch) = (paths.clone(), core.git.env().clone(), core.scratch.clone());
    let snap = blocking(move || tree::snapshot_entries(&p2, &env, &scratch)).await?;
    Ok(match snap {
        Ok(SnapshotFull::Known { root, entries }) if root.as_str() == t => Ok(Arc::new(entries)),
        Ok(SnapshotFull::Known { root, .. }) => {
            Err(SnapshotProblem::Differs(format!("snapshot {root} != {t}")))
        }
        Ok(SnapshotFull::Unknown { reason }) => Err(SnapshotProblem::Unknown(reason)),
        Err(e) => Err(SnapshotProblem::Unknown(e.to_string())),
    })
}

/// A gate to run: its index in the views, name, definition and key.
type ToRun = (usize, String, Gate, Option<CacheKey>);

/// Keys and cache lookups of every gate of the train: the gate views and the
/// gates that missed. `Err((gate, error))`: a `key_inputs` command (or a
/// `key_files` entry) of `gate` failed.
async fn derive_views(
    core: &Arc<Core>,
    train: &TrainRow,
    gates: &[(String, Gate)],
    tree_hash: &TreeHash,
    entries: &Arc<StagedEntries>,
) -> Result<Result<(Vec<TrainGate>, Vec<ToRun>), (String, String)>> {
    let mut views: Vec<TrainGate> = Vec::new();
    let mut to_run: Vec<ToRun> = Vec::new();
    let ttl = core.handle.daemon_config().marker_ttl;
    for (name, gate) in gates {
        let key = if gate.cache {
            let (g2, wt, env, tr, n, es) = (
                gate.clone(),
                train.worktree.clone(),
                core.tool_env.clone(),
                tree_hash.clone(),
                name.clone(),
                Arc::clone(entries),
            );
            match blocking(move || derive_key(&g2, &n, &tr, &es, &wt, &env)).await? {
                Ok(k) => Some(k),
                Err(e) => return Ok(Err((name.clone(), format!("{e:#}")))),
            }
        } else {
            None
        };
        let hit = match &key {
            Some(k) => matches!(
                MarkerStore::new(&train.repo).lookup(k, ttl),
                Ok(Lookup::Hit(_))
            ),
            None => false,
        };
        views.push(TrainGate {
            gate: name.clone(),
            key: key.as_ref().map(|k| k.as_str().to_owned()),
            job_id: None,
            status: if hit { "cached" } else { "pending" }.to_owned(),
            duration_ms: None,
            note: None,
        });
        if !hit {
            to_run.push((views.len() - 1, name.clone(), gate.clone(), key));
        }
    }
    Ok(Ok((views, to_run)))
}

/// Runs the base's `[queue] prepare` in the merge worktree (design section 5,
/// step 4b). `Some(entries)`: it passed and the worktree still snapshots to
/// `T`, the train goes on with these staged entries. `None`: the train was
/// dealt with (a member cancelled, the failure attributed, the snapshot
/// unusable).
async fn prepare_train(
    core: &Arc<Core>,
    train: &TrainRow,
    spec: &prepare::Spec,
    merged: &Merged,
    paths: &RepoPaths,
) -> Result<Option<Arc<StagedEntries>>> {
    let sub = prepare::submit(core, train, spec, &train.worktree, &paths.git_dir);
    let job_id = (!sub.job_id().is_empty()).then(|| sub.job_id().to_owned());
    store_prepare(
        core,
        train,
        &TrainPrepare {
            status: "running".to_owned(),
            job_id,
            ..TrainPrepare::default()
        },
    )?;
    let run = match prepare::await_run(core, train, sub).await {
        prepare::End::Cancelled => {
            member_cancelled(core, train)?;
            return Ok(None);
        }
        prepare::End::Done(run) => run,
    };
    if run.status != "pass" {
        store_prepare(core, train, &run.view(run.status))?;
        attribute(core, train, spec, merged, &run, &[]).await?;
        return Ok(None);
    }
    match snapshot_of(core, paths, &merged.t).await? {
        Ok(entries) => {
            store_prepare(core, train, &run.view("pass"))?;
            Ok(Some(entries))
        }
        Err(SnapshotProblem::Differs(_)) => {
            let (git, wt) = (core.git.clone(), train.worktree.clone());
            let dirty = blocking(move || prepare::dirty_paths(&git, &wt))
                .await?
                .unwrap_or_default();
            store_prepare(core, train, &run.view("dirty"))?;
            attribute(core, train, spec, merged, &run, &dirty).await?;
            Ok(None)
        }
        Err(problem) => {
            store_prepare(core, train, &run.view("pass"))?;
            fail_survivors(
                core,
                train,
                "tree_mismatch",
                &json!({ "error": problem.why() }),
            )
            .map_err(|e| anyhow!(store_msg(&e)))?;
            Ok(None)
        }
    }
}

/// Records the prepare view on the train (never part of the train hash).
fn store_prepare(core: &Core, train: &TrainRow, view: &TrainPrepare) -> Result<()> {
    core.write(Actor::Runner, |w| {
        w.train_to(
            &train.id,
            T::Verifying,
            T::Verifying,
            &TrainEdit {
                prepare: Some(Some(view.clone())),
                ..TrainEdit::default()
            },
        )
    })
    .map_err(|e| anyhow!(store_msg(&e)))
}

/// A member was cancelled while the train was preparing: the train is
/// dissolved and the other members go back to `queued`.
fn member_cancelled(core: &Core, train: &TrainRow) -> Result<()> {
    core.write(Actor::Runner, |w| {
        let t = store::train(w.tx(), &train.id)?
            .ok_or_else(|| store::refuse(store::Refusal::NotFound(train.id.clone())))?;
        w.dissolve(&t, T::Verifying, "member cancelled", false)
    })
    .map_err(|e| anyhow!(store_msg(&e)))
}

/// A prepare run failed (`run`) or passed and left `dirty` paths: finds out
/// whose fault it is by preparing the base alone. The base fails too: nobody's
/// fault, every member goes back to `queued` with `prepare_failed`
/// (`base: true`), paced and counted like a base that cannot be fetched. The
/// base prepares: a member's, so the single member fails and several split,
/// like a gate failure.
async fn attribute(
    core: &Arc<Core>,
    train: &TrainRow,
    spec: &prepare::Spec,
    merged: &Merged,
    run: &prepare::Run,
    dirty: &[String],
) -> Result<()> {
    let cause = if dirty.is_empty() {
        run.error_text()
    } else {
        format!(
            "prepare left files the tree does not have: {}",
            dirty.join(", ")
        )
    };
    tracing::warn!(train = %train.id, "{cause}");
    let probe = match prepare::probe_base(core, train, spec).await {
        Ok(Some(p)) => p,
        Ok(None) => return member_cancelled(core, train),
        Err(e) => {
            // No fact to attribute by: not the members' doing.
            let error = format!("{cause}; the base could not be probed: {e:#}");
            let mut detail = run.detail();
            detail["base"] = json!(true);
            return requeue_unprepared(core, train, &error, &detail);
        }
    };
    let mut detail = run.detail();
    if !dirty.is_empty() {
        detail["paths"] = json!(dirty);
        detail["message"] = json!(cause);
    }
    if probe.passed() {
        detail["base"] = json!(false);
        let (reason, what) = if dirty.is_empty() {
            ("prepare_failed", "prepare failed")
        } else {
            ("prepare_dirty", "prepare modified the tree")
        };
        let failure = Failure {
            reason,
            what: what.to_owned(),
            detail,
        };
        let id = train.id.clone();
        let outcome = core
            .write(Actor::Runner, |w| {
                // The stale remark of an earlier attempt would be shown next
                // to this failure.
                clear_key_failures(w, &id)?;
                finish_train(w, &id, merged, &[], Some(&failure))
            })
            .map_err(|e| anyhow!(store_msg(&e)))?;
        tracing::info!(train = %id, ?outcome, "prepare failed on the train, not on its base");
        return Ok(());
    }
    detail["base"] = json!(true);
    detail["base_job_id"] = json!(probe.run.job_id);
    requeue_unprepared(core, train, &cause, &detail)
}

/// Dissolves the train and puts its members back to `queued` with
/// `prepare_failed` (not `solo`: the members did nothing wrong); the third
/// consecutive attempt fails them.
fn requeue_unprepared(
    core: &Core,
    train: &TrainRow,
    error: &str,
    detail: &serde_json::Value,
) -> Result<()> {
    let id = train.id.clone();
    core.write(Actor::Runner, |w| {
        let t = store::train(w.tx(), &id)?
            .ok_or_else(|| store::refuse(store::Refusal::NotFound(id.clone())))?;
        let members: Vec<String> = store::train_jobs(w.tx(), &t)?
            .into_iter()
            .filter(|j| j.state == J::Verifying)
            .map(|j| j.id)
            .collect();
        w.dissolve(&t, T::Verifying, "prepare_failed", false)?;
        for m in &members {
            record_queued_failure_with(w, m, "prepare_failed", error, detail)?;
        }
        Ok(())
    })
    .map_err(|e| anyhow!(store_msg(&e)))
}

/// Why the verification of a train failed: handled alike for a gate that
/// failed and for gates that left the tree modified.
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    /// The job `reason`: `gate_failed` or `gate_modified_tree`.
    pub(crate) reason: &'static str,
    /// What happened, as the audit and train note put it (`gate test failed`).
    pub(crate) what: String,
    /// The job `detail`, merged into what the job already carries.
    pub(crate) detail: serde_json::Value,
}

/// `extra`'s keys over the object `base` (anything else in `base` is
/// dropped): details accumulate (`config_changed`, then a failure) instead of
/// overwriting each other.
pub(crate) fn merge_detail(
    base: Option<&serde_json::Value>,
    extra: &serde_json::Value,
) -> serde_json::Value {
    let mut out = match base {
        Some(serde_json::Value::Object(m)) => m.clone(),
        _ => serde_json::Map::new(),
    };
    if let serde_json::Value::Object(e) = extra {
        out.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    serde_json::Value::Object(out)
}

/// How [`finish_train`] ended a verifying train.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finish {
    /// A member left `verifying` (cancelled): dissolved, the others queued.
    MemberGone,
    /// A gate failed or modified the tree: the single member failed, or the
    /// members split.
    GateFailed,
    /// Every gate passed: `ready`.
    Ready,
}

/// Step 7 in one transaction: re-reads the members, then dissolves (a
/// member was cancelled meanwhile), records the gate failure, or moves the
/// train and its members to `ready` with the train hash.
pub(crate) fn finish_train(
    w: &Writer<'_, '_>,
    id: &str,
    merged: &Merged,
    views: &[TrainGate],
    failure: Option<&Failure>,
) -> rusqlite::Result<Finish> {
    let t = store::train(w.tx(), id)?
        .ok_or_else(|| store::refuse(store::Refusal::NotFound(id.to_owned())))?;
    let jobs = store::train_jobs(w.tx(), &t)?;
    let alive: Vec<&JobRow> = jobs.iter().filter(|j| j.state == J::Verifying).collect();
    if alive.len() != merged.survivors.len() {
        w.dissolve(&t, T::Verifying, "member cancelled", false)?;
        return Ok(Finish::MemberGone);
    }
    if let Some(failure) = failure {
        w.train_to(
            id,
            T::Verifying,
            T::Verifying,
            &TrainEdit {
                gates: Some(views.to_vec()),
                ..TrainEdit::default()
            },
        )?;
        let t = store::train(w.tx(), id)?
            .ok_or_else(|| store::refuse(store::Refusal::NotFound(id.to_owned())))?;
        if alive.len() == 1 {
            w.job_to(
                &alive[0].id,
                J::Verifying,
                J::Failed,
                &JobEdit {
                    reason: Some(Some(failure.reason.into())),
                    detail: Some(Some(merge_detail(
                        alive[0].detail.as_ref(),
                        &failure.detail,
                    ))),
                    note: Some(failure.what.clone()),
                    ..JobEdit::default()
                },
            )?;
            w.dissolve(&t, T::Verifying, &failure.what, false)?;
        } else {
            w.dissolve(
                &t,
                T::Verifying,
                &format!("{}; members split", failure.what),
                true,
            )?;
        }
        return Ok(Finish::GateFailed);
    }
    let member_heads: Vec<(String, String)> = alive
        .iter()
        .map(|j| (j.id.clone(), j.head_sha.clone()))
        .collect();
    let gate_keys: Vec<String> = views
        .iter()
        .map(|g| format!("{}={}", g.gate, g.key.as_deref().unwrap_or("-")))
        .collect();
    let repo_s = t.repo.to_string_lossy().into_owned();
    let hash = train_hash(&TrainHashInput {
        repo: &repo_s,
        remote: &t.remote,
        endpoint_hash: &t.endpoint_hash,
        target: &t.target,
        base_sha: &t.base_sha,
        members: &member_heads,
        merge_sha: &merged.s,
        merge_tree: &merged.t,
        gate_keys: &gate_keys,
        config_hash: t.config_hash.as_deref().filter(|_| is_trusted(&t)),
    });
    for (j, _) in &member_heads {
        w.job_to(j, J::Verifying, J::Ready, &JobEdit::default())?;
    }
    w.train_to(
        id,
        T::Verifying,
        T::Ready,
        &TrainEdit {
            gates: Some(views.to_vec()),
            train_hash: Some(Some(hash)),
            ..TrainEdit::default()
        },
    )?;
    Ok(Finish::Ready)
}

/// The failure of a gate whose job ended without passing. A job killed for
/// lack of memory says so in `what` and carries `killed_by`, so the train's
/// view tells the agent not to retry blindly.
fn gate_failure(view: &TrainGate, job_id: &str, outcome: &sluice_gate_proto::Outcome) -> Failure {
    let memory = outcome.killed_for_memory();
    let mut detail = json!({
        "gate": view.gate,
        "job_id": job_id,
        "status": view.status,
        "exit_code": outcome.exit_code,
        "log_path": outcome.log_path,
        "message": outcome.message,
    });
    if let (true, Some(map)) = (memory, detail.as_object_mut()) {
        map.insert("killed_by".to_owned(), json!(outcome.killed_by));
    }
    Failure {
        reason: "gate_failed",
        what: if memory {
            format!("gate {} was killed for lack of memory", view.gate)
        } else {
            format!("gate {} failed", view.gate)
        },
        detail,
    }
}

/// Waits for every gate; the first failure drops the other subscriptions
/// (unsubscribe). Also stops when a member leaves `verifying` (cancel).
async fn await_gates(
    core: &Arc<Core>,
    train: &TrainRow,
    subs: Vec<(usize, RunSubscription)>,
    views: &mut [TrainGate],
) -> Option<Failure> {
    let mut set: JoinSet<(usize, Option<ServerMessage>)> = JoinSet::new();
    // Which gate a task is for, to name it when the task panics.
    let mut gate_of: HashMap<tokio::task::Id, usize> = HashMap::new();
    for (idx, mut sub) in subs {
        let handle = set.spawn(async move {
            let mut last = None;
            while let Some(m) = sub.recv().await {
                let end = matches!(m, ServerMessage::Done { .. } | ServerMessage::Error { .. });
                last = Some(m);
                if end {
                    break;
                }
            }
            (idx, last)
        });
        gate_of.insert(handle.id(), idx);
    }
    let tick = core
        .handle
        .config()
        .queue
        .tick
        .max(Duration::from_millis(50));
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        tokio::select! {
            r = set.join_next() => {
                let r = r?;
                // A panicked receiver task leaves its gate without a result: that
                // is an error outcome, never a gate left "running".
                let (idx, msg, lost) = match r {
                    Ok((idx, msg)) => (idx, msg, None),
                    Err(e) => {
                        let Some(&idx) = gate_of.get(&e.id()) else { continue };
                        (idx, None, Some(format!("the gate's receiver task failed: {e}")))
                    }
                };
                match msg {
                    Some(ServerMessage::Done { job_id, outcome, .. }) => {
                        views[idx].duration_ms = Some(outcome.duration_ms);
                        let ok = matches!(outcome.status, OutcomeStatus::Pass | OutcomeStatus::Cached);
                        match outcome.status {
                            OutcomeStatus::Pass => "pass",
                            OutcomeStatus::Cached => "cached",
                            OutcomeStatus::Fail => "fail",
                            OutcomeStatus::Timeout => "timeout",
                            OutcomeStatus::Killed => "killed",
                            OutcomeStatus::Error | OutcomeStatus::Unknown => "error",
                        }.clone_into(&mut views[idx].status);
                        if ok
                            && !outcome.marker_written
                            && let Some(reason) = &outcome.not_cached_reason
                        {
                            views[idx].note = Some(format!("not cached: {reason}"));
                        }
                        if !ok {
                            set.abort_all();
                            return Some(gate_failure(&views[idx], &job_id, &outcome));
                        }
                    }
                    other => {
                        "error".clone_into(&mut views[idx].status);
                        set.abort_all();
                        let message = match other {
                            Some(ServerMessage::Error { message, .. }) => message,
                            _ => lost.unwrap_or_else(|| "the gate run ended without a result".to_owned()),
                        };
                        return Some(Failure {
                            reason: "gate_failed",
                            what: format!("gate {} failed", views[idx].gate),
                            detail: json!({
                                "gate": views[idx].gate,
                                "job_id": views[idx].job_id,
                                "status": "error",
                                "message": message,
                            }),
                        });
                    }
                }
            }
            _ = ticker.tick() => {
                let jobs = core.read(|tx| store::train_jobs(tx, train)).unwrap_or_default();
                if jobs.iter().any(|j| j.state == J::Cancelled) {
                    set.abort_all();
                    return None;
                }
            }
        }
    }
}

/// After the gates passed: tracked files of the merge worktree must still
/// match `S`. A gate that edits them would make the pre-push check dissolve
/// the train and the rebuild repeat it, an approval loop that never lands, so
/// the train fails here with `gate_modified_tree`. `ran` names the gates that
/// ran (cached ones did not touch the worktree).
async fn modified_tree(
    core: &Arc<Core>,
    train: &TrainRow,
    ran: &[String],
) -> Result<Option<Failure>> {
    let (git, wt) = (core.git.clone(), train.worktree.clone());
    let paths = blocking(move || git.status_paths(&wt, false)).await??;
    if paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(Failure {
        reason: "gate_modified_tree",
        what: "gates modified the tree".to_owned(),
        detail: json!({ "gates": ran, "paths": paths }),
    }))
}

/// A `key_inputs` command (or `key_files` entry) failed in the merge
/// worktree: nobody's fault, and every gate keeps failing the same way for
/// every member. The train is dissolved and its members go back to `queued`
/// with `key_input_failed`, paced like a base that cannot be prepared and
/// `failed` after [`MAX_PREPARE_FAILURES`] attempts.
fn key_input_failed(core: &Core, train: &TrainRow, gate: &str, error: &str) -> Result<()> {
    tracing::warn!(train = %train.id, "key inputs of gate {gate} failed: {error}");
    let (id, msg) = (train.id.clone(), format!("gate {gate}: {error}"));
    core.write(Actor::Runner, |w| {
        let t = store::train(w.tx(), &id)?
            .ok_or_else(|| store::refuse(store::Refusal::NotFound(id.clone())))?;
        let members: Vec<String> = store::train_jobs(w.tx(), &t)?
            .into_iter()
            .filter(|j| j.state == J::Verifying)
            .map(|j| j.id)
            .collect();
        w.dissolve(&t, T::Verifying, "key_input_failed", false)?;
        for m in &members {
            record_queued_failure(w, m, "key_input_failed", &msg)?;
        }
        Ok(())
    })
    .map_err(|e| anyhow!(store_msg(&e)))
}

/// Drops `key_input_failed` / `prepare_failed` and the count (and the
/// failure's own keys) from the verifying members of `train_id`: the keys
/// derived, so the failure is over.
fn clear_key_failures(w: &Writer<'_, '_>, train_id: &str) -> rusqlite::Result<()> {
    let t = store::train(w.tx(), train_id)?
        .ok_or_else(|| store::refuse(store::Refusal::NotFound(train_id.to_owned())))?;
    for j in store::train_jobs(w.tx(), &t)? {
        if j.state != J::Verifying
            || !matches!(
                j.reason.as_deref(),
                Some("key_input_failed" | "prepare_failed")
            )
        {
            continue;
        }
        let rest = match j.detail {
            Some(serde_json::Value::Object(mut m)) => {
                for k in [
                    "error",
                    "failures",
                    "job_id",
                    "status",
                    "exit_code",
                    "log_path",
                    "message",
                    "base",
                    "base_job_id",
                    "paths",
                ] {
                    m.remove(k);
                }
                (!m.is_empty()).then_some(serde_json::Value::Object(m))
            }
            other => other,
        };
        w.set_job_remark(&j.id, J::Verifying, None, rest.as_ref())?;
    }
    Ok(())
}

/// Gates a train must pass: `[queue] gates` plus the gates whose hooks
/// contain `pre-push` (the repository's push hook runs them during the
/// train's push and the daemon answers from the train's record, so the train
/// must have verified them), else the `pre-push` gates, else every gate.
/// Order: the `[queue] gates` list as written, then the added `pre-push`
/// gates by name; no duplicates.
fn select_gates(config: Option<&RepoConfig>) -> Vec<(String, Gate)> {
    let Some(c) = config else {
        return Vec::new();
    };
    let pre_push = c.gates_for_hook("pre-push");
    let mut out: Vec<(String, Gate)> = Vec::new();
    if let Some(names) = &c.queue.gates {
        for n in names {
            if let Some(g) = c.gates.get(n)
                && !out.iter().any(|(o, _)| o == n)
            {
                out.push((n.clone(), g.clone()));
            }
        }
    }
    for (n, g) in pre_push {
        if !out.iter().any(|(o, _)| o == n) {
            out.push((n.to_owned(), g.clone()));
        }
    }
    if out.is_empty() && c.queue.gates.is_none() {
        return c
            .gates
            .iter()
            .map(|(n, g)| (n.clone(), g.clone()))
            .collect();
    }
    out
}

/// The key a client would derive for `gate` on the merge worktree's
/// snapshot (`tree` and its staged `entries`; `key_inputs` run in the merge
/// worktree, `key_files` read there, no extra arguments), through the same
/// shared core function the front-ends use.
pub(crate) fn derive_key(
    gate: &Gate,
    name: &str,
    tree: &TreeHash,
    entries: &StagedEntries,
    wt: &Path,
    child_env: &EnvMap,
) -> Result<CacheKey> {
    let mut env = None;
    let (_, key) = lookup::derive_key_at(
        gate,
        name,
        lookup::key_tree(gate, tree, entries),
        &[],
        wt,
        || {
            let mut e = child_env.clone();
            e.extend(gate.env.iter().map(|(k, v)| (k.clone(), v.clone())));
            Ok(&*env.insert(e))
        },
    )?;
    Ok(key)
}

fn fail_survivors(
    core: &Core,
    train: &TrainRow,
    reason: &str,
    detail: &serde_json::Value,
) -> Result<(), StoreError> {
    let id = train.id.clone();
    core.write(Actor::Runner, |w| {
        let t = store::train(w.tx(), &id)?
            .ok_or_else(|| store::refuse(store::Refusal::NotFound(id.clone())))?;
        for j in store::train_jobs(w.tx(), &t)? {
            if j.state == J::Verifying {
                w.job_to(
                    &j.id,
                    J::Verifying,
                    J::Failed,
                    &JobEdit {
                        reason: Some(Some(reason.to_owned())),
                        detail: Some(Some(detail.clone())),
                        note: Some(reason.to_owned()),
                        ..JobEdit::default()
                    },
                )?;
            }
        }
        w.dissolve(&t, t.state, reason, false)
    })
}

/// Steps 3–4: worktree and merges. Blocking.
fn merge_members(git: &Git, t: &TrainRow, members: &[(String, String, String)]) -> Result<Merged> {
    if let Some(parent) = t.worktree.parent() {
        sluice_gate_core::dirs::create_private_dir(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if t.worktree.exists() {
        git.remove_worktree(&t.repo, &t.worktree);
    }
    let wt_s = t.worktree.to_string_lossy().into_owned();
    // `worktree add --lock --reason` needs git 2.33; the declared minimum is
    // 2.31, where `worktree lock --reason` (2.10) does the same in two steps.
    git.repo(
        &t.repo,
        &["worktree", "add", "--detach", &wt_s, &t.base_sha],
    )?;
    git.repo(
        &t.repo,
        &["worktree", "lock", "--reason", "sluice merge queue", &wt_s],
    )?;
    let wt = &t.worktree;
    let ident_ok = git
        .wt_out(wt, &["var", "GIT_COMMITTER_IDENT"])
        .is_ok_and(|o| o.status.success());
    let mut merged = Merged::default();
    let ff =
        members.len() == 1 && git.is_ancestor(&t.repo, &t.base_sha, &members[0].2)? == Some(true);
    for (job, branch, head) in members {
        // Three dots: what the member changed since it forked from the base
        // (`git diff A...B` compares merge-base(A, B) with B), not what the
        // base changed since then.
        let range = format!("{}...{head}", t.base_sha);
        let config_changed = !git
            .repo_out(
                &t.repo,
                &["diff", "--quiet", &range, "--", REPO_CONFIG_FILE],
            )?
            .status
            .success();
        if ff {
            git.wt(wt, &["checkout", "--quiet", "--detach", head])?;
            merged.survivors.push((job.clone(), config_changed));
            continue;
        }
        let before = git.wt(wt, &["rev-parse", "HEAD"])?;
        let msg = format!("Merge {branch} (sluice job {job})");
        let mut args: Vec<&str> = Vec::new();
        if !ident_ok {
            args.extend([
                "-c",
                "user.name=Sluice merge queue",
                "-c",
                "user.email=sluice@localhost",
            ]);
        }
        args.extend(["merge", "--no-ff", "--no-edit", "-m", &msg, head]);
        let out = git.wt_out(wt, &args)?;
        if !out.status.success() {
            let paths: Vec<String> = git
                .wt(wt, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect();
            if git
                .wt_out(wt, &["merge", "--abort"])
                .map_or(true, |o| !o.status.success())
            {
                git.wt(wt, &["reset", "--quiet", "--hard", &before])?;
            }
            let reason = if paths.is_empty() {
                "merge_failed"
            } else {
                "conflict"
            };
            merged.conflicts.push((job.clone(), reason, paths));
            continue;
        }
        let paths = git.status_paths(wt, true)?;
        if !paths.is_empty() {
            git.wt(wt, &["reset", "--quiet", "--hard", &before])?;
            let _ = git.wt(wt, &["clean", "-fdq"]);
            merged.conflicts.push((job.clone(), "dirty_merge", paths));
            continue;
        }
        merged.survivors.push((job.clone(), config_changed));
    }
    merged.s = git.wt(wt, &["rev-parse", "HEAD"])?;
    merged.t = git.wt(wt, &["rev-parse", "HEAD^{tree}"])?;
    Ok(merged)
}

// ---------------------------------------------------------------------------
// Push (step 9, section 6)
// ---------------------------------------------------------------------------

/// Recomputes the train hash from the stored rows (members in `state`).
pub(crate) fn current_hash(t: &TrainRow, jobs: &[JobRow], state: J) -> Option<String> {
    let members: Vec<(String, String)> = jobs
        .iter()
        .filter(|j| j.state == state)
        .map(|j| (j.id.clone(), j.head_sha.clone()))
        .collect();
    let gate_keys: Vec<String> = t
        .gates
        .iter()
        .map(|g| format!("{}={}", g.gate, g.key.as_deref().unwrap_or("-")))
        .collect();
    let repo = t.repo.to_string_lossy().into_owned();
    Some(train_hash(&TrainHashInput {
        repo: &repo,
        remote: &t.remote,
        endpoint_hash: &t.endpoint_hash,
        target: &t.target,
        base_sha: &t.base_sha,
        members: &members,
        merge_sha: t.merge_sha.as_deref()?,
        merge_tree: t.merge_tree.as_deref()?,
        gate_keys: &gate_keys,
        config_hash: t.config_hash.as_deref().filter(|_| is_trusted(t)),
    }))
}

enum PreCheck {
    Ok,
    Dissolve(String),
    FailMembers(&'static str, String),
    Retry(String),
}

fn pre_push_checks(git: &Git, t: &TrainRow, s: &str) -> PreCheck {
    let head = git
        .wt(&t.worktree, &["rev-parse", "HEAD"])
        .unwrap_or_default();
    if head != s {
        return PreCheck::Dissolve(format!("merge worktree HEAD {head} != {s}"));
    }
    // Tracked files only: untracked or ignored output left by a gate does
    // not change the commit that is pushed.
    match git.wt(
        &t.worktree,
        &["status", "--porcelain", "--untracked-files=no"],
    ) {
        Ok(st) if st.is_empty() => {}
        _ => return PreCheck::Dissolve("merge worktree is not clean".into()),
    }
    let url = match endpoint(git, &t.repo, &t.remote) {
        Ok((url, h)) if h == t.endpoint_hash => url,
        Ok(_) => return PreCheck::Dissolve("endpoint_changed".into()),
        Err(e) => return PreCheck::Retry(format!("{e:#}")),
    };
    if t.remote.is_empty()
        && git
            .checked_out_branches(&t.repo)
            .is_ok_and(|b| b.contains(&t.target))
    {
        return PreCheck::FailMembers(
            "target_checked_out",
            format!("local branch {} is checked out in a worktree", t.target),
        );
    }
    match target_at(git, t, &url) {
        Ok(Some(cur)) if cur == t.base_sha => PreCheck::Ok,
        Ok(_) => PreCheck::Dissolve("target moved".into()),
        // The approver's credentials were refused: retrying with the same
        // stored env can never succeed, so the approval would otherwise sit
        // `approved` forever while `sluice queue approve` waits for nothing.
        // Nothing was pushed; fail like a push the remote refused.
        Err(e) if push::credentials_refused(&format!("{e:#}")) => PreCheck::FailMembers(
            "push_rejected",
            format!("the remote refused the approver's credentials: {e:#}"),
        ),
        Err(e) => PreCheck::Retry(format!("{e:#}")),
    }
}

#[allow(clippy::too_many_lines, reason = "section 6 steps 1-3, in order")]
async fn push_train(core: &Arc<Core>, id: &str) -> Result<()> {
    let (t, jobs) = core
        .read(|tx| {
            let t = store::train(tx, id)?;
            let jobs = match &t {
                Some(t) => store::train_jobs(tx, t)?,
                None => Vec::new(),
            };
            Ok((t, jobs))
        })
        .map_err(|e| anyhow!(store_msg(&e)))?;
    let Some(t) = t.filter(|t| t.state == T::Approved) else {
        return Ok(());
    };
    if !core.handle.lease_held() {
        return Ok(());
    }
    let s = t.merge_sha.clone().context("approved train without S")?;
    let env = t.push_env.clone().unwrap_or_default();
    let git = core
        .git_for(&env)
        .with_network_timeout(t.settings.network_timeout());
    // Re-verify: hash, members, HEAD, cleanliness, endpoint, target.
    if current_hash(&t, &jobs, J::Approved) != t.approved_hash
        || jobs.iter().any(|j| j.cancel_requested)
    {
        core.write(Actor::Runner, |w| {
            w.dissolve(&t, T::Approved, "stale_plan", false)
        })
        .map_err(|e| anyhow!(store_msg(&e)))?;
        return Ok(());
    }
    let (g2, t2, s2) = (git.clone(), t.clone(), s.clone());
    match blocking(move || pre_push_checks(&g2, &t2, &s2)).await? {
        PreCheck::Ok => {}
        PreCheck::Retry(why) => {
            tracing::warn!(train = %id, "push postponed: {why}");
            return Ok(());
        }
        PreCheck::Dissolve(why) => {
            core.write(Actor::Runner, |w| w.dissolve(&t, T::Approved, &why, false))
                .map_err(|e| anyhow!(store_msg(&e)))?;
            return Ok(());
        }
        PreCheck::FailMembers(reason, why) => {
            core.write(Actor::Runner, |w| {
                for j in jobs.iter().filter(|j| j.state == J::Approved) {
                    w.job_to(
                        &j.id,
                        J::Approved,
                        J::Failed,
                        &JobEdit {
                            reason: Some(Some(reason.to_owned())),
                            detail: Some(Some(json!({ "error": why }))),
                            ..JobEdit::default()
                        },
                    )?;
                }
                w.dissolve(&t, T::Approved, reason, false)
            })
            .map_err(|e| anyhow!(store_msg(&e)))?;
            return Ok(());
        }
    }
    // 1. Pin ref.
    let pin = format!("refs/sluice/pending/{id}");
    {
        let (g2, repo, pin2, s2) = (git.clone(), t.repo.clone(), pin.clone(), s.clone());
        blocking(move || g2.repo(&repo, &["update-ref", &pin2, &s2])).await??;
    }
    // 2. Marker + pushing, one transaction.
    let audit = t.settings.audit_ref;
    let target_ref = format!("refs/heads/{}", t.target);
    let audit_ref = format!("refs/sluice/landed/{s}");
    let mut push_refs = vec![target_ref.clone()];
    if audit {
        push_refs.push(audit_ref.clone());
    }
    let marker = MarkerRow {
        train_id: id.to_owned(),
        sha: s.clone(),
        base_sha: t.base_sha.clone(),
        remote: t.remote.clone(),
        endpoint_hash: t.endpoint_hash.clone(),
        push_refs,
        audit_ref: audit.then(|| audit_ref.clone()),
        pin_ref: pin.clone(),
        claim_token: String::new(),
        push_pid: None,
        push_start_ticks: None,
        written_at: 0,
    };
    let wrote = core.write_durable(Actor::Runner, |w| {
        // Re-read inside the transaction: fenced on the members' state and
        // on `cancel_requested = 0`.
        let now = store::train_jobs(w.tx(), &t)?;
        let approved: Vec<&JobRow> = now.iter().filter(|j| j.state == J::Approved).collect();
        if approved.is_empty() || approved.iter().any(|j| j.cancel_requested) {
            return Err(store::refuse(store::Refusal::State(
                "no approved member left, or a cancel is pending".into(),
            )));
        }
        w.insert_marker(&marker)?;
        for j in &approved {
            w.job_to(&j.id, J::Approved, J::Pushing, &JobEdit::default())?;
        }
        w.train_to(id, T::Approved, T::Pushing, &TrainEdit::default())
    });
    if let Err(e) = wrote {
        tracing::info!(train = %id, "push not started: {}", store_msg(&e));
        let (g2, repo, pin2) = (git.clone(), t.repo.clone(), pin.clone());
        let _ = blocking(move || g2.repo(&repo, &["update-ref", "-d", &pin2])).await;
        return Ok(());
    }
    tracing::info!(train = %id, target = %t.target, "pushing");
    // 3. Push (or local ref update).
    let verdict = if t.remote.is_empty() {
        let (g2, t2, s2) = (git.clone(), t.clone(), s.clone());
        match blocking(move || {
            push::land_local(&g2, &t2.repo, &t2.id, &t2.target, &t2.base_sha, &s2, audit)
        })
        .await?
        {
            Ok(true) => PushVerdict::Landed,
            Ok(false) => PushVerdict::Moved("update-ref compare-and-swap failed".into()),
            Err(e) => PushVerdict::Ambiguous(format!("{e:#}")),
        }
    } else {
        let args = push::push_args(&t.remote, &t.target, &t.base_sha, &s, audit);
        match push::spawn(core.push_program.as_deref(), &git, &t.worktree, &args) {
            // The marker is written: whatever happened, only reconcile may
            // decide (the train must never stay `pushing` in a live daemon).
            Err(e) => PushVerdict::Ambiguous(format!("push_spawn_failed: {e:#}")),
            Ok(proc_) => {
                let (pid, ticks) = (proc_.pid, proc_.start_ticks);
                if let Err(e) = core.write(Actor::Runner, |w| w.set_marker_pid(id, pid, ticks)) {
                    tracing::warn!(train = %id, "cannot record the push pid: {}", store_msg(&e));
                }
                let timeout = Duration::from_millis(t.settings.push_timeout_ms.max(1));
                match proc_.wait(timeout).await {
                    PushEnd::TimedOut { stdout, stderr } => {
                        PushVerdict::Ambiguous(push::timeout_note(&stdout, &stderr))
                    }
                    PushEnd::Exited {
                        code,
                        stdout,
                        stderr,
                    } => push::classify(code, &stdout, &stderr, &target_ref),
                }
            }
        }
    };
    tracing::info!(train = %id, ?verdict, "push ended");
    apply_verdict(core, &t, &verdict).await
}

async fn apply_verdict(core: &Arc<Core>, t: &TrainRow, verdict: &PushVerdict) -> Result<()> {
    let id = t.id.clone();
    let r = core.write(Actor::Runner, |w| {
        let jobs = store::train_jobs(w.tx(), t)?;
        let pushing: Vec<&JobRow> = jobs.iter().filter(|j| j.state == J::Pushing).collect();
        match verdict {
            PushVerdict::Landed => {
                for j in &pushing {
                    w.job_to(&j.id, J::Pushing, J::Landed, &JobEdit::default())?;
                }
                w.train_to(
                    &id,
                    T::Pushing,
                    T::Landed,
                    &TrainEdit {
                        event_note: Some("outcome: landed".into()),
                        ..TrainEdit::default()
                    },
                )
            }
            PushVerdict::Moved(why) => {
                w.delete_marker(&id)?;
                w.dissolve(t, T::Pushing, &format!("push rejected: {why}"), false)
            }
            PushVerdict::Rejected(why) | PushVerdict::HookDeclined(why) => {
                let reason = if matches!(verdict, PushVerdict::HookDeclined(_)) {
                    "pre_push_hook"
                } else {
                    "push_rejected"
                };
                w.delete_marker(&id)?;
                for j in &pushing {
                    let to = if j.cancel_requested {
                        J::Cancelled
                    } else {
                        J::Failed
                    };
                    w.job_to(
                        &j.id,
                        J::Pushing,
                        to,
                        &JobEdit {
                            reason: Some(Some(reason.to_owned())),
                            detail: Some(Some(json!({ "output": why }))),
                            note: Some(reason.to_owned()),
                            ..JobEdit::default()
                        },
                    )?;
                }
                w.dissolve(t, T::Pushing, reason, false)
            }
            PushVerdict::Ambiguous(why) => {
                for j in &pushing {
                    w.job_to(
                        &j.id,
                        J::Pushing,
                        J::NeedsReconcile,
                        &JobEdit {
                            reason: Some(Some("needs_reconcile".into())),
                            detail: Some(Some(json!({ "error": why }))),
                            ..JobEdit::default()
                        },
                    )?;
                }
                w.train_to(
                    &id,
                    T::Pushing,
                    T::NeedsReconcile,
                    &TrainEdit {
                        note: Some(Some(why.clone())),
                        event_note: Some(why.clone()),
                        ..TrainEdit::default()
                    },
                )
            }
        }
    });
    if let Err(e) = r {
        tracing::warn!(train = %id, "push result not recorded ({}); the next daemon start parks it", store_msg(&e));
        return Ok(());
    }
    if !matches!(verdict, PushVerdict::Ambiguous(_)) {
        let (git, repo, wt) = (core.git.clone(), t.repo.clone(), t.worktree.clone());
        let pin = format!("refs/sluice/pending/{id}");
        blocking(move || {
            let _ = git.repo(&repo, &["update-ref", "-d", &pin]);
            git.remove_worktree(&repo, &wt);
        })
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{conn, fence, two_member_train, with};

    /// `t1` verifying with `j1`, `j2` verifying (both merged).
    fn verifying_pair(c: &mut rusqlite::Connection, f: &sluice_gate_daemon::server::LeaseFence) {
        two_member_train(c, f);
        with(c, |tx| {
            let w = Writer::new(tx, f, Actor::Runner)?;
            for j in ["j1", "j2"] {
                w.job_to(j, J::Merging, J::Verifying, &JobEdit::default())?;
            }
            w.train_to(
                "t1",
                T::Building,
                T::Verifying,
                &TrainEdit {
                    merge_sha: Some("s".into()),
                    merge_tree: Some("t".into()),
                    ..TrainEdit::default()
                },
            )
        })
        .unwrap();
    }

    fn merged() -> Merged {
        Merged {
            s: "s".into(),
            t: "t".into(),
            survivors: vec![("j1".into(), false), ("j2".into(), false)],
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn a_cancel_before_the_outcome_requeues_the_others() {
        let mut c = conn();
        let f = fence("tok");
        verifying_pair(&mut c, &f);
        // The gates passed; j2 is cancelled before the outcome is written.
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Cli)?.job_to(
                "j2",
                J::Verifying,
                J::Cancelled,
                &JobEdit::default(),
            )
        })
        .unwrap();
        let views = vec![TrainGate {
            gate: "test".into(),
            status: "pass".into(),
            ..TrainGate::default()
        }];
        let out = with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            finish_train(&w, "t1", &merged(), &views, None)
        })
        .unwrap();
        assert_eq!(out, Finish::MemberGone);
        let (j1, j2, t) = with(&mut c, |tx| {
            Ok((
                store::job(tx, "j1")?.unwrap(),
                store::job(tx, "j2")?.unwrap(),
                store::train(tx, "t1")?.unwrap(),
            ))
        })
        .unwrap();
        assert_eq!(j1.state, J::Queued, "never failed with reason internal");
        assert_eq!(j1.reason, None);
        assert_eq!(j2.state, J::Cancelled);
        assert_eq!(t.state, T::Dissolved);
    }

    #[test]
    fn details_accumulate_instead_of_overwriting() {
        let flag = json!({"config_changed": true});
        let failure = json!({"gate": "test", "status": "fail"});
        assert_eq!(
            merge_detail(Some(&flag), &failure),
            json!({"config_changed": true, "gate": "test", "status": "fail"})
        );
        assert_eq!(merge_detail(None, &failure), failure);
        // The later value wins on a clash; a non-object base is dropped.
        assert_eq!(
            merge_detail(Some(&json!({"gate": "old", "x": 1})), &failure),
            json!({"gate": "test", "x": 1, "status": "fail"})
        );
        assert_eq!(merge_detail(Some(&json!("text")), &flag), flag);
    }

    fn modified() -> Failure {
        Failure {
            reason: "gate_modified_tree",
            what: "gates modified the tree".into(),
            detail: json!({"gates": ["test"], "paths": ["a.txt"]}),
        }
    }

    #[test]
    fn a_modified_tree_fails_a_single_member_and_keeps_config_changed() {
        let mut c = conn();
        let f = fence("tok");
        two_member_train(&mut c, &f);
        with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            w.job_to(
                "j1",
                J::Merging,
                J::Verifying,
                &JobEdit {
                    detail: Some(Some(json!({"config_changed": true}))),
                    ..JobEdit::default()
                },
            )?;
            // j2 conflicted: the train goes on with j1 alone.
            w.job_to("j2", J::Merging, J::Conflict, &JobEdit::default())?;
            w.train_to(
                "t1",
                T::Building,
                T::Verifying,
                &TrainEdit {
                    merge_sha: Some("s".into()),
                    merge_tree: Some("t".into()),
                    ..TrainEdit::default()
                },
            )
        })
        .unwrap();
        let merged = Merged {
            survivors: vec![("j1".into(), true)],
            ..merged()
        };
        let out = with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            finish_train(&w, "t1", &merged, &[], Some(&modified()))
        })
        .unwrap();
        assert_eq!(out, Finish::GateFailed);
        let (j1, t) = with(&mut c, |tx| {
            Ok((
                store::job(tx, "j1")?.unwrap(),
                store::train(tx, "t1")?.unwrap(),
            ))
        })
        .unwrap();
        assert_eq!(j1.state, J::Failed);
        assert_eq!(j1.reason.as_deref(), Some("gate_modified_tree"));
        assert_eq!(
            j1.detail,
            Some(json!({
                "config_changed": true,
                "gates": ["test"],
                "paths": ["a.txt"]
            }))
        );
        assert_eq!(t.state, T::Dissolved);
    }

    #[test]
    fn a_modified_tree_splits_a_multi_member_train() {
        let mut c = conn();
        let f = fence("tok");
        verifying_pair(&mut c, &f);
        let out = with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            finish_train(&w, "t1", &merged(), &[], Some(&modified()))
        })
        .unwrap();
        assert_eq!(out, Finish::GateFailed);
        let (j1, j2, t) = with(&mut c, |tx| {
            Ok((
                store::job(tx, "j1")?.unwrap(),
                store::job(tx, "j2")?.unwrap(),
                store::train(tx, "t1")?.unwrap(),
            ))
        })
        .unwrap();
        for j in [&j1, &j2] {
            assert_eq!(j.state, J::Queued);
            assert!(j.solo, "split into solo trains");
        }
        assert_eq!(t.state, T::Dissolved);
        assert_eq!(
            t.note.as_deref(),
            Some("gates modified the tree; members split")
        );
    }

    #[test]
    fn a_passing_prepare_clears_the_failure_remark_and_keeps_config_changed() {
        let mut c = conn();
        let f = fence("tok");
        verifying_pair(&mut c, &f);
        let detail = json!({
            "config_changed": true, "error": "prepare fail", "failures": 2,
            "job_id": "j-1", "status": "fail", "base": true
        });
        let (j1, j2) = with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            w.set_job_remark("j1", J::Verifying, Some("prepare_failed"), Some(&detail))?;
            clear_key_failures(&w, "t1")?;
            Ok((
                store::job(tx, "j1")?.unwrap(),
                store::job(tx, "j2")?.unwrap(),
            ))
        })
        .unwrap();
        assert_eq!(j1.reason, None);
        assert_eq!(j1.detail, Some(json!({"config_changed": true})));
        assert_eq!(j2.reason, None);
    }

    #[test]
    fn a_prepare_failure_splits_a_pair_like_a_gate_failure() {
        let failure = Failure {
            reason: "prepare_failed",
            what: "prepare failed".to_owned(),
            detail: json!({"base": false, "status": "fail", "job_id": "j-1"}),
        };
        let mut c = conn();
        let f = fence("tok");
        verifying_pair(&mut c, &f);
        let out = with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            finish_train(&w, "t1", &merged(), &[], Some(&failure))
        })
        .unwrap();
        assert_eq!(out, Finish::GateFailed);
        let (j1, t) = with(&mut c, |tx| {
            Ok((
                store::job(tx, "j1")?.unwrap(),
                store::train(tx, "t1")?.unwrap(),
            ))
        })
        .unwrap();
        assert_eq!((j1.state, j1.solo), (J::Queued, true));
        assert_eq!(t.note.as_deref(), Some("prepare failed; members split"));
    }

    #[test]
    fn a_gate_note_does_not_change_the_train_hash() {
        let mut c = conn();
        let f = fence("tok");
        verifying_pair(&mut c, &f);
        let views = vec![TrainGate {
            gate: "test".into(),
            key: Some("k".into()),
            status: "pass".into(),
            note: Some("not cached: the gate modified the tree".into()),
            ..TrainGate::default()
        }];
        let out = with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            finish_train(&w, "t1", &merged(), &views, None)
        })
        .unwrap();
        assert_eq!(out, Finish::Ready);
        let t = with(&mut c, |tx| store::train(tx, "t1")).unwrap().unwrap();
        assert_eq!(t.gates[0].note.as_deref(), views[0].note.as_deref());
        let jobs = with(&mut c, |tx| store::train_jobs(tx, &t)).unwrap();
        // The recomputed hash (from keys only) still matches the stored one.
        assert_eq!(t.train_hash, current_hash(&t, &jobs, J::Ready));
        let mut plain = t.clone();
        plain.gates[0].note = None;
        assert_eq!(current_hash(&plain, &jobs, J::Ready), t.train_hash);
    }

    #[test]
    fn passing_gates_make_the_train_ready_with_a_hash() {
        let mut c = conn();
        let f = fence("tok");
        verifying_pair(&mut c, &f);
        let out = with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            finish_train(&w, "t1", &merged(), &[], None)
        })
        .unwrap();
        assert_eq!(out, Finish::Ready);
        let t = with(&mut c, |tx| store::train(tx, "t1")).unwrap().unwrap();
        assert_eq!(t.state, T::Ready);
        let jobs = with(&mut c, |tx| store::train_jobs(tx, &t)).unwrap();
        assert_eq!(t.train_hash, current_hash(&t, &jobs, J::Ready));
    }

    /// Path-scoped keys, design section 9 case 26: an agent's `verify` and
    /// the runner derive the same key for the same clean tree, scoped or not.
    #[test]
    fn agent_and_runner_derive_the_same_key() {
        use sluice_gate_core::lookup::Lookup as State;
        use sluice_gate_core::mode::CacheMode;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let dir = root.join("r");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        let mut env: EnvMap = std::env::vars()
            .filter(|(k, _)| !k.starts_with("GIT_") && !k.starts_with("CLAUDE"))
            .collect();
        for (k, v) in [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_AUTHOR_NAME", "T"),
            ("GIT_AUTHOR_EMAIL", "t@example.com"),
            ("GIT_COMMITTER_NAME", "T"),
            ("GIT_COMMITTER_EMAIL", "t@example.com"),
        ] {
            env.insert(k.into(), v.into());
        }
        env.insert("XDG_RUNTIME_DIR".into(), root.display().to_string());
        env.insert(
            "SLUICE_CONFIG".into(),
            root.join("none.toml").display().to_string(),
        );
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env_clear()
                .envs(&env)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("src/a.rs"), "a\n").unwrap();
        std::fs::write(dir.join("docs/x.md"), "x\n").unwrap();
        std::fs::write(
            dir.join(REPO_CONFIG_FILE),
            "version = 1\n[gates.scoped]\ncmd = \"true\"\npaths = [\"src\"]\n\
             key_inputs = [\"echo $FOO\"]\nenv = { FOO = \"1\" }\n\
             [gates.plain]\ncmd = \"true\"\n",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);

        for name in ["scoped", "plain"] {
            let r = lookup::resolve(&dir, &env, name, &CacheMode::Enabled).unwrap();
            let State::Miss { tree, key, .. } = lookup::lookup(&r, &CacheMode::Enabled, &[]) else {
                panic!("expected a miss");
            };
            let Ok(SnapshotFull::Known { root, entries }) =
                tree::snapshot_entries(&r.paths, &r.probe_env, &r.scratch)
            else {
                panic!("snapshot");
            };
            assert_eq!(root, tree);
            let runner = derive_key(&r.gate, name, &root, &entries, &dir, &env).unwrap();
            assert_eq!(runner, key, "gate {name}");
        }
    }

    fn selected(body: &str) -> Vec<String> {
        let c = RepoConfig::parse(
            &format!("version = 1\n{body}"),
            Path::new("/x/.sluice.toml"),
        )
        .unwrap();
        select_gates(Some(&c)).into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn queue_gates_are_extended_with_the_pre_push_gates() {
        let gates = "[gates.a]\ncmd = \"true\"\n[gates.b]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n\
                     [gates.c]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n[gates.d]\ncmd = \"true\"\n";
        // The [queue] list first, as written; then the pre-push gates not in it.
        assert_eq!(
            selected(&format!("[queue]\ngates = [\"d\", \"a\"]\n{gates}")),
            ["d", "a", "b", "c"]
        );
        // No duplicates when a listed gate also has the hook.
        assert_eq!(
            selected(&format!("[queue]\ngates = [\"c\", \"d\"]\n{gates}")),
            ["c", "d", "b"]
        );
        // No [queue] gates: the pre-push gates.
        assert_eq!(selected(gates), ["b", "c"]);
        // Neither: every gate.
        assert_eq!(
            selected("[gates.x]\ncmd = \"true\"\n[gates.y]\ncmd = \"true\"\n"),
            ["x", "y"]
        );
    }
}
