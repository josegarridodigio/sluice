//! `sluice update` against a fake release directory served through `file://`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::Sandbox;
use sha2::{Digest, Sha256};

const FAKE_VERSION: &str = "9.9.9";

fn target() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other => panic!("unsupported test architecture {other}"),
    }
}

/// A fake release directory plus a private copy of the real binary to update.
struct Fixture {
    sandbox: Sandbox,
    release: PathBuf,
    bin_dir: PathBuf,
    exe: PathBuf,
}

impl Fixture {
    fn new(latest: &str) -> Self {
        let sandbox = Sandbox::new("");
        let release = sandbox.root.path().join("release");
        let bin_dir = sandbox.root.path().join("bin");
        fs::create_dir_all(&release).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();
        let exe = bin_dir.join("sluice");
        fs::copy(env!("CARGO_BIN_EXE_sluice"), &exe).unwrap();
        let fixture = Self {
            sandbox,
            release,
            bin_dir,
            exe,
        };
        fixture.publish(FAKE_VERSION, true);
        fs::write(fixture.release.join("LATEST"), format!("{latest}\n")).unwrap();
        fixture
    }

    /// Writes the fake archive and `SHA256SUMS` (a wrong sum when `valid` is false).
    fn publish(&self, version: &str, valid: bool) {
        let name = format!("sluice-{version}-{}", target());
        let staging = self.sandbox.root.path().join("staging");
        let top = staging.join(&name);
        fs::create_dir_all(&top).unwrap();
        let script = top.join("sluice");
        fs::write(
            &script,
            format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'sluice {version}'; fi\n"),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let archive = self.release.join(format!("{name}.tar.gz"));
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .arg(&name)
            .status()
            .unwrap();
        assert!(status.success());
        let sum = if valid {
            hex::encode(Sha256::digest(fs::read(&archive).unwrap()))
        } else {
            "0".repeat(64)
        };
        fs::write(
            self.release.join("SHA256SUMS"),
            format!("{sum}  {name}.tar.gz\n"),
        )
        .unwrap();
    }

    /// Runs the private copy with the sandbox environment.
    fn update(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("update")
            .args(args)
            .current_dir(&self.sandbox.home)
            .env_clear()
            .envs(self.sandbox.env())
            .env(
                "SLUICE_UPDATE_BASE_URL",
                format!("file://{}", self.release.display()),
            );
        cmd.output().unwrap()
    }

    fn version_of_exe(&self) -> String {
        let out = Command::new(&self.exe).arg("--version").output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn exe_bytes(&self) -> Vec<u8> {
        fs::read(&self.exe).unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "sluice")
        .collect()
}

#[test]
fn check_reports_the_update_without_writing() {
    let fx = Fixture::new("v9.9.9");
    let before = fx.exe_bytes();
    let out = fx.update(&["--check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!(
            "sluice: update available: {} -> 9.9.9",
            env!("CARGO_PKG_VERSION")
        )
    );
    assert_eq!(fx.exe_bytes(), before);
}

#[test]
fn yes_replaces_the_executable() {
    let fx = Fixture::new("v9.9.9");
    let out = fx.update(&["--yes", "--no-restart"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("updated"), "{}", stdout(&out));
    assert!(stdout(&out).contains("MCP servers"), "{}", stdout(&out));
    assert_eq!(fx.version_of_exe(), "sluice 9.9.9");
    assert_eq!(leftovers(&fx.bin_dir), Vec::<String>::new());
}

#[test]
fn yes_without_no_restart_works_with_no_daemon() {
    let fx = Fixture::new("v9.9.9");
    let out = fx.update(&["--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fx.version_of_exe(), "sluice 9.9.9");
}

#[test]
fn explicit_version_is_used_without_latest_file() {
    let fx = Fixture::new("v0.0.1");
    fs::remove_file(fx.release.join("LATEST")).unwrap();
    let out = fx.update(&["--yes", "--no-restart", "--version", "v9.9.9"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fx.version_of_exe(), "sluice 9.9.9");
}

#[test]
fn override_without_version_or_latest_is_an_error() {
    let fx = Fixture::new("v9.9.9");
    fs::remove_file(fx.release.join("LATEST")).unwrap();
    let out = fx.update(&["--check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("LATEST"), "{}", stderr(&out));
}

#[test]
fn checksum_mismatch_is_refused() {
    let fx = Fixture::new("v9.9.9");
    fx.publish(FAKE_VERSION, false);
    let before = fx.exe_bytes();
    let out = fx.update(&["--yes", "--no-restart"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("checksum mismatch"),
        "{}",
        stderr(&out)
    );
    assert_eq!(fx.exe_bytes(), before);
    assert_eq!(leftovers(&fx.bin_dir), Vec::<String>::new());
}

#[test]
fn missing_checksum_line_is_refused() {
    let fx = Fixture::new("v9.9.9");
    fs::write(fx.release.join("SHA256SUMS"), "").unwrap();
    let before = fx.exe_bytes();
    let out = fx.update(&["--yes", "--no-restart"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("SHA256SUMS"), "{}", stderr(&out));
    assert_eq!(fx.exe_bytes(), before);
}

#[test]
fn already_up_to_date() {
    let current = env!("CARGO_PKG_VERSION");
    let fx = Fixture::new(&format!("v{current}"));
    let before = fx.exe_bytes();
    let out = fx.update(&["--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("sluice: already at {current} (latest v{current})")
    );
    assert_eq!(fx.exe_bytes(), before);
}

#[test]
fn without_a_terminal_it_needs_yes() {
    let fx = Fixture::new("v9.9.9");
    let before = fx.exe_bytes();
    let out = fx.update(&["--no-restart"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    assert_eq!(fx.exe_bytes(), before);
}

#[test]
fn unwritable_directory_gives_a_hint() {
    if fs::metadata("/proc/self").is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt as _;
        m.uid() == 0
    }) {
        return; // root ignores directory permissions
    }
    let fx = Fixture::new("v9.9.9");
    let before = fx.exe_bytes();
    fs::set_permissions(&fx.bin_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let out = fx.update(&["--yes", "--no-restart"]);
    fs::set_permissions(&fx.bin_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("sudo") && err.contains("SLUICE_INSTALL_DIR"),
        "{err}"
    );
    assert_eq!(fx.exe_bytes(), before);
}
