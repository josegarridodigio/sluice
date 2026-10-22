//! Markers and miss records stored under the git common dir.

use std::fs::{self, DirBuilder, File};
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::key::CacheKey;
use crate::tree::TreeHash;
use crate::{CACHE_FORMAT_VERSION, Error, Result};

/// Record of a gate that passed on a given key.
///
/// Unknown fields are tolerated on read so that a field added by a future
/// release is not destructive. The format still forbids writing new fields
/// in 0.3.0 (older binaries treat them as corrupt and delete the marker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marker {
    /// On-disk format version ([`CACHE_FORMAT_VERSION`]).
    pub format: u32,
    /// Cache key (also the file name).
    pub key: CacheKey,
    /// Working-tree hash the gate passed on.
    pub tree: TreeHash,
    /// Gate name.
    pub gate: String,
    /// Normalised command that ran.
    pub cmd: String,
    /// RFC 3339 timestamp (seconds precision, UTC).
    pub passed_at: String,
    /// Wall-clock duration of the run in milliseconds.
    pub duration_ms: u64,
    /// Exit code (always 0 for markers).
    pub exit_code: i32,
    /// Id of the job that produced the marker.
    pub job_id: String,
    /// Version of Sluice that wrote it.
    pub sluice_version: String,
    /// Tree of the commit being created, recorded inside `pre-commit`.
    pub committed_tree: Option<TreeHash>,
}

/// Record of a failed run (or a tree-changing pass), for `status` reporting.
///
/// Unknown fields are tolerated on read so that a field added by a future
/// release is not destructive. The format still forbids writing new fields
/// in 0.3.0 (older binaries treat them as corrupt and delete the marker).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissRecord {
    /// On-disk format version.
    pub format: u32,
    /// Cache key of the failed run.
    pub key: CacheKey,
    /// Gate name.
    pub gate: String,
    /// Exit code, when the process exited normally.
    pub exit_code: Option<i32>,
    /// Why this is a miss.
    pub reason: String,
    /// Id of the job.
    pub job_id: String,
    /// Where the output was logged, if anywhere.
    pub log_path: Option<PathBuf>,
    /// RFC 3339 timestamp.
    pub recorded_at: String,
}

/// Short description of a stored marker.
#[derive(Debug, Clone)]
pub struct MarkerSummary {
    /// Cache key.
    pub key: CacheKey,
    /// Gate name.
    pub gate: String,
    /// Tree hash.
    pub tree: TreeHash,
    /// RFC 3339 timestamp of the pass.
    pub passed_at: String,
    /// Tree of the index when the pass was recorded inside `pre-commit`
    /// with unstaged changes present (see [`Marker::committed_tree`]).
    pub committed_tree: Option<TreeHash>,
    /// Time since the pass.
    pub age: Duration,
}

/// Result of [`MarkerStore::lookup`].
#[derive(Debug, Clone)]
pub enum Lookup {
    /// A valid, fresh marker.
    Hit(Marker),
    /// No usable marker; carries the last miss record, if any.
    Miss(Option<MissRecord>),
}

/// Directory of markers: `<common_dir>/sluice/markers`.
#[derive(Debug, Clone)]
pub struct MarkerStore {
    dir: PathBuf,
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn age_of(time: SystemTime) -> Duration {
    SystemTime::now()
        .duration_since(time)
        .unwrap_or(Duration::ZERO)
}

fn is_miss_name(name: &str) -> bool {
    name.ends_with(".miss.json")
}

impl MarkerStore {
    /// Store for the repository whose git common dir is `common_dir`.
    pub fn new(common_dir: &Path) -> Self {
        Self {
            dir: common_dir.join("sluice").join("markers"),
        }
    }

    /// The marker directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn marker_path(&self, key: &CacheKey) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn miss_path(&self, key: &CacheKey) -> PathBuf {
        self.dir.join(format!("{key}.miss.json"))
    }

    fn ensure_dir(&self) -> Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|e| Error::io("creating marker directory", &self.dir, e))
    }

    fn write_atomic<T: Serialize>(&self, final_path: &Path, value: &T) -> Result<()> {
        self.ensure_dir()?;
        let mut data = serde_json::to_vec_pretty(value).map_err(|source| Error::MarkerFormat {
            path: final_path.to_path_buf(),
            source,
        })?;
        data.push(b'\n');
        let tmp = self.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut f = File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
            fs::rename(&tmp, final_path)
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(Error::io("writing marker", final_path, e));
        }
        Ok(())
    }

    /// Look up a marker for `key`.
    ///
    /// A marker with another format, older than `ttl` (by file mtime and by
    /// `passed_at`, whichever is older), or corrupt is deleted and reported as
    /// a miss. Corruption never fails the caller.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] only for unexpected read failures other than "not found".
    pub fn lookup(&self, key: &CacheKey, ttl: Duration) -> Result<Lookup> {
        let path = self.marker_path(key);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Lookup::Miss(self.read_miss(key).unwrap_or(None)));
            }
            Err(e) => return Err(Error::io("reading marker", path, e)),
        };
        let discard = |path: &Path| {
            let _ = fs::remove_file(path);
            Ok(Lookup::Miss(self.read_miss(key).unwrap_or(None)))
        };
        let Ok(marker) = serde_json::from_slice::<Marker>(&bytes) else {
            return discard(&path);
        };
        if marker.format != CACHE_FORMAT_VERSION || marker.key != *key {
            return discard(&path);
        }
        let mtime_age = fs::metadata(&path)
            .and_then(|m| m.modified())
            .map_or(Duration::ZERO, age_of);
        let passed_age = humantime::parse_rfc3339(&marker.passed_at).map_or(Duration::MAX, age_of);
        if mtime_age.max(passed_age) > ttl {
            return discard(&path);
        }
        Ok(Lookup::Hit(marker))
    }

    /// Atomically write a marker and delete any previous miss record.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] or [`Error::MarkerFormat`] when the write fails.
    pub fn write(&self, marker: &Marker) -> Result<()> {
        self.write_atomic(&self.marker_path(&marker.key), marker)?;
        let _ = fs::remove_file(self.miss_path(&marker.key));
        Ok(())
    }

    /// Atomically write (overwrite) a miss record.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] or [`Error::MarkerFormat`] when the write fails.
    pub fn write_miss(&self, miss: &MissRecord) -> Result<()> {
        self.write_atomic(&self.miss_path(&miss.key), miss)
    }

    /// Read the miss record for `key`, if present and valid.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for read failures other than "not found". Corrupt or
    /// foreign-format records yield `Ok(None)`.
    pub fn read_miss(&self, key: &CacheKey) -> Result<Option<MissRecord>> {
        let path = self.miss_path(key);
        match fs::read(&path) {
            Ok(b) => Ok(serde_json::from_slice::<MissRecord>(&b)
                .ok()
                .filter(|m| m.format == CACHE_FORMAT_VERSION)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io("reading miss record", path, e)),
        }
    }

    /// Files of the store as `(path, file name)`; empty when the dir is missing.
    fn entries(&self) -> Result<Vec<(PathBuf, String)>> {
        let rd = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io("listing markers", &self.dir, e)),
        };
        Ok(rd
            .filter_map(std::result::Result::ok)
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let is_json = Path::new(&name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
                (is_json && !name.starts_with('.')).then(|| (e.path(), name))
            })
            .collect())
    }

    /// All valid markers, newest first. Corrupt files are skipped.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory cannot be read.
    pub fn list(&self) -> Result<Vec<MarkerSummary>> {
        let mut out = Vec::new();
        for (path, name) in self.entries()? {
            if is_miss_name(&name) {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(m) = serde_json::from_slice::<Marker>(&bytes) else {
                continue;
            };
            let age = humantime::parse_rfc3339(&m.passed_at)
                .map(age_of)
                .or_else(|_| fs::metadata(&path).and_then(|md| md.modified()).map(age_of))
                .unwrap_or(Duration::ZERO);
            out.push(MarkerSummary {
                key: m.key,
                gate: m.gate,
                tree: m.tree,
                passed_at: m.passed_at,
                committed_tree: m.committed_tree,
                age,
            });
        }
        out.sort_by_key(|s| s.age);
        Ok(out)
    }

    /// Remove markers and miss records, optionally only those of `gate`.
    /// Returns how many files were removed.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory cannot be read.
    pub fn clear(&self, gate: Option<&str>) -> Result<usize> {
        let mut removed = 0;
        for (path, name) in self.entries()? {
            if let Some(wanted) = gate {
                let Ok(bytes) = fs::read(&path) else { continue };
                let file_gate = if is_miss_name(&name) {
                    serde_json::from_slice::<MissRecord>(&bytes).map(|m| m.gate)
                } else {
                    serde_json::from_slice::<Marker>(&bytes).map(|m| m.gate)
                };
                if file_gate.as_deref().ok() != Some(wanted) {
                    continue;
                }
            }
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Remove markers, miss records and files of a foreign `format` whose
    /// mtime is older than `ttl`. Per-file errors are ignored. Returns how
    /// many were removed.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory cannot be read.
    pub fn prune(&self, ttl: Duration) -> Result<usize> {
        let mut removed = 0;
        for (path, _) in self.entries()? {
            let expired = fs::metadata(&path)
                .and_then(|m| m.modified())
                .is_ok_and(|t| age_of(t) > ttl);
            if expired && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_hours(24);

    fn key(c: char) -> CacheKey {
        CacheKey::try_from(c.to_string().repeat(64)).unwrap()
    }

    fn tree(s: &str) -> TreeHash {
        serde_json::from_str(&format!("\"{s}\"")).unwrap()
    }

    fn ts(t: SystemTime) -> String {
        humantime::format_rfc3339_seconds(t).to_string()
    }

    fn marker(k: char, gate: &str) -> Marker {
        Marker {
            format: CACHE_FORMAT_VERSION,
            key: key(k),
            tree: tree("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            gate: gate.into(),
            cmd: "make test".into(),
            passed_at: ts(SystemTime::now()),
            duration_ms: 12,
            exit_code: 0,
            job_id: "job".into(),
            sluice_version: "0.1.0".into(),
            committed_tree: Some(tree("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")),
        }
    }

    fn miss(k: char, gate: &str) -> MissRecord {
        MissRecord {
            format: CACHE_FORMAT_VERSION,
            key: key(k),
            gate: gate.into(),
            exit_code: Some(1),
            reason: "failed".into(),
            job_id: "job".into(),
            log_path: Some(PathBuf::from("/tmp/log")),
            recorded_at: ts(SystemTime::now()),
        }
    }

    fn store() -> (tempfile::TempDir, MarkerStore) {
        let d = tempfile::tempdir().unwrap();
        let s = MarkerStore::new(d.path());
        (d, s)
    }

    fn set_mtime(path: &Path, age: Duration) {
        let f = File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn round_trip_and_hit() {
        let (_d, s) = store();
        let m = marker('a', "test");
        s.write(&m).unwrap();
        assert!(s.dir().ends_with("sluice/markers"));
        let Lookup::Hit(got) = s.lookup(&key('a'), 7 * DAY).unwrap() else {
            panic!("expected hit");
        };
        assert_eq!(got.gate, "test");
        assert_eq!(
            got.committed_tree.unwrap().as_str(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert_eq!(got.passed_at, m.passed_at);
    }

    #[test]
    fn missing_is_miss_with_record() {
        let (_d, s) = store();
        assert!(matches!(
            s.lookup(&key('a'), DAY).unwrap(),
            Lookup::Miss(None)
        ));
        s.write_miss(&miss('a', "test")).unwrap();
        let Lookup::Miss(Some(r)) = s.lookup(&key('a'), DAY).unwrap() else {
            panic!("expected miss record");
        };
        assert_eq!(r.exit_code, Some(1));
    }

    #[test]
    fn ttl_expiry_by_mtime_deletes() {
        let (_d, s) = store();
        s.write(&marker('a', "test")).unwrap();
        set_mtime(&s.dir().join(format!("{}.json", key('a'))), 3 * DAY);
        assert!(matches!(s.lookup(&key('a'), DAY).unwrap(), Lookup::Miss(_)));
        assert!(!s.dir().join(format!("{}.json", key('a'))).exists());
    }

    #[test]
    fn ttl_expiry_by_passed_at_deletes() {
        let (_d, s) = store();
        let mut m = marker('a', "test");
        m.passed_at = ts(SystemTime::now() - 3 * DAY);
        s.write(&m).unwrap();
        assert!(matches!(s.lookup(&key('a'), DAY).unwrap(), Lookup::Miss(_)));
        assert!(!s.dir().join(format!("{}.json", key('a'))).exists());
    }

    #[test]
    fn format_mismatch_deletes() {
        let (_d, s) = store();
        let mut m = marker('a', "test");
        m.format = CACHE_FORMAT_VERSION + 1;
        s.write(&m).unwrap();
        assert!(matches!(s.lookup(&key('a'), DAY).unwrap(), Lookup::Miss(_)));
        assert!(!s.dir().join(format!("{}.json", key('a'))).exists());
    }

    #[test]
    fn corrupt_json_deletes_and_misses() {
        let (_d, s) = store();
        s.write(&marker('b', "test")).unwrap();
        let p = s.dir().join(format!("{}.json", key('a')));
        fs::write(&p, "{not json").unwrap();
        assert!(matches!(
            s.lookup(&key('a'), DAY).unwrap(),
            Lookup::Miss(None)
        ));
        assert!(!p.exists());
    }

    #[test]
    fn write_deletes_previous_miss() {
        let (_d, s) = store();
        s.write_miss(&miss('a', "test")).unwrap();
        assert!(s.read_miss(&key('a')).unwrap().is_some());
        s.write(&marker('a', "test")).unwrap();
        assert!(s.read_miss(&key('a')).unwrap().is_none());
    }

    #[test]
    fn clear_all_and_by_gate() {
        let (_d, s) = store();
        s.write(&marker('a', "test")).unwrap();
        s.write(&marker('b', "lint")).unwrap();
        s.write_miss(&miss('c', "test")).unwrap();
        s.write_miss(&miss('d', "lint")).unwrap();
        assert_eq!(s.clear(Some("test")).unwrap(), 2);
        assert_eq!(s.list().unwrap().len(), 1);
        assert!(s.read_miss(&key('d')).unwrap().is_some());
        assert_eq!(s.clear(None).unwrap(), 2);
        assert!(s.list().unwrap().is_empty());
        assert_eq!(s.clear(None).unwrap(), 0);
    }

    #[test]
    fn list_is_newest_first_and_skips_corrupt() {
        let (_d, s) = store();
        let mut old = marker('a', "old");
        old.passed_at = ts(SystemTime::now() - 2 * DAY);
        let mut mid = marker('b', "mid");
        mid.passed_at = ts(SystemTime::now() - DAY);
        s.write(&old).unwrap();
        s.write(&marker('c', "new")).unwrap();
        s.write(&mid).unwrap();
        s.write_miss(&miss('d', "x")).unwrap();
        fs::write(s.dir().join(format!("{}.json", key('e'))), "garbage").unwrap();
        let gates: Vec<_> = s.list().unwrap().into_iter().map(|m| m.gate).collect();
        assert_eq!(gates, ["new", "mid", "old"]);
    }

    #[test]
    fn prune_removes_old_markers_only() {
        let (_d, s) = store();
        s.write(&marker('a', "old")).unwrap();
        s.write(&marker('b', "new")).unwrap();
        s.write_miss(&miss('c', "x")).unwrap();
        set_mtime(&s.dir().join(format!("{}.json", key('a'))), 3 * DAY);
        assert_eq!(s.prune(DAY).unwrap(), 1);
        assert_eq!(s.list().unwrap().len(), 1);
        assert!(s.read_miss(&key('c')).unwrap().is_some());
    }

    #[test]
    fn missing_dir_is_fine() {
        let (_d, s) = store();
        assert!(s.list().unwrap().is_empty());
        assert_eq!(s.prune(DAY).unwrap(), 0);
        assert_eq!(s.clear(None).unwrap(), 0);
    }
}
