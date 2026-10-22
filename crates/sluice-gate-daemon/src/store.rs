//! SQLite persistence: jobs, lease (fencing), client hit events and totals.
//!
//! The schema is documented in `kb/sluice/design/executor.md`. The store is
//! synchronous; callers use it from a blocking context. The database file is
//! never deleted or migrated: a version mismatch is a hard error.

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
}

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
    /// version fails with [`Error::Schema`]. Nothing is ever migrated or deleted.
    ///
    /// # Errors
    /// I/O, SQLite, busy or schema mismatch errors.
    pub fn open(path: &Path) -> Result<Self> {
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
        let store = Self {
            conn,
            path: path.to_path_buf(),
        };
        store.init_schema()?;
        Ok(store)
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
        let store = Self {
            conn,
            path: path.to_path_buf(),
        };
        store.check_version()?;
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

    fn init_schema(&self) -> Result<()> {
        let tx = self.tx()?;
        if Self::table_count(&tx)? == 0 {
            tx.execute_batch(SCHEMA_SQL).map_err(db)?;
            tx.execute("INSERT INTO schema (version) VALUES (?1)", [SCHEMA_VERSION])
                .map_err(db)?;
            tx.commit().map_err(db)?;
            return Ok(());
        }
        tx.commit().map_err(db)?;
        self.check_version()
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

    /// Marks every queued/running job of other instances as done with outcome `orphaned`.
    ///
    /// # Errors
    /// SQLite or busy errors.
    pub fn mark_orphaned(&self, instance: &str, finished_at: u64) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE jobs SET state = 'done', outcome = 'orphaned', finished_at = ?2 \
                 WHERE daemon_instance <> ?1 AND state IN ('queued', 'running')",
                params![instance, i(finished_at)],
            )
            .map_err(db)
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
            let file = logs_dir.join(format!("{id}.log"));
            match std::fs::remove_file(&file) {
                Ok(()) => deleted += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(format!("removing {}", file.display()), e)),
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
        assert_eq!(s.mark_orphaned("NEW", 99).unwrap(), 2);
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
        assert_eq!(s.prune_logs(&logs, 500).unwrap(), 1);
        assert!(!logs.join("old.log").exists());
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
