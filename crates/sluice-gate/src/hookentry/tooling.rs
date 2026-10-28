//! Finding and asking the hook managers' own programs (lefthook,
//! pre-commit), for the adapters of wave H2b.
//!
//! Everything here is read-only: a program is looked up on `PATH`, asked for
//! its version or its effective configuration, and never installed, updated
//! or configured.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};
use sluice_gate_core::config::EnvMap;

use super::delegate::is_executable;

/// The first executable regular file called `name` in the `PATH` of `env`.
pub fn find_on_path(name: &str, env: &EnvMap) -> Option<PathBuf> {
    let path = env.get("PATH")?;
    std::env::split_paths(path)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

/// Runs `program` with `args` in `cwd` (stdin closed, stderr discarded) and
/// returns its stdout.
///
/// # Errors
///
/// Why it could not be asked: cannot start, non-zero exit, output not UTF-8.
pub fn capture(program: &Path, args: &[&str], cwd: &Path) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    if !out.status.success() {
        return Err(format!(
            "`{} {}` exited with {}",
            program.display(),
            args.join(" "),
            out.status
        ));
    }
    String::from_utf8(out.stdout)
        .map_err(|_| format!("{} printed non-UTF-8 text", program.display()))
}

/// Lower-case hex SHA-256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The first `major.minor.patch` found in `text`.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let mut parts = word.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next()?.parse().ok()?;
            Some((major, minor, patch))
        })
}

/// One word for `sh -c`: bare when it is made of safe characters, else
/// single-quoted.
pub fn sh_word(word: &str) -> String {
    let bare = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if bare {
        word.to_owned()
    } else {
        crate::shim::quote(word)
    }
}

/// The words of a command line for `sh -c`.
pub fn sh_line(words: &[String]) -> String {
    words
        .iter()
        .map(|w| sh_word(w))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_the_first_dotted_triple() {
        assert_eq!(parse_version("2.2.2\n"), Some((2, 2, 2)));
        assert_eq!(parse_version("pre-commit 4.7.0"), Some((4, 7, 0)));
        assert_eq!(parse_version("v2.10.13-beta"), Some((2, 10, 13)));
        assert_eq!(parse_version("two"), None);
        assert_eq!(parse_version("1.2"), None);
    }

    #[test]
    fn words_are_quoted_only_when_needed() {
        let words = ["/bin/x", "run", "a b", "it's", ""].map(str::to_owned);
        assert_eq!(sh_line(&words), r"/bin/x run 'a b' 'it'\''s' ''");
    }
}
