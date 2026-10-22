//! Database migration framework (ADR-0013 G9, ADR-0011 section 7.4).
//!
//! `schema.version` stays 1 so released 0.1.0/0.2.0 binaries keep opening
//! the file (they check that version, name their columns in every `INSERT`
//! and ignore unknown tables). Additions are grouped and versioned by their
//! own one-row table (`sched_schema` here; the merge queue's `queue_schema`
//! follows the same rule), and applied on open by numbered migrations:
//!
//! - each migration is additive only (new tables, nullable columns);
//! - all pending migrations run in one `IMMEDIATE` transaction together with
//!   the version update (SQLite DDL is transactional: a failure leaves the
//!   previous version and nothing applied);
//! - before migrating an existing database, a consistent copy is written
//!   next to it as `<db>.bak-<version before>` (`VACUUM INTO`), unless one
//!   already exists;
//! - a version newer than the build is a hard error
//!   ([`crate::Error::NewerSchema`]).

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

/// One numbered, additive migration.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Version reached after it.
    pub version: u32,
    /// What it adds (logged).
    pub name: &'static str,
    /// SQL batch.
    pub sql: &'static str,
}

/// Version table of the scheduler additions.
pub const SCHED_TABLE: &str = "sched_schema";

/// `sched_schema` version this build reads and writes.
pub const SCHED_SCHEMA_VERSION: u32 = 1;

/// Migration 0 → 1 (ADR-0011 section 7.4): the job timeline columns, the
/// `job_requests` table and the `gate_stats` table.
const SCHED_1: &str = "
ALTER TABLE jobs ADD COLUMN requested_at INTEGER;
ALTER TABLE jobs ADD COLUMN admitted_at INTEGER;
ALTER TABLE jobs ADD COLUMN weight_cfg INTEGER;
ALTER TABLE jobs ADD COLUMN expected_ms INTEGER;
ALTER TABLE jobs ADD COLUMN wait_budget_ms INTEGER;
ALTER TABLE jobs ADD COLUMN wait_psi_ms INTEGER;
ALTER TABLE jobs ADD COLUMN wait_repo_cap_ms INTEGER;
ALTER TABLE jobs ADD COLUMN wait_reserved_ms INTEGER;
ALTER TABLE jobs ADD COLUMN wait_order_ms INTEGER;
ALTER TABLE jobs ADD COLUMN cpu_ms INTEGER;
ALTER TABLE jobs ADD COLUMN run_delay_ms INTEGER;
ALTER TABLE jobs ADD COLUMN demand_milli INTEGER;
ALTER TABLE jobs ADD COLUMN admission TEXT;
ALTER TABLE jobs ADD COLUMN killed_by TEXT;
CREATE TABLE job_requests (
  id INTEGER PRIMARY KEY, job_id TEXT NOT NULL, caller TEXT NOT NULL,
  session TEXT, client_pid INTEGER, via TEXT,
  requested_at INTEGER, attached_at INTEGER NOT NULL, finished_at INTEGER,
  coalesced INTEGER NOT NULL, detached INTEGER NOT NULL DEFAULT 0);
CREATE INDEX job_requests_job ON job_requests(job_id);
CREATE TABLE gate_stats (
  repo TEXT NOT NULL, gate TEXT NOT NULL, cmd_hash TEXT NOT NULL,
  job_id TEXT NOT NULL, finished_at INTEGER NOT NULL, outcome TEXT NOT NULL,
  wall_ms INTEGER NOT NULL, cpu_ms INTEGER NOT NULL, run_delay_ms INTEGER,
  demand_milli INTEGER NOT NULL);
CREATE INDEX gate_stats_key ON gate_stats(repo, gate, cmd_hash, finished_at);
";

/// The `sched_schema` migrations, in order.
pub const SCHED_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "scheduler timeline, job requests and gate statistics",
    sql: SCHED_1,
}];

/// Version recorded in `table`; 0 when the table or its row is absent.
///
/// # Errors
/// SQLite errors.
pub fn version(conn: &Connection, table: &str) -> rusqlite::Result<u32> {
    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Ok(0);
    }
    let v: Option<i64> = conn
        .query_row(&format!("SELECT max(version) FROM {table}"), [], |r| {
            r.get(0)
        })
        .optional()?
        .flatten();
    Ok(v.map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX)))
}

/// Path of the backup written before migrating from `from`.
pub fn backup_path(db: &Path, from: u32) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(format!(".bak-{from}"));
    PathBuf::from(name)
}

/// Writes a consistent copy of the database to [`backup_path`] (`VACUUM
/// INTO`, which includes the WAL) unless that file already exists (an
/// earlier attempt already saved the pre-migration state). Returns the path.
///
/// # Errors
/// SQLite errors.
pub fn backup(conn: &Connection, db: &Path, from: u32) -> rusqlite::Result<PathBuf> {
    let path = backup_path(db, from);
    if !path.exists() {
        conn.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
    }
    Ok(path)
}

/// Applies the migrations of `migrations` above the current version of
/// `table`, up to `target`, in one `IMMEDIATE` transaction, and records the
/// new version. Returns the version reached (unchanged when nothing was
/// pending).
///
/// # Errors
/// SQLite errors; nothing is applied then.
pub fn apply(
    conn: &Connection,
    table: &str,
    migrations: &[Migration],
    target: u32,
) -> rusqlite::Result<u32> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (version INTEGER NOT NULL)"
    ))?;
    let found = version(&tx, table)?;
    let mut reached = found;
    for m in migrations
        .iter()
        .filter(|m| m.version > found && m.version <= target)
    {
        tracing::info!(
            table,
            version = m.version,
            migration = m.name,
            "migrating the database"
        );
        tx.execute_batch(m.sql)?;
        reached = m.version;
    }
    if reached != found || found == 0 {
        tx.execute(&format!("DELETE FROM {table}"), [])?;
        tx.execute(
            &format!("INSERT INTO {table} (version) VALUES (?1)"),
            [reached],
        )?;
    }
    tx.commit()?;
    Ok(reached)
}
