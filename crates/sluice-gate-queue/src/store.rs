//! The queue tables in the daemon's `sluice.db` (design section 4).
//!
//! Every function takes an open [`Transaction`] (the daemon's
//! `BEGIN IMMEDIATE` from `DaemonHandle::store_tx`). Writes go through a
//! [`Writer`], created only after [`LeaseFence::check`] succeeded, and are
//! compare-and-swap updates on the expected state; each state change appends
//! a `queue_events` row. A logical refusal ([`Refusal`]) is returned as a
//! `rusqlite::Error::ToSqlConversionFailure` so the surrounding transaction rolls
//! back; [`refusal`] recovers it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use sluice_gate_daemon::server::{LeaseFence, StoreError};
use sluice_gate_proto::{ApproveVia, Caller, QueueJobState, QueueTrainState, TrainGate};

use crate::model::{
    job_state_str, job_transition_allowed, parse_job_state, parse_train_state, train_state_str,
    train_transition_allowed,
};

/// Version of the queue tables (`queue_schema.version`).
pub const QUEUE_SCHEMA_VERSION: i64 = 1;

/// Environment map (allowlisted credentials variables).
pub type EnvMap = BTreeMap<String, String>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS queue_schema (version INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS queue_jobs (
  id TEXT PRIMARY KEY, repo TEXT NOT NULL,
  worktree TEXT NOT NULL, branch TEXT NOT NULL, head_sha TEXT NOT NULL,
  remote TEXT NOT NULL, target TEXT NOT NULL,
  state TEXT NOT NULL, queue_order INTEGER NOT NULL, train_id TEXT,
  caller TEXT NOT NULL, session TEXT,
  reason TEXT, detail TEXT,
  cancel_requested INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 1,
  solo INTEGER NOT NULL DEFAULT 0, env TEXT,
  enqueued_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, finished_at INTEGER);
CREATE UNIQUE INDEX IF NOT EXISTS queue_jobs_active_branch ON queue_jobs(repo, branch)
  WHERE state NOT IN ('landed','cancelled');
CREATE TABLE IF NOT EXISTS queue_trains (
  id TEXT PRIMARY KEY, repo TEXT NOT NULL, remote TEXT NOT NULL, target TEXT NOT NULL,
  endpoint_hash TEXT NOT NULL, base_sha TEXT NOT NULL, state TEXT NOT NULL,
  merge_sha TEXT, merge_tree TEXT, worktree TEXT NOT NULL,
  gates TEXT, members TEXT NOT NULL DEFAULT '[]', settings TEXT,
  train_hash TEXT, claim_token TEXT NOT NULL,
  approved_at INTEGER, approved_via TEXT,
  approved_hash TEXT, push_env TEXT,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, landed_at INTEGER, note TEXT);
CREATE TABLE IF NOT EXISTS queue_push_markers (
  train_id TEXT PRIMARY KEY, sha TEXT NOT NULL, base_sha TEXT NOT NULL,
  remote TEXT NOT NULL, endpoint_hash TEXT NOT NULL, push_refs TEXT NOT NULL,
  audit_ref TEXT, pin_ref TEXT NOT NULL, claim_token TEXT NOT NULL,
  push_pid INTEGER, push_start_ticks INTEGER, written_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS queue_events (
  at INTEGER NOT NULL, job_id TEXT, train_id TEXT, from_state TEXT, to_state TEXT NOT NULL,
  actor TEXT NOT NULL, note TEXT);
";

/// A logical refusal of a queue write; the transaction is rolled back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The daemon's lease row no longer carries our token.
    #[error("the daemon lost its lease; nothing was written")]
    LeaseLost,
    /// No such job or train.
    #[error("{0}")]
    NotFound(String),
    /// The row is not in the expected state.
    #[error("{0}")]
    State(String),
    /// The branch already has an active job.
    #[error("{0}")]
    Duplicate(String),
    /// The train hash does not match.
    #[error("{0}")]
    Stale(String),
    /// Refused by policy.
    #[error("{0}")]
    NotAllowed(String),
    /// The queue tables are newer than this build.
    #[error("queue_schema {0} is newer than this build (expects {QUEUE_SCHEMA_VERSION})")]
    Schema(i64),
}

/// Wraps a refusal as a rusqlite error (rolls the transaction back).
#[must_use]
pub fn refuse(r: Refusal) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(r))
}

/// The refusal carried by a failed transaction, if that is what it was.
#[must_use]
pub fn refusal(e: &StoreError) -> Option<&Refusal> {
    match e {
        StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(b)) => {
            b.downcast_ref::<Refusal>()
        }
        _ => None,
    }
}

/// Who changed a row (`queue_events.actor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// The runner task.
    Runner,
    /// A CLI request.
    Cli,
    /// An MCP request.
    Mcp,
    /// `queue-reconcile`.
    Reconcile,
    /// Daemon start (`on_start`).
    DaemonStart,
}

impl Actor {
    /// Spelling in `queue_events`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runner => "runner",
            Self::Cli => "cli",
            Self::Mcp => "mcp",
            Self::Reconcile => "reconcile",
            Self::DaemonStart => "daemon-start",
        }
    }
}

/// Train settings read from the base commit's `[queue]` at claim time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TrainSettings {
    /// `[queue] push_timeout` in milliseconds.
    pub push_timeout_ms: u64,
    /// `[queue] audit_ref`.
    pub audit_ref: bool,
}

/// One `queue_jobs` row.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    /// Job id.
    pub id: String,
    /// Git common dir.
    pub repo: PathBuf,
    /// Worktree the job came from.
    pub worktree: PathBuf,
    /// Branch.
    pub branch: String,
    /// Captured head.
    pub head_sha: String,
    /// Remote (`""` for a local target).
    pub remote: String,
    /// Target branch.
    pub target: String,
    /// State.
    pub state: QueueJobState,
    /// Queue order (global, increasing).
    pub queue_order: i64,
    /// Train, once claimed.
    pub train_id: Option<String>,
    /// Who enqueued.
    pub caller: Caller,
    /// Agent session.
    pub session: Option<String>,
    /// Reason of the last side exit.
    pub reason: Option<String>,
    /// Structured detail.
    pub detail: Option<serde_json::Value>,
    /// Cancel requested while pushing.
    pub cancel_requested: bool,
    /// Attempts.
    pub attempts: u32,
    /// The next train containing this job has no other member (split).
    pub solo: bool,
    /// Enqueuer's allowlisted environment (credentials for fetches).
    pub env: EnvMap,
    /// Enqueue time (unix ms).
    pub enqueued_at: u64,
    /// Last change (unix ms).
    pub updated_at: u64,
    /// Terminal time (unix ms).
    pub finished_at: Option<u64>,
}

/// One `queue_trains` row.
#[derive(Debug, Clone, PartialEq)]
pub struct TrainRow {
    /// Train id.
    pub id: String,
    /// Git common dir.
    pub repo: PathBuf,
    /// Remote (`""` for a local target).
    pub remote: String,
    /// Target branch.
    pub target: String,
    /// Endpoint binding.
    pub endpoint_hash: String,
    /// Base commit.
    pub base_sha: String,
    /// State.
    pub state: QueueTrainState,
    /// `S`.
    pub merge_sha: Option<String>,
    /// `T`.
    pub merge_tree: Option<String>,
    /// Merge worktree.
    pub worktree: PathBuf,
    /// Gates.
    pub gates: Vec<TrainGate>,
    /// Member job ids in merge order (as claimed).
    pub members: Vec<String>,
    /// Settings from the base config.
    pub settings: TrainSettings,
    /// Train hash once ready.
    pub train_hash: Option<String>,
    /// Lease token of the daemon owning the train.
    pub claim_token: String,
    /// Approval time.
    pub approved_at: Option<u64>,
    /// Approval surface.
    pub approved_via: Option<ApproveVia>,
    /// Approved hash.
    pub approved_hash: Option<String>,
    /// Approver's allowlisted environment.
    pub push_env: Option<EnvMap>,
    /// Creation time.
    pub created_at: u64,
    /// Last change.
    pub updated_at: u64,
    /// Landing time.
    pub landed_at: Option<u64>,
    /// Note.
    pub note: Option<String>,
}

/// One `queue_push_markers` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerRow {
    /// Train.
    pub train_id: String,
    /// `S`.
    pub sha: String,
    /// Base the push leases on.
    pub base_sha: String,
    /// Remote (`""` for a local target).
    pub remote: String,
    /// Endpoint binding.
    pub endpoint_hash: String,
    /// Refs updated (`refs/heads/<target>`, audit ref).
    pub push_refs: Vec<String>,
    /// Audit ref, when pushed.
    pub audit_ref: Option<String>,
    /// Local pin ref.
    pub pin_ref: String,
    /// Lease token at write time.
    pub claim_token: String,
    /// Push process id, once spawned.
    pub push_pid: Option<u32>,
    /// Its start ticks.
    pub push_start_ticks: Option<u64>,
    /// Write time.
    pub written_at: u64,
}

/// Creates the tables (idempotent) and checks `queue_schema`.
///
/// # Errors
///
/// SQLite errors; [`Refusal::Schema`] when the tables are newer.
pub fn init(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(SCHEMA)?;
    let v: Option<i64> = tx
        .query_row("SELECT max(version) FROM queue_schema", [], |r| r.get(0))
        .optional()?
        .flatten();
    match v {
        None => {
            tx.execute(
                "INSERT INTO queue_schema(version) VALUES (?1)",
                [QUEUE_SCHEMA_VERSION],
            )?;
        }
        Some(v) if v > QUEUE_SCHEMA_VERSION => return Err(refuse(Refusal::Schema(v))),
        Some(_) => {}
    }
    Ok(())
}

fn i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_owned())
}

fn from_json<T: for<'de> Deserialize<'de> + Default>(s: Option<String>) -> T {
    s.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn caller_str(c: Caller) -> &'static str {
    match c {
        Caller::Agent => "agent",
        Caller::Human => "human",
    }
}

fn via_str(v: ApproveVia) -> &'static str {
    match v {
        ApproveVia::Cli => "cli",
        ApproveVia::Mcp => "mcp",
    }
}

const JOB_COLS: &str = "id, repo, worktree, branch, head_sha, remote, target, state, queue_order, \
     train_id, caller, session, reason, detail, cancel_requested, attempts, solo, env, \
     enqueued_at, updated_at, finished_at";

fn job_from_row(r: &Row<'_>) -> rusqlite::Result<JobRow> {
    let caller: String = r.get(10)?;
    Ok(JobRow {
        id: r.get(0)?,
        repo: PathBuf::from(r.get::<_, String>(1)?),
        worktree: PathBuf::from(r.get::<_, String>(2)?),
        branch: r.get(3)?,
        head_sha: r.get(4)?,
        remote: r.get(5)?,
        target: r.get(6)?,
        state: parse_job_state(&r.get::<_, String>(7)?),
        queue_order: r.get(8)?,
        train_id: r.get(9)?,
        caller: if caller == "agent" {
            Caller::Agent
        } else {
            Caller::Human
        },
        session: r.get(11)?,
        reason: r.get(12)?,
        detail: r
            .get::<_, Option<String>>(13)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        cancel_requested: r.get::<_, i64>(14)? != 0,
        attempts: u32::try_from(r.get::<_, i64>(15)?).unwrap_or(1),
        solo: r.get::<_, i64>(16)? != 0,
        env: from_json(r.get(17)?),
        enqueued_at: u(r.get(18)?),
        updated_at: u(r.get(19)?),
        finished_at: r.get::<_, Option<i64>>(20)?.map(u),
    })
}

const TRAIN_COLS: &str = "id, repo, remote, target, endpoint_hash, base_sha, state, merge_sha, \
     merge_tree, worktree, gates, members, settings, train_hash, claim_token, approved_at, \
     approved_via, approved_hash, push_env, created_at, updated_at, landed_at, note";

fn train_from_row(r: &Row<'_>) -> rusqlite::Result<TrainRow> {
    Ok(TrainRow {
        id: r.get(0)?,
        repo: PathBuf::from(r.get::<_, String>(1)?),
        remote: r.get(2)?,
        target: r.get(3)?,
        endpoint_hash: r.get(4)?,
        base_sha: r.get(5)?,
        state: parse_train_state(&r.get::<_, String>(6)?),
        merge_sha: r.get(7)?,
        merge_tree: r.get(8)?,
        worktree: PathBuf::from(r.get::<_, String>(9)?),
        gates: from_json(r.get(10)?),
        members: from_json(r.get(11)?),
        settings: from_json(r.get(12)?),
        train_hash: r.get(13)?,
        claim_token: r.get(14)?,
        approved_at: r.get::<_, Option<i64>>(15)?.map(u),
        approved_via: r.get::<_, Option<String>>(16)?.map(|s| {
            if s == "mcp" {
                ApproveVia::Mcp
            } else {
                ApproveVia::Cli
            }
        }),
        approved_hash: r.get(17)?,
        push_env: r
            .get::<_, Option<String>>(18)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        created_at: u(r.get(19)?),
        updated_at: u(r.get(20)?),
        landed_at: r.get::<_, Option<i64>>(21)?.map(u),
        note: r.get(22)?,
    })
}

const MARKER_COLS: &str = "train_id, sha, base_sha, remote, endpoint_hash, push_refs, audit_ref, \
     pin_ref, claim_token, push_pid, push_start_ticks, written_at";

fn marker_from_row(r: &Row<'_>) -> rusqlite::Result<MarkerRow> {
    Ok(MarkerRow {
        train_id: r.get(0)?,
        sha: r.get(1)?,
        base_sha: r.get(2)?,
        remote: r.get(3)?,
        endpoint_hash: r.get(4)?,
        push_refs: from_json(r.get(5)?),
        audit_ref: r.get(6)?,
        pin_ref: r.get(7)?,
        claim_token: r.get(8)?,
        push_pid: r
            .get::<_, Option<i64>>(9)?
            .and_then(|p| u32::try_from(p).ok()),
        push_start_ticks: r.get::<_, Option<i64>>(10)?.map(u),
        written_at: u(r.get(11)?),
    })
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// One job.
///
/// # Errors
///
/// SQLite errors.
pub fn job(tx: &Transaction<'_>, id: &str) -> rusqlite::Result<Option<JobRow>> {
    tx.query_row(
        &format!("SELECT {JOB_COLS} FROM queue_jobs WHERE id = ?1"),
        [id],
        job_from_row,
    )
    .optional()
}

/// Jobs, in queue order, optionally restricted to one repository.
///
/// # Errors
///
/// SQLite errors.
pub fn jobs(tx: &Transaction<'_>, repo: Option<&Path>) -> rusqlite::Result<Vec<JobRow>> {
    let mut stmt = tx.prepare(&format!(
        "SELECT {JOB_COLS} FROM queue_jobs WHERE (?1 IS NULL OR repo = ?1) ORDER BY queue_order"
    ))?;
    stmt.query_map([repo.map(path_str)], job_from_row)?
        .collect()
}

/// Jobs in `state`, in queue order.
///
/// # Errors
///
/// SQLite errors.
pub fn jobs_in_state(tx: &Transaction<'_>, state: QueueJobState) -> rusqlite::Result<Vec<JobRow>> {
    let mut stmt = tx.prepare(&format!(
        "SELECT {JOB_COLS} FROM queue_jobs WHERE state = ?1 ORDER BY queue_order"
    ))?;
    stmt.query_map([job_state_str(state)], job_from_row)?
        .collect()
}

/// Jobs currently attached to `train_id`, in the train's member order.
///
/// # Errors
///
/// SQLite errors.
pub fn train_jobs(tx: &Transaction<'_>, train: &TrainRow) -> rusqlite::Result<Vec<JobRow>> {
    let mut out = Vec::new();
    for id in &train.members {
        if let Some(j) = job(tx, id)?
            && j.train_id.as_deref() == Some(train.id.as_str())
        {
            out.push(j);
        }
    }
    Ok(out)
}

/// One train.
///
/// # Errors
///
/// SQLite errors.
pub fn train(tx: &Transaction<'_>, id: &str) -> rusqlite::Result<Option<TrainRow>> {
    tx.query_row(
        &format!("SELECT {TRAIN_COLS} FROM queue_trains WHERE id = ?1"),
        [id],
        train_from_row,
    )
    .optional()
}

/// Trains, oldest first, optionally restricted to one repository.
///
/// # Errors
///
/// SQLite errors.
pub fn trains(tx: &Transaction<'_>, repo: Option<&Path>) -> rusqlite::Result<Vec<TrainRow>> {
    let mut stmt = tx.prepare(&format!(
        "SELECT {TRAIN_COLS} FROM queue_trains WHERE (?1 IS NULL OR repo = ?1) \
         ORDER BY created_at, id"
    ))?;
    stmt.query_map([repo.map(path_str)], train_from_row)?
        .collect()
}

/// Trains in one of `states`, oldest first.
///
/// # Errors
///
/// SQLite errors.
pub fn trains_in(
    tx: &Transaction<'_>,
    states: &[QueueTrainState],
) -> rusqlite::Result<Vec<TrainRow>> {
    Ok(trains(tx, None)?
        .into_iter()
        .filter(|t| states.contains(&t.state))
        .collect())
}

/// The push marker of a train.
///
/// # Errors
///
/// SQLite errors.
pub fn marker(tx: &Transaction<'_>, train_id: &str) -> rusqlite::Result<Option<MarkerRow>> {
    tx.query_row(
        &format!("SELECT {MARKER_COLS} FROM queue_push_markers WHERE train_id = ?1"),
        [train_id],
        marker_from_row,
    )
    .optional()
}

/// 1-based position of a queued job among the queued jobs of its target.
///
/// # Errors
///
/// SQLite errors.
pub fn position(tx: &Transaction<'_>, j: &JobRow) -> rusqlite::Result<Option<usize>> {
    if j.state != QueueJobState::Queued {
        return Ok(None);
    }
    let n: i64 = tx.query_row(
        "SELECT count(*) FROM queue_jobs WHERE state = 'queued' AND repo = ?1 AND remote = ?2 \
         AND target = ?3 AND queue_order <= ?4",
        params![path_str(&j.repo), j.remote, j.target, j.queue_order],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(n).ok())
}

/// One audit row: `(from, to, actor, note)`.
pub type EventRow = (Option<String>, String, String, Option<String>);

/// Audit trail rows of a train or job: `(from, to, actor, note)`, oldest first.
///
/// # Errors
///
/// SQLite errors.
pub fn events(tx: &Transaction<'_>, id: &str) -> rusqlite::Result<Vec<EventRow>> {
    let mut stmt = tx.prepare(
        "SELECT from_state, to_state, actor, note FROM queue_events \
         WHERE job_id = ?1 OR train_id = ?1 ORDER BY rowid",
    )?;
    stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect()
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// A fenced writer: exists only after the lease check passed in this
/// transaction.
#[derive(Debug)]
pub struct Writer<'a, 'c> {
    tx: &'a Transaction<'c>,
    fence: &'a LeaseFence,
    actor: Actor,
    now: u64,
}

/// Fields changed together with a job state.
#[derive(Debug, Clone, Default)]
pub struct JobEdit {
    /// `Some(x)` sets `reason` to `x`.
    pub reason: Option<Option<String>>,
    /// `Some(x)` sets `detail` to `x`.
    pub detail: Option<Option<serde_json::Value>>,
    /// `Some(x)` sets `train_id` to `x`.
    pub train_id: Option<Option<String>>,
    /// New head (retry).
    pub head_sha: Option<String>,
    /// Move to the back of the queue (retry).
    pub requeue: bool,
    /// Increment `attempts`.
    pub bump_attempts: bool,
    /// Set `solo`.
    pub solo: Option<bool>,
    /// Set `cancel_requested`.
    pub cancel_requested: Option<bool>,
    /// Audit note.
    pub note: Option<String>,
}

/// Fields changed together with a train state.
#[derive(Debug, Clone, Default)]
pub struct TrainEdit {
    /// `S`.
    pub merge_sha: Option<String>,
    /// `T`.
    pub merge_tree: Option<String>,
    /// Gates.
    pub gates: Option<Vec<TrainGate>>,
    /// Members.
    pub members: Option<Vec<String>>,
    /// `Some(x)` sets `train_hash`.
    pub train_hash: Option<Option<String>>,
    /// Records an approval `(via, hash, env)`.
    pub approve: Option<(ApproveVia, String, EnvMap)>,
    /// Clears the approval.
    pub void_approval: bool,
    /// `Some(x)` sets `note`.
    pub note: Option<Option<String>>,
    /// Audit note.
    pub event_note: Option<String>,
}

impl<'a, 'c> Writer<'a, 'c> {
    /// Checks the lease fence and returns a writer.
    ///
    /// # Errors
    ///
    /// [`Refusal::LeaseLost`] (as a rusqlite error) or SQLite errors.
    pub fn new(
        tx: &'a Transaction<'c>,
        fence: &'a LeaseFence,
        actor: Actor,
    ) -> rusqlite::Result<Self> {
        if !fence.check(tx)? {
            return Err(refuse(Refusal::LeaseLost));
        }
        Ok(Self {
            tx,
            fence,
            actor,
            now: sluice_gate_core::dirs::now_ms(),
        })
    }

    /// The transaction.
    #[must_use]
    pub fn tx(&self) -> &'a Transaction<'c> {
        self.tx
    }

    /// The lease token.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.fence.token
    }

    /// Current time of this writer (unix ms).
    #[must_use]
    pub fn now(&self) -> u64 {
        self.now
    }

    /// Appends one audit row.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn event(
        &self,
        job_id: Option<&str>,
        train_id: Option<&str>,
        from: Option<&str>,
        to: &str,
        note: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO queue_events(at, job_id, train_id, from_state, to_state, actor, note) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                i(self.now),
                job_id,
                train_id,
                from,
                to,
                self.actor.as_str(),
                note
            ],
        )?;
        Ok(())
    }

    /// Inserts a queued job.
    ///
    /// # Errors
    ///
    /// [`Refusal::Duplicate`] when the branch has an active job; SQLite errors.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_job(&self, new: &NewJob) -> rusqlite::Result<JobRow> {
        let dup: Option<String> = self
            .tx
            .query_row(
                "SELECT id FROM queue_jobs WHERE repo = ?1 AND branch = ?2 \
                 AND state NOT IN ('landed','cancelled')",
                params![path_str(&new.repo), new.branch],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = dup {
            return Err(refuse(Refusal::Duplicate(format!(
                "branch {} already has an active job ({other})",
                new.branch
            ))));
        }
        let order = self.next_order()?;
        self.tx.execute(
            &format!(
                "INSERT INTO queue_jobs({JOB_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', \
                 ?8, NULL, ?9, ?10, NULL, NULL, 0, 1, 0, ?11, ?12, ?12, NULL)"
            ),
            params![
                new.id,
                path_str(&new.repo),
                path_str(&new.worktree),
                new.branch,
                new.head_sha,
                new.remote,
                new.target,
                order,
                caller_str(new.caller),
                new.session,
                json(&new.env),
                i(self.now),
            ],
        )?;
        self.event(Some(&new.id), None, None, "queued", Some("enqueue"))?;
        job(self.tx, &new.id)?.ok_or_else(|| refuse(Refusal::NotFound(new.id.clone())))
    }

    fn next_order(&self) -> rusqlite::Result<i64> {
        let max: Option<i64> =
            self.tx
                .query_row("SELECT max(queue_order) FROM queue_jobs", [], |r| r.get(0))?;
        Ok(max.unwrap_or(0) + 1)
    }

    /// Compare-and-swap of a job's state: succeeds only when the job is in
    /// `from` and the table allows `from → to`.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`], [`Refusal::State`]; SQLite errors.
    pub fn job_to(
        &self,
        id: &str,
        from: QueueJobState,
        to: QueueJobState,
        edit: &JobEdit,
    ) -> rusqlite::Result<()> {
        if !job_transition_allowed(from, to) {
            return Err(refuse(Refusal::State(format!(
                "job transition {} -> {} is not allowed",
                job_state_str(from),
                job_state_str(to)
            ))));
        }
        let mut sets = vec!["state = ?1".to_owned(), "updated_at = ?2".to_owned()];
        let mut vals: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(job_state_str(to)), Box::new(i(self.now))];
        let mut push = |col: &str, v: Box<dyn rusqlite::ToSql>| {
            vals.push(v);
            sets.push(format!("{col} = ?{}", vals.len()));
        };
        if let Some(r) = &edit.reason {
            push("reason", Box::new(r.clone()));
        }
        if let Some(d) = &edit.detail {
            push("detail", Box::new(d.as_ref().map(json)));
        }
        if let Some(t) = &edit.train_id {
            push("train_id", Box::new(t.clone()));
        }
        if let Some(h) = &edit.head_sha {
            push("head_sha", Box::new(h.clone()));
        }
        if edit.requeue {
            push("queue_order", Box::new(self.next_order()?));
        }
        if let Some(s) = edit.solo {
            push("solo", Box::new(i64::from(s)));
        }
        if let Some(c) = edit.cancel_requested {
            push("cancel_requested", Box::new(i64::from(c)));
        }
        if crate::model::job_is_terminal(to) {
            push("finished_at", Box::new(i(self.now)));
        } else {
            push("finished_at", Box::new(Option::<i64>::None));
        }
        if edit.bump_attempts {
            sets.push("attempts = attempts + 1".to_owned());
        }
        vals.push(Box::new(id.to_owned()));
        let id_idx = vals.len();
        vals.push(Box::new(job_state_str(from)));
        let from_idx = vals.len();
        let sql = format!(
            "UPDATE queue_jobs SET {} WHERE id = ?{id_idx} AND state = ?{from_idx}",
            sets.join(", ")
        );
        let n = self.tx.execute(
            &sql,
            rusqlite::params_from_iter(vals.iter().map(AsRef::as_ref)),
        )?;
        if n == 0 {
            return Err(match job(self.tx, id)? {
                None => refuse(Refusal::NotFound(format!("no queue job {id}"))),
                Some(j) => refuse(Refusal::State(format!(
                    "job {id} is {}, expected {}",
                    job_state_str(j.state),
                    job_state_str(from)
                ))),
            });
        }
        let train: Option<String> = job(self.tx, id)?.and_then(|j| j.train_id);
        self.event(
            Some(id),
            train.as_deref(),
            Some(job_state_str(from)),
            job_state_str(to),
            edit.note.as_deref(),
        )
    }

    /// Records on a `queued` job why no train could be built for it
    /// (`reason`, `detail = {"error", "failures"}`), without a state change.
    /// `failures` counts consecutive attempts that failed for one of the
    /// base reasons (`base_unavailable`, `base_config_invalid`); a claim or
    /// a retry clears it. Returns the new count, or `None` when the job is no
    /// longer `queued`.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn note_queued_failure(
        &self,
        id: &str,
        reason: &str,
        error: &str,
    ) -> rusqlite::Result<Option<u32>> {
        let Some(j) = job(self.tx, id)? else {
            return Ok(None);
        };
        if j.state != QueueJobState::Queued {
            return Ok(None);
        }
        let previous = if matches!(
            j.reason.as_deref(),
            Some("base_unavailable" | "base_config_invalid")
        ) {
            j.detail
                .as_ref()
                .and_then(|d| d.get("failures"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0)
        } else {
            0
        };
        let failures = previous.saturating_add(1);
        let detail = serde_json::json!({ "error": error, "failures": failures });
        self.tx.execute(
            "UPDATE queue_jobs SET reason = ?2, detail = ?3, updated_at = ?4 \
             WHERE id = ?1 AND state = 'queued'",
            params![id, reason, json(&detail), i(self.now)],
        )?;
        self.event(
            Some(id),
            j.train_id.as_deref(),
            Some("queued"),
            "queued",
            Some(&format!("{reason} (attempt {failures})")),
        )?;
        Ok(Some(failures))
    }

    /// Sets `cancel_requested` without a state change.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn request_cancel(&self, id: &str) -> rusqlite::Result<()> {
        self.tx.execute(
            "UPDATE queue_jobs SET cancel_requested = 1, updated_at = ?2 WHERE id = ?1",
            params![id, i(self.now)],
        )?;
        let st = job(self.tx, id)?.map_or("unknown", |j| job_state_str(j.state));
        self.event(Some(id), None, Some(st), st, Some("cancel requested"))
    }

    /// Inserts a train in `building`.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn insert_train(&self, t: &NewTrain) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO queue_trains(id, repo, remote, target, endpoint_hash, base_sha, state, \
             worktree, gates, members, settings, claim_token, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'building', ?7, '[]', ?8, ?9, ?10, ?11, ?11)",
            params![
                t.id,
                path_str(&t.repo),
                t.remote,
                t.target,
                t.endpoint_hash,
                t.base_sha,
                path_str(&t.worktree),
                json(&t.members),
                json(&t.settings),
                self.fence.token,
                i(self.now),
            ],
        )?;
        self.event(None, Some(&t.id), None, "building", Some("claimed"))
    }

    /// Compare-and-swap of a train's state.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`], [`Refusal::State`]; SQLite errors.
    pub fn train_to(
        &self,
        id: &str,
        from: QueueTrainState,
        to: QueueTrainState,
        edit: &TrainEdit,
    ) -> rusqlite::Result<()> {
        if from != to && !train_transition_allowed(from, to) {
            return Err(refuse(Refusal::State(format!(
                "train transition {} -> {} is not allowed",
                train_state_str(from),
                train_state_str(to)
            ))));
        }
        let mut sets = vec!["state = ?1".to_owned(), "updated_at = ?2".to_owned()];
        let mut vals: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(train_state_str(to)), Box::new(i(self.now))];
        let mut push = |col: &str, v: Box<dyn rusqlite::ToSql>| {
            vals.push(v);
            sets.push(format!("{col} = ?{}", vals.len()));
        };
        if let Some(s) = &edit.merge_sha {
            push("merge_sha", Box::new(s.clone()));
        }
        if let Some(s) = &edit.merge_tree {
            push("merge_tree", Box::new(s.clone()));
        }
        if let Some(g) = &edit.gates {
            push("gates", Box::new(json(g)));
        }
        if let Some(m) = &edit.members {
            push("members", Box::new(json(m)));
        }
        if let Some(h) = &edit.train_hash {
            push("train_hash", Box::new(h.clone()));
        }
        if let Some(n) = &edit.note {
            push("note", Box::new(n.clone()));
        }
        if let Some((via, hash, env)) = &edit.approve {
            push("approved_at", Box::new(i(self.now)));
            push("approved_via", Box::new(via_str(*via)));
            push("approved_hash", Box::new(hash.clone()));
            push("push_env", Box::new(json(env)));
        }
        if to == QueueTrainState::Landed {
            push("landed_at", Box::new(i(self.now)));
        }
        if edit.void_approval {
            sets.push(
                "approved_at = NULL, approved_via = NULL, approved_hash = NULL, push_env = NULL"
                    .to_owned(),
            );
        }
        vals.push(Box::new(id.to_owned()));
        let id_idx = vals.len();
        vals.push(Box::new(train_state_str(from)));
        let from_idx = vals.len();
        vals.push(Box::new(self.fence.token.clone()));
        let tok_idx = vals.len();
        let sql = format!(
            "UPDATE queue_trains SET {} WHERE id = ?{id_idx} AND state = ?{from_idx} \
             AND claim_token = ?{tok_idx}",
            sets.join(", ")
        );
        let n = self.tx.execute(
            &sql,
            rusqlite::params_from_iter(vals.iter().map(AsRef::as_ref)),
        )?;
        if n == 0 {
            return Err(match train(self.tx, id)? {
                None => refuse(Refusal::NotFound(format!("no train {id}"))),
                Some(t) if t.claim_token != self.fence.token => refuse(Refusal::LeaseLost),
                Some(t) => refuse(Refusal::State(format!(
                    "train {id} is {}, expected {}",
                    train_state_str(t.state),
                    train_state_str(from)
                ))),
            });
        }
        if from != to {
            self.event(
                None,
                Some(id),
                Some(train_state_str(from)),
                train_state_str(to),
                edit.event_note.as_deref(),
            )?;
        }
        Ok(())
    }

    /// Re-stamps every non-terminal train with this daemon's token
    /// (daemon start: the previous daemon's lease is gone).
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn adopt_trains(&self) -> rusqlite::Result<usize> {
        self.tx.execute(
            "UPDATE queue_trains SET claim_token = ?1 WHERE state NOT IN ('landed','dissolved')",
            [&self.fence.token],
        )
    }

    /// Inserts the write-ahead marker. The caller commits it with
    /// `DaemonHandle::store_tx_durable` (`synchronous = FULL`).
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn insert_marker(&self, m: &MarkerRow) -> rusqlite::Result<()> {
        self.tx.execute(
            &format!(
                "INSERT INTO queue_push_markers({MARKER_COLS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, ?10)"
            ),
            params![
                m.train_id,
                m.sha,
                m.base_sha,
                m.remote,
                m.endpoint_hash,
                json(&m.push_refs),
                m.audit_ref,
                m.pin_ref,
                self.fence.token,
                i(self.now),
            ],
        )?;
        Ok(())
    }

    /// Records the push process right after spawn.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn set_marker_pid(
        &self,
        train_id: &str,
        pid: u32,
        ticks: Option<u64>,
    ) -> rusqlite::Result<()> {
        self.tx.execute(
            "UPDATE queue_push_markers SET push_pid = ?2, push_start_ticks = ?3 WHERE train_id = ?1",
            params![train_id, i64::from(pid), ticks.map(i)],
        )?;
        Ok(())
    }

    /// Deletes a train's marker.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn delete_marker(&self, train_id: &str) -> rusqlite::Result<()> {
        self.tx.execute(
            "DELETE FROM queue_push_markers WHERE train_id = ?1",
            [train_id],
        )?;
        Ok(())
    }

    /// Dissolves a train from `from`: train → `dissolved` (approval voided,
    /// `note`), each member still attached and in a non-terminal state →
    /// `queued` (`attempts + 1`, back of the queue keeps its order, train
    /// cleared), or `cancelled` when a cancel was pending. Members in
    /// `conflict`/`failed`/`cancelled` keep their state. `solo` marks the
    /// re-queued members for single-member trains (split).
    ///
    /// # Errors
    ///
    /// Refusals of [`Writer::train_to`] / [`Writer::job_to`]; SQLite errors.
    pub fn dissolve(
        &self,
        train: &TrainRow,
        from: QueueTrainState,
        note: &str,
        solo: bool,
    ) -> rusqlite::Result<()> {
        for j in train_jobs(self.tx, train)? {
            let requeue_from = matches!(
                j.state,
                QueueJobState::Merging
                    | QueueJobState::Verifying
                    | QueueJobState::Ready
                    | QueueJobState::Approved
                    | QueueJobState::Pushing
                    | QueueJobState::NeedsReconcile
            );
            if !requeue_from {
                continue;
            }
            if j.cancel_requested && job_transition_allowed(j.state, QueueJobState::Cancelled) {
                self.job_to(
                    &j.id,
                    j.state,
                    QueueJobState::Cancelled,
                    &JobEdit {
                        note: Some(format!("{note}; cancel honoured")),
                        ..JobEdit::default()
                    },
                )?;
                continue;
            }
            self.job_to(
                &j.id,
                j.state,
                QueueJobState::Queued,
                &JobEdit {
                    train_id: Some(None),
                    bump_attempts: true,
                    solo: Some(solo),
                    note: Some(note.to_owned()),
                    ..JobEdit::default()
                },
            )?;
        }
        self.train_to(
            &train.id,
            from,
            QueueTrainState::Dissolved,
            &TrainEdit {
                void_approval: true,
                note: Some(Some(note.to_owned())),
                event_note: Some(note.to_owned()),
                ..TrainEdit::default()
            },
        )
    }
}

/// A job to insert.
#[derive(Debug, Clone)]
pub struct NewJob {
    /// Job id.
    pub id: String,
    /// Git common dir.
    pub repo: PathBuf,
    /// Worktree.
    pub worktree: PathBuf,
    /// Branch.
    pub branch: String,
    /// Head.
    pub head_sha: String,
    /// Remote (`""` local).
    pub remote: String,
    /// Target branch.
    pub target: String,
    /// Caller.
    pub caller: Caller,
    /// Session.
    pub session: Option<String>,
    /// Allowlisted env.
    pub env: EnvMap,
}

/// A train to insert.
#[derive(Debug, Clone)]
pub struct NewTrain {
    /// Train id.
    pub id: String,
    /// Git common dir.
    pub repo: PathBuf,
    /// Remote (`""` local).
    pub remote: String,
    /// Target.
    pub target: String,
    /// Endpoint binding.
    pub endpoint_hash: String,
    /// Base.
    pub base_sha: String,
    /// Merge worktree.
    pub worktree: PathBuf,
    /// Members in order.
    pub members: Vec<String>,
    /// Settings.
    pub settings: TrainSettings,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rusqlite::{Connection, TransactionBehavior};

    pub(crate) fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE lease (name TEXT PRIMARY KEY, token TEXT NOT NULL);
             INSERT INTO lease VALUES ('daemon', 'tok');",
        )
        .unwrap();
        c
    }

    pub(crate) fn fence(token: &str) -> LeaseFence {
        LeaseFence {
            instance: "i".into(),
            token: token.into(),
        }
    }

    fn with<R>(
        c: &mut Connection,
        f: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
    ) -> rusqlite::Result<R> {
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let r = f(&tx)?;
        tx.commit()?;
        Ok(r)
    }

    fn new_job(id: &str, branch: &str) -> NewJob {
        NewJob {
            id: id.into(),
            repo: "/r/.git".into(),
            worktree: "/r".into(),
            branch: branch.into(),
            head_sha: "h".into(),
            remote: "origin".into(),
            target: "main".into(),
            caller: Caller::Agent,
            session: None,
            env: EnvMap::new(),
        }
    }

    fn refusal_of(e: &rusqlite::Error) -> Option<&Refusal> {
        match e {
            rusqlite::Error::ToSqlConversionFailure(b) => b.downcast_ref::<Refusal>(),
            _ => None,
        }
    }

    #[test]
    fn schema_is_idempotent_and_newer_versions_fail_closed() {
        let mut c = conn();
        with(&mut c, init).unwrap();
        with(&mut c, init).unwrap();
        with(&mut c, |tx| {
            tx.execute("UPDATE queue_schema SET version = 2", [])?;
            Ok(())
        })
        .unwrap();
        let e = with(&mut c, init).unwrap_err();
        assert_eq!(refusal_of(&e), Some(&Refusal::Schema(2)));
    }

    #[test]
    fn base_failures_are_counted_on_queued_jobs_and_reset_by_a_claim() {
        let mut c = conn();
        with(&mut c, init).unwrap();
        let f = fence("tok");
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Cli)?.insert_job(&new_job("j1", "a"))
        })
        .unwrap();
        let note = |c: &mut Connection, reason: &str| {
            with(c, |tx| {
                Writer::new(tx, &f, Actor::Runner)?.note_queued_failure("j1", reason, "boom")
            })
            .unwrap()
        };
        assert_eq!(note(&mut c, "base_unavailable"), Some(1));
        // Consecutive failures count whatever the base reason.
        assert_eq!(note(&mut c, "base_config_invalid"), Some(2));
        let j = with(&mut c, |tx| job(tx, "j1")).unwrap().unwrap();
        assert_eq!(j.state, QueueJobState::Queued);
        assert_eq!(j.reason.as_deref(), Some("base_config_invalid"));
        assert_eq!(
            j.detail,
            Some(serde_json::json!({"error": "boom", "failures": 2}))
        );
        // A claim clears the reason: the next failure starts again at 1.
        with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            w.job_to(
                "j1",
                QueueJobState::Queued,
                QueueJobState::Merging,
                &JobEdit {
                    reason: Some(None),
                    detail: Some(None),
                    ..JobEdit::default()
                },
            )?;
            w.job_to(
                "j1",
                QueueJobState::Merging,
                QueueJobState::Queued,
                &JobEdit::default(),
            )
        })
        .unwrap();
        assert_eq!(note(&mut c, "base_unavailable"), Some(1));
        // Queued → failed is allowed (third failure); a non-queued job is skipped.
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.job_to(
                "j1",
                QueueJobState::Queued,
                QueueJobState::Failed,
                &JobEdit::default(),
            )
        })
        .unwrap();
        assert_eq!(note(&mut c, "base_unavailable"), None);
    }

    #[test]
    fn writes_are_fenced_on_the_lease() {
        let mut c = conn();
        with(&mut c, init).unwrap();
        let f = fence("other");
        let e = with(&mut c, |tx| Writer::new(tx, &f, Actor::Cli).map(|_| ())).unwrap_err();
        assert_eq!(refusal_of(&e), Some(&Refusal::LeaseLost));
    }

    #[test]
    fn enqueue_duplicate_and_cas_transitions() {
        let mut c = conn();
        with(&mut c, init).unwrap();
        let f = fence("tok");
        let j = with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Cli)?.insert_job(&new_job("j1", "a"))
        })
        .unwrap();
        assert_eq!(j.state, QueueJobState::Queued);
        assert_eq!(j.queue_order, 1);
        let e = with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Cli)?.insert_job(&new_job("j2", "a"))
        })
        .unwrap_err();
        assert!(matches!(refusal_of(&e), Some(Refusal::Duplicate(_))));
        // Wrong expected state: refused, nothing written.
        let e = with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.job_to(
                "j1",
                QueueJobState::Merging,
                QueueJobState::Verifying,
                &JobEdit::default(),
            )
        })
        .unwrap_err();
        assert!(matches!(refusal_of(&e), Some(Refusal::State(_))));
        // Disallowed transition.
        let e = with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.job_to(
                "j1",
                QueueJobState::Queued,
                QueueJobState::Landed,
                &JobEdit::default(),
            )
        })
        .unwrap_err();
        assert!(matches!(refusal_of(&e), Some(Refusal::State(_))));
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Cli)?.job_to(
                "j1",
                QueueJobState::Queued,
                QueueJobState::Cancelled,
                &JobEdit::default(),
            )
        })
        .unwrap();
        // A cancelled job frees the branch.
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Cli)?.insert_job(&new_job("j3", "a"))
        })
        .unwrap();
        let ev = with(&mut c, |tx| events(tx, "j1")).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[1].1, "cancelled");
        let j1 = with(&mut c, |tx| job(tx, "j1")).unwrap().unwrap();
        assert!(j1.finished_at.is_some());
    }

    #[test]
    fn dissolve_requeues_members_and_honours_cancel() {
        let mut c = conn();
        with(&mut c, init).unwrap();
        let f = fence("tok");
        with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            w.insert_job(&new_job("j1", "a"))?;
            w.insert_job(&new_job("j2", "b"))?;
            w.insert_train(&NewTrain {
                id: "t1".into(),
                repo: "/r/.git".into(),
                remote: "origin".into(),
                target: "main".into(),
                endpoint_hash: "e".into(),
                base_sha: "b".into(),
                worktree: "/s/merge/t1".into(),
                members: vec!["j1".into(), "j2".into()],
                settings: TrainSettings::default(),
            })?;
            for j in ["j1", "j2"] {
                w.job_to(
                    j,
                    QueueJobState::Queued,
                    QueueJobState::Merging,
                    &JobEdit {
                        train_id: Some(Some("t1".into())),
                        ..JobEdit::default()
                    },
                )?;
            }
            w.request_cancel("j2")?;
            Ok(())
        })
        .unwrap();
        with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::DaemonStart)?;
            let t = train(tx, "t1")?.unwrap();
            w.dissolve(&t, QueueTrainState::Building, "daemon restarted", false)
        })
        .unwrap();
        let (j1, j2, t) = with(&mut c, |tx| {
            Ok((
                job(tx, "j1")?.unwrap(),
                job(tx, "j2")?.unwrap(),
                train(tx, "t1")?.unwrap(),
            ))
        })
        .unwrap();
        assert_eq!(j1.state, QueueJobState::Queued);
        assert_eq!(j1.attempts, 2);
        assert_eq!(j1.train_id, None);
        assert_eq!(j2.state, QueueJobState::Cancelled);
        assert_eq!(t.state, QueueTrainState::Dissolved);
        // A train owned by another token cannot be changed.
        with(&mut c, |tx| {
            tx.execute(
                "UPDATE queue_trains SET state = 'ready', claim_token = 'old'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let e = with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.train_to(
                "t1",
                QueueTrainState::Ready,
                QueueTrainState::Approved,
                &TrainEdit::default(),
            )
        })
        .unwrap_err();
        assert_eq!(refusal_of(&e), Some(&Refusal::LeaseLost));
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::DaemonStart)?.adopt_trains()
        })
        .unwrap();
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.train_to(
                "t1",
                QueueTrainState::Ready,
                QueueTrainState::Approved,
                &TrainEdit {
                    approve: Some((ApproveVia::Cli, "h".into(), EnvMap::new())),
                    ..TrainEdit::default()
                },
            )
        })
        .unwrap();
        let t = with(&mut c, |tx| train(tx, "t1")).unwrap().unwrap();
        assert_eq!(t.approved_via, Some(ApproveVia::Cli));
        assert_eq!(t.approved_hash.as_deref(), Some("h"));
    }

    #[test]
    fn marker_round_trip() {
        let mut c = conn();
        with(&mut c, init).unwrap();
        let f = fence("tok");
        let m = MarkerRow {
            train_id: "t1".into(),
            sha: "s".into(),
            base_sha: "b".into(),
            remote: "origin".into(),
            endpoint_hash: "e".into(),
            push_refs: vec!["refs/heads/main".into()],
            audit_ref: Some("refs/sluice/landed/s".into()),
            pin_ref: "refs/sluice/pending/t1".into(),
            claim_token: "tok".into(),
            push_pid: None,
            push_start_ticks: None,
            written_at: 0,
        };
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.insert_marker(&m)
        })
        .unwrap();
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.set_marker_pid("t1", 42, Some(7))
        })
        .unwrap();
        let got = with(&mut c, |tx| marker(tx, "t1")).unwrap().unwrap();
        assert_eq!(got.push_pid, Some(42));
        assert_eq!(got.push_start_ticks, Some(7));
        assert_eq!(got.push_refs, m.push_refs);
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.delete_marker("t1")
        })
        .unwrap();
        assert!(with(&mut c, |tx| marker(tx, "t1")).unwrap().is_none());
    }
}
