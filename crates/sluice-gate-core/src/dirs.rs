//! Per-user directories and identifiers shared by the CLI and the daemon.
//!
//! See the "Directories" table in `kb/sluice/design/architecture.md`.

use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::git::EnvMap;
use crate::{Error, Result};

/// `$XDG_STATE_HOME/sluice` or `$HOME/.local/state/sluice`: logs and the database.
#[must_use]
pub fn state_dir(env: &EnvMap) -> PathBuf {
    if let Some(dir) = env.get("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("sluice");
    }
    let home = env
        .get("HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| "/".to_owned(), Clone::clone);
    PathBuf::from(home).join(".local/state/sluice")
}

/// `$XDG_CACHE_HOME/sluice` or `$HOME/.cache/sluice`: short-lived, disposable
/// facts (the forge probe of the merge queue).
#[must_use]
pub fn cache_dir(env: &EnvMap) -> PathBuf {
    if let Some(dir) = env.get("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("sluice");
    }
    let home = env
        .get("HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| "/".to_owned(), Clone::clone);
    PathBuf::from(home).join(".cache/sluice")
}

/// Directory holding `<job-id>.log` files.
#[must_use]
pub fn logs_dir(env: &EnvMap) -> PathBuf {
    state_dir(env).join("logs")
}

/// Path of the SQLite database kept by the daemon.
#[must_use]
pub fn database_path(env: &EnvMap) -> PathBuf {
    state_dir(env).join("sluice.db")
}

/// Path of the daemon's own log file (stdout/stderr when detached).
#[must_use]
pub fn daemon_log_path(env: &EnvMap) -> PathBuf {
    state_dir(env).join("daemon.log")
}

/// Runtime directory (`$XDG_RUNTIME_DIR/sluice` or `/tmp/sluice-<uid>`), created `0700`.
///
/// # Errors
///
/// See [`crate::tree::scratch_root`].
pub fn runtime_dir(env: &EnvMap) -> Result<PathBuf> {
    crate::tree::scratch_root(env)
}

/// The runtime directory path as [`runtime_dir`] would pick it, without
/// creating or touching anything (for diagnostics). `None` when the current
/// uid cannot be determined.
#[must_use]
pub fn runtime_dir_path(env: &EnvMap) -> Option<PathBuf> {
    current_uid().map(|uid| runtime_dir_for(env, uid))
}

/// `$XDG_RUNTIME_DIR/sluice` or `/tmp/sluice-<uid>`.
fn runtime_dir_for(env: &EnvMap, uid: u32) -> PathBuf {
    match env.get("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => Path::new(dir).join("sluice"),
        None => PathBuf::from(format!("/tmp/sluice-{uid}")),
    }
}

/// The effective uid (`/proc/self` is owned by it; `nix` is built without
/// its `user` feature). `None` when `/proc` is unavailable.
#[must_use]
pub fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// Bytes of `sockaddr_un.sun_path` on this platform, including the
/// terminating NUL (108 on Linux).
pub const SUN_PATH_LEN: usize =
    size_of::<nix::libc::sockaddr_un>() - size_of::<nix::libc::sa_family_t>();

/// Longest usable socket path in bytes, without the NUL: [`SUN_PATH_LEN`] - 1.
pub const SOCKET_PATH_MAX: usize = SUN_PATH_LEN - 1;

/// Test-only: base directory of the short socket fallback instead of `/tmp`
/// (the fallback becomes `<base>/sluice-<uid>/daemon.sock`). It exists so
/// the test suite never touches the user's real `/tmp/sluice-<uid>`; it is
/// not a supported setting (use `SLUICE_SOCKET` instead).
pub const TEST_FALLBACK_BASE_ENV: &str = "SLUICE_TEST_SOCKET_FALLBACK_DIR";

/// Where the daemon socket path came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketSource {
    /// `$SLUICE_SOCKET`.
    Explicit,
    /// `<runtime-dir>/daemon.sock`.
    Default,
    /// `/tmp/sluice-<uid>/daemon.sock`, because the default is too long.
    Fallback {
        /// The default path that does not fit in `sun_path`.
        preferred: PathBuf,
    },
}

/// The daemon socket path and why it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketLocation {
    /// The path the daemon and every client use.
    pub path: PathBuf,
    /// Where it came from.
    pub source: SocketSource,
}

impl SocketLocation {
    /// Why the fallback is in use, or `None` for the default path and
    /// `SLUICE_SOCKET`.
    #[must_use]
    pub fn fallback_reason(&self) -> Option<String> {
        match &self.source {
            SocketSource::Fallback { preferred } => Some(format!(
                "{} is {} bytes, longer than the {SOCKET_PATH_MAX}-byte Unix socket limit",
                preferred.display(),
                preferred.as_os_str().len()
            )),
            _ => None,
        }
    }
}

/// True when `path` fits in `sockaddr_un.sun_path` with its NUL.
#[must_use]
pub fn socket_path_fits(path: &Path) -> bool {
    path.as_os_str().len() <= SOCKET_PATH_MAX
}

fn too_long(path: &Path, what: &str) -> Error {
    Error::SocketPath {
        path: path.to_path_buf(),
        problem: format!(
            "{what} is {} bytes, longer than the {SOCKET_PATH_MAX}-byte Unix socket limit; \
             use a shorter path",
            path.as_os_str().len()
        ),
    }
}

/// Picks the socket path without touching the filesystem.
///
/// `SLUICE_SOCKET` wins and must fit (an explicit value never falls back
/// silently). Otherwise `<runtime-dir>/daemon.sock`, or, when that does not
/// fit in `sun_path`, `/tmp/sluice-<uid>/daemon.sock`. The choice depends
/// only on `env` and `uid`, so the daemon and every client agree on it.
///
/// # Errors
///
/// [`Error::SocketPath`] when the chosen path is too long.
pub fn choose_socket(env: &EnvMap, uid: u32) -> Result<SocketLocation> {
    if let Some(path) = env.get("SLUICE_SOCKET").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(path);
        if !socket_path_fits(&path) {
            return Err(too_long(&path, "SLUICE_SOCKET"));
        }
        return Ok(SocketLocation {
            path,
            source: SocketSource::Explicit,
        });
    }
    let preferred = runtime_dir_for(env, uid).join("daemon.sock");
    if socket_path_fits(&preferred) {
        return Ok(SocketLocation {
            path: preferred,
            source: SocketSource::Default,
        });
    }
    let path = fallback_socket_dir(env, uid).join("daemon.sock");
    if !socket_path_fits(&path) {
        return Err(too_long(&path, "the fallback socket path"));
    }
    Ok(SocketLocation {
        path,
        source: SocketSource::Fallback { preferred },
    })
}

/// Directory of the short socket fallback: `/tmp/sluice-<uid>` (or
/// `<base>/sluice-<uid>` under [`TEST_FALLBACK_BASE_ENV`] in tests).
#[must_use]
pub fn fallback_socket_dir(env: &EnvMap, uid: u32) -> PathBuf {
    env.get(TEST_FALLBACK_BASE_ENV)
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
        .join(format!("sluice-{uid}"))
}

/// Why `dir` is not a safe home for the socket, or `None` when it is: it
/// must be a real directory (not a symlink), owned by `uid`, and neither
/// group- nor world-writable. Nothing is created or changed.
#[must_use]
pub fn unsafe_socket_dir(dir: &Path, uid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) => return Some(format!("cannot inspect it: {e}")),
    };
    if meta.file_type().is_symlink() {
        return Some("it is a symlink; remove it so Sluice can create a private directory".into());
    }
    if !meta.is_dir() {
        return Some("it is not a directory; remove it".into());
    }
    if meta.uid() != uid {
        return Some(format!(
            "it is owned by uid {}, not by the current user (uid {uid}); remove it",
            meta.uid()
        ));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o022 != 0 {
        return Some(format!(
            "it is group or world writable (mode {mode:o}); run 'chmod 700 {}'",
            dir.display()
        ));
    }
    None
}

/// Creates the fallback directory `0700` (its last component only) and
/// refuses it when [`unsafe_socket_dir`] objects.
fn ensure_fallback_dir(dir: &Path, uid: u32) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(Error::io("creating the socket fallback directory", dir, e)),
    }
    match unsafe_socket_dir(dir, uid) {
        None => Ok(()),
        Some(problem) => Err(Error::SocketPath {
            path: dir.to_path_buf(),
            problem: format!("refusing the socket fallback directory: {problem}"),
        }),
    }
}

/// The socket location as [`socket_location`] would pick it, without
/// creating anything (for diagnostics).
///
/// # Errors
///
/// [`Error::SocketPath`] when the path is too long; [`Error::Io`] when the
/// uid cannot be read.
pub fn plan_socket(env: &EnvMap) -> Result<SocketLocation> {
    let uid = current_uid().ok_or_else(|| {
        Error::io(
            "reading the current uid",
            "/proc/self",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
    })?;
    choose_socket(env, uid)
}

/// The daemon socket location with its directory ready: the runtime
/// directory for the default path, the checked `0700` fallback directory
/// for the fallback. An explicit `SLUICE_SOCKET` is returned as given.
///
/// # Errors
///
/// [`Error::SocketPath`] when the path is too long or the fallback
/// directory is unsafe; I/O errors creating the directories.
pub fn socket_location(env: &EnvMap) -> Result<SocketLocation> {
    let location = plan_socket(env)?;
    match &location.source {
        SocketSource::Explicit => {}
        SocketSource::Default => {
            runtime_dir(env)?;
        }
        SocketSource::Fallback { .. } => {
            if let (Some(dir), Some(uid)) = (location.path.parent(), current_uid()) {
                ensure_fallback_dir(dir, uid)?;
            }
        }
    }
    Ok(location)
}

/// [`socket_path`] without creating any directory (for diagnostics).
#[must_use]
pub fn socket_path_no_create(env: &EnvMap) -> Option<PathBuf> {
    plan_socket(env).ok().map(|l| l.path)
}

/// Daemon socket path: `$SLUICE_SOCKET`, `<runtime>/daemon.sock`, or the
/// short fallback when that is too long (see [`socket_location`]).
///
/// # Errors
///
/// See [`socket_location`].
pub fn socket_path(env: &EnvMap) -> Result<PathBuf> {
    socket_location(env).map(|l| l.path)
}

/// Lock held by the daemon for its whole life (`<runtime>/daemon.lock`).
///
/// # Errors
///
/// When the runtime directory cannot be created.
pub fn daemon_lock_path(env: &EnvMap) -> Result<PathBuf> {
    Ok(runtime_dir(env)?.join("daemon.lock"))
}

/// Lock taken by clients while autostarting the daemon (`<runtime>/start.lock`).
///
/// # Errors
///
/// When the runtime directory cannot be created.
pub fn start_lock_path(env: &EnvMap) -> Result<PathBuf> {
    Ok(runtime_dir(env)?.join("start.lock"))
}

/// Creates `path` and its missing parents with mode `0700` (private to the
/// user). Existing directories keep their mode.
///
/// # Errors
///
/// The I/O error of the failing `mkdir`.
pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Current time as RFC 3339 with seconds precision.
#[must_use]
pub fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

/// A job id unique on this machine that sorts chronologically:
/// `<unix-ms>-<pid>-<nanos hex>`.
#[must_use]
pub fn job_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}-{}-{:06x}",
        now.as_millis(),
        std::process::id(),
        now.subsec_nanos() & 0xff_ffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_prefers_xdg_state_home() {
        let mut env = EnvMap::new();
        env.insert("HOME".into(), "/home/u".into());
        assert_eq!(
            state_dir(&env),
            PathBuf::from("/home/u/.local/state/sluice")
        );
        env.insert("XDG_STATE_HOME".into(), "/tmp/state".into());
        assert_eq!(state_dir(&env), PathBuf::from("/tmp/state/sluice"));
        assert_eq!(logs_dir(&env), PathBuf::from("/tmp/state/sluice/logs"));
        assert_eq!(
            database_path(&env),
            PathBuf::from("/tmp/state/sluice/sluice.db")
        );
    }

    #[test]
    fn private_dirs_are_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a/b");
        create_private_dir(&dir).unwrap();
        create_private_dir(&dir).unwrap();
        for d in [tmp.path().join("a"), dir] {
            let mode = std::fs::metadata(&d).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", d.display());
        }
    }

    #[test]
    fn socket_path_honours_override() {
        let mut env = EnvMap::new();
        env.insert("SLUICE_SOCKET".into(), "/tmp/x.sock".into());
        assert_eq!(socket_path(&env).unwrap(), PathBuf::from("/tmp/x.sock"));
    }

    fn env_with(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// A runtime dir whose `<dir>/sluice/daemon.sock` is exactly `len` bytes.
    fn runtime_of_socket_len(len: usize) -> String {
        let suffix = "/sluice/daemon.sock".len();
        format!("/r{}", "x".repeat(len - suffix - 2))
    }

    #[test]
    fn the_limit_matches_the_kernel_and_std() {
        assert_eq!(SUN_PATH_LEN, 108);
        let fits = PathBuf::from(format!("/{}", "a".repeat(SOCKET_PATH_MAX - 1)));
        let over = PathBuf::from(format!("/{}", "a".repeat(SOCKET_PATH_MAX)));
        assert!(socket_path_fits(&fits));
        assert!(!socket_path_fits(&over));
        assert!(std::os::unix::net::SocketAddr::from_pathname(&fits).is_ok());
        assert!(std::os::unix::net::SocketAddr::from_pathname(&over).is_err());
    }

    #[test]
    fn the_default_path_is_kept_up_to_the_limit() {
        let dir = runtime_of_socket_len(SOCKET_PATH_MAX);
        let loc = choose_socket(&env_with(&[("XDG_RUNTIME_DIR", &dir)]), 1000).unwrap();
        assert_eq!(loc.source, SocketSource::Default);
        assert_eq!(loc.path.as_os_str().len(), SOCKET_PATH_MAX);
        assert_eq!(loc.fallback_reason(), None);

        let loc = choose_socket(&EnvMap::new(), 1000).unwrap();
        assert_eq!(loc.path, PathBuf::from("/tmp/sluice-1000/daemon.sock"));
        assert_eq!(loc.source, SocketSource::Default);
    }

    #[test]
    fn a_too_long_default_falls_back_deterministically() {
        let dir = runtime_of_socket_len(SOCKET_PATH_MAX + 1);
        let env = env_with(&[("XDG_RUNTIME_DIR", &dir)]);
        let a = choose_socket(&env, 1000).unwrap();
        let b = choose_socket(&env, 1000).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.path, PathBuf::from("/tmp/sluice-1000/daemon.sock"));
        let preferred = PathBuf::from(&dir).join("sluice/daemon.sock");
        assert_eq!(a.source, SocketSource::Fallback { preferred });
        let reason = a.fallback_reason().unwrap();
        assert!(
            reason.contains("108 bytes") && reason.contains("107-byte"),
            "{reason}"
        );

        let env = env_with(&[("XDG_RUNTIME_DIR", &dir), (TEST_FALLBACK_BASE_ENV, "/t")]);
        let c = choose_socket(&env, 7).unwrap();
        assert_eq!(c.path, PathBuf::from("/t/sluice-7/daemon.sock"));
    }

    #[test]
    fn an_explicit_socket_wins_and_is_never_replaced() {
        let dir = runtime_of_socket_len(200);
        let env = env_with(&[("XDG_RUNTIME_DIR", &dir), ("SLUICE_SOCKET", "/s/d.sock")]);
        let loc = choose_socket(&env, 1000).unwrap();
        assert_eq!(loc.path, PathBuf::from("/s/d.sock"));
        assert_eq!(loc.source, SocketSource::Explicit);

        let long = format!("/{}", "s".repeat(SOCKET_PATH_MAX));
        let err = choose_socket(&env_with(&[("SLUICE_SOCKET", &long)]), 1000).unwrap_err();
        let text = err.to_string();
        assert!(
            matches!(err, Error::SocketPath { .. }) && text.contains("SLUICE_SOCKET is 108 bytes"),
            "{text}"
        );
    }

    #[test]
    fn a_too_long_fallback_is_an_error() {
        let dir = runtime_of_socket_len(150);
        let base = format!("/{}", "b".repeat(100));
        let env = env_with(&[("XDG_RUNTIME_DIR", &dir), (TEST_FALLBACK_BASE_ENV, &base)]);
        let err = choose_socket(&env, 1000).unwrap_err();
        assert!(err.to_string().contains("fallback socket path"), "{err}");
    }

    #[test]
    fn the_fallback_dir_is_created_private_and_checked() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let uid = current_uid().unwrap();
        let dir = tmp.path().join(format!("sluice-{uid}"));
        ensure_fallback_dir(&dir, uid).unwrap();
        ensure_fallback_dir(&dir, uid).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert_eq!(unsafe_socket_dir(&dir, uid), None);

        // Another owner (simulated with a different uid).
        let why = unsafe_socket_dir(&dir, uid + 1).unwrap();
        assert!(why.contains("owned by uid"), "{why}");

        // Group or world writable.
        for bad in [0o777, 0o770, 0o702] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(bad)).unwrap();
            let err = ensure_fallback_dir(&dir, uid).unwrap_err().to_string();
            assert!(
                err.contains("group or world writable") && err.contains(&dir.display().to_string()),
                "{err}"
            );
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(unsafe_socket_dir(&dir, uid), None);

        // A symlink, even to a private directory.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let err = ensure_fallback_dir(&link, uid).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");

        // A regular file.
        let file = tmp.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert!(
            unsafe_socket_dir(&file, uid)
                .unwrap()
                .contains("not a directory")
        );
    }

    #[test]
    fn socket_location_creates_the_fallback_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let uid = current_uid().unwrap();
        let runtime = tmp.path().join("x".repeat(120));
        let env = env_with(&[
            ("XDG_RUNTIME_DIR", runtime.to_str().unwrap()),
            (TEST_FALLBACK_BASE_ENV, tmp.path().to_str().unwrap()),
        ]);
        let loc = socket_location(&env).unwrap();
        assert!(matches!(loc.source, SocketSource::Fallback { .. }));
        assert_eq!(
            loc.path,
            tmp.path().join(format!("sluice-{uid}/daemon.sock"))
        );
        assert!(loc.path.parent().unwrap().is_dir());
        assert_eq!(socket_path_no_create(&env), Some(loc.path));
    }

    #[test]
    fn job_ids_are_unique_and_well_formed() {
        let a = job_id();
        let b = job_id();
        assert_ne!(a, b);
        assert_eq!(a.split('-').count(), 3);
        assert!(now_ms() > 1_700_000_000_000);
        assert!(now_rfc3339().ends_with('Z'));
    }
}
