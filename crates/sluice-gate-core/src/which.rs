//! `PATH` resolution of a command name, skipping Sluice's own shims.
//!
//! Used by `sluice exec` (and later by the shell shims) to find the real
//! binary behind a command name. Design: `kb/sluice/design/command-control.md`
//! sections 2.1 and 5.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::EnvMap;

/// First line after the shebang of every shim Sluice writes.
pub const SHIM_MARKER: &str = "# sluice-shim v1";

/// Environment variable naming the shim directory (set by the shims).
pub const SHIM_DIR_VAR: &str = "SLUICE_SHIM_DIR";

/// Why a command cannot be run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotRunnable {
    /// Nothing with that name (exit 127).
    #[error("command not found")]
    NotFound,
    /// Found, but not an executable regular file (exit 126).
    #[error("permission denied")]
    NotExecutable(PathBuf),
}

impl NotRunnable {
    /// The shell's exit code for this failure: 127 or 126.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::NotFound => 127,
            Self::NotExecutable(_) => 126,
        }
    }
}

/// Default shim directory: `$XDG_DATA_HOME/sluice/shims` or
/// `$HOME/.local/share/sluice/shims`.
pub fn default_shim_dir(env: &EnvMap) -> Option<PathBuf> {
    if let Some(d) = env.get("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(d).join("sluice/shims"));
    }
    env.get("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".local/share/sluice/shims"))
}

/// `Some(true)` for an executable regular file, `Some(false)` for anything
/// else that exists, `None` when nothing exists there.
fn is_executable_file(path: &Path) -> Option<bool> {
    let meta = std::fs::metadata(path).ok()?;
    Some(meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// True when the first 64 bytes of `path` contain [`SHIM_MARKER`].
fn is_shim(path: &Path) -> bool {
    let mut head = [0u8; 64];
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut n = 0;
    while n < head.len() {
        match file.read(&mut head[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    head[..n]
        .windows(SHIM_MARKER.len())
        .any(|w| w == SHIM_MARKER.as_bytes())
}

/// Resolves `name` like a POSIX shell would.
///
/// A name containing `/` is taken relative to `cwd`. Otherwise every entry
/// of `env["PATH"]` is tried in order (an empty entry means `cwd`). With
/// `skip_shims`, the shim directory (`SLUICE_SHIM_DIR`, else the default
/// one) is skipped, as is every candidate carrying [`SHIM_MARKER`] or that
/// is the running `sluice` binary itself, so a shim can never resolve to
/// itself.
///
/// # Errors
///
/// [`NotRunnable::NotFound`] when nothing matches, [`NotRunnable::NotExecutable`]
/// when the only matches are not executable regular files.
pub fn resolve(
    name: &str,
    env: &EnvMap,
    cwd: &Path,
    skip_shims: bool,
) -> Result<PathBuf, NotRunnable> {
    if name.is_empty() {
        return Err(NotRunnable::NotFound);
    }
    if name.contains('/') {
        let path = cwd.join(name);
        return match is_executable_file(&path) {
            Some(true) => Ok(path),
            Some(false) => Err(NotRunnable::NotExecutable(path)),
            None => Err(NotRunnable::NotFound),
        };
    }
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    let (shim_dir, myself) = if skip_shims {
        let dir = env
            .get(SHIM_DIR_VAR)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| default_shim_dir(env))
            .and_then(|d| canon(&d));
        let me = std::env::current_exe().ok().and_then(|p| canon(&p));
        (dir, me)
    } else {
        (None, None)
    };
    let path_var = env.get("PATH").map_or("", String::as_str);
    let mut not_executable = None;
    for entry in path_var.split(':') {
        let dir = if entry.is_empty() {
            cwd.to_path_buf()
        } else {
            cwd.join(entry)
        };
        if shim_dir.is_some() && canon(&dir) == shim_dir {
            continue;
        }
        let candidate = dir.join(name);
        match is_executable_file(&candidate) {
            Some(true) => {
                let own = myself.is_some() && canon(&candidate) == myself;
                if skip_shims && (own || is_shim(&candidate)) {
                    continue;
                }
                return Ok(candidate);
            }
            Some(false)
                if not_executable.is_none()
                    && std::fs::metadata(&candidate).is_ok_and(|m| m.is_file()) =>
            {
                not_executable = Some(candidate);
            }
            _ => {}
        }
    }
    Err(not_executable.map_or(NotRunnable::NotFound, NotRunnable::NotExecutable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn exe(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn resolves_skips_shims_and_reports_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let (shims, a, b) = (
            tmp.path().join("shims"),
            tmp.path().join("a"),
            tmp.path().join("b"),
        );
        for d in [&shims, &a, &b] {
            fs::create_dir(d).unwrap();
        }
        exe(&shims.join("make"), "#!/bin/sh\n# sluice-shim v1\n");
        exe(&a.join("make"), "#!/bin/sh\n# sluice-shim v1\nexec x\n");
        exe(&b.join("make"), "#!/bin/sh\necho real\n");
        fs::write(a.join("plain"), "data").unwrap();
        let mut env = EnvMap::new();
        env.insert(
            "PATH".into(),
            format!("{}:{}:{}", shims.display(), a.display(), b.display()),
        );
        env.insert(SHIM_DIR_VAR.into(), shims.display().to_string());
        let cwd = tmp.path();
        assert_eq!(resolve("make", &env, cwd, true).unwrap(), b.join("make"));
        assert_eq!(
            resolve("make", &env, cwd, false).unwrap(),
            shims.join("make")
        );
        assert_eq!(resolve("nope", &env, cwd, true), Err(NotRunnable::NotFound));
        assert_eq!(
            resolve("plain", &env, cwd, true),
            Err(NotRunnable::NotExecutable(a.join("plain")))
        );
        assert_eq!(
            resolve("plain", &env, cwd, true).unwrap_err().exit_code(),
            126
        );
        assert_eq!(
            resolve("./b/make", &env, cwd, true).unwrap(),
            cwd.join("./b/make")
        );
        assert_eq!(
            resolve("./b/none", &env, cwd, true),
            Err(NotRunnable::NotFound)
        );
        assert_eq!(resolve("", &env, cwd, true), Err(NotRunnable::NotFound));
    }
}
