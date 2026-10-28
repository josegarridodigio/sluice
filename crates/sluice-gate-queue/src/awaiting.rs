//! Trains that land through a pull request (ADR-0025) once their branch is
//! on the remote: what a client reports about the pull request
//! (`queue-pr-facts`), what `queue cancel` does to a waiting job, and the
//! branch cleanup that follows.
//!
//! The daemon never talks to a forge. A client reads the pull request with
//! the user's own login and reports a [`PrFact`]; [`crate::pr::decide_pr`]
//! decides from it and from what the daemon reads itself (the remote's refs,
//! the target's history). Nothing here merges.

use std::sync::Arc;

use sluice_gate_core::forge::landing::branch_of;
use sluice_gate_core::forge::{PrFact, PrObservation};
use sluice_gate_proto::{
    ApproveVia, ErrorCode, QueueJobState as J, QueueTrainState as T, ReconcileDecision,
    ReconcileResult, ServerMessage,
};

use crate::extension::{err, job_reply, store_err};
use crate::git::{EnvMap, allowlisted};
use crate::pr::{PrDecision, TrainFacts, decide_pr, gather_pr};
use crate::runner::{Core, blocking, store_msg};
use crate::store::{
    self, Actor, JobEdit, JobRow, MarkerRow, PrColumns, TrainEdit, TrainRow, Writer,
};
use crate::target::endpoint;

/// A train waiting for its merge, as the facts handler loaded it.
struct Loaded {
    train: TrainRow,
    jobs: Vec<JobRow>,
    marker: MarkerRow,
}

fn load(
    core: &Core,
    train_id: &str,
) -> Result<Option<Loaded>, sluice_gate_daemon::server::StoreError> {
    core.read(|tx| {
        let Some(train) = store::train(tx, train_id)? else {
            return Ok(None);
        };
        let jobs = store::train_jobs(tx, &train)?;
        Ok(store::marker(tx, train_id)?.map(|marker| Loaded {
            train,
            jobs,
            marker,
        }))
    })
}

fn refused(train_id: &str, why: impl Into<String>) -> ReconcileResult {
    ReconcileResult {
        train_id: train_id.to_owned(),
        decision: ReconcileDecision::Refused,
        reason: Some(why.into()),
    }
}

/// The reply to `queue-pr-facts`: one result per fact.
pub async fn pr_facts(core: &Arc<Core>, id: u64, facts: &[PrFact], env: &EnvMap) -> ServerMessage {
    let mut results = Vec::new();
    for fact in facts {
        results.push(one(core, fact, env).await);
    }
    ServerMessage::QueueReconcile { id, results }
}

async fn one(core: &Arc<Core>, fact: &PrFact, env: &EnvMap) -> ReconcileResult {
    let loaded = match load(core, &fact.train_id) {
        Ok(Some(l)) if l.marker.pr.branch.is_some() => l,
        Ok(_) => return refused(&fact.train_id, "no_pull_request_train"),
        Err(e) => return refused(&fact.train_id, store_msg(&e)),
    };
    match loaded.train.state {
        T::AwaitingMerge => judge(core, &loaded, fact).await,
        T::Dissolved => clean_branch(core, &loaded, fact, env).await,
        _ => refused(&fact.train_id, "not_awaiting_merge"),
    }
}

/// The remote facts the daemon reads for a train.
fn facts_of<'a>(t: &'a TrainRow, branch: &'a str, s: &'a str) -> TrainFacts<'a> {
    TrainFacts {
        repo: &t.repo,
        remote: &t.remote,
        target: &t.target,
        endpoint_hash: &t.endpoint_hash,
        branch,
        merge_sha: s,
        merge_tree: t.merge_tree.as_deref(),
        base_sha: &t.base_sha,
    }
}

async fn judge(core: &Arc<Core>, l: &Loaded, fact: &PrFact) -> ReconcileResult {
    let t = &l.train;
    let git = core
        .git_for(&t.push_env.clone().unwrap_or_default())
        .with_network_timeout(t.settings.network_timeout());
    let (t2, observation) = (t.clone(), fact.observation.clone());
    let input = blocking(move || {
        let s = t2.merge_sha.clone().unwrap_or_default();
        let branch = branch_of(&t2.id);
        gather_pr(&git, &facts_of(&t2, &branch, &s), &observation)
    })
    .await;
    let Ok(input) = input else {
        return refused(&fact.train_id, "internal");
    };
    let decision = decide_pr(&input);
    tracing::info!(train = %t.id, ?input, ?decision, "pull request facts");
    let applied = core.write(Actor::Reconcile, |w| apply(w, l, fact, &decision));
    let (decision_out, reason) = match (&decision, applied) {
        (_, Err(e)) => (ReconcileDecision::Refused, Some(store_msg(&e))),
        (PrDecision::Landed(note), Ok(())) => (ReconcileDecision::Landed, note.map(str::to_owned)),
        (PrDecision::Failed(why), Ok(())) => (ReconcileDecision::Failed, Some((*why).to_owned())),
        (PrDecision::Wait { note, .. }, Ok(())) => (ReconcileDecision::AwaitingMerge, note.clone()),
    };
    if matches!(
        decision_out,
        ReconcileDecision::Landed | ReconcileDecision::Failed
    ) {
        drop_pin(core, t).await;
    }
    ReconcileResult {
        train_id: fact.train_id.clone(),
        decision: decision_out,
        reason,
    }
}

/// The pin ref (`refs/sluice/pending/<id>`) of a train that ended.
async fn drop_pin(core: &Arc<Core>, t: &TrainRow) {
    let (git, repo, pin) = (
        core.git.clone(),
        t.repo.clone(),
        format!("refs/sluice/pending/{}", t.id),
    );
    let _ = blocking(move || git.repo(&repo, &["update-ref", "-d", &pin])).await;
}

fn observed_state(o: &PrObservation) -> Option<&'static str> {
    match o {
        PrObservation::Open { .. } => Some("open"),
        PrObservation::Merged { .. } => Some("merged"),
        PrObservation::Closed { .. } => Some("closed"),
        _ => None,
    }
}

/// What a report changes in the marker's pull-request columns.
fn record(pr: &mut PrColumns, fact: &PrFact, decision: &PrDecision, now: u64) {
    if let Some(n) = fact.number
        && (pr.number.is_none() || fact.created)
    {
        pr.number = Some(n);
        pr.url.clone_from(&fact.url);
    }
    if fact.created {
        pr.created = true;
    }
    if !fact.auto_merge.is_default() {
        pr.auto_merge = fact.auto_merge;
    }
    pr.observed_at = Some(now);
    match &fact.observation {
        PrObservation::Unreachable { .. } | PrObservation::CreateFailed { .. } => {}
        other => pr.observed_state = observed_state(other).map(str::to_owned),
    }
    match decision {
        PrDecision::Wait { note, target_moved } => {
            pr.note.clone_from(note);
            if matches!(fact.observation, PrObservation::Open { .. }) {
                pr.target_moved = *target_moved;
            }
        }
        PrDecision::Landed(note) => pr.note = note.map(str::to_owned),
        PrDecision::Failed(why) => pr.note = Some((*why).to_owned()),
    }
}

fn apply(
    w: &Writer<'_, '_>,
    l: &Loaded,
    fact: &PrFact,
    decision: &PrDecision,
) -> rusqlite::Result<()> {
    let t = &l.train;
    let mut pr = store::marker(w.tx(), &t.id)?.map_or_else(|| l.marker.pr.clone(), |m| m.pr);
    record(&mut pr, fact, decision, w.now());
    w.set_pr(&t.id, &pr)?;
    let waiting: Vec<&JobRow> = l
        .jobs
        .iter()
        .filter(|j| j.state == J::AwaitingMerge)
        .collect();
    match decision {
        PrDecision::Wait { .. } => Ok(()),
        PrDecision::Landed(note) => {
            for j in &waiting {
                w.job_to(
                    &j.id,
                    J::AwaitingMerge,
                    J::Landed,
                    &JobEdit {
                        note: Some("pull request merged".into()),
                        ..JobEdit::default()
                    },
                )?;
            }
            w.train_to(
                &t.id,
                T::AwaitingMerge,
                T::Landed,
                &TrainEdit {
                    note: Some(note.map(str::to_owned)),
                    event_note: Some(note.unwrap_or("outcome: pull request merged").to_owned()),
                    ..TrainEdit::default()
                },
            )
        }
        PrDecision::Failed(why) => {
            for j in &waiting {
                w.job_to(
                    &j.id,
                    J::AwaitingMerge,
                    J::Failed,
                    &JobEdit {
                        reason: Some(Some((*why).to_owned())),
                        detail: Some(Some(serde_json::json!({
                            "pull_request": pr.url,
                            "branch": pr.branch,
                        }))),
                        note: Some((*why).to_owned()),
                        ..JobEdit::default()
                    },
                )?;
            }
            w.dissolve(t, T::AwaitingMerge, why, false)
        }
    }
}

/// A reconcile of a `needs_reconcile` pull-request train that found its
/// branch on the remote: the jobs and the train go on to wait for the merge.
///
/// # Errors
///
/// Refusals of the store.
pub fn resume_waiting(
    w: &Writer<'_, '_>,
    t: &TrainRow,
    parked: &[&JobRow],
) -> rusqlite::Result<()> {
    for j in parked {
        w.job_to(
            &j.id,
            J::NeedsReconcile,
            J::AwaitingMerge,
            &JobEdit {
                reason: Some(None),
                detail: Some(None),
                note: Some("reconcile: branch pushed".into()),
                ..JobEdit::default()
            },
        )?;
    }
    if let Some(m) = store::marker(w.tx(), &t.id)? {
        let mut pr = m.pr;
        pr.since = Some(w.now());
        w.set_pr(&t.id, &pr)?;
    }
    w.train_to(
        &t.id,
        T::NeedsReconcile,
        T::AwaitingMerge,
        &TrainEdit {
            note: Some(None),
            event_note: Some("reconcile: branch pushed".into()),
            ..TrainEdit::default()
        },
    )
}

/// `queue cancel` of a job whose train waits for its merge: the job is
/// cancelled and the train dissolved (its other members go back to the
/// queue). The pull request is the client's to close and the branch is
/// deleted afterwards ([`clean_branch`]), so nothing here touches the forge.
/// Refused for MCP: the pull request exists because a human approved.
pub async fn cancel(
    core: &Arc<Core>,
    id: u64,
    j: &JobRow,
    actor: Actor,
    via: Option<ApproveVia>,
) -> ServerMessage {
    if via == Some(ApproveVia::Mcp) {
        return err(
            id,
            ErrorCode::NotAllowed,
            format!(
                "job {} waits for a pull request a human opened: only a human at 'sluice queue \
                 cancel' can withdraw it",
                j.id
            ),
        );
    }
    let train = j.train_id.clone();
    let r = core.write(actor, |w| {
        w.job_to(
            &j.id,
            J::AwaitingMerge,
            J::Cancelled,
            &JobEdit {
                note: Some("cancel".into()),
                ..JobEdit::default()
            },
        )?;
        if let Some(tid) = &train
            && let Some(t) = store::train(w.tx(), tid)?
            && t.state == T::AwaitingMerge
        {
            w.dissolve(&t, T::AwaitingMerge, "member cancelled", false)?;
        }
        Ok(train.clone())
    });
    match r {
        Ok(Some(tid)) => {
            if let Ok(Some(t)) = core.read(|tx| store::train(tx, &tid)) {
                drop_pin(core, &t).await;
            }
            job_reply(core, id, &j.id)
        }
        Ok(None) => job_reply(core, id, &j.id),
        Err(e) => store_err(id, &e),
    }
}

/// The follow-up to a cancel: the client says the pull request of the
/// dissolved train is closed (or gone); Sluice deletes the branch it created,
/// with a lease on the commit it pushed, and only when the pull request was
/// its own. Anything else leaves the branch (`sluice doctor` lists it).
async fn clean_branch(
    core: &Arc<Core>,
    l: &Loaded,
    fact: &PrFact,
    env: &EnvMap,
) -> ReconcileResult {
    let (t, pr) = (&l.train, &l.marker.pr);
    let id = &fact.train_id;
    if pr.branch_deleted {
        return refused(id, "branch_already_deleted");
    }
    if !matches!(
        fact.observation,
        PrObservation::Closed { .. } | PrObservation::Missing
    ) {
        return refused(id, "train_dissolved");
    }
    // The branch is always Sluice's; the pull request may be somebody else's
    // (found, not opened): deleting its branch would close it.
    let ours = pr.created || (pr.number.is_none() && fact.number.is_none());
    if !ours {
        note_branch(
            core,
            t,
            "branch kept: the pull request was not opened by Sluice",
        );
        return refused(id, "pull_request_not_ours");
    }
    let git = core
        .git_for(&allowlisted(env))
        .with_network_timeout(t.settings.network_timeout());
    let (t2, s) = (t.clone(), t.merge_sha.clone().unwrap_or_default());
    let outcome = blocking(move || delete_branch(&git, &t2, &s)).await;
    match outcome {
        Ok(Ok(())) => {
            let _ = core.write(Actor::Reconcile, |w| {
                let mut cols = store::marker(w.tx(), &t.id)?.map_or_else(|| pr.clone(), |m| m.pr);
                cols.branch_deleted = true;
                cols.note = None;
                w.set_pr(&t.id, &cols)
            });
            ReconcileResult {
                train_id: id.clone(),
                decision: ReconcileDecision::Cancelled,
                reason: Some("branch_deleted".into()),
            }
        }
        Ok(Err(why)) => {
            note_branch(core, t, &format!("branch_delete_failed: {why}"));
            refused(id, format!("branch_delete_failed: {why}"))
        }
        Err(e) => refused(id, format!("{e:#}")),
    }
}

fn note_branch(core: &Core, t: &TrainRow, note: &str) {
    let _ = core.write(Actor::Reconcile, |w| {
        if let Some(m) = store::marker(w.tx(), &t.id)? {
            let mut cols = m.pr;
            cols.note = Some(note.to_owned());
            w.set_pr(&t.id, &cols)?;
        }
        Ok(())
    });
}

/// Deletes `sluice/train/<id>` on the train's remote while it is at `S`; a
/// branch that is already gone counts as deleted. Blocking.
fn delete_branch(git: &crate::git::Git, t: &TrainRow, s: &str) -> Result<(), String> {
    let (url, hash) = endpoint(git, &t.repo, &t.remote).map_err(|e| format!("{e:#}"))?;
    if hash != t.endpoint_hash {
        return Err("endpoint_changed".into());
    }
    let branch = branch_of(&t.id);
    let reference = format!("refs/heads/{branch}");
    let refs = git
        .ls_remote(&t.repo, &url, &[&reference])
        .map_err(|e| format!("{e:#}"))?;
    match refs.get(&reference) {
        None => Ok(()),
        Some(sha) if sha == s => git
            .delete_remote_branch(&t.repo, &url, &branch, s, t.settings.network_timeout())
            .map_err(|e| format!("{e:#}")),
        Some(_) => Err("the branch moved: it is not the commit Sluice pushed".into()),
    }
}
