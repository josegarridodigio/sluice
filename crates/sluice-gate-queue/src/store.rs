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
use sluice_gate_proto::{
    ApproveVia, Caller, QueueJobState, QueueTrainState, TrainGate, TrainPrepare,
};

use sluice_gate_core::config::Land;
use sluice_gate_core::forge::AutoMerge;

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

/// Targets that refused a direct push as protected (ADR-0025), one row per
/// endpoint and target. A table of its own, created in place by `init`
/// without bumping `queue_schema`: older binaries do not know it and never
/// read it.
const PROTECTED_TARGETS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS queue_protected_targets (
  endpoint_hash TEXT NOT NULL, target TEXT NOT NULL,
  first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL,
  evidence TEXT NOT NULL, hits INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (endpoint_hash, target));
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
///
/// A missing field (or a whole unreadable value) falls back to the real
/// `[queue]` defaults, never to zeros: a zero push timeout would kill the
/// push after 1 ms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrainSettings {
    /// `[queue] push_timeout` in milliseconds.
    pub push_timeout_ms: u64,
    /// `[queue] audit_ref`.
    pub audit_ref: bool,
    /// `[queue] network_timeout` in milliseconds; `0` also means the default.
    pub network_timeout_ms: u64,
    /// How the train lands (ADR-0025): from the target commit's `[queue]
    /// land` (or the user config's), never from a member.
    pub land: Land,
}

impl Default for TrainSettings {
    fn default() -> Self {
        let q = sluice_gate_core::config::QueueConfig::default();
        Self {
            push_timeout_ms: u64::try_from(q.push_timeout.as_millis()).unwrap_or(u64::MAX),
            audit_ref: q.audit_ref,
            network_timeout_ms: u64::try_from(q.network_timeout.as_millis()).unwrap_or(u64::MAX),
            land: Land::Push,
        }
    }
}

impl TrainSettings {
    /// Deadline of the train's network calls (watch, pre-push checks,
    /// reconcile).
    #[must_use]
    pub fn network_timeout(&self) -> std::time::Duration {
        if self.network_timeout_ms == 0 {
            sluice_gate_core::config::DEFAULT_NETWORK_TIMEOUT
        } else {
            std::time::Duration::from_millis(self.network_timeout_ms)
        }
    }
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
    /// A human asked (`--trust-member-config`) that this job's train be built
    /// from the branch's own `.sluice.toml`; the job is then always alone.
    pub trust_config: bool,
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
    /// State of the `[queue] prepare` run (column added by an in-place
    /// migration; never part of the train hash).
    pub prepare: Option<TrainPrepare>,
    /// `Some("member")` when the train runs its member's `.sluice.toml`
    /// (column added by an in-place migration).
    pub config_source: Option<String>,
    /// For such a train: `sha256` of the member's config blob, or `absent`;
    /// part of the train hash.
    pub config_hash: Option<String>,
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
    /// The pull-request side of a train that lands through one.
    pub pr: PrColumns,
}

/// The pull-request columns of a marker (added in place; ADR-0025). Empty
/// for a train that lands by push.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrColumns {
    /// `sluice/train/<id>`: set when the train lands through a pull request.
    pub branch: Option<String>,
    /// Pull request number.
    pub number: Option<u64>,
    /// Pull request address.
    pub url: Option<String>,
    /// Sluice's client opened it.
    pub created: bool,
    /// What the approver's client did about auto-merge.
    pub auto_merge: AutoMerge,
    /// When the train started waiting for the merge (unix ms).
    pub since: Option<u64>,
    /// When a client last reported it (unix ms).
    pub observed_at: Option<u64>,
    /// What the forge last said (`open`, `merged`, `closed`).
    pub observed_state: Option<String>,
    /// Commits the target gained since the base, as last measured.
    pub target_moved: Option<u64>,
    /// Remark of the last report.
    pub note: Option<String>,
    /// Sluice deleted the remote branch.
    pub branch_deleted: bool,
}

/// Creates the tables (idempotent) and checks `queue_schema`.
///
/// # Errors
///
/// SQLite errors; [`Refusal::Schema`] when the tables are newer.
pub fn init(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(SCHEMA)?;
    tx.execute_batch(PROTECTED_TARGETS_SCHEMA)?;
    add_column_if_missing(tx, "queue_trains", "prepare", "TEXT")?;
    add_column_if_missing(tx, "queue_trains", "config_source", "TEXT")?;
    add_column_if_missing(tx, "queue_trains", "config_hash", "TEXT")?;
    add_column_if_missing(
        tx,
        "queue_jobs",
        "trust_config",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    for (column, ty) in MARKER_PR_COLUMNS {
        add_column_if_missing(tx, "queue_push_markers", column, ty)?;
    }
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

/// Columns of `queue_push_markers` added for pull-request landing
/// (ADR-0025): nullable, or `NOT NULL` with a default.
const MARKER_PR_COLUMNS: [(&str, &str); 11] = [
    ("pr_branch", "TEXT"),
    ("pr_number", "INTEGER"),
    ("pr_url", "TEXT"),
    ("pr_created", "INTEGER NOT NULL DEFAULT 0"),
    ("pr_auto_merge", "TEXT"),
    ("pr_since", "INTEGER"),
    ("pr_observed_at", "INTEGER"),
    ("pr_observed_state", "TEXT"),
    ("pr_target_moved", "INTEGER"),
    ("pr_note", "TEXT"),
    ("pr_branch_deleted", "INTEGER NOT NULL DEFAULT 0"),
];

/// Adds a column to a table created by an earlier release (nullable, or
/// `NOT NULL` with a default); no
/// `queue_schema` bump, older binaries ignore it (they name their columns).
fn add_column_if_missing(
    tx: &Transaction<'_>,
    table: &str,
    column: &str,
    ty: &str,
) -> rusqlite::Result<()> {
    let present = {
        let mut stmt = tx.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        let mut found = false;
        for name in names {
            found |= name? == column;
        }
        found
    };
    if !present {
        tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
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

/// Parses a stored JSON column; absent or unreadable values give the type's
/// default, an unreadable one is logged.
fn from_json<T: for<'de> Deserialize<'de> + Default>(s: Option<String>) -> T {
    let Some(text) = s else {
        return T::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        tracing::warn!(
            "stored {} does not parse ({e}); using its default",
            std::any::type_name::<T>()
        );
        T::default()
    })
}

fn caller_str(c: Caller) -> &'static str {
    match c {
        Caller::Agent => "agent",
        Caller::Human => "human",
        Caller::Unknown => "unknown",
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
     enqueued_at, updated_at, finished_at, trust_config";

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
        trust_config: r.get::<_, i64>(21)? != 0,
    })
}

const TRAIN_COLS: &str = "id, repo, remote, target, endpoint_hash, base_sha, state, merge_sha, \
     merge_tree, worktree, gates, members, settings, train_hash, claim_token, approved_at, \
     approved_via, approved_hash, push_env, created_at, updated_at, landed_at, note, prepare, \
     config_source, config_hash";

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
        prepare: r
            .get::<_, Option<String>>(23)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        config_source: r.get(24)?,
        config_hash: r.get(25)?,
    })
}

const MARKER_COLS: &str = "train_id, sha, base_sha, remote, endpoint_hash, push_refs, audit_ref, \
     pin_ref, claim_token, push_pid, push_start_ticks, written_at, pr_branch, pr_number, pr_url, \
     pr_created, pr_auto_merge, pr_since, pr_observed_at, pr_observed_state, pr_target_moved, \
     pr_note, pr_branch_deleted";

fn auto_merge_str(a: AutoMerge) -> Option<&'static str> {
    match a {
        AutoMerge::NotRequested => None,
        AutoMerge::Armed => Some("armed"),
        AutoMerge::Failed => Some("failed"),
    }
}

fn parse_auto_merge(s: Option<&str>) -> AutoMerge {
    match s {
        Some("armed") => AutoMerge::Armed,
        Some("failed") => AutoMerge::Failed,
        _ => AutoMerge::NotRequested,
    }
}

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
        pr: PrColumns {
            branch: r.get(12)?,
            number: r.get::<_, Option<i64>>(13)?.map(u),
            url: r.get(14)?,
            created: r.get::<_, i64>(15)? != 0,
            auto_merge: parse_auto_merge(r.get::<_, Option<String>>(16)?.as_deref()),
            since: r.get::<_, Option<i64>>(17)?.map(u),
            observed_at: r.get::<_, Option<i64>>(18)?.map(u),
            observed_state: r.get(19)?,
            target_moved: r.get::<_, Option<i64>>(20)?.map(u),
            note: r.get(21)?,
            branch_deleted: r.get::<_, i64>(22)? != 0,
        },
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

/// Trains that are not finished (neither `landed` nor `dissolved`),
/// oldest first, optionally restricted to one repository.
///
/// # Errors
///
/// SQLite errors.
pub fn live_trains(tx: &Transaction<'_>, repo: Option<&Path>) -> rusqlite::Result<Vec<TrainRow>> {
    let mut stmt = tx.prepare(&format!(
        "SELECT {TRAIN_COLS} FROM queue_trains WHERE state NOT IN ('landed','dissolved') \
         AND (?1 IS NULL OR repo = ?1) ORDER BY created_at, id"
    ))?;
    stmt.query_map([repo.map(path_str)], train_from_row)?
        .collect()
}

/// Finished (`landed`/`dissolved`) trains changed at or after `since`
/// (unix ms): the ones whose merge worktree may still need removing.
///
/// # Errors
///
/// SQLite errors.
pub fn finished_trains_since(tx: &Transaction<'_>, since: u64) -> rusqlite::Result<Vec<TrainRow>> {
    let mut stmt = tx.prepare(&format!(
        "SELECT {TRAIN_COLS} FROM queue_trains WHERE state IN ('landed','dissolved') \
         AND updated_at >= ?1 ORDER BY created_at, id"
    ))?;
    stmt.query_map([i(since)], train_from_row)?.collect()
}

/// Rows removed by [`Writer::prune`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pruned {
    /// `landed`/`cancelled` jobs.
    pub jobs: usize,
    /// `landed`/`dissolved` trains.
    pub trains: usize,
    /// Push markers of finished trains.
    pub markers: usize,
    /// Audit rows.
    pub events: usize,
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

/// A target that refused a direct push as protected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedTarget {
    /// First refusal (milliseconds since the epoch).
    pub first_seen: u64,
    /// The server's words at the last refusal.
    pub evidence: String,
}

/// What is remembered about `target` on the endpoint `endpoint_hash`.
///
/// # Errors
///
/// SQLite errors.
pub fn protected_target(
    tx: &Transaction<'_>,
    endpoint_hash: &str,
    target: &str,
) -> rusqlite::Result<Option<ProtectedTarget>> {
    tx.query_row(
        "SELECT first_seen, evidence FROM queue_protected_targets \
         WHERE endpoint_hash = ?1 AND target = ?2",
        params![endpoint_hash, target],
        |r| {
            Ok(ProtectedTarget {
                first_seen: u(r.get(0)?),
                evidence: r.get(1)?,
            })
        },
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
    /// Set `trust_config` (`retry`).
    pub trust_config: Option<bool>,
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
    /// `Some(x)` sets `prepare`.
    pub prepare: Option<Option<TrainPrepare>>,
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
                 ?8, NULL, ?9, ?10, NULL, NULL, 0, 1, 0, ?11, ?12, ?12, NULL, ?13)"
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
                i64::from(new.trust_config),
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
        // The train of the job before this change, for the audit row.
        let train_before: Option<String> = job(self.tx, id)?.and_then(|j| j.train_id);
        if let Some(t) = &edit.train_id {
            push("train_id", Box::new(t.clone()));
        } else if matches!(to, QueueJobState::Conflict | QueueJobState::Failed) {
            // A conflicting or failed job leaves its train: the train goes on
            // with the other members, and nothing done to this job later
            // (cancel, retry) may touch that train.
            push("train_id", Box::new(Option::<String>::None));
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
        if let Some(t) = edit.trust_config {
            push("trust_config", Box::new(i64::from(t)));
        }
        if let Some(c) = edit.cancel_requested {
            push("cancel_requested", Box::new(i64::from(c)));
        }
        if crate::model::job_is_terminal(to) {
            push("finished_at", Box::new(i(self.now)));
            // The credentials allowlist is not needed any more.
            push("env", Box::new(Option::<String>::None));
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
        let train: Option<String> = job(self.tx, id)?.and_then(|j| j.train_id).or(train_before);
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
    /// base reasons (`base_unavailable`, `base_config_invalid`,
    /// `key_input_failed`, `prepare_failed`); a claim (except one that follows
    /// a `key_input_failed` or `prepare_failed`, whose count spans the attempts) or a retry clears
    /// it. Returns the new count, or `None` when the job is no
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
        self.note_queued_failure_with(id, reason, error, &serde_json::Value::Null)
    }

    /// Like [`Writer::note_queued_failure`], with the keys of the object
    /// `extra` added to the recorded detail (`job_id`, `log_path`, ...).
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn note_queued_failure_with(
        &self,
        id: &str,
        reason: &str,
        error: &str,
        extra: &serde_json::Value,
    ) -> rusqlite::Result<Option<u32>> {
        let Some(j) = job(self.tx, id)? else {
            return Ok(None);
        };
        if j.state != QueueJobState::Queued {
            return Ok(None);
        }
        let previous = if matches!(
            j.reason.as_deref(),
            Some(
                "base_unavailable" | "base_config_invalid" | "key_input_failed" | "prepare_failed"
            )
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
        let mut detail = serde_json::json!({ "error": error, "failures": failures });
        if let (Some(d), Some(e)) = (detail.as_object_mut(), extra.as_object()) {
            d.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
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

    /// Replaces `reason` and `detail` of the job `id` while it is in `state`,
    /// without a state change or an audit event (a stale remark of an
    /// earlier attempt is dropped).
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn set_job_remark(
        &self,
        id: &str,
        state: QueueJobState,
        reason: Option<&str>,
        detail: Option<&serde_json::Value>,
    ) -> rusqlite::Result<()> {
        self.tx.execute(
            "UPDATE queue_jobs SET reason = ?2, detail = ?3 WHERE id = ?1 AND state = ?4",
            params![id, reason, detail.map(json), job_state_str(state)],
        )?;
        Ok(())
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
             worktree, gates, members, settings, claim_token, created_at, updated_at, \
             config_source, config_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'building', ?7, '[]', ?8, ?9, ?10, ?11, ?11, \
             ?12, ?13)",
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
                t.config_source,
                t.config_hash,
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
        if let Some(p) = &edit.prepare {
            push("prepare", Box::new(p.as_ref().map(json)));
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
        if matches!(to, QueueTrainState::Landed | QueueTrainState::Dissolved)
            && edit.approve.is_none()
        {
            // Terminal: the approver's credentials allowlist is dropped.
            sets.push("push_env = NULL".to_owned());
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

    /// Retention (user config `[queue] retention`): deletes what finished
    /// before `cutoff` (unix ms): `landed`/`cancelled` jobs, `landed`/
    /// `dissolved` trains no job still points to, the markers of finished
    /// trains, and audit rows. Live rows are never touched.
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn prune(&self, cutoff: u64) -> rusqlite::Result<Pruned> {
        let c = i(cutoff);
        let jobs = self.tx.execute(
            "DELETE FROM queue_jobs WHERE state IN ('landed','cancelled') \
             AND coalesce(finished_at, updated_at) < ?1",
            [c],
        )?;
        let markers = self.tx.execute(
            "DELETE FROM queue_push_markers WHERE written_at < ?1 AND train_id IN \
             (SELECT id FROM queue_trains WHERE state IN ('landed','dissolved'))",
            [c],
        )?;
        let trains = self.tx.execute(
            "DELETE FROM queue_trains WHERE state IN ('landed','dissolved') AND updated_at < ?1 \
             AND id NOT IN (SELECT train_id FROM queue_jobs WHERE train_id IS NOT NULL) \
             AND id NOT IN (SELECT train_id FROM queue_push_markers)",
            [c],
        )?;
        let events = self
            .tx
            .execute("DELETE FROM queue_events WHERE at < ?1", [c])?;
        Ok(Pruned {
            jobs,
            trains,
            markers,
            events,
        })
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
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, ?10, ?11, NULL, NULL, 0, \
                 NULL, NULL, NULL, NULL, NULL, NULL, 0)"
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
                m.pr.branch,
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

    /// Remembers that `target` on `endpoint_hash` refused a direct push as
    /// protected (keeps the first date, refreshes the evidence).
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn record_protected_target(
        &self,
        endpoint_hash: &str,
        target: &str,
        evidence: &str,
    ) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO queue_protected_targets(endpoint_hash, target, first_seen, last_seen, \
             evidence) VALUES (?1, ?2, ?3, ?3, ?4) \
             ON CONFLICT(endpoint_hash, target) DO UPDATE SET last_seen = ?3, evidence = ?4, \
             hits = hits + 1",
            params![endpoint_hash, target, i(self.now), evidence],
        )?;
        Ok(())
    }

    /// Forgets a refusal: a push to the target landed, so it is not
    /// protected (any longer).
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn clear_protected_target(
        &self,
        endpoint_hash: &str,
        target: &str,
    ) -> rusqlite::Result<()> {
        self.tx.execute(
            "DELETE FROM queue_protected_targets WHERE endpoint_hash = ?1 AND target = ?2",
            params![endpoint_hash, target],
        )?;
        Ok(())
    }

    /// Writes the pull-request columns of a train's marker (all of them).
    ///
    /// # Errors
    ///
    /// SQLite errors.
    pub fn set_pr(&self, train_id: &str, pr: &PrColumns) -> rusqlite::Result<()> {
        self.tx.execute(
            "UPDATE queue_push_markers SET pr_branch = ?2, pr_number = ?3, pr_url = ?4, \
             pr_created = ?5, pr_auto_merge = ?6, pr_since = ?7, pr_observed_at = ?8, \
             pr_observed_state = ?9, pr_target_moved = ?10, pr_note = ?11, \
             pr_branch_deleted = ?12 WHERE train_id = ?1",
            params![
                train_id,
                pr.branch,
                pr.number.map(i),
                pr.url,
                i64::from(pr.created),
                auto_merge_str(pr.auto_merge),
                pr.since.map(i),
                pr.observed_at.map(i),
                pr.observed_state,
                pr.target_moved.map(i),
                pr.note,
                i64::from(pr.branch_deleted),
            ],
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
                    | QueueJobState::AwaitingMerge
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
    /// `--trust-member-config` was given at enqueue.
    pub trust_config: bool,
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
    /// `Some("member")` for a train built from its member's config.
    pub config_source: Option<String>,
    /// Hash of that config (`absent` when the member has none).
    pub config_hash: Option<String>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rusqlite::{Connection, TransactionBehavior};

    #[test]
    fn stored_settings_missing_fields_keep_the_real_defaults() {
        let defaults = TrainSettings::default();
        assert_eq!(defaults.push_timeout_ms, 600_000);
        assert!(defaults.audit_ref);
        assert_eq!(defaults.network_timeout_ms, 60_000);
        // A row written before `network_timeout_ms` existed.
        let old: TrainSettings = from_json(Some(
            r#"{"push_timeout_ms":120000,"audit_ref":false}"#.to_owned(),
        ));
        assert_eq!(old.push_timeout_ms, 120_000);
        assert!(!old.audit_ref);
        assert_eq!(old.network_timeout_ms, 60_000);
        // Fields missing altogether, an empty object, an absent column and
        // an unreadable value all give the defaults, never a zero timeout.
        for text in [
            Some(r#"{"audit_ref":false}"#),
            Some("{}"),
            Some("not json"),
            None,
        ] {
            let got: TrainSettings = from_json(text.map(str::to_owned));
            assert_eq!(got.push_timeout_ms, 600_000, "{text:?}");
            assert_eq!(
                got.network_timeout(),
                sluice_gate_core::config::DEFAULT_NETWORK_TIMEOUT
            );
        }
    }

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

    pub(crate) fn with<R>(
        c: &mut Connection,
        f: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<R>,
    ) -> rusqlite::Result<R> {
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let r = f(&tx)?;
        tx.commit()?;
        Ok(r)
    }

    pub(crate) fn new_job(id: &str, branch: &str) -> NewJob {
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
            trust_config: false,
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
                config_source: None,
                config_hash: None,
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

    /// `j1`, `j2` claimed into train `t1` (state `building`, jobs `merging`).
    pub(crate) fn two_member_train(c: &mut Connection, f: &LeaseFence) {
        with(c, init).unwrap();
        with(c, |tx| {
            let w = Writer::new(tx, f, Actor::Runner)?;
            let mut j1 = new_job("j1", "a");
            j1.env = EnvMap::from([("HOME".to_owned(), "/h".to_owned())]);
            w.insert_job(&j1)?;
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
                config_source: None,
                config_hash: None,
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
            Ok(())
        })
        .unwrap();
    }

    fn columns(c: &mut Connection, table: &str) -> Vec<String> {
        with(c, |tx| {
            let mut stmt = tx.prepare(&format!("PRAGMA table_info({table})"))?;
            stmt.query_map([], |r| r.get::<_, String>(1))?.collect()
        })
        .unwrap()
    }

    /// A database of an earlier release has no `prepare` column: `init` adds it
    /// in place, any number of times, without bumping `queue_schema`.
    #[test]
    fn the_prepare_column_is_added_in_place_and_idempotently() {
        let mut c = conn();
        c.execute_batch(SCHEMA).unwrap();
        c.execute("INSERT INTO queue_schema(version) VALUES (1)", [])
            .unwrap();
        assert!(!columns(&mut c, "queue_trains").contains(&"prepare".to_owned()));
        with(&mut c, init).unwrap();
        with(&mut c, init).unwrap();
        assert!(columns(&mut c, "queue_trains").contains(&"prepare".to_owned()));
        let v: i64 = with(&mut c, |tx| {
            tx.query_row("SELECT max(version) FROM queue_schema", [], |r| r.get(0))
        })
        .unwrap();
        assert_eq!(v, QUEUE_SCHEMA_VERSION);
    }

    /// A database of an earlier release has no `queue_protected_targets`
    /// table: `init` creates it in place, any number of times, without
    /// bumping `queue_schema`; the tables an older binary reads are untouched.
    #[test]
    fn the_protected_targets_table_is_added_in_place_and_idempotently() {
        let mut c = conn();
        c.execute_batch(SCHEMA).unwrap();
        c.execute("INSERT INTO queue_schema(version) VALUES (1)", [])
            .unwrap();
        assert_eq!(
            columns(&mut c, "queue_protected_targets"),
            Vec::<String>::new()
        );
        with(&mut c, init).unwrap();
        with(&mut c, init).unwrap();
        assert!(columns(&mut c, "queue_protected_targets").contains(&"evidence".to_owned()));
        let v: i64 = with(&mut c, |tx| {
            tx.query_row("SELECT max(version) FROM queue_schema", [], |r| r.get(0))
        })
        .unwrap();
        assert_eq!(v, QUEUE_SCHEMA_VERSION);
    }

    #[test]
    fn a_refused_push_is_remembered_per_endpoint_and_target_until_a_push_lands() {
        let mut c = conn();
        with(&mut c, init).unwrap();
        let f = fence("tok");
        let known = |c: &mut Connection, hash: &str, target: &str| {
            with(c, |tx| protected_target(tx, hash, target)).unwrap()
        };
        assert_eq!(known(&mut c, "h1", "main"), None);
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.record_protected_target("h1", "main", "GH006 one")
        })
        .unwrap();
        let first = known(&mut c, "h1", "main").expect("remembered");
        assert_eq!(first.evidence, "GH006 one");
        // Another endpoint or target knows nothing about it.
        assert_eq!(known(&mut c, "h2", "main"), None);
        assert_eq!(known(&mut c, "h1", "dev"), None);
        // A second refusal keeps the date and refreshes the evidence.
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.record_protected_target("h1", "main", "GH006 two")
        })
        .unwrap();
        let again = known(&mut c, "h1", "main").expect("remembered");
        assert_eq!(
            (again.first_seen, again.evidence.as_str()),
            (first.first_seen, "GH006 two")
        );
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.clear_protected_target("h1", "main")
        })
        .unwrap();
        assert_eq!(known(&mut c, "h1", "main"), None);
    }

    #[test]
    fn the_trust_columns_are_added_in_place_and_round_trip() {
        let mut c = conn();
        c.execute_batch(SCHEMA).unwrap();
        c.execute("INSERT INTO queue_schema(version) VALUES (1)", [])
            .unwrap();
        // A row written before the columns existed reads as untrusted.
        c.execute(
            "INSERT INTO queue_jobs(id, repo, worktree, branch, head_sha, remote, target, state, \
             queue_order, caller, enqueued_at, updated_at) \
             VALUES ('old', '/r/.git', '/r', 'b', 'h', 'origin', 'main', 'queued', 1, 'human', 1, 1)",
            [],
        )
        .unwrap();
        with(&mut c, init).unwrap();
        with(&mut c, init).unwrap();
        assert!(columns(&mut c, "queue_jobs").contains(&"trust_config".to_owned()));
        for col in ["config_source", "config_hash"] {
            assert!(columns(&mut c, "queue_trains").contains(&col.to_owned()));
        }
        let old = with(&mut c, |tx| job(tx, "old")).unwrap().unwrap();
        assert!(!old.trust_config);
        // A job enqueued with the flag, and a train built from member config.
        let f = fence("tok");
        let mut trusted = new_job("j1", "a");
        trusted.trust_config = true;
        with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            w.insert_job(&trusted)?;
            w.insert_train(&NewTrain {
                id: "t1".into(),
                repo: "/r/.git".into(),
                remote: "origin".into(),
                target: "main".into(),
                endpoint_hash: "e".into(),
                base_sha: "b".into(),
                worktree: "/s/merge/t1".into(),
                members: vec!["j1".into()],
                settings: TrainSettings::default(),
                config_source: Some("member".into()),
                config_hash: Some("abc".into()),
            })
        })
        .unwrap();
        let (j, t) = with(&mut c, |tx| {
            Ok((job(tx, "j1")?.unwrap(), train(tx, "t1")?.unwrap()))
        })
        .unwrap();
        assert!(j.trust_config);
        assert_eq!(t.config_source.as_deref(), Some("member"));
        assert_eq!(t.config_hash.as_deref(), Some("abc"));
    }

    #[test]
    fn the_prepare_view_round_trips_and_stays_when_the_state_changes() {
        let mut c = conn();
        let f = fence("tok");
        two_member_train(&mut c, &f);
        let t = with(&mut c, |tx| train(tx, "t1")).unwrap().unwrap();
        assert_eq!(t.prepare, None);
        let view = TrainPrepare {
            status: "pass".into(),
            job_id: Some("j-9".into()),
            duration_ms: Some(1200),
            log_path: Some("/l/j-9.log".into()),
        };
        with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            w.train_to(
                "t1",
                QueueTrainState::Building,
                QueueTrainState::Verifying,
                &TrainEdit {
                    prepare: Some(Some(view.clone())),
                    ..TrainEdit::default()
                },
            )?;
            // Another edit leaves the column alone.
            w.train_to(
                "t1",
                QueueTrainState::Verifying,
                QueueTrainState::Verifying,
                &TrainEdit::default(),
            )
        })
        .unwrap();
        let t = with(&mut c, |tx| train(tx, "t1")).unwrap().unwrap();
        assert_eq!(t.prepare, Some(view));
    }

    #[test]
    fn prepare_failures_are_counted_with_their_extra_detail() {
        let mut c = conn();
        with(&mut c, init).unwrap();
        let f = fence("tok");
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Cli)?.insert_job(&new_job("j1", "a"))
        })
        .unwrap();
        let note = |c: &mut Connection, job: &str| {
            with(c, |tx| {
                Writer::new(tx, &f, Actor::Runner)?.note_queued_failure_with(
                    "j1",
                    "prepare_failed",
                    "prepare fail (exit code 1)",
                    &serde_json::json!({"job_id": job, "base": true}),
                )
            })
            .unwrap()
        };
        assert_eq!(note(&mut c, "a"), Some(1));
        assert_eq!(note(&mut c, "b"), Some(2));
        let j = with(&mut c, |tx| job(tx, "j1")).unwrap().unwrap();
        assert_eq!(
            j.detail,
            Some(serde_json::json!({
                "error": "prepare fail (exit code 1)",
                "failures": 2,
                "job_id": "b",
                "base": true
            }))
        );
    }

    fn env_column(c: &mut Connection, table: &str, col: &str, id: &str) -> Option<String> {
        with(c, |tx| {
            tx.query_row(
                &format!("SELECT {col} FROM {table} WHERE id = ?1"),
                [id],
                |r| r.get(0),
            )
        })
        .unwrap()
    }

    #[test]
    fn a_conflicting_member_leaves_its_train() {
        let mut c = conn();
        let f = fence("tok");
        two_member_train(&mut c, &f);
        with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.job_to(
                "j2",
                QueueJobState::Merging,
                QueueJobState::Conflict,
                &JobEdit::default(),
            )
        })
        .unwrap();
        let (j2, t, ev) = with(&mut c, |tx| {
            Ok((
                job(tx, "j2")?.unwrap(),
                train(tx, "t1")?.unwrap(),
                events(tx, "j2")?,
            ))
        })
        .unwrap();
        assert_eq!(j2.train_id, None);
        // The train still lists every claimed member, but only j1 is attached.
        assert_eq!(t.members, ["j1", "j2"]);
        let attached = with(&mut c, |tx| train_jobs(tx, &t)).unwrap();
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0].id, "j1");
        // The audit row still names the train the job left.
        let last = ev.last().unwrap();
        assert_eq!(last.1, "conflict");
        let tid: Option<String> = with(&mut c, |tx| {
            tx.query_row(
                "SELECT train_id FROM queue_events WHERE job_id = 'j2' AND to_state = 'conflict'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
        assert_eq!(tid.as_deref(), Some("t1"));
    }

    #[test]
    fn credentials_are_dropped_when_rows_finish() {
        let mut c = conn();
        let f = fence("tok");
        two_member_train(&mut c, &f);
        assert!(env_column(&mut c, "queue_jobs", "env", "j1").is_some());
        // Approve, then land: the train's push_env and the jobs' env go.
        with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            for (from, to) in [
                (QueueJobState::Merging, QueueJobState::Verifying),
                (QueueJobState::Verifying, QueueJobState::Ready),
                (QueueJobState::Ready, QueueJobState::Approved),
                (QueueJobState::Approved, QueueJobState::Pushing),
                (QueueJobState::Pushing, QueueJobState::Landed),
            ] {
                w.job_to("j1", from, to, &JobEdit::default())?;
            }
            for (from, to) in [
                (QueueTrainState::Building, QueueTrainState::Verifying),
                (QueueTrainState::Verifying, QueueTrainState::Ready),
            ] {
                w.train_to("t1", from, to, &TrainEdit::default())?;
            }
            w.train_to(
                "t1",
                QueueTrainState::Ready,
                QueueTrainState::Approved,
                &TrainEdit {
                    approve: Some((
                        ApproveVia::Cli,
                        "h".into(),
                        EnvMap::from([("SSH_AUTH_SOCK".to_owned(), "/s".to_owned())]),
                    )),
                    ..TrainEdit::default()
                },
            )?;
            w.train_to(
                "t1",
                QueueTrainState::Approved,
                QueueTrainState::Pushing,
                &TrainEdit::default(),
            )
        })
        .unwrap();
        assert!(env_column(&mut c, "queue_trains", "push_env", "t1").is_some());
        with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            w.train_to(
                "t1",
                QueueTrainState::Pushing,
                QueueTrainState::Landed,
                &TrainEdit::default(),
            )?;
            w.job_to(
                "j2",
                QueueJobState::Merging,
                QueueJobState::Cancelled,
                &JobEdit::default(),
            )
        })
        .unwrap();
        assert_eq!(env_column(&mut c, "queue_trains", "push_env", "t1"), None);
        assert_eq!(env_column(&mut c, "queue_jobs", "env", "j1"), None);
        assert_eq!(env_column(&mut c, "queue_jobs", "env", "j2"), None);
        let t = with(&mut c, |tx| train(tx, "t1")).unwrap().unwrap();
        assert!(t.landed_at.is_some());
        assert_eq!(t.approved_hash.as_deref(), Some("h"), "history kept");
    }

    #[test]
    fn retention_prunes_only_finished_rows() {
        let mut c = conn();
        let f = fence("tok");
        two_member_train(&mut c, &f);
        // j1 cancelled, train t1 dissolved with j2 back in the queue.
        with(&mut c, |tx| {
            let w = Writer::new(tx, &f, Actor::Runner)?;
            w.job_to(
                "j1",
                QueueJobState::Merging,
                QueueJobState::Cancelled,
                &JobEdit::default(),
            )?;
            let t = train(tx, "t1")?.unwrap();
            w.dissolve(&t, QueueTrainState::Building, "test", false)
        })
        .unwrap();
        // Nothing is older than the cutoff yet.
        let now = sluice_gate_core::dirs::now_ms();
        let p = with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.prune(now.saturating_sub(60_000))
        })
        .unwrap();
        assert_eq!(p, Pruned::default());
        let p = with(&mut c, |tx| {
            Writer::new(tx, &f, Actor::Runner)?.prune(now + 60_000)
        })
        .unwrap();
        assert_eq!(p.jobs, 1, "the cancelled job");
        assert_eq!(p.trains, 1, "the dissolved train");
        assert!(p.events > 0);
        let (j1, j2, t1, live) = with(&mut c, |tx| {
            Ok((
                job(tx, "j1")?,
                job(tx, "j2")?,
                train(tx, "t1")?,
                live_trains(tx, None)?,
            ))
        })
        .unwrap();
        assert!(j1.is_none() && t1.is_none());
        assert_eq!(j2.unwrap().state, QueueJobState::Queued, "live rows stay");
        assert_eq!(live.len(), 0);
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
            pr: PrColumns::default(),
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
