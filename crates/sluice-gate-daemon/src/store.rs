//! SQLite persistence: jobs, lease (fencing), client hit events, totals and
//! the scheduler timeline (job requests, gate statistics).
//!
//! The schema is documented in `kb/sluice/design/executor.md`. The store is
//! synchronous; callers use it from a blocking context. The database file is
//! never deleted. `schema.version` stays 1 (a mismatch is a hard error);
//! additions are versioned by their own tables and applied on open by the
//! [`crate::migrate`] framework (`sched_schema`, ADR-0011 section 7.4),
//! after a `.bak-<version>` copy of the file.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior};
use rusqlite::{params, types::Type};

use crate::{Error, Result};

/// Schema version this build reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

const SCHEMA_SQL: &str = "
CREATE TABLE schema (version INTEGER NOT NULL);
CREATE TABLE lease (
  name TEXT PRIMARY KEY, owner TEXT NOT NULL, token TEXT NOT NULL,
  heartbeat_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE TABLE jobs (
  id TEXT PRIMARY KEY, key TEXT, tree TEXT, repo TEXT NOT NULL, gate TEXT NOT NULL,
  cmd TEXT NOT NULL, weight INTEGER NOT NULL, caller TEXT NOT NULL, session TEXT,
  state TEXT NOT NULL,
  outcome TEXT,
  exit_code INTEGER,
  queued_at INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER,
  log_path TEXT, pgid INTEGER, proc_start_ticks INTEGER,
  daemon_instance TEXT NOT NULL, subscribers_max INTEGER NOT NULL DEFAULT 1,
  coalesced INTEGER NOT NULL DEFAULT 0, marker_written INTEGER NOT NULL DEFAULT 0);
CREATE INDEX jobs_state ON jobs(state);
CREATE INDEX jobs_queued_at ON jobs(queued_at);
CREATE TABLE events (
  at INTEGER NOT NULL, repo TEXT NOT NULL, gate TEXT NOT NULL, saved_ms INTEGER NOT NULL);
";

const JOB_COLUMNS: &str = "id, key, tree, repo, gate, cmd, weight, caller, session, state, \
    outcome, exit_code, queued_at, started_at, finished_at, log_path, pgid, proc_start_ticks, \
    daemon_instance, subscribers_max, coalesced, marker_written";

/// Lifecycle state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for budget.
    Queued,
    /// Executing.
    Running,
    /// Finished (see the outcome).
    Done,
}

impl JobState {
    /// The string stored in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// One row of the `jobs` table. Timestamps are unix milliseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    /// Job id.
    pub id: String,
    /// Coalescing key.
    pub key: Option<String>,
    /// Tree hash.
    pub tree: Option<String>,
    /// Repository root.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Command line.
    pub cmd: String,
    /// Budget weight.
    pub weight: u32,
    /// `agent` or `human`.
    pub caller: String,
    /// Session id of the requester.
    pub session: Option<String>,
    /// Lifecycle state.
    pub state: JobState,
    /// pass, fail, timeout, killed, cached, error or orphaned.
    pub outcome: Option<String>,
    /// Process exit code.
    pub exit_code: Option<i32>,
    /// When the job was queued.
    pub queued_at: u64,
    /// When the job started running.
    pub started_at: Option<u64>,
    /// When the job finished.
    pub finished_at: Option<u64>,
    /// Log file.
    pub log_path: Option<PathBuf>,
    /// Process group id.
    pub pgid: Option<i32>,
    /// Start time of the process, in clock ticks since boot.
    pub proc_start_ticks: Option<u64>,
    /// Daemon instance that owns the job.
    pub daemon_instance: String,
    /// Peak number of subscribers.
    pub subscribers_max: u32,
    /// Requests coalesced into this job.
    pub coalesced: u32,
    /// Whether the success marker was written.
    pub marker_written: bool,
}

/// Handle to the SQLite database.
pub struct Store {
    conn: Connection,
    path: PathBuf,
    /// `sched_schema` version of the file (0: no timeline columns).
    sched: u32,
    /// The file has the `gate_memory` table (`containment_schema` 2).
    memory: bool,
    /// The file has the `gate_memory_bound` table (`containment_schema` 3).
    bounds: bool,
    /// The file has the `outside_growth` table (`containment_schema` 4).
    growth: bool,
}

/// Scheduling facts written when a job is queued (`sched_schema` 1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueuedMeta {
    /// Client send time.
    pub requested_at: Option<u64>,
    /// Configured weight after the clamp.
    pub weight_cfg: u32,
    /// Expected run time from history.
    pub expected_ms: Option<u64>,
}

/// Scheduling and measurement facts written when a job ends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FinishMeta {
    /// Wait accumulators: budget, psi, repo cap, reserved, order (ms).
    pub waits: [u64; 5],
    /// Measured CPU time.
    pub cpu_ms: Option<u64>,
    /// Measured run-queue wait.
    pub run_delay_ms: Option<u64>,
    /// Measured demand, milli-CPUs.
    pub demand_milli: Option<u32>,
    /// `last-subscriber`, `early-verdict`, `stop` or `superseded`.
    pub killed_by: Option<String>,
}

/// One request (subscriber) of a job (`job_requests`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRow {
    /// Job the request joined.
    pub job_id: String,
    /// `agent`, `human` or `unknown`.
    pub caller: String,
    /// Agent session, if any.
    pub session: Option<String>,
    /// Client process id.
    pub client_pid: Option<u32>,
    /// `mcp`, `cli`, `hook`, `exec` or `queue`.
    pub via: String,
    /// Client send time.
    pub requested_at: Option<u64>,
    /// When the request reached the scheduler.
    pub attached_at: u64,
    /// Whether it joined an existing job.
    pub coalesced: bool,
}

/// One measured run of a gate (`gate_stats`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateStat {
    /// Repository identity.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// 16 hex of the command hash.
    pub cmd_hash: String,
    /// Job id.
    pub job_id: String,
    /// End time.
    pub finished_at: u64,
    /// `pass` or `fail`.
    pub outcome: String,
    /// Wall time.
    pub wall_ms: u64,
    /// CPU time.
    pub cpu_ms: u64,
    /// Run-queue wait (`None` without `schedstat`).
    pub run_delay_ms: Option<u64>,
    /// Demand, milli-CPUs.
    pub demand_milli: u32,
}

/// The working set of one finished run (`gate_memory`, `containment_schema` 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateMemory {
    /// Repository identity.
    pub repo: PathBuf,
    /// Gate name (`exec:<label>` for `sluice exec`).
    pub gate: String,
    /// 16 hex of the command hash (empty for `exec`).
    pub cmd_hash: String,
    /// Job id.
    pub job_id: String,
    /// End time.
    pub finished_at: u64,
    /// `pass` or `killed` (a run killed for lack of memory); `fail` or
    /// `timeout` for a lower bound (`gate_memory_bound`, ADR-0022).
    pub outcome: String,
    /// Largest sampled `anon` + swap of the job (resident set without a
    /// cgroup), bytes.
    pub anon_peak_bytes: u64,
    /// `memory.peak` of the job's cgroup (page cache included), bytes; only
    /// shown, never used for admission.
    pub peak_bytes: Option<u64>,
    /// The run was killed for lack of memory.
    pub oom_killed: bool,
}

/// Rows of `outside_growth` kept (the reserve reads the newest 20).
pub const OUTSIDE_GROWTH_KEEP: i64 = 200;

/// How much the memory outside gate jobs grew while one heavy job ran
/// (ADR-0026).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideGrowth {
    /// Job id.
    pub job_id: String,
    /// End time.
    pub finished_at: u64,
    /// The job's own peak working set, bytes.
    pub gate_peak_bytes: u64,
    /// Largest memory outside gates during the run minus that at its first
    /// sample, bytes (0 when it shrank).
    pub growth_bytes: u64,
}

impl GateMemory {
    /// The row is a lower bound (a run that ended `fail` or `timeout`), not
    /// a finished run's measured working set.
    #[must_use]
    pub fn is_lower_bound(&self) -> bool {
        self.outcome == "fail" || self.outcome == "timeout"
    }
}

/// A job with its timeline (`sluice logs --timeline`). The timeline fields
/// are `None` on rows written by releases without them.
#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    /// The base row.
    pub job: JobRow,
    /// Client send time.
    pub requested_at: Option<u64>,
    /// Admission decision time.
    pub admitted_at: Option<u64>,
    /// Configured weight after the clamp.
    pub weight_cfg: Option<u32>,
    /// Expected run time.
    pub expected_ms: Option<u64>,
    /// Wait accumulators: budget, psi, repo cap, reserved, order (ms).
    pub waits: [Option<u64>; 5],
    /// `nominal`, `over` or `guard`.
    pub admission: Option<String>,
    /// Who killed it.
    pub killed_by: Option<String>,
    /// Measured CPU time.
    pub cpu_ms: Option<u64>,
    /// Measured run-queue wait.
    pub run_delay_ms: Option<u64>,
    /// Measured demand, milli-CPUs.
    pub demand_milli: Option<u32>,
    /// Requests that joined the job.
    pub requests: u32,
    /// Requests that left before the end.
    pub detached: u32,
}

/// Rows kept per `gate_stats` key.
const GATE_STATS_KEEP: i64 = 20;
/// Rows kept per `gate_memory` key.
const GATE_MEMORY_KEEP: i64 = 20;

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn db(e: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(f, _) = &e
        && matches!(
            f.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        )
    {
        return Error::Busy;
    }
    Error::Sqlite(e)
}

fn io(context: String, source: std::io::Error) -> Error {
    Error::Io { context, source }
}

fn i(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn u(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn bad_state(idx: usize, s: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        Type::Text,
        format!("unknown job state {s:?}").into(),
    )
}

fn job_from_row(r: &Row<'_>) -> rusqlite::Result<JobRow> {
    let state: String = r.get(9)?;
    let state = JobState::parse(&state).ok_or_else(|| bad_state(9, &state))?;
    Ok(JobRow {
        id: r.get(0)?,
        key: r.get(1)?,
        tree: r.get(2)?,
        repo: PathBuf::from(r.get::<_, String>(3)?),
        gate: r.get(4)?,
        cmd: r.get(5)?,
        weight: r.get(6)?,
        caller: r.get(7)?,
        session: r.get(8)?,
        state,
        outcome: r.get(10)?,
        exit_code: r.get(11)?,
        queued_at: u(r.get(12)?),
        started_at: r.get::<_, Option<i64>>(13)?.map(u),
        finished_at: r.get::<_, Option<i64>>(14)?.map(u),
        log_path: r.get::<_, Option<String>>(15)?.map(PathBuf::from),
        pgid: r.get(16)?,
        proc_start_ticks: r.get::<_, Option<i64>>(17)?.map(u),
        daemon_instance: r.get(18)?,
        subscribers_max: r.get(19)?,
        coalesced: r.get(20)?,
        marker_written: r.get::<_, i64>(21)? != 0,
    })
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn rfc3339(ms: u64) -> String {
    let t = UNIX_EPOCH + Duration::from_millis(ms);
    humantime::format_rfc3339_seconds(t).to_string()
}

impl Store {
    /// Opens (creating if needed) the database in WAL mode.
    ///
    /// An empty database gets the schema; an existing one with another
    /// `schema.version` fails with [`Error::Schema`]. Pending additive
    /// migrations ([`crate::migrate::SCHED_MIGRATIONS`]) are then applied,
    /// after a `.bak-<version>` copy of an existing file; a newer
    /// `sched_schema` fails with [`Error::NewerSchema`]. Nothing is deleted.
    ///
    /// # Errors
    /// I/O, SQLite, busy or schema mismatch errors.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, crate::migrate::SCHED_MIGRATIONS)
    }

    /// [`Store::open`] with an explicit migration list (tests inject a
    /// failing migration to prove the rollback).
    pub(crate) fn open_with(path: &Path, migrations: &[crate::migrate::Migration]) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            sluice_gate_core::dirs::create_private_dir(parent)
                .map_err(|e| io(format!("creating {}", parent.display()), e))?;
        }
        let conn = Connection::open(path).map_err(db)?;
        conn.busy_timeout(Duration::from_millis(2000)).map_err(db)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))
            .map_err(db)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db)?;
        conn.pragma_update(None, "foreign_keys", false)
            .map_err(db)?;
        let mut store = Self {
            conn,
            path: path.to_path_buf(),
            sched: 0,
            memory: false,
            bounds: false,
            growth: false,
        };
        let fresh = store.init_schema()?;
        store.migrate(fresh, migrations)?;
        // Additive only (one new table): no backup, and released binaries
        // ignore the table and its version row.
        crate::migrate::apply(
            &store.conn,
            crate::migrate::CONTAINMENT_TABLE,
            crate::migrate::CONTAINMENT_MIGRATIONS,
            crate::migrate::CONTAINMENT_SCHEMA_VERSION,
        )
        .map_err(db)?;
        store.memory = true;
        store.bounds = true;
        store.growth = true;
        crate::meta::record_writer(&store.conn).map_err(db)?;
        Ok(store)
    }

    /// Applies the pending `sched_schema` migrations (backup first unless
    /// the database was just created).
    fn migrate(&mut self, fresh: bool, migrations: &[crate::migrate::Migration]) -> Result<()> {
        use crate::migrate::{self, SCHED_TABLE};
        let target = migrations.iter().map(|m| m.version).max().unwrap_or(0);
        let found = migrate::version(&self.conn, SCHED_TABLE).map_err(db)?;
        if found > target {
            return Err(Error::NewerSchema {
                table: SCHED_TABLE,
                writer: crate::meta::describe_writer(&self.conn),
                path: self.path.clone(),
                found,
                expected: target,
            });
        }
        if found < target {
            if !fresh {
                let bak = migrate::backup(&self.conn, &self.path, found).map_err(db)?;
                tracing::info!(backup = %bak.display(), "database copied before migrating");
            }
            self.sched = migrate::apply(&self.conn, SCHED_TABLE, migrations, target).map_err(db)?;
        } else {
            self.sched = found;
        }
        Ok(())
    }

    /// `sched_schema` version of the open file (0: written by a release
    /// without the scheduler timeline and opened read-only).
    #[must_use]
    pub fn sched_version(&self) -> u32 {
        self.sched
    }

    /// Opens the database read-only (for client diagnostics) and checks the schema version.
    ///
    /// # Errors
    /// SQLite, busy or schema mismatch errors.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(db)?;
        conn.busy_timeout(Duration::from_millis(2000)).map_err(db)?;
        let mut store = Self {
            conn,
            path: path.to_path_buf(),
            sched: 0,
            memory: false,
            bounds: false,
            growth: false,
        };
        store.check_version()?;
        let containment =
            crate::migrate::version(&store.conn, crate::migrate::CONTAINMENT_TABLE).map_err(db)?;
        store.memory = containment >= 2;
        store.bounds = containment >= 3;
        store.growth = containment >= 4;
        // Only the columns this build knows are ever selected, so a newer
        // (additive) `sched_schema` reads fine.
        store.sched = crate::migrate::version(&store.conn, crate::migrate::SCHED_TABLE)
            .map_err(db)?
            .min(crate::migrate::SCHED_SCHEMA_VERSION);
        Ok(store)
    }

    /// Database file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The underlying connection (for [`crate::server::DaemonHandle::store_tx`]).
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    fn tx(&self) -> Result<Transaction<'_>> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate).map_err(db)
    }

    fn table_count(conn: &Connection) -> Result<i64> {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
            [],
            |r| r.get(0),
        )
        .map_err(db)
    }

    /// Creates the base schema in an empty database (returns `true`), else
    /// checks `schema.version`.
    fn init_schema(&self) -> Result<bool> {
        let tx = self.tx()?;
        if Self::table_count(&tx)? == 0 {
            tx.execute_batch(SCHEMA_SQL).map_err(db)?;
            tx.execute("INSERT INTO schema (version) VALUES (?1)", [SCHEMA_VERSION])
                .map_err(db)?;
            tx.commit().map_err(db)?;
            return Ok(true);
        }
        tx.commit().map_err(db)?;
        self.check_version()?;
        Ok(false)
    }

    fn check_version(&self) -> Result<()> {
        let has_schema: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema'",
                [],
                |r| r.get(0),
            )
            .map_err(db)?;
        let found = if has_schema == 0 {
            0
        } else {
            self.conn
                .query_row("SELECT version FROM schema LIMIT 1", [], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()
                .map_err(db)?
                .map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX))
        };
        if found == SCHEMA_VERSION {
            Ok(())
        } else {
            Err(Error::Schema {
                writer: crate::meta::describe_writer(&self.conn),
                path: self.path.clone(),
                found,
                expected: SCHEMA_VERSION,
            })
        }
    }

    // ---- Lease ----

    /// Takes the lease when it is free, expired or already owned by `owner`.
    /// Returns `false` when another owner holds an unexpired lease.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn acquire_lease(
        &self,
        name: &str,
        owner: &str,
        token: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<bool> {
        let tx = self.tx()?;
        let current: Option<(String, i64)> = tx
            .query_row(
                "SELECT owner, expires_at FROM lease WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db)?;
        let free = match current {
            None => true,
            Some((cur_owner, expires)) => cur_owner == owner || u(expires) <= now_ms,
        };
        if free {
            tx.execute(
                "INSERT OR REPLACE INTO lease (name, owner, token, heartbeat_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    name,
                    owner,
                    token,
                    i(now_ms),
                    i(now_ms.saturating_add(ttl_ms))
                ],
            )
            .map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(free)
    }

    /// Extends the lease when `token` still matches; `false` means the lease was lost.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn heartbeat(&self, name: &str, token: &str, now_ms: u64, ttl_ms: u64) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE lease SET heartbeat_at = ?3, expires_at = ?4 WHERE name = ?1 AND token = ?2",
                params![name, token, i(now_ms), i(now_ms.saturating_add(ttl_ms))],
            )
            .map_err(db)?;
        Ok(n > 0)
    }

    /// Deletes the lease when `token` matches.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn release_lease(&self, name: &str, token: &str) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM lease WHERE name = ?1 AND token = ?2",
                params![name, token],
            )
            .map_err(db)?;
        Ok(n > 0)
    }

    /// Reads a lease as `(owner, token, heartbeat_at, expires_at)`.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn lease(&self, name: &str) -> Result<Option<(String, String, u64, u64)>> {
        self.conn
            .query_row(
                "SELECT owner, token, heartbeat_at, expires_at FROM lease WHERE name = ?1",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?, u(r.get(2)?), u(r.get(3)?))),
            )
            .optional()
            .map_err(db)
    }

    // ---- Jobs ----

    /// Inserts a job row.
    ///
    /// # Errors
    /// SQLite or busy errors (including duplicate ids).
    pub fn insert_job(&self, row: &JobRow) -> Result<()> {
        self.conn
            .execute(
                &format!(
                    "INSERT INTO jobs ({JOB_COLUMNS}) VALUES \
                     (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                      ?17, ?18, ?19, ?20, ?21, ?22)"
                ),
                params![
                    row.id,
                    row.key,
                    row.tree,
                    path_str(&row.repo),
                    row.gate,
                    row.cmd,
                    row.weight,
                    row.caller,
                    row.session,
                    row.state.as_str(),
                    row.outcome,
                    row.exit_code,
                    i(row.queued_at),
                    row.started_at.map(i),
                    row.finished_at.map(i),
                    row.log_path.as_deref().map(path_str),
                    row.pgid,
                    row.proc_start_ticks.map(i),
                    row.daemon_instance,
                    row.subscribers_max,
                    row.coalesced,
                    i64::from(row.marker_written),
                ],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Marks a queued job as running, fenced by instance. `false` when nothing changed.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn mark_started(
        &self,
        id: &str,
        instance: &str,
        started_at: u64,
        pgid: Option<i32>,
        proc_start_ticks: Option<u64>,
        log_path: &Path,
    ) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE jobs SET state = 'running', started_at = ?3, pgid = ?4, \
                 proc_start_ticks = ?5, log_path = ?6 \
                 WHERE id = ?1 AND daemon_instance = ?2 AND state = 'queued'",
                params![
                    id,
                    instance,
                    i(started_at),
                    pgid,
                    proc_start_ticks.map(i),
                    path_str(log_path)
                ],
            )
            .map_err(db)?;
        Ok(n > 0)
    }

    /// Marks a queued/running job as done, fenced by instance. `false` when nothing changed.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn mark_done(
        &self,
        id: &str,
        instance: &str,
        finished_at: u64,
        outcome: &str,
        exit_code: Option<i32>,
        marker_written: bool,
    ) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE jobs SET state = 'done', outcome = ?3, exit_code = ?4, finished_at = ?5, \
                 marker_written = ?6 \
                 WHERE id = ?1 AND daemon_instance = ?2 AND state IN ('queued', 'running')",
                params![
                    id,
                    instance,
                    outcome,
                    exit_code,
                    i(finished_at),
                    i64::from(marker_written)
                ],
            )
            .map_err(db)?;
        Ok(n > 0)
    }

    /// Updates the subscriber statistics of a job.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn set_subscribers(&self, id: &str, subscribers_max: u32, coalesced: u32) -> Result<()> {
        self.conn
            .execute(
                "UPDATE jobs SET subscribers_max = ?2, coalesced = ?3 WHERE id = ?1",
                params![id, subscribers_max, coalesced],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Reads one job.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn job(&self, id: &str) -> Result<Option<JobRow>> {
        self.conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [id],
                job_from_row,
            )
            .optional()
            .map_err(db)
    }

    /// The most recently queued job (ties broken by id, descending).
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn newest_job(&self) -> Result<Option<JobRow>> {
        self.conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY queued_at DESC, id DESC LIMIT 1"),
                [],
                job_from_row,
            )
            .optional()
            .map_err(db)
    }

    fn query_jobs(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<JobRow>> {
        let mut stmt = self.conn.prepare(sql).map_err(db)?;
        let rows = stmt.query_map(args, job_from_row).map_err(db)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db)
    }

    /// Jobs in a state, oldest queued first.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn jobs_in_state(&self, state: JobState) -> Result<Vec<JobRow>> {
        self.query_jobs(
            &format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE state = ?1 ORDER BY queued_at ASC, id ASC"
            ),
            &[&state.as_str()],
        )
    }

    /// Running jobs that belong to another daemon instance.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn orphans(&self, instance: &str) -> Result<Vec<JobRow>> {
        self.query_jobs(
            &format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'running' AND daemon_instance <> ?1 \
                 ORDER BY queued_at ASC, id ASC"
            ),
            &[&instance],
        )
    }

    /// Closes every job left open by other instances: running ones become
    /// `killed` (their process group was terminated by the reaper or by the
    /// start-up reconciliation), queued ones `orphaned` (they never ran).
    /// Returns `(killed, orphaned)`.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn close_orphans(&self, instance: &str, finished_at: u64) -> Result<(usize, usize)> {
        let tx = self.tx()?;
        let killed = tx
            .execute(
                "UPDATE jobs SET state = 'done', outcome = 'killed', finished_at = ?2 \
                 WHERE daemon_instance <> ?1 AND state = 'running'",
                params![instance, i(finished_at)],
            )
            .map_err(db)?;
        let orphaned = tx
            .execute(
                "UPDATE jobs SET state = 'done', outcome = 'orphaned', finished_at = ?2 \
                 WHERE daemon_instance <> ?1 AND state = 'queued'",
                params![instance, i(finished_at)],
            )
            .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok((killed, orphaned))
    }

    // ---- Interventions (containment_schema 1) ----

    /// Stores an intervention; a row with the same time, leaf and cause is
    /// kept once. Returns whether a new row was written.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn record_intervention(&self, row: &crate::interventions::InterventionRow) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO interventions (at_ms, session, leaf, cause, detail) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![i(row.at_ms), row.session, row.leaf, row.cause, row.detail],
            )
            .map_err(db)?;
        Ok(n > 0)
    }

    /// The most recent interventions at or after `since_ms`, newest first.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn recent_interventions(
        &self,
        since_ms: u64,
        limit: usize,
    ) -> Result<Vec<crate::interventions::InterventionRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT at_ms, session, leaf, cause, detail FROM interventions \
                 WHERE at_ms >= ?1 ORDER BY at_ms DESC, id DESC LIMIT ?2",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map(params![i(since_ms), i(limit as u64)], |r| {
                Ok(crate::interventions::InterventionRow {
                    at_ms: u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                    session: r.get(1)?,
                    leaf: r.get(2)?,
                    cause: r.get(3)?,
                    detail: r.get(4)?,
                })
            })
            .map_err(db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db)?;
        Ok(rows)
    }

    // ---- Timeline (sched_schema 1) ----
    //
    // Every write below is a no-op on a file without the timeline columns
    // (never the case for a read-write store, which migrates on open).

    /// Records the queue-time facts of a job.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn set_queued_meta(&self, id: &str, meta: QueuedMeta) -> Result<()> {
        if self.sched < 1 {
            return Ok(());
        }
        self.conn
            .execute(
                "UPDATE jobs SET requested_at = ?2, weight_cfg = ?3, expected_ms = ?4 WHERE id = ?1",
                params![
                    id,
                    meta.requested_at.map(i),
                    meta.weight_cfg,
                    meta.expected_ms.map(i)
                ],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Records the effective weight a job was admitted with (`jobs.weight`
    /// holds the enqueue-time weight until then).
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn set_weight(&self, id: &str, weight: u32) -> Result<()> {
        self.conn
            .execute(
                "UPDATE jobs SET weight = ?2 WHERE id = ?1",
                params![id, weight],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Records the admission decision of a job.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn set_admitted(&self, id: &str, admitted_at: u64, admission: &str) -> Result<()> {
        if self.sched < 1 {
            return Ok(());
        }
        self.conn
            .execute(
                "UPDATE jobs SET admitted_at = ?2, admission = ?3 WHERE id = ?1",
                params![id, i(admitted_at), admission],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Records the end-of-job facts (waits, measured demand, killer).
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn set_finish_meta(&self, id: &str, meta: &FinishMeta) -> Result<()> {
        if self.sched < 1 {
            return Ok(());
        }
        let w = meta.waits;
        self.conn
            .execute(
                "UPDATE jobs SET wait_budget_ms = ?2, wait_psi_ms = ?3, wait_repo_cap_ms = ?4, \
                 wait_reserved_ms = ?5, wait_order_ms = ?6, cpu_ms = ?7, run_delay_ms = ?8, \
                 demand_milli = ?9, killed_by = ?10 WHERE id = ?1",
                params![
                    id,
                    i(w[0]),
                    i(w[1]),
                    i(w[2]),
                    i(w[3]),
                    i(w[4]),
                    meta.cpu_ms.map(i),
                    meta.run_delay_ms.map(i),
                    meta.demand_milli,
                    meta.killed_by,
                ],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Inserts a request row; returns its id (`None` without the table).
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn insert_request(&self, row: &RequestRow) -> Result<Option<i64>> {
        if self.sched < 1 {
            return Ok(None);
        }
        self.conn
            .execute(
                "INSERT INTO job_requests (job_id, caller, session, client_pid, via, \
                 requested_at, attached_at, coalesced) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    row.job_id,
                    row.caller,
                    row.session,
                    row.client_pid,
                    row.via,
                    row.requested_at.map(i),
                    i(row.attached_at),
                    i64::from(row.coalesced),
                ],
            )
            .map_err(db)?;
        Ok(Some(self.conn.last_insert_rowid()))
    }

    /// Closes a request row: when its subscriber got `done` or left.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn close_request(&self, id: i64, finished_at: u64, detached: bool) -> Result<()> {
        if self.sched < 1 {
            return Ok(());
        }
        self.conn
            .execute(
                "UPDATE job_requests SET finished_at = ?2, detached = ?3 \
                 WHERE id = ?1 AND finished_at IS NULL",
                params![id, i(finished_at), i64::from(detached)],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Inserts one measured run and keeps the newest 20 of its key.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn insert_gate_stat(&self, s: &GateStat) -> Result<()> {
        if self.sched < 1 {
            return Ok(());
        }
        let tx = self.tx()?;
        let repo = path_str(&s.repo);
        tx.execute(
            "INSERT INTO gate_stats (repo, gate, cmd_hash, job_id, finished_at, outcome, \
             wall_ms, cpu_ms, run_delay_ms, demand_milli) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                repo,
                s.gate,
                s.cmd_hash,
                s.job_id,
                i(s.finished_at),
                s.outcome,
                i(s.wall_ms),
                i(s.cpu_ms),
                s.run_delay_ms.map(i),
                s.demand_milli,
            ],
        )
        .map_err(db)?;
        tx.execute(
            "DELETE FROM gate_stats WHERE repo = ?1 AND gate = ?2 AND cmd_hash = ?3 \
             AND rowid NOT IN (SELECT rowid FROM gate_stats WHERE repo = ?1 AND gate = ?2 \
             AND cmd_hash = ?3 ORDER BY finished_at DESC, rowid DESC LIMIT ?4)",
            params![repo, s.gate, s.cmd_hash, GATE_STATS_KEEP],
        )
        .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(())
    }

    /// The newest `per_key` passing runs of every key, oldest first within
    /// a key (rows of failed runs, written by earlier releases, are not
    /// learning samples).
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn gate_stats_recent(&self, per_key: usize) -> Result<Vec<GateStat>> {
        if self.sched < 1 {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT repo, gate, cmd_hash, job_id, finished_at, outcome, wall_ms, cpu_ms, \
                 run_delay_ms, demand_milli FROM gate_stats WHERE outcome = 'pass' \
                 ORDER BY repo, gate, cmd_hash, finished_at DESC, rowid DESC",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(GateStat {
                    repo: PathBuf::from(r.get::<_, String>(0)?),
                    gate: r.get(1)?,
                    cmd_hash: r.get(2)?,
                    job_id: r.get(3)?,
                    finished_at: u(r.get(4)?),
                    outcome: r.get(5)?,
                    wall_ms: u(r.get(6)?),
                    cpu_ms: u(r.get(7)?),
                    run_delay_ms: r.get::<_, Option<i64>>(8)?.map(u),
                    demand_milli: r.get(9)?,
                })
            })
            .map_err(db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db)?;
        let mut out: Vec<GateStat> = Vec::with_capacity(rows.len());
        let mut group: Vec<GateStat> = Vec::new();
        for row in rows {
            let same = group.last().is_some_and(|g| {
                g.repo == row.repo && g.gate == row.gate && g.cmd_hash == row.cmd_hash
            });
            if !same {
                out.extend(group.drain(..).rev());
            }
            if group.len() < per_key {
                group.push(row);
            }
        }
        out.extend(group.drain(..).rev());
        Ok(out)
    }

    /// The unbounded learned weight of one gate in one repository
    /// (ADR-0015, for `sluice doctor`): `round_half_up(P75(demand))` over the
    /// newest [`crate::scheduler::LEARN_WINDOW`] passing runs of the gate
    /// (all its command hashes together), without the clamp to the declared
    /// ceiling. `None` before [`crate::scheduler::LEARN_AFTER`] samples.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn unbounded_learned(&self, repo: &Path, gate: &str) -> Result<Option<u32>> {
        if self.sched < 1 {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT demand_milli FROM gate_stats WHERE repo = ?1 AND gate = ?2 \
                 AND outcome = 'pass' ORDER BY finished_at DESC, rowid DESC LIMIT ?3",
            )
            .map_err(db)?;
        let limit = i64::try_from(crate::scheduler::LEARN_WINDOW).unwrap_or(i64::MAX);
        let mut demands = stmt
            .query_map(params![path_str(repo), gate, limit], |r| r.get::<_, u32>(0))
            .map_err(db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db)?;
        demands.reverse();
        Ok(crate::scheduler::learned_weight(&demands))
    }

    /// Deletes the statistics of keys with no run since `unseen_since_ms`;
    /// returns the rows deleted.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn prune_gate_stats(&self, unseen_since_ms: u64) -> Result<usize> {
        if self.sched < 1 {
            return Ok(0);
        }
        self.conn
            .execute(
                "DELETE FROM gate_stats WHERE (repo, gate, cmd_hash) IN \
                 (SELECT repo, gate, cmd_hash FROM gate_stats GROUP BY repo, gate, cmd_hash \
                  HAVING max(finished_at) < ?1)",
                [i(unseen_since_ms)],
            )
            .map_err(db)
    }

    /// Inserts the working set of one finished run and keeps the newest 20
    /// of its key. A file without `gate_memory` (opened read-only before the
    /// migration) ignores it.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn insert_gate_memory(&self, m: &GateMemory) -> Result<()> {
        if !self.memory {
            return Ok(());
        }
        self.insert_memory_row("gate_memory", m)
    }

    /// Inserts the peak of a run that ended `fail` or `timeout`, a lower
    /// bound of what its gate needs, and keeps the newest 20 of its key. A
    /// file without `gate_memory_bound` ignores it.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn insert_gate_bound(&self, m: &GateMemory) -> Result<()> {
        if !self.bounds {
            return Ok(());
        }
        self.insert_memory_row("gate_memory_bound", m)
    }

    /// `table` is one of the two constant names above, never user input.
    fn insert_memory_row(&self, table: &str, m: &GateMemory) -> Result<()> {
        let tx = self.tx()?;
        let repo = path_str(&m.repo);
        tx.execute(
            &format!(
                "INSERT INTO {table} (repo, gate, cmd_hash, job_id, finished_at, outcome, \
             anon_peak_bytes, peak_bytes, oom_killed) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
            ),
            params![
                repo,
                m.gate,
                m.cmd_hash,
                m.job_id,
                i(m.finished_at),
                m.outcome,
                i(m.anon_peak_bytes),
                m.peak_bytes.map(i),
                i64::from(m.oom_killed),
            ],
        )
        .map_err(db)?;
        tx.execute(
            &format!(
                "DELETE FROM {table} WHERE repo = ?1 AND gate = ?2 AND cmd_hash = ?3 \
                 AND rowid NOT IN (SELECT rowid FROM {table} WHERE repo = ?1 AND gate = ?2 \
                 AND cmd_hash = ?3 ORDER BY finished_at DESC, rowid DESC LIMIT ?4)"
            ),
            params![repo, m.gate, m.cmd_hash, GATE_MEMORY_KEEP],
        )
        .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(())
    }

    /// Records how much the memory outside gate jobs grew while a heavy job
    /// ran (ADR-0026) and keeps the newest [`OUTSIDE_GROWTH_KEEP`] rows. A
    /// file without `outside_growth` ignores it.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn insert_outside_growth(&self, g: &OutsideGrowth) -> Result<()> {
        if !self.growth {
            return Ok(());
        }
        let tx = self.tx()?;
        tx.execute(
            "INSERT INTO outside_growth (job_id, finished_at, gate_peak_bytes, growth_bytes)              VALUES (?1, ?2, ?3, ?4)",
            params![
                g.job_id,
                i(g.finished_at),
                i(g.gate_peak_bytes),
                i(g.growth_bytes)
            ],
        )
        .map_err(db)?;
        tx.execute(
            "DELETE FROM outside_growth WHERE rowid NOT IN (SELECT rowid FROM outside_growth              ORDER BY finished_at DESC, rowid DESC LIMIT ?1)",
            [OUTSIDE_GROWTH_KEEP],
        )
        .map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(())
    }

    /// The growth of the newest `limit` rows, newest first.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn outside_growth_recent(&self, limit: usize) -> Result<Vec<u64>> {
        if !self.growth {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT growth_bytes FROM outside_growth                  ORDER BY finished_at DESC, rowid DESC LIMIT ?1",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(db)?;
        rows.map(|r| r.map(|v| u64::try_from(v).unwrap_or(0)).map_err(db))
            .collect()
    }

    /// The newest `per_key` working sets of every key, oldest first within
    /// a key.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn gate_memory_recent(&self, per_key: usize) -> Result<Vec<GateMemory>> {
        if !self.memory {
            return Ok(Vec::new());
        }
        self.memory_rows_recent("gate_memory", per_key)
    }

    /// The newest `per_key` lower bounds (runs that ended `fail` or
    /// `timeout`) of every key, oldest first within a key.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn gate_bounds_recent(&self, per_key: usize) -> Result<Vec<GateMemory>> {
        if !self.bounds {
            return Ok(Vec::new());
        }
        self.memory_rows_recent("gate_memory_bound", per_key)
    }

    /// `table` is one of the two constant names above, never user input.
    fn memory_rows_recent(&self, table: &str, per_key: usize) -> Result<Vec<GateMemory>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT repo, gate, cmd_hash, job_id, finished_at, outcome, anon_peak_bytes, \
                 peak_bytes, oom_killed FROM {table} \
                 ORDER BY repo, gate, cmd_hash, finished_at DESC, rowid DESC"
            ))
            .map_err(db)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(GateMemory {
                    repo: PathBuf::from(r.get::<_, String>(0)?),
                    gate: r.get(1)?,
                    cmd_hash: r.get(2)?,
                    job_id: r.get(3)?,
                    finished_at: u(r.get(4)?),
                    outcome: r.get(5)?,
                    anon_peak_bytes: u(r.get(6)?),
                    peak_bytes: r.get::<_, Option<i64>>(7)?.map(u),
                    oom_killed: r.get::<_, i64>(8)? != 0,
                })
            })
            .map_err(db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db)?;
        let mut out: Vec<GateMemory> = Vec::with_capacity(rows.len());
        let mut group: Vec<GateMemory> = Vec::new();
        for row in rows {
            let same = group.last().is_some_and(|g| {
                g.repo == row.repo && g.gate == row.gate && g.cmd_hash == row.cmd_hash
            });
            if !same {
                out.extend(group.drain(..).rev());
            }
            if group.len() < per_key {
                group.push(row);
            }
        }
        out.extend(group.drain(..).rev());
        Ok(out)
    }

    /// The weight each `gate_memory` key last ran with: `jobs.weight` (the
    /// effective weight, see [`Store::set_weight`]) of the newest run whose
    /// job row still exists. Keys whose jobs were pruned are absent.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn gate_memory_weights(&self) -> Result<Vec<(PathBuf, String, String, u32)>> {
        if !self.memory {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.repo, m.gate, m.cmd_hash, j.weight FROM gate_memory m \
                 JOIN jobs j ON j.id = m.job_id \
                 ORDER BY m.finished_at, m.rowid",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    PathBuf::from(r.get::<_, String>(0)?),
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, u32>(3)?,
                ))
            })
            .map_err(db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db)?;
        // Oldest first: a later row of the same key wins.
        let mut newest: std::collections::BTreeMap<(PathBuf, String, String), u32> =
            std::collections::BTreeMap::new();
        for (repo, gate, cmd_hash, weight) in rows {
            newest.insert((repo, gate, cmd_hash), weight);
        }
        Ok(newest
            .into_iter()
            .map(|((repo, gate, cmd_hash), w)| (repo, gate, cmd_hash, w))
            .collect())
    }

    /// Deletes the working sets of keys with no run since `unseen_since_ms`
    /// (the same rule as [`Store::prune_gate_stats`]); returns the rows deleted.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn prune_gate_memory(&self, unseen_since_ms: u64) -> Result<usize> {
        let mut deleted = 0;
        for (present, table) in [
            (self.memory, "gate_memory"),
            (self.bounds, "gate_memory_bound"),
        ] {
            if present {
                deleted += self
                    .conn
                    .execute(
                        &format!(
                            "DELETE FROM {table} WHERE (repo, gate, cmd_hash) IN \
                             (SELECT repo, gate, cmd_hash FROM {table} \
                              GROUP BY repo, gate, cmd_hash HAVING max(finished_at) < ?1)"
                        ),
                        [i(unseen_since_ms)],
                    )
                    .map_err(db)?;
            }
        }
        Ok(deleted)
    }

    fn timeline_of(&self, job: JobRow) -> Result<Timeline> {
        let mut t = Timeline {
            job,
            requested_at: None,
            admitted_at: None,
            weight_cfg: None,
            expected_ms: None,
            waits: [None; 5],
            admission: None,
            killed_by: None,
            cpu_ms: None,
            run_delay_ms: None,
            demand_milli: None,
            requests: 0,
            detached: 0,
        };
        if self.sched < 1 {
            return Ok(t);
        }
        let opt = |r: &Row<'_>, n: usize| r.get::<_, Option<i64>>(n).map(|v| v.map(u));
        self.conn
            .query_row(
                "SELECT requested_at, admitted_at, weight_cfg, expected_ms, wait_budget_ms, \
                 wait_psi_ms, wait_repo_cap_ms, wait_reserved_ms, wait_order_ms, admission, \
                 killed_by, cpu_ms, run_delay_ms, demand_milli, \
                 (SELECT count(*) FROM job_requests q WHERE q.job_id = jobs.id), \
                 (SELECT count(*) FROM job_requests q WHERE q.job_id = jobs.id AND q.detached) \
                 FROM jobs WHERE id = ?1",
                [&t.job.id],
                |r| {
                    t.requested_at = opt(r, 0)?;
                    t.admitted_at = opt(r, 1)?;
                    t.weight_cfg = r.get(2)?;
                    t.expected_ms = opt(r, 3)?;
                    for k in 0..5 {
                        t.waits[k] = opt(r, 4 + k)?;
                    }
                    t.admission = r.get(9)?;
                    t.killed_by = r.get(10)?;
                    t.cpu_ms = opt(r, 11)?;
                    t.run_delay_ms = opt(r, 12)?;
                    t.demand_milli = r.get(13)?;
                    t.requests = r.get(14)?;
                    t.detached = r.get(15)?;
                    Ok(())
                },
            )
            .map_err(db)?;
        Ok(t)
    }

    /// One job with its timeline.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn timeline(&self, id: &str) -> Result<Option<Timeline>> {
        match self.job(id)? {
            Some(job) => self.timeline_of(job).map(Some),
            None => Ok(None),
        }
    }

    /// Jobs queued since `since_ms`, oldest first, with their timelines.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn timelines_since(&self, since_ms: u64) -> Result<Vec<Timeline>> {
        let jobs = self.query_jobs(
            &format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE queued_at >= ?1 ORDER BY queued_at ASC, id ASC"
            ),
            &[&i(since_ms)],
        )?;
        jobs.into_iter().map(|j| self.timeline_of(j)).collect()
    }

    // ---- Events and totals ----

    /// Records a client-side cache hit.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn record_hit(&self, at: u64, repo: &Path, gate: &str, saved_ms: u64) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO events (at, repo, gate, saved_ms) VALUES (?1, ?2, ?3, ?4)",
                params![i(at), path_str(repo), gate, i(saved_ms)],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Cumulative counters since `since_ms` (`None` = all time). Jobs are
    /// selected by `queued_at`, events by `at`.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn totals(&self, since_ms: Option<u64>) -> Result<sluice_gate_proto::Totals> {
        let since = since_ms.map(i);
        let tx = self.tx()?;
        let (runs, passes, failures, timeouts, coalesced, wait, run, saved_jobs, first_job): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            Option<i64>,
        ) = tx
            .query_row(
                "SELECT \
                   COALESCE(SUM(state = 'done' AND outcome IN \
                     ('pass','fail','timeout','killed','cached','error')), 0), \
                   COALESCE(SUM(state = 'done' AND outcome = 'pass'), 0), \
                   COALESCE(SUM(state = 'done' AND outcome = 'fail'), 0), \
                   COALESCE(SUM(state = 'done' AND outcome = 'timeout'), 0), \
                   COALESCE(SUM(coalesced), 0), \
                   COALESCE(SUM(CASE WHEN started_at IS NOT NULL \
                     THEN started_at - queued_at END), 0), \
                   COALESCE(SUM(CASE WHEN state = 'done' AND started_at IS NOT NULL \
                     AND finished_at IS NOT NULL THEN finished_at - started_at END), 0), \
                   COALESCE(SUM(CASE WHEN state = 'done' AND outcome = 'pass' \
                     AND started_at IS NOT NULL AND finished_at IS NOT NULL \
                     THEN coalesced * (finished_at - started_at) END), 0), \
                   MIN(queued_at) \
                 FROM jobs WHERE ?1 IS NULL OR queued_at >= ?1",
                [since],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .map_err(db)?;
        let (hits, saved_events, first_event): (i64, i64, Option<i64>) = tx
            .query_row(
                "SELECT count(*), COALESCE(SUM(saved_ms), 0), MIN(at) \
                 FROM events WHERE ?1 IS NULL OR at >= ?1",
                [since],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(db)?;
        tx.commit().map_err(db)?;

        let since_label = match since_ms {
            Some(ms) => rfc3339(ms),
            None => match (first_job, first_event) {
                (None, None) => String::new(),
                (a, b) => rfc3339(u([a, b].into_iter().flatten().min().unwrap_or(0))),
            },
        };
        Ok(sluice_gate_proto::Totals {
            since: since_label,
            runs: u(runs),
            passes: u(passes),
            failures: u(failures),
            timeouts: u(timeouts),
            hits: u(hits),
            coalesced: u(coalesced),
            wait_ms_total: u(wait),
            run_ms_total: u(run),
            time_saved_ms: u(saved_events).saturating_add(u(saved_jobs)),
            // Not persisted: the server fills it from its in-memory PSI state.
            psi_blocked_ms_total: 0,
            // Per daemon instance, filled by the server from its sampler.
            ..sluice_gate_proto::Totals::default()
        })
    }

    /// Deletes `<logs_dir>/<id>.log` of done jobs finished before `older_than_ms`.
    /// Missing files are ignored; returns how many files were deleted.
    ///
    /// # Errors
    /// SQLite, busy or I/O errors.
    pub fn prune_logs(&self, logs_dir: &Path, older_than_ms: u64) -> Result<usize> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM jobs WHERE state = 'done' AND finished_at < ?1")
            .map_err(db)?;
        let ids = stmt
            .query_map([i(older_than_ms)], |r| r.get::<_, String>(0))
            .map_err(db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db)?;
        let mut deleted = 0;
        for id in ids {
            if id.is_empty() || id.contains(['/', '\\']) || id.contains("..") {
                continue;
            }
            // `<id>.log` for gates, `<id>.exec` (the output spool) for exec jobs.
            for ext in ["log", "exec"] {
                let file = logs_dir.join(format!("{id}.{ext}"));
                match std::fs::remove_file(&file) {
                    Ok(()) => deleted += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io(format!("removing {}", file.display()), e)),
                }
            }
        }
        deleted += self.prune_unowned_logs(logs_dir, older_than_ms)?;
        Ok(deleted)
    }

    /// Deletes `<logs_dir>/<id>.log` files that belong to no job row (logs of
    /// in-process fallback runs) whose mtime is before `older_than_ms`. Only
    /// regular files named like a job id (`[A-Za-z0-9_-]+.log`) are touched,
    /// plus the `mcp-<pid>.log` files of `sluice mcp` (same retention). The
    /// CLI's `client.log` is never deleted here: it rotates by size.
    fn prune_unowned_logs(&self, logs_dir: &Path, older_than_ms: u64) -> Result<usize> {
        let entries = match std::fs::read_dir(logs_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(io(format!("reading {}", logs_dir.display()), e)),
        };
        let cutoff = UNIX_EPOCH + Duration::from_millis(older_than_ms);
        let mut known = self
            .conn
            .prepare("SELECT 1 FROM jobs WHERE id = ?1")
            .map_err(db)?;
        let mut deleted = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".log")) else {
                continue;
            };
            if id.is_empty()
                || name.to_str() == Some(sluice_gate_core::logfile::CLIENT_LOG)
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() || meta.modified().map_or(true, |m| m >= cutoff) {
                continue;
            }
            let is_mcp = name
                .to_str()
                .is_some_and(sluice_gate_core::logfile::is_mcp_log_name);
            if !is_mcp && known.exists([id]).map_err(db)? {
                continue;
            }
            let file = entry.path();
            match std::fs::remove_file(&file) {
                Ok(()) => deleted += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(format!("removing {}", file.display()), e)),
            }
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state").join("sluice.db");
        (d, p)
    }

    fn job(id: &str, instance: &str, queued_at: u64) -> JobRow {
        JobRow {
            id: id.into(),
            key: Some("k".into()),
            tree: None,
            repo: PathBuf::from("/repo"),
            gate: "test".into(),
            cmd: "cargo test".into(),
            weight: 2,
            caller: "agent".into(),
            session: None,
            state: JobState::Queued,
            outcome: None,
            exit_code: None,
            queued_at,
            started_at: None,
            finished_at: None,
            log_path: None,
            pgid: None,
            proc_start_ticks: None,
            daemon_instance: instance.into(),
            subscribers_max: 1,
            coalesced: 0,
            marker_written: false,
        }
    }

    fn run_job(s: &Store, id: &str, queued: u64, start: u64, end: u64, outcome: &str) {
        s.insert_job(&job(id, "A", queued)).unwrap();
        assert!(
            s.mark_started(id, "A", start, None, None, Path::new("/l"))
                .unwrap()
        );
        assert!(s.mark_done(id, "A", end, outcome, Some(0), false).unwrap());
    }

    #[test]
    fn open_creates_schema_and_reopens() {
        let (_d, p) = tmp();
        {
            let s = Store::open(&p).unwrap();
            assert_eq!(s.path(), p);
            s.insert_job(&job("j1", "A", 1)).unwrap();
        }
        let s = Store::open(&p).unwrap();
        assert_eq!(s.job("j1").unwrap().unwrap(), job("j1", "A", 1));
        assert!(s.job("nope").unwrap().is_none());
    }

    fn columns(s: &Store, table: &str) -> Vec<String> {
        let mut stmt = s
            .conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    /// A database as 0.2.0 leaves it: base schema, `schema.version` 1, no
    /// `sched_schema`.
    fn v020_db(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let conn = Connection::open(p).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute("INSERT INTO schema (version) VALUES (1)", [])
            .unwrap();
    }

    #[test]
    fn interventions_are_stored_once_and_listed_newest_first() {
        use crate::interventions::InterventionRow;
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        assert_eq!(
            crate::migrate::version(&s.conn, crate::migrate::CONTAINMENT_TABLE).unwrap(),
            crate::migrate::CONTAINMENT_SCHEMA_VERSION
        );
        let row = |at_ms: u64, leaf: &str| InterventionRow {
            at_ms,
            session: Some("abc".into()),
            leaf: leaf.into(),
            cause: "oom_kill".into(),
            detail: Some("1 process(es) killed by the kernel OOM killer".into()),
        };
        assert!(s.record_intervention(&row(1000, "s-abc/tools")).unwrap());
        assert!(
            !s.record_intervention(&row(1000, "s-abc/tools")).unwrap(),
            "the same event is kept once"
        );
        assert!(s.record_intervention(&row(3000, "jobs/j1")).unwrap());
        assert!(s.record_intervention(&row(2000, "s-abc/ui")).unwrap());
        let all = s.recent_interventions(0, 10).unwrap();
        assert_eq!(
            all.iter().map(|r| r.at_ms).collect::<Vec<_>>(),
            [3000, 2000, 1000]
        );
        assert_eq!(all[2], row(1000, "s-abc/tools"));
        assert_eq!(s.recent_interventions(1500, 10).unwrap().len(), 2);
        assert_eq!(s.recent_interventions(0, 1).unwrap().len(), 1);
        // A reopen applies nothing again and keeps the rows.
        drop(s);
        let s = Store::open(&p).unwrap();
        assert_eq!(s.recent_interventions(0, 10).unwrap().len(), 3);
    }

    #[test]
    fn a_fresh_database_is_migrated_without_a_backup() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        assert_eq!(s.sched_version(), crate::migrate::SCHED_SCHEMA_VERSION);
        assert!(columns(&s, "jobs").contains(&"killed_by".to_owned()));
        assert!(columns(&s, "job_requests").contains(&"via".to_owned()));
        assert!(columns(&s, "gate_stats").contains(&"demand_milli".to_owned()));
        assert!(!crate::migrate::backup_path(&p, 0).exists());
        let version: u32 = s
            .conn
            .query_row("SELECT version FROM schema", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1, "schema.version stays 1");
    }

    #[test]
    fn migration_backs_up_and_is_idempotent() {
        let (_d, p) = tmp();
        v020_db(&p);
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute(
                "INSERT INTO jobs (id, repo, gate, cmd, weight, caller, state, queued_at, \
                 daemon_instance) VALUES ('old', '/r', 'test', 'true', 1, 'human', 'done', 5, 'x')",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&p).unwrap();
        let bak = crate::migrate::backup_path(&p, 0);
        assert!(bak.exists(), "backup written before migrating");
        assert_eq!(s.sched_version(), 1);
        assert!(s.job("old").unwrap().is_some());
        let old = s.timeline("old").unwrap().unwrap();
        assert_eq!(old.admitted_at, None, "old rows read as unknown");
        // The backup is the pre-migration file.
        let backup = Connection::open(&bak).unwrap();
        assert_eq!(crate::migrate::version(&backup, "sched_schema").unwrap(), 0);
        let jobs: i64 = backup
            .query_row("SELECT count(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jobs, 1);
        drop((s, backup));
        // Second open: nothing to do, the backup is untouched.
        let before = std::fs::metadata(&bak).unwrap().modified().unwrap();
        let s = Store::open(&p).unwrap();
        assert_eq!(s.sched_version(), 1);
        assert_eq!(std::fs::metadata(&bak).unwrap().modified().unwrap(), before);
        let rows: i64 = s
            .conn
            .query_row("SELECT count(*) FROM sched_schema", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_failing_migration_rolls_back_everything() {
        let (_d, p) = tmp();
        v020_db(&p);
        let broken = [
            crate::migrate::SCHED_MIGRATIONS[0],
            crate::migrate::Migration {
                version: 2,
                name: "broken",
                sql: "ALTER TABLE jobs ADD COLUMN extra INTEGER; SELECT * FROM no_such_table;",
            },
        ];
        assert!(Store::open_with(&p, &broken).is_err());
        let conn = Connection::open(&p).unwrap();
        assert_eq!(crate::migrate::version(&conn, "sched_schema").unwrap(), 0);
        drop(conn);
        let ro = Store::open_read_only(&p).unwrap();
        assert_eq!(ro.sched_version(), 0);
        assert!(!columns(&ro, "jobs").contains(&"killed_by".to_owned()));
        assert!(!columns(&ro, "jobs").contains(&"extra".to_owned()));
        drop(ro);
        // The real list then applies cleanly.
        assert_eq!(Store::open(&p).unwrap().sched_version(), 1);
    }

    #[test]
    fn a_newer_sched_schema_is_refused_read_write_but_read_only_works() {
        let (_d, p) = tmp();
        drop(Store::open(&p).unwrap());
        let conn = Connection::open(&p).unwrap();
        conn.execute("UPDATE sched_schema SET version = 7", [])
            .unwrap();
        drop(conn);
        match Store::open(&p) {
            Err(Error::NewerSchema {
                found, expected, ..
            }) => assert_eq!((found, expected), (7, 1)),
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(Store::open_read_only(&p).unwrap().sched_version(), 1);
    }

    #[test]
    fn timeline_requests_and_gate_stats() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        s.insert_job(&job("j1", "A", 100)).unwrap();
        s.set_queued_meta(
            "j1",
            QueuedMeta {
                requested_at: Some(90),
                weight_cfg: 8,
                expected_ms: Some(12_000),
            },
        )
        .unwrap();
        let r1 = s
            .insert_request(&RequestRow {
                job_id: "j1".into(),
                caller: "agent".into(),
                session: None,
                client_pid: Some(7),
                via: "mcp".into(),
                requested_at: Some(90),
                attached_at: 100,
                coalesced: false,
            })
            .unwrap()
            .unwrap();
        let r2 = s
            .insert_request(&RequestRow {
                job_id: "j1".into(),
                caller: "human".into(),
                session: None,
                client_pid: None,
                via: "cli".into(),
                requested_at: None,
                attached_at: 150,
                coalesced: true,
            })
            .unwrap()
            .unwrap();
        s.set_weight("j1", 1).unwrap();
        assert_eq!(s.job("j1").unwrap().unwrap().weight, 1);
        s.set_admitted("j1", 200, "over").unwrap();
        assert!(
            s.mark_started("j1", "A", 210, None, None, Path::new("/l"))
                .unwrap()
        );
        s.close_request(r2, 250, true).unwrap();
        assert!(s.mark_done("j1", "A", 900, "killed", None, false).unwrap());
        s.close_request(r1, 900, false).unwrap();
        s.set_finish_meta(
            "j1",
            &FinishMeta {
                waits: [60, 0, 0, 0, 40],
                cpu_ms: Some(3_000),
                run_delay_ms: Some(500),
                demand_milli: Some(5_000),
                killed_by: Some("early-verdict".into()),
            },
        )
        .unwrap();
        let t = s.timeline("j1").unwrap().unwrap();
        assert_eq!(
            (
                t.requested_at,
                t.job.queued_at,
                t.admitted_at,
                t.job.started_at
            ),
            (Some(90), 100, Some(200), Some(210))
        );
        assert_eq!(t.job.finished_at, Some(900));
        assert_eq!(t.weight_cfg, Some(8));
        assert_eq!(t.expected_ms, Some(12_000));
        assert_eq!(t.waits, [Some(60), Some(0), Some(0), Some(0), Some(40)]);
        assert_eq!(t.admission.as_deref(), Some("over"));
        assert_eq!(t.killed_by.as_deref(), Some("early-verdict"));
        assert_eq!(
            (t.cpu_ms, t.run_delay_ms, t.demand_milli),
            (Some(3_000), Some(500), Some(5_000))
        );
        assert_eq!((t.requests, t.detached), (2, 1));
        assert_eq!(s.timelines_since(0).unwrap().len(), 1);
        assert_eq!(s.timelines_since(101).unwrap(), Vec::new());
    }

    #[test]
    fn gate_stats_keep_twenty_per_key_and_prune_unseen_keys() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        let stat = |n: u64, key: &str| GateStat {
            repo: PathBuf::from("/r"),
            gate: "test".into(),
            cmd_hash: key.into(),
            job_id: format!("j{n}"),
            finished_at: n,
            outcome: "pass".into(),
            wall_ms: 1_000 + n,
            cpu_ms: 500,
            run_delay_ms: None,
            demand_milli: u32::try_from(n).unwrap(),
        };
        for n in 0..25 {
            s.insert_gate_stat(&stat(n, "aaaa")).unwrap();
        }
        s.insert_gate_stat(&stat(3, "bbbb")).unwrap();
        let all = s.gate_stats_recent(100).unwrap();
        assert_eq!(
            all.iter().filter(|g| g.cmd_hash == "aaaa").count(),
            20,
            "pruned to 20"
        );
        let recent = s.gate_stats_recent(10).unwrap();
        let a: Vec<u64> = recent
            .iter()
            .filter(|g| g.cmd_hash == "aaaa")
            .map(|g| g.finished_at)
            .collect();
        assert_eq!(a, (15..25).collect::<Vec<_>>(), "newest ten, oldest first");
        // A failed run (written by an earlier release) is not a sample.
        let mut failed = stat(30, "aaaa");
        failed.outcome = "fail".into();
        s.insert_gate_stat(&failed).unwrap();
        assert!(
            s.gate_stats_recent(10)
                .unwrap()
                .iter()
                .all(|g| g.outcome == "pass" && g.finished_at != 30)
        );
        assert_eq!(s.prune_gate_stats(10).unwrap(), 1, "bbbb unseen since 3");
        assert!(
            s.gate_stats_recent(10)
                .unwrap()
                .iter()
                .all(|g| g.cmd_hash == "aaaa")
        );
    }

    #[test]
    fn gate_memory_keeps_twenty_per_key_and_prunes_with_the_stats() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        let sample = |n: u64, key: &str| GateMemory {
            repo: PathBuf::from("/r"),
            gate: "test".into(),
            cmd_hash: key.into(),
            job_id: format!("j{n}"),
            finished_at: n,
            outcome: if n.is_multiple_of(2) {
                "pass"
            } else {
                "killed"
            }
            .into(),
            anon_peak_bytes: 1_000 + n,
            peak_bytes: (!n.is_multiple_of(3)).then_some(5_000 + n),
            oom_killed: n % 2 == 1,
        };
        for n in 0..25 {
            s.insert_gate_memory(&sample(n, "aaaa")).unwrap();
        }
        s.insert_gate_memory(&sample(3, "bbbb")).unwrap();
        let all = s.gate_memory_recent(100).unwrap();
        assert_eq!(
            all.iter().filter(|m| m.cmd_hash == "aaaa").count(),
            20,
            "pruned to 20"
        );
        let recent = s.gate_memory_recent(5).unwrap();
        let a: Vec<u64> = recent
            .iter()
            .filter(|m| m.cmd_hash == "aaaa")
            .map(|m| m.finished_at)
            .collect();
        assert_eq!(a, (20..25).collect::<Vec<_>>(), "newest five, oldest first");
        // Everything round-trips, killed runs and a missing `memory.peak` too.
        assert_eq!(
            recent.iter().find(|m| m.finished_at == 23),
            Some(&sample(23, "aaaa"))
        );
        assert_eq!(
            recent.iter().find(|m| m.finished_at == 24),
            Some(&sample(24, "aaaa"))
        );
        assert!(
            s.gate_memory_recent(5)
                .unwrap()
                .iter()
                .any(|m| m.peak_bytes.is_none())
        );
        assert_eq!(s.prune_gate_memory(10).unwrap(), 1, "bbbb unseen since 3");
        assert!(
            s.gate_memory_recent(10)
                .unwrap()
                .iter()
                .all(|m| m.cmd_hash == "aaaa")
        );
    }

    #[test]
    fn lower_bounds_live_in_their_own_table_and_prune_with_the_stats() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        let bound = |n: u64, key: &str| GateMemory {
            repo: PathBuf::from("/r"),
            gate: "test".into(),
            cmd_hash: key.into(),
            job_id: format!("j{n}"),
            finished_at: n,
            outcome: if n.is_multiple_of(2) {
                "fail"
            } else {
                "timeout"
            }
            .into(),
            anon_peak_bytes: 100 + n,
            peak_bytes: None,
            oom_killed: false,
        };
        assert!(bound(1, "a").is_lower_bound() && bound(2, "a").is_lower_bound());
        for n in 0..25 {
            s.insert_gate_bound(&bound(n, "aaaa")).unwrap();
        }
        s.insert_gate_bound(&bound(3, "bbbb")).unwrap();
        // Released binaries read every row of `gate_memory`: none is there.
        assert_eq!(s.gate_memory_recent(100).unwrap(), Vec::new());
        let all = s.gate_bounds_recent(100).unwrap();
        assert_eq!(all.iter().filter(|m| m.cmd_hash == "aaaa").count(), 20);
        let recent = s.gate_bounds_recent(2).unwrap();
        let a: Vec<u64> = recent
            .iter()
            .filter(|m| m.cmd_hash == "aaaa")
            .map(|m| m.finished_at)
            .collect();
        assert_eq!(a, [23, 24], "newest two, oldest first");
        assert_eq!(
            recent.iter().find(|m| m.finished_at == 24),
            Some(&bound(24, "aaaa"))
        );
        assert_eq!(s.prune_gate_memory(10).unwrap(), 1, "bbbb unseen since 3");
        assert!(
            s.gate_bounds_recent(100)
                .unwrap()
                .iter()
                .all(|m| m.cmd_hash == "aaaa")
        );
    }

    #[test]
    fn lower_bounds_are_ignored_by_a_file_without_the_table() {
        let (_d, p) = tmp();
        v020_db(&p);
        let s = Store::open_read_only(&p).unwrap();
        let row = GateMemory {
            repo: PathBuf::from("/r"),
            gate: "test".into(),
            cmd_hash: "aaaa".into(),
            job_id: "j".into(),
            finished_at: 1,
            outcome: "fail".into(),
            anon_peak_bytes: 1,
            peak_bytes: None,
            oom_killed: false,
        };
        s.insert_gate_bound(&row).unwrap();
        assert_eq!(s.gate_bounds_recent(5).unwrap(), Vec::new());
    }

    #[test]
    fn gate_memory_weights_come_from_the_jobs_and_skip_pruned_ones() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        let run = |id: &str, gate: &str, weight: u32, at: u64| {
            let mut row = job(id, "x", at);
            row.weight = weight;
            s.insert_job(&row).unwrap();
            s.insert_gate_memory(&GateMemory {
                repo: PathBuf::from("/repo"),
                gate: gate.into(),
                cmd_hash: "aaaa".into(),
                job_id: id.into(),
                finished_at: at,
                outcome: "pass".into(),
                anon_peak_bytes: 1,
                peak_bytes: None,
                oom_killed: false,
            })
            .unwrap();
        };
        run("a1", "lint", 2, 1);
        run("a2", "lint", 1, 2);
        run("b1", "test", 8, 3);
        // A run whose job row is gone (pruned) has no weight.
        s.insert_gate_memory(&GateMemory {
            repo: PathBuf::from("/repo"),
            gate: "fmt".into(),
            cmd_hash: "aaaa".into(),
            job_id: "pruned".into(),
            finished_at: 4,
            outcome: "pass".into(),
            anon_peak_bytes: 1,
            peak_bytes: None,
            oom_killed: false,
        })
        .unwrap();
        let weights: Vec<(String, u32)> = s
            .gate_memory_weights()
            .unwrap()
            .into_iter()
            .map(|(_, gate, _, w)| (gate, w))
            .collect();
        assert_eq!(
            weights,
            [("lint".to_owned(), 1), ("test".to_owned(), 8)],
            "the newest run of a key sets its weight; no job row, no weight"
        );
    }

    #[test]
    fn gate_memory_is_ignored_by_a_file_without_the_table() {
        // A read-only open of a database no migration has touched yet.
        let (_d, p) = tmp();
        v020_db(&p);
        let s = Store::open_read_only(&p).unwrap();
        let sample = GateMemory {
            repo: PathBuf::from("/r"),
            gate: "test".into(),
            cmd_hash: "aaaa".into(),
            job_id: "j".into(),
            finished_at: 1,
            outcome: "pass".into(),
            anon_peak_bytes: 1,
            peak_bytes: None,
            oom_killed: false,
        };
        s.insert_gate_memory(&sample).unwrap();
        assert_eq!(s.gate_memory_recent(5).unwrap(), Vec::new());
        assert_eq!(s.prune_gate_memory(10).unwrap(), 0);
    }

    #[test]
    fn unbounded_learned_weight_is_the_p75_demand_without_a_clamp() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        let stat = |n: u64, gate: &str, demand: u32, outcome: &str| GateStat {
            repo: PathBuf::from("/r"),
            gate: gate.into(),
            cmd_hash: "aaaa".into(),
            job_id: format!("{gate}{n}"),
            finished_at: n,
            outcome: outcome.into(),
            wall_ms: 1_000,
            cpu_ms: 500,
            run_delay_ms: None,
            demand_milli: demand,
        };
        let repo = Path::new("/r");
        assert_eq!(s.unbounded_learned(repo, "test").unwrap(), None, "no runs");
        for n in 0..2 {
            s.insert_gate_stat(&stat(n, "test", 9_000, "pass")).unwrap();
        }
        assert_eq!(
            s.unbounded_learned(repo, "test").unwrap(),
            None,
            "warm-up: fewer than three samples"
        );
        s.insert_gate_stat(&stat(2, "test", 9_400, "pass")).unwrap();
        assert_eq!(s.unbounded_learned(repo, "test").unwrap(), Some(9));
        // Failed runs and other gates or repositories are not samples.
        s.insert_gate_stat(&stat(3, "test", 100, "fail")).unwrap();
        s.insert_gate_stat(&stat(4, "lint", 100, "pass")).unwrap();
        assert_eq!(s.unbounded_learned(repo, "test").unwrap(), Some(9));
        assert_eq!(
            s.unbounded_learned(Path::new("/other"), "test").unwrap(),
            None
        );
        // Only the newest window counts: ten quiet runs push the old ones out.
        for n in 10..20 {
            s.insert_gate_stat(&stat(n, "test", 1_200, "pass")).unwrap();
        }
        assert_eq!(s.unbounded_learned(repo, "test").unwrap(), Some(1));
    }

    #[test]
    fn version_mismatch_is_schema_error() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        s.conn
            .execute("UPDATE schema SET version = 99", [])
            .unwrap();
        drop(s);
        match Store::open(&p) {
            Err(Error::Schema {
                found,
                expected,
                path,
                ..
            }) => {
                assert_eq!((found, expected), (99, SCHEMA_VERSION));
                assert_eq!(path, p);
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(matches!(
            Store::open_read_only(&p),
            Err(Error::Schema { .. })
        ));
    }

    #[test]
    fn lease_acquire_renew_fence() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        assert!(s.acquire_lease("daemon", "a", "ta", 1000, 500).unwrap());
        assert!(!s.acquire_lease("daemon", "b", "tb", 1200, 500).unwrap());
        assert!(!s.heartbeat("daemon", "tb", 1200, 500).unwrap());
        assert!(s.heartbeat("daemon", "ta", 1400, 500).unwrap());
        assert_eq!(
            s.lease("daemon").unwrap(),
            Some(("a".into(), "ta".into(), 1400, 1900))
        );
        assert!(!s.acquire_lease("daemon", "b", "tb", 1899, 500).unwrap());
        assert!(s.acquire_lease("daemon", "b", "tb", 1900, 500).unwrap());
        assert!(!s.heartbeat("daemon", "ta", 1950, 500).unwrap());
        assert!(!s.release_lease("daemon", "ta").unwrap());
        assert!(s.release_lease("daemon", "tb").unwrap());
        assert!(s.lease("daemon").unwrap().is_none());
        assert!(s.acquire_lease("daemon", "c", "tc", 2000, 500).unwrap());
        assert!(s.acquire_lease("daemon", "c", "tc2", 2100, 500).unwrap());
    }

    #[test]
    fn job_lifecycle_is_fenced() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        s.insert_job(&job("j1", "A", 10)).unwrap();
        assert!(
            !s.mark_started("j1", "B", 20, None, None, Path::new("/l"))
                .unwrap()
        );
        assert_eq!(s.job("j1").unwrap().unwrap().state, JobState::Queued);
        assert!(
            s.mark_started("j1", "A", 20, Some(42), Some(7), Path::new("/l/j1.log"))
                .unwrap()
        );
        assert!(
            !s.mark_started("j1", "A", 21, None, None, Path::new("/x"))
                .unwrap()
        );
        let r = s.job("j1").unwrap().unwrap();
        assert_eq!(r.state, JobState::Running);
        assert_eq!(
            (r.started_at, r.pgid, r.proc_start_ticks, r.log_path),
            (
                Some(20),
                Some(42),
                Some(7),
                Some(PathBuf::from("/l/j1.log"))
            )
        );
        assert!(!s.mark_done("j1", "B", 30, "fail", Some(1), true).unwrap());
        assert_eq!(s.job("j1").unwrap().unwrap().state, JobState::Running);
        assert!(s.mark_done("j1", "A", 30, "pass", Some(0), true).unwrap());
        assert!(!s.mark_done("j1", "A", 31, "fail", Some(1), false).unwrap());
        let r = s.job("j1").unwrap().unwrap();
        assert_eq!(r.state, JobState::Done);
        assert_eq!(r.outcome.as_deref(), Some("pass"));
        assert_eq!(
            (r.exit_code, r.finished_at, r.marker_written),
            (Some(0), Some(30), true)
        );
        s.set_subscribers("j1", 3, 2).unwrap();
        let r = s.job("j1").unwrap().unwrap();
        assert_eq!((r.subscribers_max, r.coalesced), (3, 2));
        assert_eq!(s.jobs_in_state(JobState::Done).unwrap().len(), 1);
        assert_eq!(s.jobs_in_state(JobState::Queued).unwrap().len(), 0);
    }

    #[test]
    fn orphans_and_newest() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        assert!(s.newest_job().unwrap().is_none());
        s.insert_job(&job("a", "OLD", 1)).unwrap();
        s.insert_job(&job("b", "OLD", 2)).unwrap();
        s.insert_job(&job("c", "NEW", 3)).unwrap();
        s.insert_job(&job("d", "NEW", 3)).unwrap();
        assert!(
            s.mark_started("a", "OLD", 5, None, None, Path::new("/l"))
                .unwrap()
        );
        assert!(
            s.mark_started("c", "NEW", 5, None, None, Path::new("/l"))
                .unwrap()
        );
        let o = s.orphans("NEW").unwrap();
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].id, "a");
        assert_eq!(s.newest_job().unwrap().unwrap().id, "d");
        assert_eq!(s.close_orphans("NEW", 99).unwrap(), (1, 1));
        let was_running = s.job("a").unwrap().unwrap();
        assert_eq!(was_running.state, JobState::Done);
        assert_eq!(was_running.outcome.as_deref(), Some("killed"));
        let b = s.job("b").unwrap().unwrap();
        assert_eq!(b.state, JobState::Done);
        assert_eq!(b.outcome.as_deref(), Some("orphaned"));
        assert_eq!(b.finished_at, Some(99));
        assert_eq!(s.job("c").unwrap().unwrap().state, JobState::Running);
        assert_eq!(s.orphans("NEW").unwrap().len(), 0);
    }

    #[test]
    fn totals_fixture() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        assert_eq!(
            s.totals(None).unwrap(),
            sluice_gate_proto::Totals::default()
        );
        run_job(&s, "p1", 1000, 1100, 1500, "pass"); // wait 100, run 400
        run_job(&s, "p2", 2000, 2300, 2600, "pass"); // wait 300, run 300
        run_job(&s, "f1", 3000, 3000, 3200, "fail"); // wait 0, run 200
        run_job(&s, "c1", 4000, 4050, 4250, "pass"); // wait 50, run 200
        s.set_subscribers("c1", 2, 1).unwrap();
        s.record_hit(5000, Path::new("/repo"), "test", 700).unwrap();
        s.record_hit(6000, Path::new("/repo"), "test", 800).unwrap();
        let t = s.totals(None).unwrap();
        assert_eq!(t.runs, 4);
        assert_eq!(t.passes, 3);
        assert_eq!(t.failures, 1);
        assert_eq!(t.timeouts, 0);
        assert_eq!(t.hits, 2);
        assert_eq!(t.coalesced, 1);
        assert_eq!(t.wait_ms_total, 450);
        assert_eq!(t.run_ms_total, 1100);
        assert_eq!(t.time_saved_ms, 700 + 800 + 200);
        assert_eq!(t.since, "1970-01-01T00:00:01Z");
        let t = s.totals(Some(3500)).unwrap();
        assert_eq!((t.runs, t.hits, t.coalesced), (1, 2, 1));
        assert_eq!(t.since, "1970-01-01T00:00:03Z");
    }

    #[test]
    fn prune_logs_only_old_done() {
        let (d, p) = tmp();
        let s = Store::open(&p).unwrap();
        let logs = d.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        run_job(&s, "old", 1, 2, 100, "pass");
        run_job(&s, "new", 1, 2, 900, "pass");
        s.insert_job(&job("queued", "A", 1)).unwrap();
        run_job(&s, "nolog", 1, 2, 50, "pass");
        for id in ["old", "new", "queued"] {
            std::fs::write(logs.join(format!("{id}.log")), "x").unwrap();
        }
        std::fs::write(logs.join("old.exec"), "x").unwrap();
        std::fs::write(logs.join("new.exec"), "x").unwrap();
        assert_eq!(s.prune_logs(&logs, 500).unwrap(), 2);
        assert!(!logs.join("old.log").exists());
        assert!(!logs.join("old.exec").exists());
        assert!(logs.join("new.exec").exists());
        assert!(logs.join("new.log").exists());
        assert!(logs.join("queued.log").exists());
        assert_eq!(s.prune_logs(&logs, 500).unwrap(), 0);
    }

    #[test]
    fn prune_logs_removes_old_fallback_logs() {
        let (d, p) = tmp();
        let s = Store::open(&p).unwrap();
        let logs = d.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        s.insert_job(&job("queued-old", "A", 1)).unwrap();
        let old = UNIX_EPOCH + Duration::from_millis(100);
        for name in [
            "fallback-old.log",
            "fallback-new.log",
            "queued-old.log",
            "odd name.log",
            "notes.txt",
        ] {
            let f = logs.join(name);
            std::fs::write(&f, "x").unwrap();
            if name != "fallback-new.log" {
                std::fs::File::options()
                    .write(true)
                    .open(&f)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
        }
        // Retention cutoff at 1 s after the epoch: only `fallback-old` is
        // old, owned by no row and well named.
        assert_eq!(s.prune_logs(&logs, 1_000).unwrap(), 1);
        assert!(!logs.join("fallback-old.log").exists());
        for kept in [
            "fallback-new.log",
            "queued-old.log",
            "odd name.log",
            "notes.txt",
        ] {
            assert!(logs.join(kept).exists(), "{kept}");
        }
    }

    #[test]
    fn prune_logs_handles_mcp_and_client_logs() {
        let (d, p) = tmp();
        let s = Store::open(&p).unwrap();
        let logs = d.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let old = UNIX_EPOCH + Duration::from_millis(100);
        for name in ["mcp-41.log", "mcp-42.log", "client.log", "client.log.1"] {
            let f = logs.join(name);
            std::fs::write(&f, "x").unwrap();
            if name != "mcp-42.log" {
                std::fs::File::options()
                    .write(true)
                    .open(&f)
                    .unwrap()
                    .set_modified(old)
                    .unwrap();
            }
        }
        assert_eq!(s.prune_logs(&logs, 1_000).unwrap(), 1);
        assert!(!logs.join("mcp-41.log").exists());
        for kept in ["mcp-42.log", "client.log", "client.log.1"] {
            assert!(logs.join(kept).exists(), "{kept}");
        }
    }

    #[test]
    fn read_only_reads_but_cannot_write() {
        let (_d, p) = tmp();
        let s = Store::open(&p).unwrap();
        s.insert_job(&job("j1", "A", 1)).unwrap();
        let ro = Store::open_read_only(&p).unwrap();
        assert!(ro.job("j1").unwrap().is_some());
        assert!(ro.insert_job(&job("j2", "A", 2)).is_err());
        assert!(ro.record_hit(1, Path::new("/r"), "g", 1).is_err());
    }
}
