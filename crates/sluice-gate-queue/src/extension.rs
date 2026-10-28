//! [`QueueExtension`]: the merge queue plugged into the daemon through
//! `sluice_gate_daemon::server::Extension`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context as _, anyhow};
use sluice_gate_daemon::server::{
    BoxFuture, DaemonHandle, Extension, ExtensionCtx, StoreError, Vouch,
};
use sluice_gate_proto::{
    ApproveVia, ClientMessage, EnqueueRequest, ErrorCode, FEATURE_QUEUE,
    FEATURE_QUEUE_TRUST_CONFIG, LearnedProtection, QUEUE_STATUS_SCHEMA_VERSION, QueueJobState as J,
    QueueJobView, QueueStatusReport, QueueSummary, QueueTrainState as T, QueueTrainView,
    ReconcileDecision, ReconcileResult, RunRequest, ServerMessage,
};

use crate::git::{EnvMap, allowlisted};
use crate::model::{job_cancellable_now, job_is_active};
use crate::reconcile::{Decision, TrainFacts, decide, gather};
use crate::runner::{self, CONFIG_SOURCE_MEMBER, Core, current_hash, is_trusted, store_msg};
use crate::store::{self, Actor, JobEdit, JobRow, NewJob, Refusal, TrainEdit, TrainRow};
use crate::target;

/// Options of the extension.
#[derive(Debug, Clone, Default)]
pub struct QueueOptions {
    /// Replaces the daemon's sanitised environment as the base of every git
    /// call and gate process of the queue (tests isolate the user's git
    /// config this way). `None` in production.
    pub base_env: Option<EnvMap>,
    /// Program spawned for the push instead of `git` (tests inject a spawn
    /// failure). `None` in production.
    pub push_program: Option<PathBuf>,
    /// Login shell resolved for the train tool environment instead of the
    /// user's (tests; with `base_env` set the login shell is otherwise not
    /// resolved, see [`crate::toolenv`]). `None` in production.
    pub login_shell: Option<PathBuf>,
}

/// The merge queue as a daemon extension.
#[derive(Debug, Default)]
pub struct QueueExtension {
    options: QueueOptions,
    core: OnceLock<Arc<Core>>,
}

impl QueueExtension {
    /// A new, not yet started extension.
    #[must_use]
    pub fn new(options: QueueOptions) -> Self {
        Self {
            options,
            core: OnceLock::new(),
        }
    }
}

fn err(id: u64, code: ErrorCode, message: impl Into<String>) -> ServerMessage {
    ServerMessage::Error {
        id,
        code,
        message: message.into(),
    }
}

fn store_err(id: u64, e: &StoreError) -> ServerMessage {
    let code = match store::refusal(e) {
        Some(Refusal::NotFound(_)) => ErrorCode::NotFound,
        Some(Refusal::State(_)) => ErrorCode::ConflictState,
        Some(Refusal::Duplicate(_)) => ErrorCode::DuplicateBranch,
        Some(Refusal::Stale(_)) => ErrorCode::StalePlan,
        Some(Refusal::NotAllowed(_)) => ErrorCode::NotAllowed,
        Some(Refusal::LeaseLost | Refusal::Schema(_)) | None => ErrorCode::Internal,
    };
    err(id, code, store_msg(e))
}

fn rfc3339(ms: u64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_millis(ms)).to_string()
}

fn job_view(tx: &rusqlite::Transaction<'_>, j: &JobRow) -> rusqlite::Result<QueueJobView> {
    Ok(QueueJobView {
        job_id: j.id.clone(),
        repo: j.repo.clone(),
        worktree: j.worktree.clone(),
        branch: j.branch.clone(),
        head_sha: j.head_sha.clone(),
        remote: j.remote.clone(),
        target: j.target.clone(),
        state: j.state,
        train_id: j.train_id.clone(),
        position: store::position(tx, j)?,
        caller: j.caller,
        session: j.session.clone(),
        reason: j.reason.clone(),
        detail: j.detail.clone(),
        cancel_requested: j.cancel_requested,
        attempts: j.attempts,
        enqueued_at: rfc3339(j.enqueued_at),
        updated_at: rfc3339(j.updated_at),
        finished_at: j.finished_at.map(rfc3339),
        learned_protection: None,
    })
}

fn train_view(t: &TrainRow) -> QueueTrainView {
    QueueTrainView {
        train_id: t.id.clone(),
        repo: t.repo.clone(),
        remote: t.remote.clone(),
        target: t.target.clone(),
        base_sha: t.base_sha.clone(),
        state: t.state,
        members: t.members.clone(),
        merge_sha: t.merge_sha.clone(),
        gates: t.gates.clone(),
        train_hash: if matches!(
            t.state,
            T::Ready | T::Approved | T::Pushing | T::Landed | T::NeedsReconcile
        ) {
            t.train_hash.clone()
        } else {
            None
        },
        approved_via: t.approved_via,
        approved_at: t.approved_at.map(rfc3339),
        created_at: rfc3339(t.created_at),
        updated_at: rfc3339(t.updated_at),
        landed_at: t.landed_at.map(rfc3339),
        note: t.note.clone(),
        push_timeout_ms: Some(t.settings.push_timeout_ms),
        network_timeout_ms: Some(
            u64::try_from(t.settings.network_timeout().as_millis()).unwrap_or(u64::MAX),
        ),
        prepare: t.prepare.clone(),
        config_source: t.config_source.clone(),
        // Filled by `status`, which runs git for it.
        config_diff: None,
    }
}

fn job_reply(core: &Core, id: u64, job_id: &str) -> ServerMessage {
    match core.read(|tx| match store::job(tx, job_id)? {
        Some(j) => Ok(Some(job_view(tx, &j)?)),
        None => Ok(None),
    }) {
        Ok(Some(job)) => ServerMessage::QueueJob { id, job },
        Ok(None) => err(id, ErrorCode::NotFound, format!("no queue job {job_id}")),
        Err(e) => store_err(id, &e),
    }
}

/// The audit actor of a request from `via` (`None`: an older client, the CLI).
fn actor_of(via: Option<ApproveVia>) -> Actor {
    match via {
        Some(ApproveVia::Mcp) => Actor::Mcp,
        Some(ApproveVia::Cli) | None => Actor::Cli,
    }
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

impl Extension for QueueExtension {
    fn features(&self) -> &[&'static str] {
        &[FEATURE_QUEUE, FEATURE_QUEUE_TRUST_CONFIG]
    }

    fn on_start<'a>(&'a self, handle: &'a DaemonHandle) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            handle
                .store_tx(store::init)
                .map_err(|e| anyhow!(store_msg(&e)))?;
            // The login shell may take seconds: resolve it off the async
            // threads, before the runner starts.
            let (h, base, push, shell) = (
                handle.clone(),
                self.options.base_env.clone(),
                self.options.push_program.clone(),
                self.options.login_shell.clone(),
            );
            let core = tokio::task::spawn_blocking(move || {
                Core::with_push_program(h, base, push, shell.as_deref())
            })
            .await
            .map_err(|e| anyhow!("tool environment resolution failed: {e}"))??;
            let core = Arc::new(core);
            start_recovery(&core).context("parking interrupted trains")?;
            // Retention prune and sweep of `<state>/merge/` leftovers (then
            // once per hour from the runner).
            let c2 = Arc::clone(&core);
            tokio::task::spawn_blocking(move || runner::maintenance(&c2))
                .await
                .map_err(|e| anyhow!("maintenance failed: {e}"))?;
            let _ = self.core.set(core);
            Ok(())
        })
    }

    fn run(self: Arc<Self>, handle: DaemonHandle) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            if let Some(core) = self.core.get() {
                runner::run(Arc::clone(core)).await;
            } else {
                let mut s = handle.shutdown();
                let _ = s.wait_for(|v| *v).await;
            }
        })
    }

    fn handle<'a>(
        &'a self,
        msg: ClientMessage,
        _ctx: &'a ExtensionCtx,
    ) -> BoxFuture<'a, Option<ServerMessage>> {
        Box::pin(async move {
            let id = match &msg {
                ClientMessage::QueueEnqueue { id, .. }
                | ClientMessage::QueueStatus { id, .. }
                | ClientMessage::QueueApprove { id, .. }
                | ClientMessage::QueueRetry { id, .. }
                | ClientMessage::QueueCancel { id, .. }
                | ClientMessage::QueueReconcile { id, .. } => *id,
                _ => return None,
            };
            let Some(core) = self.core.get() else {
                return Some(err(
                    id,
                    ErrorCode::Internal,
                    "the merge queue failed to start",
                ));
            };
            if !core.handle.config().queue.enabled {
                return Some(err(
                    id,
                    ErrorCode::NotSupported,
                    "the merge queue is disabled in the user config ([queue] enabled = false)",
                ));
            }
            let reply = match msg {
                ClientMessage::QueueEnqueue { id, request } => enqueue(core, id, request).await,
                ClientMessage::QueueStatus { id, repo } => status(core, id, repo.as_deref()).await,
                ClientMessage::QueueApprove {
                    id,
                    train_id,
                    train_hash,
                    via,
                    env,
                } => approve(core, id, &train_id, &train_hash, via, &env),
                ClientMessage::QueueRetry {
                    id,
                    job_id,
                    head_sha,
                    via,
                    trust_member_config,
                    env,
                } => {
                    let r = Retry {
                        job_id: &job_id,
                        head_sha: &head_sha,
                        via,
                        trust: trust_member_config,
                        env: &env,
                    };
                    retry(core, id, &r).await
                }
                ClientMessage::QueueCancel { id, job_id, via } => {
                    cancel(core, id, &job_id, actor_of(via))
                }
                ClientMessage::QueueReconcile { id, repo } => {
                    reconcile(core, id, repo.as_deref()).await
                }
                _ => return None,
            };
            core.notify.notify_one();
            Some(reply)
        })
    }

    fn vouch_run<'a>(
        &'a self,
        request: &'a RunRequest,
        peer_pid: Option<u32>,
    ) -> BoxFuture<'a, Option<Vouch>> {
        Box::pin(async move {
            let core = self.core.get()?;
            if !core.handle.config().queue.enabled {
                return None;
            }
            crate::vouch::vouch(core, request, peer_pid).await
        })
    }

    fn status_summary(&self) -> BoxFuture<'_, Option<QueueSummary>> {
        Box::pin(async move {
            let core = self.core.get()?;
            core.read(|tx| {
                let jobs = store::jobs(tx, None)?;
                let trains = store::trains(tx, None)?;
                Ok(QueueSummary {
                    active: jobs.iter().filter(|j| job_is_active(j.state)).count(),
                    ready: trains.iter().filter(|t| t.state == T::Ready).count(),
                    needs_reconcile: trains
                        .iter()
                        .filter(|t| t.state == T::NeedsReconcile)
                        .count(),
                    tool_env: Some(crate::toolenv::view(&core.tool_env, &core.tool_env_source)),
                })
            })
            .ok()
        })
    }
}

/// Daemon start: adopt trains, dissolve `building`/`verifying` trains, park
/// `pushing` trains as `needs_reconcile` (a live push is reported, not killed).
fn start_recovery(core: &Core) -> anyhow::Result<()> {
    core.write(Actor::DaemonStart, |w| {
        w.adopt_trains()?;
        for t in store::trains(w.tx(), None)? {
            match t.state {
                T::Building | T::Verifying => w.dissolve(&t, t.state, "daemon restarted", false)?,
                T::Pushing => {
                    let m = store::marker(w.tx(), &t.id)?;
                    let alive = m.as_ref().and_then(|m| {
                        let pid = m.push_pid?;
                        sluice_gate_daemon::executor::process_matches(pid, m.push_start_ticks)
                            .then_some(pid)
                    });
                    let note = match (&m, alive) {
                        (_, Some(pid)) => format!("daemon start: push process {pid} still running"),
                        (Some(_), None) => "daemon start found a push marker".to_owned(),
                        (None, None) => "daemon start: pushing without a marker".to_owned(),
                    };
                    if let Some(pid) = alive {
                        tracing::warn!(train = %t.id, pid, "a push from the previous daemon is still running; not killed");
                    }
                    for j in store::train_jobs(w.tx(), &t)? {
                        if j.state == J::Pushing {
                            w.job_to(&j.id, J::Pushing, J::NeedsReconcile, &JobEdit {
                                reason: Some(Some("needs_reconcile".into())),
                                note: Some(note.clone()),
                                ..JobEdit::default()
                            })?;
                        }
                    }
                    w.train_to(&t.id, T::Pushing, T::NeedsReconcile, &TrainEdit {
                        note: Some(Some(note.clone())),
                        event_note: Some(note),
                        ..TrainEdit::default()
                    })?;
                }
                _ => {}
            }
        }
        // Jobs stranded in merging/verifying without a live train.
        for st in [J::Merging, J::Verifying] {
            for j in store::jobs_in_state(w.tx(), st)? {
                w.job_to(&j.id, st, J::Queued, &JobEdit {
                    train_id: Some(None),
                    bump_attempts: true,
                    note: Some("daemon restarted".into()),
                    ..JobEdit::default()
                })?;
            }
        }
        Ok(())
    })
    .map_err(|e| anyhow!(store_msg(&e)))
}

async fn enqueue(core: &Arc<Core>, id: u64, req: EnqueueRequest) -> ServerMessage {
    if req.trust_member_config {
        let via = req.via.unwrap_or(ApproveVia::Cli);
        let refusal = trust_refusal(core.handle.config(), via, &req.env).or_else(|| {
            (req.caller == sluice_gate_proto::Caller::Agent)
                .then(|| trust_refusal_text("this client declared itself an agent"))
        });
        if let Some(refusal) = refusal {
            return err(id, ErrorCode::NotAllowed, refusal);
        }
    }
    let repo = canonical(&req.repo);
    let git = core.git_for(&req.env);
    let (g2, r2, branch, head, base) = (
        git.clone(),
        repo.clone(),
        req.branch.clone(),
        req.head_sha.clone(),
        req.base.clone(),
    );
    let resolved = tokio::task::spawn_blocking(move || {
        if !r2.join("objects").is_dir() {
            return Err((
                ErrorCode::BadRequest,
                format!("{} is not a git common dir", r2.display()),
            ));
        }
        if branch.trim().is_empty() || branch == "HEAD" {
            return Err((
                ErrorCode::BadRequest,
                "a branch is required (detached HEAD?)".to_owned(),
            ));
        }
        let Some(full) = g2.resolve_commit(&r2, &head).ok().flatten() else {
            return Err((
                ErrorCode::BadRequest,
                format!("{head} is not a commit of {}", r2.display()),
            ));
        };
        // The head's config only picks the default target: an explicit base
        // does not need it, so an unparsable file is refused only without one.
        let defaults = match target::defaults_at(&g2, &r2, &full) {
            Ok(d) => d,
            Err(e) if base.as_deref().is_none_or(|b| b.trim().is_empty()) => {
                return Err((ErrorCode::BadRequest, e.to_string()));
            }
            Err(_) => sluice_gate_core::config::QueueConfig::default(),
        };
        // Remote queries of the resolution (`ls-remote`) are bounded by the
        // head's `[queue] network_timeout`: a stall only delays this request.
        let g2 = g2.with_network_timeout(defaults.network_timeout);
        match target::resolve(&g2, &r2, &branch, base.as_deref(), &defaults) {
            Ok(t) => {
                // The endpoint the learned `target_protected` facts hang on.
                let endpoint = (!t.is_local())
                    .then(|| target::endpoint_hash(&g2, &r2, &t.remote).ok())
                    .flatten();
                Ok((full, t, endpoint))
            }
            Err(e) => Err((
                if e.is_policy() {
                    ErrorCode::NotAllowed
                } else {
                    ErrorCode::BadRequest
                },
                e.to_string(),
            )),
        }
    })
    .await;
    let (head_sha, tgt, endpoint) = match resolved {
        Ok(Ok(v)) => v,
        Ok(Err((ecode, why))) => return err(id, ecode, why),
        Err(e) => return err(id, ErrorCode::Internal, e.to_string()),
    };
    let new = NewJob {
        id: sluice_gate_core::dirs::job_id(),
        repo,
        worktree: canonical(&req.worktree),
        branch: req.branch.clone(),
        head_sha,
        remote: tgt.remote.clone(),
        target: tgt.branch.clone(),
        caller: req.caller,
        session: req.session.clone(),
        env: allowlisted(&req.env),
        trust_config: req.trust_member_config,
    };
    let actor = actor_of(req.via);
    match core.write(actor, |w| {
        let j = w.insert_job(&new)?;
        let mut view = job_view(w.tx(), &j)?;
        if let Some(hash) = &endpoint {
            view.learned_protection =
                store::protected_target(w.tx(), hash, &j.target)?.map(|p| LearnedProtection {
                    first_seen: rfc3339(p.first_seen),
                    evidence: p.evidence,
                });
        }
        Ok(view)
    }) {
        Ok(job) => ServerMessage::QueueJob { id, job },
        Err(e) => store_err(id, &e),
    }
}

async fn status(core: &Arc<Core>, id: u64, repo: Option<&Path>) -> ServerMessage {
    let repo = repo.map(canonical);
    let read = core.read(|tx| {
        let jobs = store::jobs(tx, repo.as_deref())?
            .iter()
            .map(|j| job_view(tx, j))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut trains = Vec::new();
        // Trusted trains still to be judged: (index, repo, base, head).
        let mut to_diff = Vec::new();
        for t in store::trains(tx, repo.as_deref())? {
            if is_trusted(&t) && matches!(t.state, T::Verifying | T::Ready) {
                let head = store::train_jobs(tx, &t)?
                    .first()
                    .map(|j| j.head_sha.clone());
                if let Some(head) = head {
                    to_diff.push((trains.len(), t.repo.clone(), t.base_sha.clone(), head));
                }
            }
            trains.push(train_view(&t));
        }
        Ok((
            QueueStatusReport {
                schema: QUEUE_STATUS_SCHEMA_VERSION,
                trains,
                jobs,
            },
            to_diff,
        ))
    });
    match read {
        Ok((mut status, to_diff)) => {
            if !to_diff.is_empty() {
                let git = core.git.clone();
                let diffs = runner::blocking(move || {
                    to_diff
                        .into_iter()
                        .map(|(i, repo, base, head)| {
                            (i, crate::trust::config_diff(&git, &repo, &base, &head))
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
                for (i, diff) in diffs {
                    status.trains[i].config_diff = Some(diff);
                }
            }
            ServerMessage::QueueStatus { id, status }
        }
        Err(e) => store_err(id, &e),
    }
}

/// Why an approval is refused by the agent policy, or `None`. Unless the
/// user config sets `[queue] allow_agent_approval = true`, an approval
/// through MCP is refused, and so is one through the CLI whose client
/// environment (the agent-detection variables the CLI sends next to the
/// allowlisted ones) is detected as an agent's with the user's
/// `[agent_detection]` rules.
pub fn agent_approval_refusal(
    config: &sluice_gate_core::config::UserConfig,
    via: ApproveVia,
    env: &EnvMap,
) -> Option<String> {
    if config.queue.allow_agent_approval {
        return None;
    }
    match agent_origin(config, via, env)? {
        Origin::Mcp => Some(
            "approval through MCP is disabled; set [queue] allow_agent_approval = true in the user config"
                .to_owned(),
        ),
        Origin::Agent(rule) => Some(format!(
            "approval is reserved to humans and this client is an agent ({rule}); \
             set [queue] allow_agent_approval = true in the user config to allow it"
        )),
    }
}

/// Where a request that is not a human's at the CLI came from.
enum Origin {
    /// The MCP tools.
    Mcp,
    /// A CLI whose environment is detected as an agent's (the rule that
    /// matched).
    Agent(String),
}

/// `None` for a human at the CLI; the agent-ish origin otherwise.
fn agent_origin(
    config: &sluice_gate_core::config::UserConfig,
    via: ApproveVia,
    env: &EnvMap,
) -> Option<Origin> {
    if via == ApproveVia::Mcp {
        return Some(Origin::Mcp);
    }
    let detection = &config.agent_detection;
    match sluice_gate_core::agent::detect(env, &detection.env_any_set, &detection.env_any_equals) {
        sluice_gate_core::agent::Caller::Agent { rule } => Some(Origin::Agent(rule)),
        sluice_gate_core::agent::Caller::Human => None,
    }
}

/// The refusal text of everything `--trust-member-config` touches.
fn trust_refusal_text(who: &str) -> String {
    format!(
        "building a train from the branch's own .sluice.toml (--trust-member-config) is reserved \
         to a human at the CLI, and {who}: a branch that chooses its own gates needs a person \
         to read its configuration. [queue] allow_agent_approval does not change this"
    )
}

/// Why a request that enqueues or retries with `--trust-member-config`, or
/// approves a train built that way, is refused, or `None`. Stricter than
/// [`agent_approval_refusal`]: `[queue] allow_agent_approval` never lifts it.
pub fn trust_refusal(
    config: &sluice_gate_core::config::UserConfig,
    via: ApproveVia,
    env: &EnvMap,
) -> Option<String> {
    Some(trust_refusal_text(&match agent_origin(config, via, env)? {
        Origin::Mcp => "this request comes through MCP".to_owned(),
        Origin::Agent(rule) => format!("this client is an agent ({rule})"),
    }))
}

fn approve(
    core: &Core,
    id: u64,
    train_id: &str,
    hash: &str,
    via: ApproveVia,
    env: &EnvMap,
) -> ServerMessage {
    if let Some(refusal) = agent_approval_refusal(core.handle.config(), via, env) {
        return err(id, ErrorCode::NotAllowed, refusal);
    }
    let actor = if via == ApproveVia::Mcp {
        Actor::Mcp
    } else {
        Actor::Cli
    };
    // The agent check below needs the client's whole environment (its
    // detection variables); only the allowlist is stored for the push.
    let client_env = env;
    let env = allowlisted(env);
    let r = core.write(actor, |w| {
        let Some(t) = store::train(w.tx(), train_id)? else {
            return Err(store::refuse(Refusal::NotFound(format!(
                "no train {train_id}"
            ))));
        };
        // A train that runs its branch's own configuration: humans only, even
        // when the user allowed agents to approve.
        if is_trusted(&t)
            && let Some(refusal) = trust_refusal(core.handle.config(), via, client_env)
        {
            return Err(store::refuse(Refusal::NotAllowed(format!(
                "train {train_id} runs the {CONFIG_SOURCE_MEMBER}'s own .sluice.toml; {refusal}"
            ))));
        }
        if t.state == T::Dissolved {
            return Err(store::refuse(Refusal::Stale(format!(
                "train {train_id} was dissolved ({}); check 'sluice queue status'",
                t.note.as_deref().unwrap_or("rebuilt")
            ))));
        }
        if t.state != T::Ready {
            return Err(store::refuse(Refusal::State(format!(
                "train {train_id} is {}, not ready",
                crate::model::train_state_str(t.state)
            ))));
        }
        let jobs = store::train_jobs(w.tx(), &t)?;
        let current = current_hash(&t, &jobs, J::Ready);
        if t.train_hash.as_deref() != Some(hash) || current.as_deref() != Some(hash) {
            return Err(store::refuse(Refusal::Stale(format!(
                "train {train_id} changed since it was displayed; check 'sluice queue status'"
            ))));
        }
        for j in jobs.iter().filter(|j| j.state == J::Ready) {
            w.job_to(&j.id, J::Ready, J::Approved, &JobEdit::default())?;
        }
        w.train_to(
            train_id,
            T::Ready,
            T::Approved,
            &TrainEdit {
                approve: Some((via, hash.to_owned(), env.clone())),
                ..TrainEdit::default()
            },
        )?;
        store::train(w.tx(), train_id)
    });
    match r {
        Ok(Some(t)) => ServerMessage::QueueApproved {
            id,
            train: train_view(&t),
        },
        Ok(None) => err(id, ErrorCode::NotFound, format!("no train {train_id}")),
        Err(e) => store_err(id, &e),
    }
}

/// The fields of a `queue-retry` request.
struct Retry<'a> {
    job_id: &'a str,
    head_sha: &'a str,
    via: Option<ApproveVia>,
    /// `--trust-member-config`.
    trust: bool,
    /// The client's agent-detection variables.
    env: &'a EnvMap,
}

async fn retry(core: &Arc<Core>, id: u64, req: &Retry<'_>) -> ServerMessage {
    let (job_id, head_sha) = (req.job_id, req.head_sha);
    let actor = actor_of(req.via);
    if req.trust
        && let Some(refusal) = trust_refusal(
            core.handle.config(),
            req.via.unwrap_or(ApproveVia::Cli),
            req.env,
        )
    {
        return err(id, ErrorCode::NotAllowed, refusal);
    }
    let Ok(Some(j)) = core.read(|tx| store::job(tx, job_id)) else {
        return err(id, ErrorCode::NotFound, format!("no queue job {job_id}"));
    };
    let (git, repo, head) = (core.git.clone(), j.repo.clone(), head_sha.to_owned());
    let full = tokio::task::spawn_blocking(move || git.resolve_commit(&repo, &head).ok().flatten())
        .await
        .ok()
        .flatten();
    let Some(full) = full else {
        return err(
            id,
            ErrorCode::BadRequest,
            format!("{head_sha} is not a commit"),
        );
    };
    let r = core.write(actor, |w| {
        let Some(j) = store::job(w.tx(), job_id)? else {
            return Err(store::refuse(Refusal::NotFound(format!(
                "no queue job {job_id}"
            ))));
        };
        if !matches!(j.state, J::Conflict | J::Failed) {
            return Err(store::refuse(Refusal::State(format!(
                "job {job_id} is {}; only conflict or failed jobs can be retried",
                crate::model::job_state_str(j.state)
            ))));
        }
        w.job_to(
            job_id,
            j.state,
            J::Queued,
            &JobEdit {
                head_sha: Some(full.clone()),
                requeue: true,
                bump_attempts: true,
                train_id: Some(None),
                reason: Some(None),
                detail: Some(None),
                solo: Some(false),
                // A plain retry goes back to the base's configuration.
                trust_config: Some(req.trust),
                note: Some(if req.trust {
                    "retry (trusting the member's config)".into()
                } else {
                    "retry".into()
                }),
                ..JobEdit::default()
            },
        )
    });
    match r {
        Ok(()) => job_reply(core, id, job_id),
        Err(e) => store_err(id, &e),
    }
}

fn cancel(core: &Core, id: u64, job_id: &str, actor: Actor) -> ServerMessage {
    let r = core.write(actor, |w| {
        let Some(j) = store::job(w.tx(), job_id)? else {
            return Err(store::refuse(Refusal::NotFound(format!(
                "no queue job {job_id}"
            ))));
        };
        if matches!(j.state, J::Pushing | J::NeedsReconcile) {
            return w.request_cancel(job_id);
        }
        if !job_cancellable_now(j.state) {
            return Err(store::refuse(Refusal::State(format!(
                "job {job_id} is {}",
                crate::model::job_state_str(j.state)
            ))));
        }
        w.job_to(
            job_id,
            j.state,
            J::Cancelled,
            &JobEdit {
                note: Some("cancel".into()),
                ..JobEdit::default()
            },
        )?;
        // A ready/approved train loses its approval at once; trains being
        // built notice the cancelled member and dissolve themselves.
        if matches!(j.state, J::Ready | J::Approved)
            && let Some(tid) = &j.train_id
            && let Some(t) = store::train(w.tx(), tid)?
            && matches!(t.state, T::Ready | T::Approved)
        {
            w.dissolve(&t, t.state, "member cancelled", false)?;
        }
        Ok(())
    });
    match r {
        Ok(()) => job_reply(core, id, job_id),
        Err(e) => store_err(id, &e),
    }
}

async fn reconcile(core: &Arc<Core>, id: u64, repo: Option<&Path>) -> ServerMessage {
    let repo = repo.map(canonical);
    let trains = match core.read(|tx| {
        let mut out = Vec::new();
        for t in store::trains(tx, repo.as_deref())? {
            if t.state == T::NeedsReconcile {
                let jobs = store::train_jobs(tx, &t)?;
                let m = store::marker(tx, &t.id)?;
                out.push((t, jobs, m));
            }
        }
        Ok(out)
    }) {
        Ok(v) => v,
        Err(e) => return store_err(id, &e),
    };
    let mut results = Vec::new();
    for (t, jobs, m) in trains {
        let env = t.push_env.clone().unwrap_or_default();
        let git = core
            .git_for(&env)
            .with_network_timeout(t.settings.network_timeout());
        let t2 = t.clone();
        let push = m
            .as_ref()
            .and_then(|m| m.push_pid.map(|p| (p, m.push_start_ticks)));
        let cancel_requested = jobs.iter().any(|j| j.cancel_requested);
        let input = tokio::task::spawn_blocking(move || {
            let s = t2.merge_sha.clone().unwrap_or_default();
            gather(
                &git,
                &TrainFacts {
                    repo: &t2.repo,
                    remote: &t2.remote,
                    target: &t2.target,
                    endpoint_hash: &t2.endpoint_hash,
                    merge_sha: &s,
                    base_sha: &t2.base_sha,
                    push,
                    cancel_requested,
                },
            )
        })
        .await;
        let Ok(input) = input else {
            results.push(ReconcileResult {
                train_id: t.id.clone(),
                decision: ReconcileDecision::Refused,
                reason: Some("internal".into()),
            });
            continue;
        };
        let decision = decide(&input);
        tracing::info!(train = %t.id, ?input, ?decision, "reconcile");
        let applied = apply_decision(core, &t, &decision);
        // Pin ref and worktree go only once the decision is recorded: a
        // refused write leaves the train `needs_reconcile`, and the next
        // reconcile needs both.
        let cleanup = applied.is_ok() && !matches!(decision, Decision::Refuse(_));
        let (dec, reason) = match (&decision, applied) {
            (_, Err(e)) => (ReconcileDecision::Refused, Some(store_msg(&e))),
            (Decision::Refuse(r), Ok(())) => (ReconcileDecision::Refused, Some((*r).to_owned())),
            (Decision::Landed(n), Ok(())) => (ReconcileDecision::Landed, n.map(str::to_owned)),
            (Decision::Ready, Ok(())) => (ReconcileDecision::Ready, None),
            (Decision::Queued, Ok(())) => (ReconcileDecision::Queued, None),
            (Decision::Cancelled, Ok(())) => (ReconcileDecision::Cancelled, None),
        };
        if cleanup {
            let (git, repo, wt, tid) = (
                core.git.clone(),
                t.repo.clone(),
                t.worktree.clone(),
                t.id.clone(),
            );
            let keep_wt = matches!(decision, Decision::Ready);
            let _ = tokio::task::spawn_blocking(move || {
                let _ = git.repo(
                    &repo,
                    &["update-ref", "-d", &format!("refs/sluice/pending/{tid}")],
                );
                if !keep_wt {
                    git.remove_worktree(&repo, &wt);
                }
            })
            .await;
        }
        results.push(ReconcileResult {
            train_id: t.id.clone(),
            decision: dec,
            reason,
        });
    }
    ServerMessage::QueueReconcile { id, results }
}

fn apply_decision(core: &Core, t: &TrainRow, d: &Decision) -> Result<(), StoreError> {
    if matches!(d, Decision::Refuse(_)) {
        return Ok(());
    }
    core.write(Actor::Reconcile, |w| {
        let jobs = store::train_jobs(w.tx(), t)?;
        let parked: Vec<&JobRow> = jobs
            .iter()
            .filter(|j| j.state == J::NeedsReconcile)
            .collect();
        w.delete_marker(&t.id)?;
        match d {
            Decision::Refuse(_) => Ok(()),
            Decision::Landed(note) => {
                for j in &parked {
                    w.job_to(
                        &j.id,
                        J::NeedsReconcile,
                        J::Landed,
                        &JobEdit {
                            reason: Some(None),
                            detail: Some(None),
                            note: Some("reconcile: landed".into()),
                            ..JobEdit::default()
                        },
                    )?;
                }
                w.train_to(
                    &t.id,
                    T::NeedsReconcile,
                    T::Landed,
                    &TrainEdit {
                        note: Some(note.map(str::to_owned)),
                        event_note: Some(note.unwrap_or("reconcile: landed").to_owned()),
                        ..TrainEdit::default()
                    },
                )
            }
            Decision::Ready => {
                for j in &parked {
                    w.job_to(
                        &j.id,
                        J::NeedsReconcile,
                        J::Ready,
                        &JobEdit {
                            reason: Some(None),
                            detail: Some(None),
                            note: Some("reconcile: nothing landed".into()),
                            ..JobEdit::default()
                        },
                    )?;
                }
                w.train_to(
                    &t.id,
                    T::NeedsReconcile,
                    T::Ready,
                    &TrainEdit {
                        void_approval: true,
                        note: Some(Some("nothing landed; approve again".into())),
                        event_note: Some("reconcile: nothing landed, approval voided".into()),
                        ..TrainEdit::default()
                    },
                )
            }
            Decision::Queued => w.dissolve(t, T::NeedsReconcile, "reconcile: target moved", false),
            Decision::Cancelled => w.dissolve(
                t,
                T::NeedsReconcile,
                "reconcile: nothing landed, cancel honoured",
                false,
            ),
        }
    })
}
