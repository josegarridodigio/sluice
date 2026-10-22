//! Cache-key derivation (see `kb/sluice/design/cache-key.md`, section "Key").

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::tree::TreeHash;
use crate::{Error, Result};

/// Domain-separation prefix hashed first into every key.
pub const KEY_PREFIX: &str = "sluice-cache-v1";

/// A cache key: 64 lowercase hex characters (SHA-256).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct CacheKey(String);

impl CacheKey {
    /// The key as a lowercase hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for CacheKey {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(value))
        } else {
            Err(format!(
                "invalid cache key '{value}': expected 64 lowercase hex characters"
            ))
        }
    }
}

/// Everything that feeds into a cache key.
#[derive(Debug, Clone)]
pub struct KeyMaterial {
    /// Hash of the working tree.
    pub tree: TreeHash,
    /// Name of the gate.
    pub gate_name: String,
    /// Command, already normalised with [`normalize_cmd`].
    pub cmd: String,
    /// Environment entries declared by the gate (sorted by key).
    pub env: BTreeMap<String, String>,
    /// `(command as written, sha256 hex of its stdout)`.
    pub key_inputs: Vec<(String, String)>,
    /// `(relative path as written, sha256 hex of content or the literal "absent")`.
    pub key_files: Vec<(String, String)>,
}

/// Derive the cache key.
///
/// Layout: `KEY_PREFIX \0 tree \0 gate \0 cmd \0`, then `k=v \0` per env pair
/// in sorted order, then `cmd \0 sha \0` per key input, then
/// `path \0 sha_or_absent \0` per key file.
pub fn derive(material: &KeyMaterial) -> CacheKey {
    let mut h = Sha256::new();
    let mut field = |s: &str| {
        h.update(s.as_bytes());
        h.update([0u8]);
    };
    field(KEY_PREFIX);
    field(material.tree.as_str());
    field(&material.gate_name);
    field(&material.cmd);
    for (k, v) in &material.env {
        field(&format!("{k}={v}"));
    }
    for (cmd, sha) in &material.key_inputs {
        field(cmd);
        field(sha);
    }
    for (path, sha) in &material.key_files {
        field(path);
        field(sha);
    }
    CacheKey(hex::encode(h.finalize()))
}

/// Trim a command and collapse every whitespace run into one space.
pub fn normalize_cmd(cmd: &str) -> String {
    cmd.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Run the `key_inputs` commands and hash the `key_files`.
///
/// Each input runs through `sh -c` in `repo_root` with `env` added to the
/// inherited environment; stderr is discarded. Files are resolved relative to
/// `repo_root`; a missing file yields `"absent"`.
///
/// # Errors
///
/// [`Error::KeyInput`] when a command cannot be spawned or exits non-zero,
/// [`Error::Io`] when a file exists but cannot be read.
#[allow(clippy::type_complexity)]
pub fn collect_inputs(
    key_inputs: &[String],
    key_files: &[PathBuf],
    repo_root: &Path,
    env: &BTreeMap<String, String>,
) -> Result<(Vec<(String, String)>, Vec<(String, String)>)> {
    let mut inputs = Vec::with_capacity(key_inputs.len());
    for cmd in key_inputs {
        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(repo_root)
            .envs(env)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| Error::KeyInput {
                cmd: cmd.clone(),
                reason: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(Error::KeyInput {
                cmd: cmd.clone(),
                reason: format!("exited with {}", output.status),
            });
        }
        inputs.push((cmd.clone(), hex::encode(Sha256::digest(&output.stdout))));
    }

    let mut files = Vec::with_capacity(key_files.len());
    for rel in key_files {
        let full = repo_root.join(rel);
        let digest = match std::fs::read(&full) {
            Ok(bytes) => hex::encode(Sha256::digest(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "absent".to_owned(),
            Err(e) => return Err(Error::io("reading key file", full, e)),
        };
        files.push((rel.to_string_lossy().into_owned(), digest));
    }
    Ok((inputs, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(s: &str) -> TreeHash {
        serde_json::from_str(&format!("\"{s}\"")).unwrap()
    }

    fn material() -> KeyMaterial {
        KeyMaterial {
            tree: tree("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            gate_name: "test".into(),
            cmd: "npm test".into(),
            env: BTreeMap::from([("A".into(), "1".into()), ("B".into(), "2".into())]),
            key_inputs: vec![("echo hi".into(), "f00".into())],
            key_files: vec![("Cargo.lock".into(), "absent".into())],
        }
    }

    #[test]
    fn golden_vector_matches_byte_layout() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"sluice-cache-v1\0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0test\0npm test\0A=1\0B=2\0echo hi\0f00\0Cargo.lock\0absent\0");
        let expected = hex::encode(Sha256::digest(&bytes));
        let key = derive(&material());
        assert_eq!(key.as_str(), expected);
        assert_eq!(key.as_str().len(), 64);
    }

    #[test]
    fn env_order_is_irrelevant() {
        let mut a = material();
        a.env = BTreeMap::new();
        a.env.insert("B".into(), "2".into());
        a.env.insert("A".into(), "1".into());
        assert_eq!(derive(&a), derive(&material()));
    }

    #[test]
    fn changing_any_field_changes_key() {
        let base = derive(&material());
        let mut m = material();
        m.gate_name = "lint".into();
        assert_ne!(derive(&m), base);
        let mut m = material();
        m.key_files[0].1 = "deadbeef".into();
        assert_ne!(derive(&m), base);
    }

    #[test]
    fn normalizes_whitespace() {
        assert_eq!(normalize_cmd("  npm \t test\n --x  "), "npm test --x");
        assert_eq!(normalize_cmd(""), "");
    }

    #[test]
    fn cache_key_validation() {
        assert!(CacheKey::try_from("a".repeat(64)).is_ok());
        assert!(CacheKey::try_from("A".repeat(64)).is_err());
        assert!(CacheKey::try_from("a".repeat(63)).is_err());
        assert!(CacheKey::try_from("g".repeat(64)).is_err());
    }

    #[test]
    fn collects_inputs_and_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("present.txt"), "data").unwrap();
        let env = BTreeMap::from([("FOO".to_owned(), "bar".to_owned())]);
        let (inputs, files) = collect_inputs(
            &["echo hi".into(), "printf %s \"$FOO\"".into()],
            &[PathBuf::from("present.txt"), PathBuf::from("missing.txt")],
            dir.path(),
            &env,
        )
        .unwrap();
        assert_eq!(inputs[0].0, "echo hi");
        assert_eq!(inputs[0].1, hex::encode(Sha256::digest(b"hi\n")));
        assert_eq!(inputs[1].1, hex::encode(Sha256::digest(b"bar")));
        assert_eq!(
            files[0],
            ("present.txt".into(), hex::encode(Sha256::digest(b"data")))
        );
        assert_eq!(files[1], ("missing.txt".into(), "absent".into()));
    }

    #[test]
    fn failing_input_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            collect_inputs(&["exit 3".into()], &[], dir.path(), &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, Error::KeyInput { ref cmd, .. } if cmd == "exit 3"));
    }
}
