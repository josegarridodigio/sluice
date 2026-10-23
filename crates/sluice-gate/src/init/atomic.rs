//! Atomic file writes for `init`.
//!
//! A hook can be running under `sh` while `init` rewires it, and a shell
//! reads its script as it goes, so a half-written file is a broken hook. The
//! new text goes to a temporary file in the same directory, is synced, gets
//! the target's mode and replaces the target with one `rename`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Mode of an existing `.sluice.toml`-like file that is created.
const DEFAULT_MODE: u32 = 0o644;

/// The file a write to `target` replaces: the target of a symlink, so the
/// link itself stays.
fn resolve(target: &Path) -> PathBuf {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf())
        }
        _ => target.to_path_buf(),
    }
}

fn temp_name(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    target.with_file_name(format!(".{name}.sluice-tmp-{}-{n}", std::process::id()))
}

/// Writes `bytes` to `target` atomically. An existing file keeps its mode;
/// a new one gets `new_mode` (0644 when `None`).
pub fn write(target: &Path, bytes: &[u8], new_mode: Option<u32>) -> io::Result<()> {
    let target = resolve(target);
    let mode = match fs::metadata(&target) {
        Ok(meta) => meta.permissions().mode() & 0o7777,
        Err(_) => new_mode.unwrap_or(DEFAULT_MODE),
    };
    let temp = temp_name(&target);
    let result = write_temp(&temp, bytes, mode).and_then(|()| fs::rename(&temp, &target));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_temp(temp: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("sluice-tmp"))
            .collect()
    }

    #[test]
    fn replaces_the_file_keeps_the_mode_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        write(&path, b"new", Some(0o755)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o750
        );
        assert_ne!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(leftovers(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_new_file_gets_the_requested_mode() {
        let dir = tempfile::tempdir().unwrap();
        let hook = dir.path().join("hook");
        write(&hook, b"x", Some(0o755)).unwrap();
        assert_eq!(
            fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let config = dir.path().join("cfg");
        write(&config, b"x", None).unwrap();
        assert_eq!(
            fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn a_symlink_stays_and_its_target_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::write(&real, "old").unwrap();
        symlink("real", &link).unwrap();
        write(&link, b"new", None).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(leftovers(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_failed_rename_removes_the_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // A directory cannot be replaced by a file: the rename fails.
        let target = dir.path().join("dir");
        fs::create_dir(&target).unwrap();
        assert!(write(&target, b"x", None).is_err());
        assert_eq!(leftovers(dir.path()), Vec::<String>::new());
    }
}
