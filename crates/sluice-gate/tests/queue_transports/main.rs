//! The merge queue against authenticated remotes (alpha plan task H2),
//! driven through the real `sluice` binary like `queue_e2e.rs`:
//!
//! - SSH: an unprivileged `sshd` on a free high port (generated host key,
//!   `AuthorizedKeysFile` with the test's client keys, `StrictModes no`,
//!   `UsePAM no`), keys held by a test `ssh-agent`, remote
//!   `ssh://<user>@127.0.0.1:<port>/<bare>`, `GIT_SSH_COMMAND` with a
//!   private `UserKnownHostsFile` and `StrictHostKeyChecking=accept-new`.
//!   Skipped (not failed) with a printed reason when OpenSSH is missing.
//! - Smart HTTP: `git http-backend` behind a Python `http.server` with basic
//!   auth (`git_http_server.py`), remote `http://127.0.0.1:<port>/remote.git`,
//!   credentials from a `GIT_ASKPASS` helper.
//!
//! Credentials only ever reach the daemon through the client env allowlist
//! (`SSH_AUTH_SOCK`, `GIT_SSH_COMMAND`, `GIT_ASKPASS`, ...): the daemon itself
//! is started by a credential-free CLI call, and every case greps the queue
//! database, the daemon log, `client.log`, `sluice doctor --bundle` and the
//! CLI output for the private keys and passwords.
//!
//! Evidence: `kb/sluice/changes/queue-transports.md`.

#[path = "../common/mod.rs"]
mod common;
mod servers;

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::Value;
use servers::{Creds, HttpRemote, SshRemote};

/// Upper bound of every wait.
const WAIT: Duration = Duration::from_secs(40);

/// `[queue] push_timeout` of the sandbox repository.
const PUSH_TIMEOUT: Duration = Duration::from_secs(12);

/// One cacheable gate; small network and push deadlines.
const CONFIG: &str = r#"
[gates.test]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\""
hooks = ["pre-push"]

[queue]
push_timeout = "12s"
network_timeout = "5s"
"#;

const GATE_ONLY: &str = r#"version = 1
[gates.test]
cmd = "true"
hooks = ["pre-push"]
"#;

// ------------------------------------------------------------------ harness

/// A sandbox whose bare remote `srv/remote.git` is served over a transport.
struct Q {
    sb: Sandbox,
    root: PathBuf,
    srv: PathBuf,
    bare: PathBuf,
    /// Every CLI output, grepped for secrets too.
    outputs: RefCell<Vec<String>>,
}

impl Q {
    fn new() -> Self {
        let sb = Sandbox::new(CONFIG);
        let root = sb.root.path().to_path_buf();
        let srv = root.join("srv");
        fs::create_dir_all(&srv).expect("srv");
        let bare = srv.join("remote.git");
        fs::write(
            sb.home.join(".config/sluice/config.toml"),
            "version = 1\n[queue]\ntick = \"200ms\"\n",
        )
        .expect("user config");
        let repo = sb.repo.display().to_string();
        sb.git_in(
            &root,
            &["clone", "-q", "--bare", &repo, bare.to_str().unwrap()],
        );
        Self {
            sb,
            root,
            srv,
            bare,
            outputs: RefCell::new(Vec::new()),
        }
    }

    /// `origin` = `url`, with `origin/main` and `origin/HEAD` set locally
    /// (no network), then the daemon is started by a credential-free call.
    fn set_origin(&self, url: &str) {
        self.sb.git(&["remote", "add", "origin", url]);
        self.sb
            .git(&["update-ref", "refs/remotes/origin/main", "refs/heads/main"]);
        self.sb.git(&["remote", "set-head", "origin", "main"]);
        self.status();
    }

    fn run_in(&self, dir: &Path, args: &[&str], creds: &Creds) -> Output {
        let out = self
            .sb
            .sluice_in(dir)
            .args(args)
            .envs(creds.iter().map(|(k, v)| (k, v)))
            .output()
            .expect("sluice");
        self.record(&out);
        out
    }

    fn record(&self, out: &Output) {
        self.outputs.borrow_mut().push(show(out));
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.sb.repo, args, &Vec::new())
    }

    fn status(&self) -> Value {
        self.status_in(&self.sb.repo)
    }

    /// `queue status --json` shows the repository of the current directory.
    fn status_in(&self, dir: &Path) -> Value {
        let out = self.run_in(dir, &["queue", "status", "--json"], &Vec::new());
        assert!(out.status.success(), "queue status: {}", show(&out));
        serde_json::from_slice(&out.stdout).expect("status --json")
    }

    fn wait(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        self.wait_in(&self.sb.repo, what, pred)
    }

    fn wait_in(&self, dir: &Path, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let st = self.status_in(dir);
            if pred(&st) {
                return st;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {st:#}\ndaemon log:\n{}",
                self.daemon_log()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn wait_ready(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        self.wait_ready_in(&self.sb.repo, what, pred)
    }

    fn wait_ready_in(&self, dir: &Path, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let st = self.wait_in(dir, what, |v| {
            trains(v).any(|t| t["state"] == "ready" && pred(t))
        });
        trains(&st)
            .find(|t| t["state"] == "ready" && pred(t))
            .cloned()
            .expect("ready train")
    }

    /// A linked worktree on a new branch from `main` with one commit.
    fn worktree(&self, repo: &Path, branch: &str, file: &str) -> PathBuf {
        let wt = self.root.join(format!("wt-{}", branch.replace('/', "-")));
        self.sb.git_in(
            repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                wt.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt.join(file), format!("{branch}\n")).expect("write");
        self.sb.git_in(&wt, &["add", "-A"]);
        self.sb.git_in(&wt, &["commit", "-q", "-m", branch]);
        wt
    }

    /// `sluice queue enqueue` in `dir` with `creds`; returns the job id.
    fn enqueue(&self, dir: &Path, creds: &Creds) -> String {
        let out = self.run_in(dir, &["queue", "enqueue"], creds);
        assert!(out.status.success(), "enqueue failed: {}", show(&out));
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let start = text.find("(job ").expect("job id") + 5;
        text[start..].split(')').next().expect("job id").to_owned()
    }

    /// `sluice queue approve --yes` with `creds`; output and duration.
    fn approve(&self, train: &Value, creds: &Creds) -> (Output, Duration) {
        self.approve_in(&self.sb.repo, train, creds)
    }

    fn approve_in(&self, dir: &Path, train: &Value, creds: &Creds) -> (Output, Duration) {
        let started = Instant::now();
        let out = self.run_in(dir, &["queue", "approve", train_id(train), "--yes"], creds);
        (out, started.elapsed())
    }

    /// `sluice queue approve --yes` as a background process.
    fn spawn_approve(&self, train: &Value, creds: &Creds) -> Child {
        StdCommand::new(Sandbox::binary())
            .args(["queue", "approve", train_id(train), "--yes"])
            .current_dir(&self.sb.repo)
            .env_clear()
            .envs(self.sb.env())
            .envs(creds.iter().map(|(k, v)| (k, v)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn approve")
    }

    fn reconcile(&self) -> String {
        let out = self.run(&["queue", "reconcile"]);
        assert!(out.status.success(), "reconcile: {}", show(&out));
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn bare_ref(&self, r: &str) -> Option<String> {
        let out = StdCommand::new("git")
            .args(["--git-dir", self.bare.to_str().unwrap()])
            .args(["rev-parse", "--verify", "--quiet", r])
            .env_clear()
            .envs(self.sb.env())
            .output()
            .expect("git");
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    fn state_dir(&self) -> PathBuf {
        self.sb.home.join("state/sluice")
    }

    fn daemon_log(&self) -> String {
        fs::read_to_string(self.state_dir().join("daemon.log")).unwrap_or_default()
    }

    /// A second repository with a plain local bare remote, for "the runner
    /// keeps serving other repositories".
    fn other_repo(&self) -> PathBuf {
        let repo = self.root.join("repo2");
        let bare = self.root.join("remote2.git");
        fs::create_dir_all(&repo).expect("repo2");
        let git = |args: &[&str]| self.sb.git_in(&repo, args);
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.com"]);
        fs::write(repo.join(".sluice.toml"), GATE_ONLY).expect("config");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        self.sb.git_in(
            &self.root,
            &[
                "clone",
                "-q",
                "--bare",
                repo.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        git(&["remote", "add", "origin", bare.to_str().unwrap()]);
        git(&["fetch", "-q", "origin"]);
        git(&["remote", "set-head", "origin", "main"]);
        repo
    }

    /// No secret anywhere Sluice writes or prints: the state directory
    /// (queue database with its WAL, `daemon.log`, `logs/client.log`, job
    /// logs), the extracted `sluice doctor --bundle` and every CLI output.
    fn assert_no_leaks(&self, secrets: &[String], creds: &Creds) {
        assert_ne!(secrets.len(), 0, "no secret to look for");
        // One `sluice run` (with the credentials in its env) so client.log
        // has a line.
        let out = self.run_in(&self.sb.repo, &["run", "test"], creds);
        assert!(out.status.success(), "sluice run: {}", show(&out));
        let state = self.state_dir();
        assert!(state.join("sluice.db").is_file(), "no queue database");
        assert!(state.join("logs/client.log").is_file(), "no client.log");
        assert!(
            self.daemon_log().contains("train claimed"),
            "the daemon log is not where the test looks:\n{}",
            self.daemon_log()
        );
        let mut files = Vec::new();
        collect_files(&state, &mut files);

        let bundle = self.root.join("bundle.tar.gz");
        let out = self.run_in(
            &self.sb.repo,
            &["doctor", "--bundle", bundle.to_str().unwrap()],
            creds,
        );
        assert!(bundle.is_file(), "doctor --bundle: {}", show(&out));
        let extract = self.root.join("bundle");
        fs::create_dir_all(&extract).expect("bundle dir");
        let st = StdCommand::new("tar")
            .arg("-xzf")
            .arg(&bundle)
            .arg("-C")
            .arg(&extract)
            .status()
            .expect("tar");
        assert!(st.success());
        let before = files.len();
        collect_files(&extract, &mut files);
        assert!(files.len() > before, "empty bundle");

        for f in &files {
            let bytes = fs::read(f).unwrap_or_default();
            for s in secrets {
                assert!(
                    !contains(&bytes, s.as_bytes()),
                    "{} leaks a credential ({}…)",
                    f.display(),
                    &s[..8]
                );
            }
        }
        for (i, text) in self.outputs.borrow().iter().enumerate() {
            for s in secrets {
                assert!(
                    !text.contains(s.as_str()),
                    "CLI output #{i} leaks a credential: {text}"
                );
            }
        }
        eprintln!(
            "[evidence] no credential in {} files (state dir + doctor bundle) and {} CLI outputs",
            files.len(),
            self.outputs.borrow().len()
        );
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_files(&path, out);
        } else if ft.is_file() {
            out.push(path);
        }
    }
}

fn show(out: &Output) -> String {
    format!(
        "exit {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn trains(v: &Value) -> impl Iterator<Item = &Value> {
    v["trains"].as_array().into_iter().flatten()
}

fn train<'a>(v: &'a Value, id: &str) -> &'a Value {
    trains(v)
        .find(|t| t["train_id"] == id)
        .unwrap_or_else(|| panic!("no train {id}: {v:#}"))
}

fn train_id(t: &Value) -> &str {
    t["train_id"].as_str().expect("train_id")
}

fn job<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|j| j["job_id"] == id)
        .unwrap_or_else(|| panic!("no job {id}: {v:#}"))
}

fn members(t: &Value) -> Vec<String> {
    t["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str().map(str::to_owned))
        .collect()
}

fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn evidence(label: &str, text: &str) {
    eprintln!("[evidence] {label}:\n{}", text.trim_end());
}

/// On drop (also when the test panics), aborts a `pre-receive` hook still
/// blocked on the FIFO so no push outlives the test.
struct Unblock(PathBuf);

impl Drop for Unblock {
    fn drop(&mut self) {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        if let Ok(mut f) = fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&self.0)
        {
            let _ = f.write_all(b"abort\n");
        }
    }
}

// ------------------------------------------------------------------ transports

/// The transport under test, with its credentials sets.
struct Remote {
    url: String,
    good: Creds,
    wrong: Creds,
    stall: Creds,
    secrets: Vec<String>,
    server: Server,
}

enum Server {
    Ssh(SshRemote),
    Http(HttpRemote),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Ssh,
    Http,
}

/// Starts the servers for `q`'s bare remote and points `origin` at them;
/// `None` when the transport is unavailable (the test is skipped).
fn remote(kind: Kind, q: &Q) -> Option<Remote> {
    let r = match kind {
        Kind::Ssh => {
            let s = servers::ssh_remote(&q.root.join("ssh"), &q.bare)?;
            Remote {
                url: s.url.clone(),
                good: s.good.clone(),
                wrong: s.wrong.clone(),
                stall: s.stall.clone(),
                secrets: s.secrets.clone(),
                server: Server::Ssh(s),
            }
        }
        Kind::Http => {
            let h = servers::http_remote(&q.root.join("http"), &q.srv, "remote.git")?;
            Remote {
                url: h.url.clone(),
                good: h.good.clone(),
                wrong: h.wrong.clone(),
                stall: h.stall.clone(),
                secrets: h.secrets.clone(),
                server: Server::Http(h),
            }
        }
    };
    q.set_origin(&r.url);
    Some(r)
}

impl Remote {
    fn server_log(&self) -> String {
        match &self.server {
            Server::Ssh(s) => fs::read_to_string(&s.sshd.log).unwrap_or_default(),
            Server::Http(_) => String::new(),
        }
    }
}

// ------------------------------------------------------------------ cases

/// Enqueue → ready → `approve` with the approver's credentials → the
/// landing commit and the audit ref are on the remote.
fn lands(kind: Kind) {
    let q = Q::new();
    let Some(r) = remote(kind, &q) else { return };
    let base = q.bare_ref("refs/heads/main").unwrap();
    let wa = q.worktree(&q.sb.repo, "feat/a", "a1.txt");
    let ja = q.enqueue(&wa, &r.good);
    let t = q.wait_ready("ready", |_| true);
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);
    let (out, took) = q.approve(&t, &r.good);
    evidence(&format!("{kind:?} approve ({took:?})"), &show(&out));
    assert!(
        out.status.success(),
        "{}\nserver log:\n{}",
        show(&out),
        r.server_log()
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("sluice: landed "));
    let st = q.status();
    let s = train(&st, train_id(&t))["merge_sha"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), s);
    assert_eq!(
        q.bare_ref(&format!("refs/sluice/landed/{s}")).unwrap(),
        s,
        "audit ref"
    );
    assert_eq!(job(&st, &ja)["state"], "landed");
    q.assert_no_leaks(&r.secrets, &r.good);
}

/// Approving with credentials the remote refuses is a definitive failure
/// reported by `approve` at once (jobs `failed`, `push_rejected`), never a
/// hang: before the fix the pre-push `ls-remote` failure kept the train
/// `approved` forever and `approve` waited for its whole deadline. Over
/// HTTP with anonymous reads the refusal comes from the push itself, which
/// used to end `needs_reconcile`.
fn wrong_credentials(kind: Kind) {
    let q = Q::new();
    let Some(r) = remote(kind, &q) else { return };
    let base = q.bare_ref("refs/heads/main").unwrap();
    let wa = q.worktree(&q.sb.repo, "feat/a", "a1.txt");
    let ja = q.enqueue(&wa, &r.good);
    let t = q.wait_ready("ready", |_| true);

    let refused = |t: &Value, label: &str| {
        let (out, took) = q.approve(t, &r.wrong);
        evidence(&format!("{kind:?} {label} ({took:?})"), &show(&out));
        assert!(!out.status.success(), "{}", show(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("push_rejected"), "{}", show(&out));
        assert!(
            took < Duration::from_secs(20),
            "approve took {took:?}: {}",
            show(&out)
        );
        let st = q.status();
        let j = job(&st, &ja);
        assert_eq!(j["state"], "failed", "{st:#}");
        assert_eq!(j["reason"], "push_rejected", "{st:#}");
        assert_eq!(train(&st, train_id(t))["state"], "dissolved", "{st:#}");
        assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);
    };
    refused(&t, "approve with the wrong credentials (pre-push check)");

    if let Server::Http(h) = &r.server {
        // Reads without credentials: the pre-push check passes and the push
        // itself gets the 401.
        h.set_anonymous_read(true);
        let out = q.run_in(&q.sb.repo, &["queue", "retry", &ja], &Vec::new());
        assert!(out.status.success(), "retry: {}", show(&out));
        let t2 = q.wait_ready("ready again", |x| x["train_id"] != t["train_id"]);
        refused(&t2, "approve with the wrong password (push)");
        let st = q.status();
        let out = job(&st, &ja)["detail"]["output"]
            .as_str()
            .unwrap_or_default();
        assert!(out.contains("Authentication failed"), "{st:#}");
    }
    q.assert_no_leaks(&r.secrets, &r.wrong);
}

/// A remote that accepts the connection and never answers the push: the
/// push is bounded by `push_timeout` (`needs_reconcile`, reported by
/// `approve`), another repository's train lands meanwhile, reconcile over
/// the transport proves nothing landed (`ready`) and a re-approval with
/// working credentials lands.
fn stalled_push(kind: Kind) {
    let q = Q::new();
    let Some(r) = remote(kind, &q) else { return };
    let base = q.bare_ref("refs/heads/main").unwrap();
    let wa = q.worktree(&q.sb.repo, "feat/a", "a1.txt");
    let ja = q.enqueue(&wa, &r.good);
    let t = q.wait_ready("ready", |_| true);
    let id = train_id(&t).to_owned();

    let started = Instant::now();
    let approver = q.spawn_approve(&t, &r.stall);
    q.wait("pushing", |v| train(v, &id)["state"] == "pushing");

    // The runner keeps serving another repository meanwhile.
    let repo2 = q.other_repo();
    let wz = q.worktree(&repo2, "feat/z", "z.txt");
    let jz = q.enqueue(&wz, &Vec::new());
    let tz = q.wait_ready_in(&repo2, "the other repository's train", |x| {
        members(x) == vec![jz.clone()]
    });
    let (out, _) = q.approve_in(&repo2, &tz, &Vec::new());
    assert!(out.status.success(), "{}", show(&out));
    let st = q.status_in(&repo2);
    assert_eq!(train(&st, train_id(&tz))["state"], "landed", "{st:#}");
    let st = q.status();
    assert_eq!(
        train(&st, &id)["state"],
        "pushing",
        "the other train landed while this push stalled: {st:#}"
    );

    let out = approver.wait_with_output().expect("approve");
    let took = started.elapsed();
    q.record(&out);
    evidence(
        &format!("{kind:?} approve on a stalled push ({took:?})"),
        &show(&out),
    );
    assert!(!out.status.success(), "{}", show(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("needs reconcile"),
        "{}",
        show(&out)
    );
    assert!(
        took < PUSH_TIMEOUT + Duration::from_secs(15),
        "approve took {took:?}"
    );
    let st = q.status();
    let tr = train(&st, &id);
    assert_eq!(tr["state"], "needs_reconcile", "{st:#}");
    assert!(
        tr["note"]
            .as_str()
            .unwrap_or_default()
            .contains("push_timeout"),
        "{st:#}"
    );
    assert_eq!(job(&st, &ja)["state"], "needs_reconcile");

    // Reconcile reads the remote with the stored approver env: nothing
    // landed, the train is pushable again (approval voided).
    let out = q.reconcile();
    evidence(&format!("{kind:?} reconcile after the stall"), &out);
    assert!(out.contains(&format!("train {id}: ready")), "{out}");
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);
    let t = q.wait_ready("ready after reconcile", |x| x["train_id"] == id.as_str());
    let (out, _) = q.approve(&t, &r.good);
    assert!(out.status.success(), "{}", show(&out));
    let s = t["merge_sha"].as_str().unwrap();
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), s);
    q.assert_no_leaks(&r.secrets, &r.stall);
}

/// SIGKILL of the daemon while `git push` is blocked in the remote's
/// `pre-receive` hook (behind the transport); the restarted daemon parks
/// the train, reconcile refuses while the push lives and, once the hook is
/// released, reads the remote over the transport and decides `landed`.
fn reconcile_after_killed_push(kind: Kind) {
    let q = Q::new();
    let Some(r) = remote(kind, &q) else { return };
    let fifo = q.root.join("fifo");
    let pid_file = q.root.join("hook.pid");
    let done_file = q.root.join("hook.done");
    let st = StdCommand::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(st.success());
    let hook = q.bare.join("hooks/pre-receive");
    fs::create_dir_all(hook.parent().unwrap()).expect("hooks");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\necho $$ > '{pid}.tmp' && mv '{pid}.tmp' '{pid}'\nv=$(cat '{fifo}')\n\
             touch '{done}'\n[ \"$v\" = go ]\n",
            pid = pid_file.display(),
            fifo = fifo.display(),
            done = done_file.display()
        ),
    )
    .expect("hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
    let _unblock = Unblock(fifo.clone());

    let base = q.bare_ref("refs/heads/main").unwrap();
    let wa = q.worktree(&q.sb.repo, "feat/a", "a1.txt");
    let ja = q.enqueue(&wa, &r.good);
    let t = q.wait_ready("ready", |_| true);
    let id = train_id(&t).to_owned();
    let s = t["merge_sha"].as_str().unwrap().to_owned();
    let daemon_pid = q.sb.status_json()["daemon"]["pid"].as_u64().unwrap();

    let mut approver = q.spawn_approve(&t, &r.good);
    wait_for("the push to block in pre-receive", || pid_file.exists());
    assert_eq!(train(&q.status(), &id)["state"], "pushing");
    let _ = approver.kill();
    let _ = approver.wait();
    kill(
        Pid::from_raw(i32::try_from(daemon_pid).unwrap()),
        Signal::SIGKILL,
    )
    .expect("SIGKILL the daemon");
    wait_for("the daemon to die", || {
        !Path::new(&format!("/proc/{daemon_pid}")).exists()
            || fs::read_to_string(format!("/proc/{daemon_pid}/stat"))
                .is_ok_and(|s| s.contains(") Z "))
    });

    // A credential-free CLI call starts the new daemon, which parks the train.
    let st = q.wait("needs_reconcile after restart", |v| {
        train(v, &id)["state"] == "needs_reconcile"
    });
    assert_eq!(job(&st, &ja)["state"], "needs_reconcile");
    let out = q.reconcile();
    evidence(&format!("{kind:?} reconcile while pushing"), &out);
    assert!(out.contains("refused"), "{out}");
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);

    fs::write(&fifo, "go\n").expect("release the hook");
    wait_for("the hook to end", || done_file.exists());
    let deadline = Instant::now() + WAIT;
    let out = loop {
        let out = q.reconcile();
        if !out.contains("refused") {
            break out;
        }
        assert!(Instant::now() < deadline, "reconcile kept refusing: {out}");
        thread::sleep(Duration::from_millis(100));
    };
    evidence(&format!("{kind:?} reconcile after the hook ended"), &out);
    assert!(out.contains(&format!("train {id}: landed")), "{out}");
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), s);
    let st = q.wait("landed", |v| train(v, &id)["state"] == "landed");
    assert_eq!(job(&st, &ja)["state"], "landed");
    q.assert_no_leaks(&r.secrets, &r.good);
}

// ------------------------------------------------------------------ tests

#[test]
fn ssh_push_lands_with_the_approver_agent() {
    lands(Kind::Ssh);
}

#[test]
fn http_push_lands_with_the_approver_askpass() {
    lands(Kind::Http);
}

#[test]
fn ssh_wrong_key_fails_definitively_without_hanging() {
    wrong_credentials(Kind::Ssh);
}

#[test]
fn http_wrong_password_fails_definitively_without_hanging() {
    wrong_credentials(Kind::Http);
}

#[test]
fn ssh_stalled_push_is_bounded_and_other_repositories_keep_landing() {
    stalled_push(Kind::Ssh);
}

#[test]
fn http_stalled_push_is_bounded_and_other_repositories_keep_landing() {
    stalled_push(Kind::Http);
}

#[test]
fn ssh_reconcile_after_a_killed_push() {
    reconcile_after_killed_push(Kind::Ssh);
}

#[test]
fn http_reconcile_after_a_killed_push() {
    reconcile_after_killed_push(Kind::Http);
}

/// A password embedded in the remote URL (`http://user:pw@host/...`) never
/// reaches the queue's state: neither the failing base fetch's error
/// (recorded in the job detail and the daemon log; it used to carry the
/// whole git command line, URL included) nor a successful landing.
#[test]
fn http_password_in_the_remote_url_never_leaks() {
    let q = Q::new();
    let Some(h) = servers::http_remote(&q.root.join("http"), &q.srv, "remote.git") else {
        return;
    };
    let with_password = |pw: &str| format!("http://carol:{pw}@127.0.0.1:{}/remote.git", h.port);
    q.set_origin(&with_password(servers::WRONG_PASSWORD));
    let wa = q.worktree(&q.sb.repo, "feat/a", "a1.txt");
    let ja = q.enqueue(&wa, &Vec::new());
    let st = q.wait("the base fetch to fail for good", |v| {
        job(v, &ja)["state"] == "failed"
    });
    let j = job(&st, &ja);
    evidence("base fetch failure", &j.to_string());
    assert_eq!(j["reason"], "base_unavailable", "{st:#}");
    assert!(j.to_string().contains("127.0.0.1"), "{st:#}");

    q.sb.git(&[
        "remote",
        "set-url",
        "origin",
        &with_password(&h.carol_password),
    ]);
    let out = q.run(&["queue", "retry", &ja]);
    assert!(out.status.success(), "retry: {}", show(&out));
    let t = q.wait_ready("ready", |_| true);
    let (out, _) = q.approve(&t, &Vec::new());
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(
        q.bare_ref("refs/heads/main").unwrap(),
        t["merge_sha"].as_str().unwrap()
    );
    q.assert_no_leaks(&h.secrets, &Vec::new());
}
