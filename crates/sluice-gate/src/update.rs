//! `sluice update [--check] [--version <vX.Y.Z>] [--yes] [--no-restart]`.
//!
//! Self-update from the GitHub release assets, with the same layout the
//! installer (`scripts/install.sh`) reads. Network access goes through the
//! `curl` binary (fallback `wget`) and extraction through `tar`, so the
//! binary carries no HTTP or TLS stack. The archive is verified against
//! `SHA256SUMS` and the extracted binary must report the expected version
//! before the running executable is replaced with an atomic `rename`.

use std::fmt::Display;
use std::fs;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use sha2::{Digest, Sha256};
use sluice_gate_core::dirs;
use sluice_gate_core::git::EnvMap;
use sluice_gate_proto::client::{self, ClientError};

use crate::daemon_cmd;
use crate::run::EXIT_ERROR;

/// Exit status of a usage error (also what clap uses).
const EXIT_USAGE: u8 = 2;
/// GitHub repository the releases come from.
const REPO: &str = "josegarridodigio/sluice";
/// Overrides the release location: a directory (or `file://` URL) with the
/// archives, `SHA256SUMS` and optionally a `LATEST` file holding the tag.
const BASE_URL_ENV: &str = "SLUICE_UPDATE_BASE_URL";
/// Version of the running binary.
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Options of `sluice update`.
#[derive(Debug)]
pub struct UpdateArgs {
    pub check: bool,
    pub version: Option<String>,
    pub yes: bool,
    pub no_restart: bool,
}

/// A failed update with the exit status to report.
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn error(message: impl Display) -> Self {
        Self {
            code: EXIT_ERROR,
            message: message.to_string(),
        }
    }

    fn usage(message: impl Display) -> Self {
        Self {
            code: EXIT_USAGE,
            message: message.to_string(),
        }
    }
}

type Outcome<T = ()> = Result<T, Failure>;

/// A parsed `X.Y.Z` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u64, u64, u64);

impl Version {
    /// Parses `X.Y.Z`, with an optional leading `v`.
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.');
        let mut next = || {
            let part = parts.next()?;
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            part.parse::<u64>().ok()
        };
        let version = Self(next()?, next()?, next()?);
        parts.next().is_none().then_some(version)
    }
}

impl Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Release target triple of a `std::env::consts::ARCH` value.
fn target_for(arch: &str) -> Option<&'static str> {
    match arch {
        "x86_64" => Some("x86_64-unknown-linux-musl"),
        "aarch64" => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

/// How files are downloaded: `curl`, or `wget` when `curl` is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fetcher {
    Curl,
    Wget,
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

impl Fetcher {
    fn detect() -> Outcome<Self> {
        if on_path("curl") {
            Ok(Self::Curl)
        } else if on_path("wget") {
            Ok(Self::Wget)
        } else {
            Err(Failure::error(
                "curl or wget is required to download updates",
            ))
        }
    }

    /// Downloads `url` into `dest`.
    fn download(self, url: &str, dest: &Path) -> Result<(), String> {
        let mut cmd = match self {
            Self::Curl => {
                let mut c = Command::new("curl");
                c.args(["-fsSL", "-o"]).arg(dest).arg(url);
                c
            }
            Self::Wget => {
                let mut c = Command::new("wget");
                c.args(["-q", "-O"]).arg(dest).arg(url);
                c
            }
        };
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run the download tool: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }

    /// Reads a small text file.
    fn read_text(self, url: &str, dir: &Path) -> Result<String, String> {
        let dest = dir.join("remote.txt");
        self.download(url, &dest)?;
        fs::read_to_string(&dest).map_err(|e| e.to_string())
    }

    /// Tag the `releases/latest` redirect points at.
    fn latest_tag(self) -> Result<String, String> {
        let url = format!("https://github.com/{REPO}/releases/latest");
        let final_url = match self {
            Self::Curl => {
                let out = Command::new("curl")
                    .args(["-fsSLI", "-o", "/dev/null", "-w", "%{url_effective}"])
                    .arg(&url)
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| format!("cannot run curl: {e}"))?;
                if !out.status.success() {
                    return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
                }
                String::from_utf8_lossy(&out.stdout).trim().to_owned()
            }
            Self::Wget => {
                // The redirect is not followed: its `Location` header is the tag URL.
                let out = Command::new("wget")
                    .args(["-q", "-S", "--max-redirect=0", "-O", "/dev/null"])
                    .arg(&url)
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|e| format!("cannot run wget: {e}"))?;
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .find_map(|l| {
                        l.trim()
                            .strip_prefix("Location:")
                            .map(|v| v.trim().to_owned())
                    })
                    .ok_or("the release redirect has no Location header")?
            }
        };
        final_url
            .rsplit_once("/tag/")
            .map(|(_, tag)| tag.trim_end_matches('/').to_owned())
            .ok_or_else(|| format!("no release found (got {final_url})"))
    }
}

/// Resolves the tag to install: `--version`, `LATEST` of the override, or the redirect.
fn resolve(
    requested: Option<&str>,
    base_override: Option<&str>,
    fetcher: Fetcher,
    scratch: &Path,
) -> Outcome<Version> {
    let tag = match (requested, base_override) {
        (Some(v), _) => v.to_owned(),
        (None, Some(base)) => fetcher
            .read_text(&format!("{base}/LATEST"), scratch)
            .map_err(|e| {
                Failure::error(format!(
                    "{BASE_URL_ENV} is set: pass --version or provide a LATEST file ({e})"
                ))
            })?
            .trim()
            .to_owned(),
        (None, None) => fetcher
            .latest_tag()
            .map_err(|e| Failure::error(format!("cannot resolve the latest release: {e}")))?,
    };
    Version::parse(&tag)
        .ok_or_else(|| Failure::usage(format!("'{tag}' is not a release tag like v1.2.3")))
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// SHA-256 of a file as lowercase hex.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 16 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Finds the asset of `target` in a `SHA256SUMS` body: `(checksum, file name)`.
fn find_asset<'a>(sums: &'a str, target: &str) -> Option<(&'a str, &'a str)> {
    let suffix = format!("-{target}.tar.gz");
    sums.lines().find_map(|line| {
        let (sum, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim().trim_start_matches('*');
        (name.starts_with("sluice-") && name.ends_with(&suffix) && !name.contains('/'))
            .then_some((sum, name))
    })
}

/// Downloads, verifies and unpacks the release; returns the new binary.
fn fetch_binary(
    fetcher: Fetcher,
    files_url: &str,
    version: Version,
    target: &str,
    work: &Path,
) -> Outcome<PathBuf> {
    let sums_path = work.join("SHA256SUMS");
    fetcher
        .download(&format!("{files_url}/SHA256SUMS"), &sums_path)
        .map_err(|e| {
            Failure::error(format!(
                "could not download SHA256SUMS (does the release exist?): {e}"
            ))
        })?;
    let sums = fs::read_to_string(&sums_path).map_err(Failure::error)?;
    let (expected, asset) = find_asset(&sums, target)
        .ok_or_else(|| Failure::error(format!("no asset for {target} in SHA256SUMS")))?;
    let wanted = format!("sluice-{version}-{target}.tar.gz");
    if asset != wanted {
        return Err(Failure::error(format!(
            "SHA256SUMS lists {asset}, expected {wanted}"
        )));
    }
    println!("sluice: downloading {asset}");
    let archive = work.join(asset);
    fetcher
        .download(&format!("{files_url}/{asset}"), &archive)
        .map_err(|e| Failure::error(format!("could not download {asset}: {e}")))?;
    let actual = sha256_file(&archive).map_err(Failure::error)?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(Failure::error(format!(
            "checksum mismatch for {asset} (expected {expected}, got {actual}); nothing was changed"
        )));
    }
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(work)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Failure::error(format!("cannot run tar: {e}")))?;
    if !out.status.success() {
        return Err(Failure::error(format!(
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let binary = work.join(asset.trim_end_matches(".tar.gz")).join("sluice");
    if !binary.is_file() {
        return Err(Failure::error("the archive does not contain sluice"));
    }
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).map_err(Failure::error)?;
    let reported = Command::new(&binary)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Failure::error(format!("the downloaded binary does not run: {e}")))?;
    let text = String::from_utf8_lossy(&reported.stdout).trim().to_owned();
    if !reported.status.success() || text != format!("sluice {version}") {
        return Err(Failure::error(format!(
            "the downloaded binary reports '{text}', expected 'sluice {version}'"
        )));
    }
    Ok(binary)
}

/// Replaces `target` with `new` through a sibling file and an atomic `rename`.
fn replace_executable(new: &Path, target: &Path) -> Outcome {
    let dir = target
        .parent()
        .ok_or_else(|| Failure::error("the executable has no parent directory"))?;
    let staged = dir.join(format!(".sluice.new.{}", std::process::id()));
    let result = fs::copy(new, &staged)
        .and_then(|_| fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)))
        .and_then(|()| fs::rename(&staged, target));
    if let Err(e) = result {
        let _ = fs::remove_file(&staged);
        return Err(if e.kind() == std::io::ErrorKind::PermissionDenied {
            Failure::error(format!(
                "cannot write to {}: permission denied; re-run with sudo, or reinstall into a \
                 directory you own with the installer (SLUICE_INSTALL_DIR=<dir>)",
                dir.display()
            ))
        } else {
            Failure::error(format!("cannot update {}: {e}", target.display()))
        });
    }
    Ok(())
}

/// Stops an idle daemon so the next command starts the new version.
fn restart_daemon() {
    let env: EnvMap = sluice_gate_core::config::process_env();
    let Ok(socket) = dirs::socket_path(&env) else {
        return;
    };
    let busy = match client::connect(&socket).and_then(client::Client::status) {
        Ok(report) => report.running.len() + report.queued.len(),
        Err(ClientError::NotRunning(_)) => return,
        Err(e) => {
            eprintln!("sluice: warning: cannot query the daemon: {e}");
            return;
        }
    };
    if busy > 0 {
        println!(
            "sluice: the daemon is running {busy} gate(s); it keeps the old version until \
             'sluice daemon stop'"
        );
        return;
    }
    // Prints its own result; the next `sluice` call autostarts the new daemon.
    let _ = daemon_cmd::stop(false);
}

/// `sluice update`.
pub fn run(args: &UpdateArgs) -> ExitCode {
    match do_update(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            if f.code == EXIT_ERROR {
                eprintln!("sluice: error: {}", f.message);
            } else {
                eprintln!("sluice: {}", f.message);
            }
            ExitCode::from(f.code)
        }
    }
}

fn do_update(args: &UpdateArgs) -> Outcome {
    let env: EnvMap = sluice_gate_core::config::process_env();
    let current = Version::parse(CURRENT)
        .ok_or_else(|| Failure::error(format!("cannot parse own version '{CURRENT}'")))?;
    let base_override = env
        .get(BASE_URL_ENV)
        .filter(|v| !v.is_empty())
        .map(|v| v.trim_end_matches('/').to_owned());
    let fetcher = Fetcher::detect()?;
    let work = tempfile::Builder::new()
        .prefix("sluice-update-")
        .tempdir()
        .map_err(|e| Failure::error(format!("cannot create a temporary directory: {e}")))?;

    let latest = resolve(
        args.version.as_deref(),
        base_override.as_deref(),
        fetcher,
        work.path(),
    )?;
    if latest == current {
        println!("sluice: already at {current} (latest v{latest})");
        return Ok(());
    }
    let downgrade = latest < current;
    if args.check {
        if downgrade && args.version.is_none() {
            println!("sluice: already at {current} (latest v{latest})");
        } else {
            println!("sluice: update available: {current} -> {latest}");
        }
        return Ok(());
    }
    let target = target_for(std::env::consts::ARCH).ok_or_else(|| {
        Failure::error(format!(
            "unsupported architecture: {}",
            std::env::consts::ARCH
        ))
    })?;
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| Failure::error(format!("cannot locate the running executable: {e}")))?;

    if !args.yes {
        if !std::io::stdin().is_terminal() {
            return Err(Failure::usage(
                "updating needs a terminal to confirm; run it in a terminal or pass --yes",
            ));
        }
        let verb = if downgrade { "Downgrade" } else { "Update" };
        if !confirm(&format!("{verb} sluice {current} -> {latest}? [y/N] ")) {
            return Err(Failure::error("not updated"));
        }
    }

    let files_url = base_override
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases/download/v{latest}"));
    let binary = fetch_binary(fetcher, &files_url, latest, target, work.path())?;
    replace_executable(&binary, &exe)?;
    println!("sluice: updated {current} -> {latest} ({})", exe.display());
    if exe.parent().is_some_and(|d| d.ends_with(".cargo/bin")) {
        println!(
            "sluice: note: this binary lives in ~/.cargo/bin; cargo's install metadata does not \
             know about this version (harmless). A later `cargo install --path` or \
             `cargo install sluice-gate` would overwrite it."
        );
    }
    if !args.no_restart {
        restart_daemon();
    }
    println!(
        "sluice: running MCP servers (`sluice mcp`, started by agent sessions) keep the old \
         binary until the agent reconnects."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_with_and_without_prefix() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("0.10.0\n"), Some(Version(0, 10, 0)));
        for bad in [
            "1.2",
            "1.2.3.4",
            "v1.2.x",
            "",
            "1..3",
            "1.2.3-rc1",
            "v-1.2.3",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn compares_numerically() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("v0.1.0"), v("0.1.0"));
    }

    #[test]
    fn maps_architectures_to_targets() {
        assert_eq!(target_for("x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(target_for("aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(target_for("riscv64"), None);
    }

    #[test]
    fn finds_the_asset_line() {
        let sums = "aa  sluice-0.2.0-aarch64-unknown-linux-musl.tar.gz\n\
                    bb *sluice-0.2.0-x86_64-unknown-linux-musl.tar.gz\n";
        assert_eq!(
            find_asset(sums, "x86_64-unknown-linux-musl"),
            Some(("bb", "sluice-0.2.0-x86_64-unknown-linux-musl.tar.gz"))
        );
        assert_eq!(find_asset(sums, "riscv64-unknown-linux-musl"), None);
    }
}
