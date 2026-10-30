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
//!   next to it as `<db>.bak-<version before>` (`VACUUM INTO` a temporary
//!   file, fsync, rename), unless a backup that passes `PRAGMA quick_check`
//!   already exists (a damaged one is replaced);
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

/// Version table of the containment additions (its own table, so the
/// `sched_schema` that released binaries check does not move).
pub const CONTAINMENT_TABLE: &str = "containment_schema";

/// `containment_schema` version this build reads and writes.
pub const CONTAINMENT_SCHEMA_VERSION: u32 = 4;

/// Migration 0 -> 1 of the containment group: the `interventions` table
/// (kills by the kernel OOM killer or `systemd-oomd` on Sluice's cgroups).
const CONTAINMENT_1: &str = "
CREATE TABLE interventions (
  id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, session TEXT,
  leaf TEXT NOT NULL, cause TEXT NOT NULL, detail TEXT);
CREATE UNIQUE INDEX interventions_once ON interventions(at_ms, leaf, cause);
";

/// Migration 1 -> 2 of the containment group (ADR-0019): `gate_memory`, the
/// working set of finished runs that memory-aware admission learns from. A
/// table of its own, not columns of `gate_stats`: that would need
/// `sched_schema` 2, which released binaries refuse to open.
const CONTAINMENT_2: &str = "
CREATE TABLE gate_memory (
  repo TEXT NOT NULL, gate TEXT NOT NULL, cmd_hash TEXT NOT NULL,
  job_id TEXT NOT NULL, finished_at INTEGER NOT NULL, outcome TEXT NOT NULL,
  anon_peak_bytes INTEGER NOT NULL, peak_bytes INTEGER,
  oom_killed INTEGER NOT NULL DEFAULT 0);
CREATE INDEX gate_memory_key ON gate_memory(repo, gate, cmd_hash, finished_at);
";

/// Migration 2 -> 3 of the containment group (ADR-0022): `gate_memory_bound`,
/// the peaks of runs that ended `fail` or `timeout`, lower bounds of what the
/// gate needs. A table of its own, with the columns of `gate_memory`: released
/// binaries read every row of `gate_memory` as a finished run's working set,
/// so a bound stored there would be taken for a measurement.
const CONTAINMENT_3: &str = "
CREATE TABLE gate_memory_bound (
  repo TEXT NOT NULL, gate TEXT NOT NULL, cmd_hash TEXT NOT NULL,
  job_id TEXT NOT NULL, finished_at INTEGER NOT NULL, outcome TEXT NOT NULL,
  anon_peak_bytes INTEGER NOT NULL, peak_bytes INTEGER,
  oom_killed INTEGER NOT NULL DEFAULT 0);
CREATE INDEX gate_memory_bound_key
  ON gate_memory_bound(repo, gate, cmd_hash, finished_at);
";

/// Migration 3 -> 4 of the containment group (ADR-0026): `outside_growth`,
/// how much the memory used outside gate jobs grew while each heavy job ran,
/// for the observed reserve of memory admission. A table of its own: older
/// binaries never read it.
const CONTAINMENT_4: &str = "
CREATE TABLE outside_growth (
  job_id TEXT NOT NULL, finished_at INTEGER NOT NULL,
  gate_peak_bytes INTEGER NOT NULL, growth_bytes INTEGER NOT NULL);
CREATE INDEX outside_growth_time ON outside_growth(finished_at);
";

/// The `containment_schema` migrations, in order.
pub const CONTAINMENT_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "containment interventions",
        sql: CONTAINMENT_1,
    },
    Migration {
        version: 2,
        name: "gate memory working sets",
        sql: CONTAINMENT_2,
    },
    Migration {
        version: 3,
        name: "gate memory lower bounds",
        sql: CONTAINMENT_3,
    },
    Migration {
        version: 4,
        name: "memory growth outside gates",
        sql: CONTAINMENT_4,
    },
];

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

/// Writes a consistent copy of the database to [`backup_path`] unless a
/// sound one already exists (an earlier attempt already saved the
/// pre-migration state). The copy is made with `VACUUM INTO` (which
/// includes the WAL) into a temporary file next to it, fsynced, then
/// renamed into place, so a crash never leaves a partial backup under the
/// final name. An existing backup is trusted only when it opens and passes
/// `PRAGMA quick_check`; otherwise it is replaced. Returns the path.
///
/// # Errors
/// SQLite errors, and I/O errors as [`rusqlite::Error::ToSqlConversionFailure`].
pub fn backup(conn: &Connection, db: &Path, from: u32) -> rusqlite::Result<PathBuf> {
    let path = backup_path(db, from);
    if path.exists() {
        if backup_is_sound(&path) {
            return Ok(path);
        }
        tracing::warn!(
            backup = %path.display(),
            "the existing database backup fails its integrity check; writing a new one"
        );
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let _ = std::fs::remove_file(&tmp);
    let written = conn
        .execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()])
        .map(|_| ())
        .and_then(|()| persist(&tmp, &path).map_err(io_error));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map(|()| path)
}

/// Whether an existing backup opens read-only and passes `PRAGMA quick_check`.
fn backup_is_sound(path: &Path) -> bool {
    let Ok(conn) = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return false;
    };
    conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
        .is_ok_and(|v| v == "ok")
}

/// fsyncs `tmp`, renames it to `path` and fsyncs their directory.
fn persist(tmp: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::File::open(tmp)?.sync_all()?;
    std::fs::rename(tmp, path)?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

fn io_error(e: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(e))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_row(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (42);")
            .unwrap();
        conn
    }

    fn row_of(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT v FROM t", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_backup_is_written_whole_and_kept_when_sound() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sluice.db");
        let conn = db_with_row(&db);
        let path = backup(&conn, &db, 0).unwrap();
        assert_eq!(path, backup_path(&db, 0));
        assert_eq!(row_of(&path), 42);
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.contains(".tmp-")), "{names:?}");
        // A sound backup is kept: the pre-migration state is not overwritten.
        conn.execute("UPDATE t SET v = 7", []).unwrap();
        backup(&conn, &db, 0).unwrap();
        assert_eq!(row_of(&path), 42);
    }

    #[test]
    fn a_damaged_backup_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sluice.db");
        let conn = db_with_row(&db);
        let path = backup_path(&db, 0);
        // A partial copy left by a crash: the SQLite header and garbage.
        let mut junk = b"SQLite format 3\0".to_vec();
        junk.extend(std::iter::repeat_n(0xA5u8, 4096));
        std::fs::write(&path, &junk).unwrap();
        assert!(!backup_is_sound(&path));
        backup(&conn, &db, 0).unwrap();
        assert!(backup_is_sound(&path));
        assert_eq!(row_of(&path), 42);
    }
}
