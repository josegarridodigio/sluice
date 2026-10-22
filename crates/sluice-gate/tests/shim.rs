//! Acceptance tests of the shell shims (alpha task H5; test plan in
//! `kb/sluice/design/command-control.md` section 7).
//!
//! The "real" binaries are small `sh` scripts in a `bin` directory of the
//! sandbox; every command goes through `sh -c 'exec <name> "$@"'` with a
//! controlled `PATH`, as an agent's shell would run it.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// A tool name no system has, added to the heavy list so "not found
/// anywhere" can be tested even where a real `make` exists.
const FAKE_TOOL: &str = "sluicefaketool";

struct Shims {
    sb: Sandbox,
    /// The default shim directory of the sandbox.
    dir: PathBuf,
    /// Directory holding the fake real binaries.
    bin: PathBuf,
}

fn exe(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Sandbox with budget `budget`, `extra` user config, the shims installed in
/// the default directory and fake `make`, `docker` in `bin`.
fn setup(budget: u32, extra: &str) -> Shims {
    let sb = Sandbox::new("");
    fs::write(
        sb.home.join(".config/sluice/config.toml"),
        format!(
            "version = 1\n[daemon]\nbudget = {budget}\n[admission]\npsi_enabled = false\n\
             [agent]\nheavy_commands_extra = [\"{FAKE_TOOL}\"]\n{extra}\n"
        ),
    )
    .unwrap();
    let bin = sb.root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // `make`: prints where it runs from, or does what its first argument says.
    exe(
        &bin.join("make"),
        r#"case "$1" in
  exit) exit "$2" ;;
  cat) cat ;;
  -f) cat ;;
  binary) cat "$2" ;;
  streams) printf out1; printf err1 >&2; printf out2; printf err2 >&2 ;;
  tty) if [ -t 1 ]; then echo tty; else echo notty; fi ;;
  env) echo "job=${SLUICE_JOB:-none}" ;;
  recurse) exec make inner ;;
  wait) touch "$2.started"; while [ ! -f "$2.release" ]; do sleep 0.05; done; echo waited ;;
  *) echo "real make $*" ;;
esac"#,
    );
    exe(
        &bin.join("docker"),
        r#"echo "real docker $* job=${SLUICE_JOB:-none}""#,
    );
    let shims = Shims {
        dir: sb.home.join(".local/share/sluice/shims"),
        sb,
        bin,
    };
    let out = shims.sluice(&["shim", "install"]);
    assert!(out.status.success(), "install: {out:?}");
    shims
}

impl Shims {
    fn sluice(&self, args: &[&str]) -> Output {
        StdCommand::new(Sandbox::binary())
            .args(args)
            .current_dir(&self.sb.repo)
            .env_clear()
            .envs(self.sb.env())
            .output()
            .expect("spawn sluice")
    }

    /// `PATH` made of `front` entries, then the system `PATH`.
    fn path(front: &[&Path]) -> String {
        let mut entries: Vec<String> = front.iter().map(|p| p.display().to_string()).collect();
        entries.push(std::env::var("PATH").unwrap_or_default());
        entries.join(":")
    }

    /// `sh -c 'exec <name> "$@"' sh <args>` with this `PATH` and extra env.
    fn command(&self, path: &str, env: &[(&str, &str)], name: &str, args: &[&str]) -> StdCommand {
        let mut cmd = StdCommand::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("exec {name} \"$@\""))
            .arg("sh")
            .args(args)
            .current_dir(&self.sb.repo)
            .env_clear()
            .envs(self.sb.env())
            .env("PATH", path)
            .envs(env.iter().copied())
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, path: &str, env: &[(&str, &str)], name: &str, args: &[&str]) -> Output {
        let child = self
            .command(path, env, name, args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let Ok(out) = rx.recv_timeout(Duration::from_secs(60)) else {
            let _ = kill(pid, Signal::SIGKILL);
            panic!("`{name} {args:?}` did not finish (recursion?)");
        };
        out.unwrap()
    }

    fn run_stdin(&self, path: &str, env: &[(&str, &str)], args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command(path, env, "make", args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    /// Default PATH: shims first, then the fake binaries, then the system.
    fn default_path(&self) -> String {
        Self::path(&[&self.dir, &self.bin])
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const AGENT: &[(&str, &str)] = &[("CLAUDECODE", "1")];
const FORCED: &[(&str, &str)] = &[("SLUICE_SHIMS", "1")];

// ---------------------------------------------------------------------------
// Resolution and recursion
// ---------------------------------------------------------------------------

#[test]
fn resolution_skips_the_shim_dir_wherever_and_however_it_appears() {
    let s = setup(4, "");
    let root = s.sb.root.path();
    let empty = root.join("empty");
    fs::create_dir(&empty).unwrap();
    let link = root.join("link-to-shims");
    symlink(&s.dir, &link).unwrap();
    // A second copy of the shims in another directory.
    let other = root.join("other-shims");
    let out = s.sluice(&["shim", "install", "--dir", other.to_str().unwrap()]);
    assert!(out.status.success(), "{out:?}");
    let nested = root.join("nested");
    fs::create_dir(&nested).unwrap();
    let nested_link = nested.join("shims");
    symlink(&other, &nested_link).unwrap();

    let paths: Vec<(&str, String)> = vec![
        ("first", Shims::path(&[&s.dir, &s.bin])),
        ("middle", Shims::path(&[&empty, &s.dir, &empty, &s.bin])),
        ("twice", Shims::path(&[&s.dir, &s.dir, &s.bin, &s.dir])),
        ("symlink", Shims::path(&[&link, &s.dir, &s.bin])),
        ("other copy first", Shims::path(&[&other, &s.dir, &s.bin])),
        (
            "nested symlinked copies",
            Shims::path(&[&nested_link, &link, &other, &s.dir, &s.bin]),
        ),
    ];
    for (what, path) in &paths {
        for env in [&[][..], AGENT] {
            let out = s.run(path, env, "make", &["a", "b c"]);
            assert!(out.status.success(), "{what}: {out:?}");
            assert_eq!(stdout(&out), "real make a b c\n", "{what} env={env:?}");
        }
        // The real binary calling `make` again goes through the shim again
        // and still lands on the real one.
        let out = s.run(path, &[], "make", &["recurse"]);
        assert_eq!(stdout(&out), "real make inner\n", "{what}: {out:?}");
    }
}

#[test]
fn only_shims_in_path_exit_127_like_a_shell_without_recursion() {
    let s = setup(4, "");
    let other = s.sb.root.path().join("other-shims");
    assert!(
        s.sluice(&["shim", "install", "--dir", other.to_str().unwrap()])
            .status
            .success()
    );
    let link = s.sb.root.path().join("link");
    symlink(&s.dir, &link).unwrap();
    let path = Shims::path(&[&s.dir, &link, &other, &s.dir]);
    for env in [&[][..], AGENT] {
        let started = Instant::now();
        let out = s.run(&path, env, FAKE_TOOL, &["x"]);
        assert_eq!(out.status.code(), Some(127), "{out:?}");
        assert_eq!(stderr(&out), format!("{FAKE_TOOL}: command not found\n"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
    // No daemon was started for a command that does not exist.
    assert!(!s.sb.socket().exists());
}

/// Replaces the recorded `sluice` path of every shim in `dir` with a path
/// that does not exist (as after the binary was moved or removed).
fn break_sluice_path(dir: &Path) {
    let me = fs::canonicalize(Sandbox::binary()).unwrap();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        fs::write(
            &path,
            text.replace(&me.display().to_string(), "/nonexistent/sluice"),
        )
        .unwrap();
    }
}

#[test]
fn wrapper_falls_back_to_the_real_binary_when_sluice_is_gone() {
    let s = setup(4, "");
    let other = s.sb.root.path().join("other-shims");
    assert!(
        s.sluice(&["shim", "install", "--dir", other.to_str().unwrap()])
            .status
            .success()
    );
    let link = s.sb.root.path().join("link");
    symlink(&s.dir, &link).unwrap();
    break_sluice_path(&s.dir);
    break_sluice_path(&other);
    let path = Shims::path(&[&link, &s.dir, &other, &s.bin]);
    let out = s.run(&path, AGENT, "make", &["a"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), "real make a\n");
    let out = s.run(
        &Shims::path(&[&link, &s.dir, &other]),
        AGENT,
        FAKE_TOOL,
        &[],
    );
    assert_eq!(out.status.code(), Some(127), "{out:?}");
    assert!(stderr(&out).contains("not found"), "{out:?}");
    assert!(!s.sb.socket().exists());
}

// ---------------------------------------------------------------------------
// Fidelity
// ---------------------------------------------------------------------------

fn random_file(s: &Shims) -> (PathBuf, Vec<u8>) {
    let mut random = vec![0u8; 256 * 1024];
    fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut random)
        .unwrap();
    let path = s.sb.root.path().join("random.bin");
    fs::write(&path, &random).unwrap();
    (path, random)
}

#[test]
fn exit_codes_streams_and_binary_output_are_preserved() {
    let s = setup(4, "");
    let path = s.default_path();
    let (file, random) = random_file(&s);
    for env in [&[][..], AGENT, FORCED] {
        for code in ["0", "1", "42", "255"] {
            let out = s.run(&path, env, "make", &["exit", code]);
            assert_eq!(out.status.code(), Some(code.parse().unwrap()), "{env:?}");
        }
        let out = s.run(&path, env, "make", &["streams"]);
        assert!(out.status.success());
        assert_eq!(out.stdout, b"out1out2", "{env:?}");
        assert_eq!(out.stderr, b"err1err2", "{env:?}");
        let out = s.run(&path, env, "make", &["binary", file.to_str().unwrap()]);
        assert!(out.status.success());
        assert!(out.stdout == random, "binary stdout differs ({env:?})");
    }
}

#[test]
fn stdin_is_piped_to_direct_runs_and_to_dash_arguments() {
    let s = setup(4, "");
    let path = s.default_path();
    // Human session: the real binary has the caller's stdin.
    let out = s.run_stdin(&path, &[], &["cat"], b"hello\n\0bytes");
    assert_eq!(out.stdout, b"hello\n\0bytes");
    // Agent session: a `-` argument means "reads stdin": direct run.
    let out = s.run_stdin(&path, AGENT, &["-f", "-"], b"from stdin");
    assert_eq!(out.stdout, b"from stdin");
    // Agent session through the queue: stdin is /dev/null (documented).
    let out = s.run_stdin(&path, AGENT, &["cat"], b"ignored");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(out.stdout, b"");
}

#[test]
fn a_terminal_is_kept_in_direct_runs() {
    if StdCommand::new("script").arg("--version").output().is_err() {
        eprintln!("skipped: `script` is not installed");
        return;
    }
    let s = setup(4, "");
    let out = StdCommand::new("script")
        .args(["-qec", "make tty", "/dev/null"])
        .current_dir(&s.sb.repo)
        .env_clear()
        .envs(s.sb.env())
        .env("PATH", s.default_path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        stdout(&out).contains("tty") && !stdout(&out).contains("notty"),
        "{out:?}"
    );
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

#[test]
fn human_sessions_run_the_real_binary_without_contacting_the_daemon() {
    let s = setup(4, "");
    let path = s.default_path();
    for env in [
        &[][..],
        &[("SLUICE_SHIMS", "0"), ("CLAUDECODE", "1")][..],
        &[("SLUICE_AGENT", "0"), ("CLAUDECODE", "1")][..],
    ] {
        let out = s.run(&path, env, "make", &["env"]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(stdout(&out), "job=none\n", "{env:?}");
        assert_eq!(stderr(&out), "", "{env:?}");
    }
    assert!(!s.sb.socket().exists(), "a daemon was contacted");
    assert!(!s.sb.home.join("state/sluice").exists());
}

#[test]
fn inside_a_job_and_for_light_subcommands_shims_pass_through() {
    let s = setup(4, "");
    let path = s.default_path();
    let out = s.run(
        &path,
        &[("SLUICE_JOB", "outer-1"), ("SLUICE_SHIMS", "1")],
        "make",
        &["env"],
    );
    assert_eq!(stdout(&out), "job=outer-1\n", "{out:?}");
    let out = s.run(&path, AGENT, "docker", &["ps"]);
    assert_eq!(stdout(&out), "real docker ps job=none\n", "{out:?}");
    assert!(!s.sb.socket().exists(), "a daemon was contacted");
    // A heavy docker subcommand is queued.
    let out = s.run(&path, AGENT, "docker", &["build", "."]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.starts_with("real docker build . job=") && !text.contains("job=none"));
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn agent_sessions_go_through_the_queue_and_show_in_status() {
    let s = setup(4, "");
    let path = s.default_path();
    for (env, tag) in [(AGENT, "agent"), (FORCED, "forced")] {
        let marker = s.sb.root.path().join(tag);
        let marker_s = marker.display().to_string();
        let child = s
            .command(&path, env, "make", &["wait", &marker_s])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = PathBuf::from(format!("{marker_s}.started"));
        wait_for("the shimmed make to start", || started.exists());
        let status = s.sb.status_json();
        let running = status["running"].as_array().cloned().unwrap_or_default();
        assert!(
            running
                .iter()
                .any(|j| j["kind"] == "exec" && j["label"] == "make wait"),
            "{status:#}"
        );
        fs::write(format!("{marker_s}.release"), "").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(stdout(&out), "waited\n");
    }
    // The command saw SLUICE_JOB (set by the daemon for every job).
    let out = s.run(&path, AGENT, "make", &["env"]);
    let text = stdout(&out);
    assert!(text.starts_with("job=") && text != "job=none\n", "{out:?}");
}

/// Real `make` processes running with `tag` as last argument (`/proc` scan):
/// `/bin/sh <bin>/make <tag>`.
fn proc_count(bin: &Path, tag: &str) -> usize {
    let real = bin.join("make").display().to_string();
    let Ok(entries) = fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().parse::<u32>().is_ok())
        .filter(|e| {
            fs::read(e.path().join("cmdline")).is_ok_and(|c| {
                let args: Vec<&[u8]> = c
                    .strip_suffix(b"\0")
                    .unwrap_or(&c)
                    .split(|b| *b == 0)
                    .collect();
                args.get(1).is_some_and(|a| *a == real.as_bytes())
                    && args.last().is_some_and(|a| *a == tag.as_bytes())
            })
        })
        .count()
}

#[test]
fn ten_concurrent_shimmed_makes_stay_within_the_budget() {
    // Budget 4, weight 2: at most two at a time.
    let s = setup(4, "exec_weight = 2");
    let log = s.sb.root.path().join("concurrency.log");
    exe(
        &s.bin.join("make"),
        &format!(
            "echo start >> '{log}'; sleep 0.5; echo end >> '{log}'",
            log = log.display()
        ),
    );
    let tag = format!("sluice-h5-{}", std::process::id());
    let path = s.default_path();
    let children: Vec<Child> = (0..10)
        .map(|_| {
            s.command(&path, AGENT, "make", &[&tag])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut peak_proc = 0;
    let deadline = Instant::now() + Duration::from_secs(120);
    while fs::read_to_string(&log)
        .unwrap_or_default()
        .matches("end")
        .count()
        < 10
    {
        assert!(Instant::now() < deadline, "the 10 makes did not finish");
        peak_proc = peak_proc.max(proc_count(&s.bin, &tag));
        thread::sleep(Duration::from_millis(15));
    }
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
    }
    let (mut current, mut peak) = (0i32, 0i32);
    for line in fs::read_to_string(&log).unwrap().lines() {
        current += if line == "start" { 1 } else { -1 };
        peak = peak.max(current);
    }
    assert!(peak <= 2, "counter peak {peak} > 2");
    assert!(peak_proc <= 2, "/proc peak {peak_proc} > 2");
    assert!(peak_proc >= 1, "the /proc probe never saw a make");
    assert_eq!(s.sb.status_json()["totals"]["runs"], 10);
}

// ---------------------------------------------------------------------------
// install / uninstall / status
// ---------------------------------------------------------------------------

#[test]
fn install_and_uninstall_are_idempotent_and_leave_foreign_files_alone() {
    let s = setup(4, "");
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&s.dir), 0o700);
    assert_eq!(mode(&s.dir.join("make")), 0o755);
    let snapshot = || {
        let mut v: Vec<(String, String)> = fs::read_dir(&s.dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    fs::read_to_string(e.path()).unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    };
    let before = snapshot();
    assert!(before.iter().any(|(n, _)| n == FAKE_TOOL));
    let out = s.sluice(&["shim", "install"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("0 new, 0 updated"), "{out:?}");
    assert_eq!(snapshot(), before, "a second install changed files");

    // A foreign `cargo` and an unrelated file.
    fs::remove_file(s.dir.join("cargo")).unwrap();
    fs::write(s.dir.join("cargo"), "#!/bin/sh\necho mine\n").unwrap();
    fs::write(s.dir.join("notes.txt"), "keep me\n").unwrap();
    let out = s.sluice(&["shim", "install"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        stderr(&out).contains("refused to overwrite files Sluice did not write: cargo"),
        "{out:?}"
    );
    assert_eq!(
        fs::read_to_string(s.dir.join("cargo")).unwrap(),
        "#!/bin/sh\necho mine\n"
    );
    let status: serde_json::Value =
        serde_json::from_slice(&s.sluice(&["shim", "status", "--json"]).stdout).unwrap();
    let state = |name: &str| {
        status["shims"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .map(|r| r["state"].as_str().unwrap().to_owned())
    };
    assert_eq!(state("cargo").as_deref(), Some("foreign"));
    assert_eq!(state("make").as_deref(), Some("current"));
    assert_eq!(status["in_path"], false);

    // Uninstall keeps the foreign files (and so the directory).
    let out = s.sluice(&["shim", "uninstall"]);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("kept files Sluice did not write: cargo notes.txt"));
    let mut left: Vec<String> = fs::read_dir(&s.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["cargo", "notes.txt"]);
    assert!(s.sluice(&["shim", "uninstall"]).status.success());

    // --force replaces the foreign shim name, never the unrelated file.
    let out = s.sluice(&["shim", "install", "--force"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        fs::read_to_string(s.dir.join("cargo"))
            .unwrap()
            .contains("# sluice-shim v1")
    );
    assert_eq!(
        fs::read_to_string(s.dir.join("notes.txt")).unwrap(),
        "keep me\n"
    );

    // A command dropped from the heavy list loses its shim on the next install.
    fs::write(
        s.sb.home.join(".config/sluice/config.toml"),
        "version = 1\n[agent]\nheavy_commands = [\"make\"]\n",
    )
    .unwrap();
    let out = s.sluice(&["shim", "install"]);
    assert!(out.status.success(), "{out:?}");
    let mut left: Vec<String> = fs::read_dir(&s.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["make", "notes.txt", "npx"]);
    fs::remove_file(s.dir.join("notes.txt")).unwrap();
    assert!(s.sluice(&["shim", "uninstall"]).status.success());
    assert!(!s.dir.exists(), "the empty directory was kept");
}

#[test]
fn status_reports_path_order_and_this_shells_mode() {
    let s = setup(4, "");
    let out = StdCommand::new(Sandbox::binary())
        .args(["shim", "status", "--json"])
        .current_dir(&s.sb.repo)
        .env_clear()
        .envs(s.sb.env())
        .env("PATH", Shims::path(&[&s.bin, &s.dir]))
        .env("SLUICE_SHIMS", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["in_path"], true);
    assert_eq!(v["path_index"], 1);
    assert_eq!(v["this_shell"]["queue"], true);
    let make = v["shims"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "make")
        .unwrap()
        .clone();
    // The fake make comes first in PATH: the shim is shadowed.
    assert_eq!(make["active"], false);
    assert_eq!(make["real"], s.bin.join("make").display().to_string());
    // Text form.
    let out = StdCommand::new(Sandbox::binary())
        .args(["shim", "status"])
        .current_dir(&s.sb.repo)
        .env_clear()
        .envs(s.sb.env())
        .env("PATH", s.default_path())
        .output()
        .unwrap();
    let text = stdout(&out);
    assert!(text.contains("in PATH:    yes (entry 1 of PATH)"), "{text}");
    assert!(
        text.contains("commands run directly (human session)"),
        "{text}"
    );
}
