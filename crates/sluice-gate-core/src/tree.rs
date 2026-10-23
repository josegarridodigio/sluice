//! Working-tree snapshot from a private index and a throwaway object dir.
//!
//! See `kb/sluice/design/cache-key.md`, section "Tree hash": the real index,
//! `index.lock` and `.git/objects` are never written.

use std::fmt;
use std::fs::{self, DirBuilder, File, FileTimes};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};
use crate::git::{self, EnvMap, RepoPaths};

/// Prefix of every scratch snapshot directory.
const SCRATCH_PREFIX: &str = "snap-";
/// Scratch directories older than this are left over by killed runs.
const STALE_AFTER: Duration = Duration::from_hours(1);
/// Index mode of a gitlink (submodule or embedded repository).
const GITLINK_MODE: &str = "160000";
/// Forced off: split index would write a shared-index file into the real git dir.
const NO_SPLIT: [&str; 2] = ["-c", "core.splitIndex=false"];

/// A git tree id: 40 (SHA-1) or 64 (SHA-256) lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TreeHash(String);

/// Error returned when a string is not a valid [`TreeHash`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tree hash {0:?}: expected 40 or 64 lowercase hex characters")]
pub struct InvalidTreeHash(pub String);

impl TreeHash {
    /// Returns the hex id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TreeHash {
    type Error = InvalidTreeHash;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        let valid = matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(value))
        } else {
            Err(InvalidTreeHash(value))
        }
    }
}

impl From<TreeHash> for String {
    fn from(value: TreeHash) -> Self {
        value.0
    }
}

impl fmt::Display for TreeHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The tree a gate is about to test, or why no tree describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    /// The content is exactly this tree.
    Known(TreeHash),
    /// No tree can be trusted; the gate runs uncached.
    Unknown {
        /// Human-readable reason, printed to the user.
        reason: String,
    },
}

impl Snapshot {
    /// Returns the tree when known.
    #[must_use]
    pub fn tree(&self) -> Option<&TreeHash> {
        match self {
            Self::Known(tree) => Some(tree),
            Self::Unknown { .. } => None,
        }
    }

    fn unknown(reason: impl Into<String>) -> Self {
        Self::Unknown {
            reason: reason.into(),
        }
    }
}

/// The staged entries of a snapshot: the raw `ls-files -s -z` output of the
/// private index after `add -A` (`<mode> SP <oid> SP <stage> TAB <path> NUL`
/// per record), parsed on iteration. Path scoping filters and hashes these
/// in process (see `kb/sluice/design/path-scoped-keys.md`, section 4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagedEntries {
    raw: Vec<u8>,
}

/// One record of [`StagedEntries`], borrowing from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagedEntry<'a> {
    /// Octal mode as written by git (`100644`, `100755`, `120000`, ...).
    pub mode: &'a [u8],
    /// Object id as hex text (40 or 64 characters).
    pub oid: &'a [u8],
    /// Root-relative path, raw bytes.
    pub path: &'a [u8],
}

impl StagedEntries {
    /// Wraps raw `ls-files -s -z` output.
    #[must_use]
    pub fn from_raw(raw: Vec<u8>) -> Self {
        Self { raw }
    }

    /// The records in output order (index order: byte order of the paths).
    /// Malformed records are skipped.
    pub fn iter(&self) -> impl Iterator<Item = StagedEntry<'_>> {
        self.raw
            .split(|b| *b == 0)
            .filter(|r| !r.is_empty())
            .filter_map(parse_stage_record)
    }

    /// Number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// `true` when there is no record.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

/// Parses `<mode> SP <oid> SP <stage> TAB <path>`.
fn parse_stage_record(record: &[u8]) -> Option<StagedEntry<'_>> {
    let tab = record.iter().position(|b| *b == b'\t')?;
    let (meta, path) = (&record[..tab], &record[tab + 1..]);
    let mut fields = meta.split(|b| *b == b' ');
    let mode = fields.next()?;
    let oid = fields.next()?;
    Some(StagedEntry { mode, oid, path })
}

/// [`Snapshot`] plus the staged entries the tree was written from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotFull {
    /// The content is exactly this tree, made of these entries.
    Known {
        /// Root tree of the working copy.
        root: TreeHash,
        /// Staged entries of the private index.
        entries: StagedEntries,
    },
    /// No tree can be trusted; the gate runs uncached.
    Unknown {
        /// Human-readable reason, printed to the user.
        reason: String,
    },
}

impl SnapshotFull {
    /// Returns the root tree when known.
    #[must_use]
    pub fn tree(&self) -> Option<&TreeHash> {
        match self {
            Self::Known { root, .. } => Some(root),
            Self::Unknown { .. } => None,
        }
    }
}

impl From<SnapshotFull> for Snapshot {
    fn from(value: SnapshotFull) -> Self {
        match value {
            SnapshotFull::Known { root, .. } => Self::Known(root),
            SnapshotFull::Unknown { reason } => Self::Unknown { reason },
        }
    }
}

/// Converts the result of [`git_failure`] (always `Unknown`).
fn failure_full(s: Snapshot) -> SnapshotFull {
    SnapshotFull::Unknown {
        reason: match s {
            Snapshot::Unknown { reason } => reason,
            Snapshot::Known(t) => format!("unexpected tree {t} from a git failure"),
        },
    }
}

/// Returns the directory that holds scratch snapshots, creating it with mode
/// `0700`: `$XDG_RUNTIME_DIR/sluice/` when set, else `/tmp/sluice-<uid>/`.
///
/// Stale `snap-*` directories older than one hour are removed on every call
/// (errors ignored).
///
/// # Errors
///
/// [`Error::Io`] when the directory cannot be created, or when it exists but
/// is not a directory owned by the current user.
pub fn scratch_root(env: &EnvMap) -> Result<PathBuf> {
    // `nix` is built without its `user` feature; on Linux `/proc/self` is
    // owned by the effective uid of the process.
    let uid = fs::metadata("/proc/self")
        .map_err(|e| Error::io("reading the current uid", "/proc/self", e))?
        .uid();
    let root = match env.get("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => Path::new(dir).join("sluice"),
        None => PathBuf::from(format!("/tmp/sluice-{uid}")),
    };
    match DirBuilder::new().mode(0o700).create(&root) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(&root)
                .map_err(|e| Error::io("creating scratch root", &root, e))?;
        }
        Err(e) => return Err(Error::io("creating scratch root", &root, e)),
    }
    let meta =
        fs::symlink_metadata(&root).map_err(|e| Error::io("inspecting scratch root", &root, e))?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(Error::io(
            "scratch root is not a directory owned by the current user",
            &root,
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        ));
    }
    if meta.mode() & 0o777 != 0o700 {
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::io("restricting scratch root", &root, e))?;
    }
    sweep_stale(&root);
    Ok(root)
}

/// Removes `snap-*` directories under `root` older than [`STALE_AFTER`].
fn sweep_stale(root: &Path) {
    let Some(cutoff) = SystemTime::now().checked_sub(STALE_AFTER) else {
        return;
    };
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(SCRATCH_PREFIX)
        {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|m| m < cutoff);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// A private directory removed on drop, however the snapshot ends.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn create(root: &Path) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        loop {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let path = root.join(format!("{SCRATCH_PREFIX}{pid}-{nanos:x}-{n}"));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(Error::io("creating scratch dir", &path, e)),
            }
        }
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A private copy of the index plus a fresh object dir, and the env using them.
struct PrivateIndex {
    dir: ScratchDir,
    env: EnvMap,
}

impl PrivateIndex {
    /// Copies the index (preserving mtime) and builds the private env.
    fn new(paths: &RepoPaths, env: &EnvMap, scratch_root: &Path) -> Result<Self> {
        let dir = ScratchDir::create(scratch_root)?;
        let index = dir.0.join("index");
        let objects = dir.0.join("objects");
        copy_preserving_mtime(&paths.index_file, &index)?;
        fs::create_dir(&objects).map_err(|e| Error::io("creating scratch objects", &objects, e))?;
        let mut alternates = paths.objects_dir.to_string_lossy().into_owned();
        if let Some(inherited) = env
            .get("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .filter(|v| !v.is_empty())
        {
            alternates.push(':');
            alternates.push_str(inherited);
        }
        let mut private = env.clone();
        private.insert(
            "GIT_INDEX_FILE".into(),
            index.to_string_lossy().into_owned(),
        );
        private.insert(
            "GIT_OBJECT_DIRECTORY".into(),
            objects.to_string_lossy().into_owned(),
        );
        private.insert("GIT_ALTERNATE_OBJECT_DIRECTORIES".into(), alternates);
        // Guard against a future change that would stage into the real index.
        for key in ["GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY"] {
            assert!(
                Path::new(&private[key]).starts_with(&dir.0),
                "{key} escapes the scratch dir"
            );
        }
        Ok(Self { dir, env: private })
    }

    fn git(&self, args: &[&str], paths: &RepoPaths, read_only: bool) -> Result<Vec<u8>> {
        git::run_raw(args, &paths.toplevel, &self.env, read_only)
    }

    /// `write-tree` on the private index.
    fn write_tree(&self, paths: &RepoPaths) -> Result<Snapshot> {
        let args = [NO_SPLIT[0], NO_SPLIT[1], "write-tree"];
        match self.git(&args, paths, false) {
            Ok(out) => {
                let id = String::from_utf8_lossy(&out).trim().to_owned();
                Ok(TreeHash::try_from(id)
                    .map_or_else(|e| Snapshot::unknown(e.to_string()), Snapshot::Known))
            }
            Err(e) => git_failure("could not write tree", e),
        }
    }
}

/// Copies `from` to `to` keeping its mtime (git's racily-clean detection
/// compares entry mtimes with the index mtime). A missing source means an
/// empty index, so nothing is copied.
fn copy_preserving_mtime(from: &Path, to: &Path) -> Result<()> {
    let meta = match fs::metadata(from) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io("reading index metadata", from, e)),
    };
    fs::copy(from, to).map_err(|e| Error::io("copying index", from, e))?;
    let mut times = FileTimes::new();
    if let Ok(m) = meta.modified() {
        times = times.set_modified(m);
    }
    if let Ok(a) = meta.accessed() {
        times = times.set_accessed(a);
    }
    File::options()
        .write(true)
        .open(to)
        .and_then(|f| f.set_times(times))
        .map_err(|e| Error::io("preserving index mtime", to, e))
}

/// Turns a git failure into `Unknown`; a missing git binary stays an error.
fn git_failure(context: &str, err: Error) -> Result<Snapshot> {
    match err {
        Error::GitCommand { stderr, .. } => {
            let detail = stderr.lines().next().unwrap_or_default();
            Ok(Snapshot::unknown(if detail.is_empty() {
                context.to_owned()
            } else {
                format!("{context}: {detail}")
            }))
        }
        other => Err(other),
    }
}

/// Inspects the index git handed us: unmerged, assume-unchanged and
/// skip-worktree entries make the tree unknown. One `ls-files -v` spawn
/// covers all three (tag `M` is unmerged, lowercase is assume-unchanged,
/// `S` is skip-worktree).
fn hidden_entry_reason(paths: &RepoPaths, env: &EnvMap) -> Result<Option<String>> {
    let out = match git::run_raw(&["ls-files", "-v", "-z"], &paths.toplevel, env, true) {
        Ok(out) => out,
        Err(e) => {
            return git_failure("could not list the index", e).map(|s| match s {
                Snapshot::Unknown { reason } => Some(reason),
                Snapshot::Known(_) => None,
            });
        }
    };
    for record in out.split(|b| *b == 0) {
        let Some(space) = record.iter().position(|b| *b == b' ') else {
            continue;
        };
        let tag = &record[..space];
        let path = String::from_utf8_lossy(&record[space + 1..]);
        if tag.eq_ignore_ascii_case(b"M") {
            return Ok(Some(format!("{UNMERGED_REASON}: {path}")));
        }
        if tag == b"S" || tag.iter().any(u8::is_ascii_lowercase) {
            return Ok(Some(format!("{HIDDEN_REASON}: {path}")));
        }
    }
    Ok(None)
}

/// Prefix of the reason for an unmerged index.
const UNMERGED_REASON: &str = "unmerged index";
/// Prefix of the reason for skip-worktree or assume-unchanged entries.
const HIDDEN_REASON: &str = "skip-worktree or assume-unchanged entry";
/// Prefix of the reason for submodules.
const SUBMODULE_REASON: &str = "submodules are not supported yet";

/// Short code of an `Unknown` reason, for `client.log` (`reason=<code>`).
#[must_use]
pub fn unknown_code(reason: &str) -> &'static str {
    if reason.starts_with(HIDDEN_REASON) {
        "skip-worktree"
    } else if reason.starts_with(UNMERGED_REASON) {
        "unmerged"
    } else if reason.starts_with(SUBMODULE_REASON) {
        "submodules"
    } else {
        "git-error"
    }
}

/// Returns the first gitlink path in `ls-files -s -z` output.
fn first_gitlink(stage_output: &[u8]) -> Option<String> {
    stage_output.split(|b| *b == 0).find_map(|record| {
        let tab = record.iter().position(|b| *b == b'\t')?;
        record[..tab]
            .starts_with(GITLINK_MODE.as_bytes())
            .then(|| String::from_utf8_lossy(&record[tab + 1..]).into_owned())
    })
}

/// Working-tree snapshot: the tree `git add -A && git write-tree` would
/// produce, computed on a private copy of the index with a throwaway object
/// dir. `env` is the probe env from [`git::probe_env`].
///
/// Unmerged, assume-unchanged or skip-worktree entries, gitlinks, and any
/// git failure yield [`Snapshot::Unknown`]. The scratch dir is always removed.
///
/// # Errors
///
/// [`Error::GitMissing`] when git cannot be spawned, [`Error::Io`] when the
/// scratch dir or the index copy cannot be created.
pub fn snapshot(paths: &RepoPaths, env: &EnvMap, scratch_root: &Path) -> Result<Snapshot> {
    snapshot_entries(paths, env, scratch_root).map(Snapshot::from)
}

/// [`snapshot`] that also returns the staged entries (the `ls-files -s -z`
/// output it already reads to reject gitlinks), so path-scoped keys cost no
/// extra git spawn.
///
/// # Errors
///
/// See [`snapshot`].
pub fn snapshot_entries(
    paths: &RepoPaths,
    env: &EnvMap,
    scratch_root: &Path,
) -> Result<SnapshotFull> {
    if let Some(reason) = hidden_entry_reason(paths, env)? {
        return Ok(SnapshotFull::Unknown { reason });
    }
    let private = PrivateIndex::new(paths, env, scratch_root)?;
    if let Err(e) = private.git(&[NO_SPLIT[0], NO_SPLIT[1], "add", "-A"], paths, false) {
        return git_failure("could not stage working copy", e).map(failure_full);
    }
    let staged = match private.git(&["ls-files", "-s", "-z"], paths, true) {
        Ok(out) => {
            if let Some(path) = first_gitlink(&out) {
                return Ok(SnapshotFull::Unknown {
                    reason: format!("{SUBMODULE_REASON}: {path}"),
                });
            }
            out
        }
        Err(e) => return git_failure("could not list the staged index", e).map(failure_full),
    };
    let result = private.write_tree(paths);
    drop(private.dir);
    Ok(match result? {
        Snapshot::Known(root) => SnapshotFull::Known {
            root,
            entries: StagedEntries::from_raw(staged),
        },
        Snapshot::Unknown { reason } => SnapshotFull::Unknown { reason },
    })
}

/// Tree of the index as git handed it to us, without `add -A`: the content
/// being committed inside `pre-commit`. Uses the same private-copy technique,
/// so `write-tree` never touches the real index or object dir.
///
/// # Errors
///
/// [`Error::GitMissing`] when git cannot be spawned, [`Error::Io`] when the
/// scratch dir or the index copy cannot be created.
pub fn index_tree(paths: &RepoPaths, env: &EnvMap, scratch_root: &Path) -> Result<Snapshot> {
    let private = PrivateIndex::new(paths, env, scratch_root)?;
    private.write_tree(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{base_env, git, git_with, init_repo};
    use crate::git::{discover, probe_env};
    use std::collections::BTreeSet;
    use std::time::Instant;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        scratch: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("repo");
        let scratch = base.join("scratch");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&scratch).unwrap();
        init_repo(&root);
        Fixture {
            _tmp: tmp,
            root,
            scratch,
        }
    }

    fn snap_with(cwd: &Path, env: &EnvMap, scratch: &Path) -> Snapshot {
        let paths = discover(cwd, env).unwrap();
        let probe = probe_env(env, Some(&paths));
        let result = snapshot(&paths, &probe, scratch).unwrap();
        assert_eq!(fs::read_dir(scratch).unwrap().count(), 0, "scratch leaked");
        result
    }

    fn snap(cwd: &Path, scratch: &Path) -> TreeHash {
        match snap_with(cwd, &base_env(), scratch) {
            Snapshot::Known(t) => t,
            Snapshot::Unknown { reason } => panic!("unknown: {reason}"),
        }
    }

    fn object_files(dir: &Path) -> BTreeSet<PathBuf> {
        let mut out = BTreeSet::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    out.insert(e.path());
                }
            }
        }
        out
    }

    #[test]
    fn tree_hash_validation_and_serde() {
        let sha1 = "a".repeat(40);
        assert!(TreeHash::try_from(sha1.clone()).is_ok());
        assert!(TreeHash::try_from("b".repeat(64)).is_ok());
        assert!(TreeHash::try_from("A".repeat(40)).is_err());
        assert!(TreeHash::try_from("g".repeat(40)).is_err());
        assert!(TreeHash::try_from("a".repeat(41)).is_err());
        let t = TreeHash::try_from(sha1.clone()).unwrap();
        assert_eq!(serde_json::to_string(&t).unwrap(), format!("\"{sha1}\""));
        let back: TreeHash = serde_json::from_str(&format!("\"{sha1}\"")).unwrap();
        assert_eq!(back, t);
        assert!(serde_json::from_str::<TreeHash>("\"zz\"").is_err());
        assert_eq!(t.to_string(), sha1);
    }

    #[test]
    fn clean_tree_equals_head_tree() {
        let f = fixture();
        let start = Instant::now();
        let tree = snap(&f.root, &f.scratch);
        eprintln!("snapshot() wall time: {:?}", start.elapsed());
        assert_eq!(tree.as_str(), git(&f.root, &["rev-parse", "HEAD^{tree}"]));
        assert_eq!(tree.as_str(), git(&f.root, &["write-tree"]));
    }

    #[test]
    fn working_tree_changes_move_the_hash() {
        let f = fixture();
        let clean = snap(&f.root, &f.scratch);

        fs::write(f.root.join("a.txt"), "two\n").unwrap();
        let modified = snap(&f.root, &f.scratch);
        assert_ne!(modified, clean);
        fs::write(f.root.join("a.txt"), "one\n").unwrap();
        assert_eq!(snap(&f.root, &f.scratch), clean);

        fs::write(f.root.join("new.txt"), "n\n").unwrap();
        let untracked = snap(&f.root, &f.scratch);
        assert_ne!(untracked, clean);
        fs::remove_file(f.root.join("new.txt")).unwrap();

        fs::write(f.root.join(".gitignore"), "*.log\n").unwrap();
        let with_ignore = snap(&f.root, &f.scratch);
        fs::write(f.root.join("debug.log"), "x\n").unwrap();
        assert_eq!(snap(&f.root, &f.scratch), with_ignore);
        fs::remove_file(f.root.join(".gitignore")).unwrap();
        fs::remove_file(f.root.join("debug.log")).unwrap();

        fs::remove_file(f.root.join("a.txt")).unwrap();
        assert_ne!(snap(&f.root, &f.scratch), clean);
        fs::write(f.root.join("a.txt"), "one\n").unwrap();
        assert_eq!(snap(&f.root, &f.scratch), clean);

        fs::set_permissions(f.root.join("a.txt"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(snap(&f.root, &f.scratch), clean);
    }

    #[test]
    fn snapshot_entries_are_root_relative_staged_records() {
        let f = fixture();
        let sub = f.root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("inner.txt"), "i\n").unwrap();
        fs::write(f.root.join("x y.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(f.root.join("x y.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let paths = discover(&sub, &base_env()).unwrap();
        let probe = probe_env(&base_env(), Some(&paths));
        let SnapshotFull::Known { root, entries } =
            snapshot_entries(&paths, &probe, &f.scratch).unwrap()
        else {
            panic!("unknown");
        };
        assert_eq!(root, snap(&f.root, &f.scratch));
        let got: Vec<(String, String)> = entries
            .iter()
            .map(|e| {
                (
                    String::from_utf8_lossy(e.mode).into_owned(),
                    String::from_utf8_lossy(e.path).into_owned(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("100644".to_owned(), "a.txt".to_owned()),
                ("100644".into(), "sub/inner.txt".into()),
                ("100755".into(), "x y.sh".into()),
            ]
        );
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.oid.len() == 40));
        assert!(StagedEntries::default().is_empty());
    }

    #[test]
    fn worktrees_with_same_content_share_the_hash() {
        let f = fixture();
        let wt = f.root.parent().unwrap().join("wt2");
        git(
            &f.root,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"],
        );
        let main = snap(&f.root, &f.scratch);
        assert_eq!(snap(&wt, &f.scratch), main);
        fs::write(wt.join("a.txt"), "changed\n").unwrap();
        assert_ne!(snap(&wt, &f.scratch), main);
    }

    #[test]
    fn real_index_and_objects_are_untouched() {
        let f = fixture();
        let git_dir = f.root.join(".git");
        let index = git_dir.join("index");
        let bytes = fs::read(&index).unwrap();
        let mtime = fs::metadata(&index).unwrap().modified().unwrap();
        let objects = object_files(&git_dir.join("objects"));
        fs::write(f.root.join("brand-new.txt"), "never seen before\n").unwrap();

        let tree = snap(&f.root, &f.scratch);
        assert_ne!(tree.as_str(), git(&f.root, &["rev-parse", "HEAD^{tree}"]));

        assert_eq!(fs::read(&index).unwrap(), bytes);
        assert_eq!(fs::metadata(&index).unwrap().modified().unwrap(), mtime);
        assert!(!git_dir.join("index.lock").exists());
        assert_eq!(object_files(&git_dir.join("objects")), objects);
    }

    #[test]
    fn index_tree_follows_inherited_index_file() {
        let f = fixture();
        let alt = f.root.join(".git/alt-index");
        fs::copy(f.root.join(".git/index"), &alt).unwrap();
        fs::write(f.root.join("staged.txt"), "s\n").unwrap();
        let mut env = base_env();
        env.insert("GIT_INDEX_FILE".into(), alt.display().to_string());
        git_with(&f.root, &env, &["add", "staged.txt"]);
        let expected = git_with(&f.root, &env, &["write-tree"]);

        let paths = discover(&f.root, &env).unwrap();
        assert_eq!(paths.index_file, alt);
        let probe = probe_env(&env, Some(&paths));
        assert_eq!(
            index_tree(&paths, &probe, &f.scratch).unwrap(),
            Snapshot::Known(TreeHash::try_from(expected).unwrap())
        );
        // The real index does not have the file.
        let plain = discover(&f.root, &base_env()).unwrap();
        let head = git(&f.root, &["rev-parse", "HEAD^{tree}"]);
        assert_eq!(
            index_tree(&plain, &base_env(), &f.scratch).unwrap(),
            Snapshot::Known(TreeHash::try_from(head).unwrap())
        );
    }

    #[test]
    fn hook_git_dir_from_subdirectory() {
        let f = fixture();
        let sub = f.root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("inner.txt"), "i\n").unwrap();
        fs::write(f.root.join("a.txt"), "edited\n").unwrap();
        let expected = snap(&f.root, &f.scratch);

        let mut env = base_env();
        env.insert("GIT_DIR".into(), f.root.join(".git").display().to_string());
        let paths = discover(&sub, &env).unwrap();
        assert_eq!(paths.toplevel, f.root);
        assert_eq!(
            snap_with(&sub, &env, &f.scratch),
            Snapshot::Known(expected.clone())
        );

        env.insert("GIT_PREFIX".into(), "sub/".into());
        assert_eq!(
            snap_with(&sub, &env, &f.scratch),
            Snapshot::Known(expected.clone())
        );

        // Relative GIT_DIR as git exports it to hooks run from the toplevel.
        let mut rel = base_env();
        rel.insert("GIT_DIR".into(), ".git".into());
        assert_eq!(
            snap_with(&f.root, &rel, &f.scratch),
            Snapshot::Known(expected)
        );
    }

    #[test]
    fn hook_git_dir_of_linked_worktree() {
        let f = fixture();
        let wt = f.root.parent().unwrap().join("wt2");
        git(
            &f.root,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"],
        );
        let sub = wt.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("x.txt"), "x\n").unwrap();
        let expected = snap(&wt, &f.scratch);

        let mut env = base_env();
        env.insert(
            "GIT_DIR".into(),
            f.root.join(".git/worktrees/wt2").display().to_string(),
        );
        let paths = discover(&sub, &env).unwrap();
        assert_eq!(paths.toplevel, wt);
        assert_eq!(paths.common_dir, f.root.join(".git"));
        assert_eq!(paths.objects_dir, f.root.join(".git/objects"));
        assert_eq!(snap_with(&sub, &env, &f.scratch), Snapshot::Known(expected));
    }

    #[test]
    fn unmerged_index_is_unknown() {
        let f = fixture();
        git(&f.root, &["checkout", "-q", "-b", "side"]);
        fs::write(f.root.join("a.txt"), "side\n").unwrap();
        git(&f.root, &["commit", "-q", "-am", "side"]);
        git(&f.root, &["checkout", "-q", "main"]);
        fs::write(f.root.join("a.txt"), "main\n").unwrap();
        git(&f.root, &["commit", "-q", "-am", "main"]);
        let merge = std::process::Command::new("git")
            .args(["merge", "-q", "side"])
            .current_dir(&f.root)
            .env_clear()
            .envs(base_env())
            .output()
            .unwrap();
        assert!(!merge.status.success());
        match snap_with(&f.root, &base_env(), &f.scratch) {
            Snapshot::Unknown { reason } => assert!(reason.contains("unmerged"), "{reason}"),
            Snapshot::Known(t) => panic!("expected unknown, got {t}"),
        }
    }

    #[test]
    fn assume_unchanged_and_gitlinks_are_unknown() {
        let f = fixture();
        git(&f.root, &["update-index", "--assume-unchanged", "a.txt"]);
        assert!(matches!(
            snap_with(&f.root, &base_env(), &f.scratch),
            Snapshot::Unknown { .. }
        ));
        git(&f.root, &["update-index", "--no-assume-unchanged", "a.txt"]);

        let nested = f.root.join("nested");
        fs::create_dir(&nested).unwrap();
        init_repo(&nested);
        match snap_with(&f.root, &base_env(), &f.scratch) {
            Snapshot::Unknown { reason } => assert!(reason.contains("submodules"), "{reason}"),
            Snapshot::Known(t) => panic!("expected unknown, got {t}"),
        }
    }

    #[test]
    fn scratch_root_is_private_and_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = base_env();
        env.insert("XDG_RUNTIME_DIR".into(), tmp.path().display().to_string());
        let root = scratch_root(&env).unwrap();
        assert_eq!(root, tmp.path().join("sluice"));
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let old = root.join("snap-old");
        let fresh = root.join("snap-fresh");
        let other = root.join("keep-me");
        for d in [&old, &fresh, &other] {
            fs::create_dir(d).unwrap();
        }
        let past = SystemTime::now() - Duration::from_hours(2);
        for d in [&old, &other] {
            File::open(d)
                .unwrap()
                .set_times(FileTimes::new().set_modified(past))
                .unwrap();
        }
        scratch_root(&env).unwrap();
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(other.exists());
    }
}
