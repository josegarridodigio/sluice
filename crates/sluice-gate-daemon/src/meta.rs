//! The `meta` key-value table: which release last opened the database.
//!
//! The table is additive (`CREATE TABLE IF NOT EXISTS`), so releases that do
//! not know it ignore it. It exists to word the downgrade refusal: a binary
//! that meets a newer schema can say which release wrote the database.

use rusqlite::{Connection, OptionalExtension};

/// Key holding the version of the Sluice release that last opened the database.
const WRITER_KEY: &str = "writer_version";

/// Records `sluice_gate_core::SLUICE_VERSION` as the writer of the database.
/// Called after a successful open, never on a refused database.
pub(crate) fn record_writer(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (WRITER_KEY, sluice_gate_core::SLUICE_VERSION),
    )?;
    Ok(())
}

/// Describes the release that wrote the database, for error messages. Never
/// fails (read only): a database without the row predates the `meta` table.
pub(crate) fn describe_writer(conn: &Connection) -> String {
    let version: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [WRITER_KEY], |r| {
            r.get(0)
        })
        .optional()
        .unwrap_or(None);
    match version {
        Some(v) => format!("sluice {v}"),
        None => "sluice 0.2.0 or older".to_owned(),
    }
}
