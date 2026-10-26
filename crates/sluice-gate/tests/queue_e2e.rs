//! Acceptance tests of the local merge queue (`kb/sluice/design/merge-queue.md`
//! section 13) driven through the real `sluice` binary: the CLI autostarts the
//! daemon (with the queue extension), every sandbox has its own repository,
//! private HOME/XDG directories and a local bare remote, and test 6 talks to
//! `sluice mcp` through an rmcp child-process client.
//!
//! Evidence and coverage notes: `kb/sluice/changes/phase-6-acceptance.md`.

mod common;

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::Sandbox;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use sluice_gate_proto::{ApproveVia, ClientMessage, ErrorCode, ServerMessage};

/// Upper bound of every wait (a loaded CI machine runs these in parallel).
const WAIT: Duration = Duration::from_secs(30);

/// One cacheable gate that counts its real runs.
const GATE: &str = r#"
[gates.test]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\""
hooks = ["pre-push"]
"#;

/// A sandbox with a bare remote `origin` (a `git clone --bare` of the repo).
struct Q {
    sb: Sandbox,
    root: PathBuf,
    bare: PathBuf,
}

impl Q {
    fn new(config: &str) -> Self {
        Self::with_user(config, "")
    }

    /// `user` is appended to the user config's `[queue]` table.
    fn with_user(config: &str, user: &str) -> Self {
        let sb = Sandbox::new(config);
        let root = sb.root.path().to_path_buf();
        let q = Self {
            bare: root.join("remote.git"),
            root,
            sb,
        };
        q.user_config(&format!("tick = \"200ms\"\n{user}"));
        let bare = q.bare.display().to_string();
        let repo = q.sb.repo.display().to_string();
        q.sb.git_in(&q.root, &["clone", "-q", "--bare", &repo, &bare]);
        q.sb.git(&["remote", "add", "origin", &bare]);
        q.sb.git(&["fetch", "-q", "origin"]);
        q.sb.git(&["remote", "set-head", "origin", "main"]);
        q
    }

    fn user_config(&self, queue: &str) {
        self.sb.write_user_config(&format!("[queue]\n{queue}"));
    }

    /// A linked worktree on a new branch from `main` with one commit.
    fn worktree(&self, branch: &str, files: &[(&str, &str)]) -> (PathBuf, String) {
        let wt = self.root.join(format!("wt-{}", branch.replace('/', "-")));
        self.sb.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            wt.to_str().unwrap(),
            "main",
        ]);
        for (path, body) in files {
            fs::write(wt.join(path), body).expect("write");
        }
        self.sb.git_in(&wt, &["add", "-A"]);
        self.sb.git_in(&wt, &["commit", "-q", "-m", branch]);
        let sha = self.sb.git_in(&wt, &["rev-parse", "HEAD"]);
        (wt, sha)
    }

    /// `sluice queue enqueue` in `dir`; returns the job id.
    fn enqueue(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.run_in(dir, &[&["queue", "enqueue"], args].concat());
        assert!(out.status.success(), "enqueue failed: {}", show(&out));
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let start = text.find("(job ").expect("job id") + 5;
        text[start..].split(')').next().expect("job id").to_owned()
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.sb.sluice_in(dir).args(args).output().expect("sluice")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.sb.repo, args)
    }

    fn status(&self) -> Value {
        let out = self.run(&["queue", "status", "--json"]);
        assert!(out.status.success(), "queue status: {}", show(&out));
        serde_json::from_slice(&out.stdout).expect("status --json")
    }

    fn status_text(&self) -> String {
        let out = self.run(&["queue", "status"]);
        assert!(out.status.success(), "queue status: {}", show(&out));
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Polls `queue status --json` until `pred` holds.
    fn wait(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let st = self.status();
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
        let st = self.wait(what, |v| {
            trains(v).any(|t| t["state"] == "ready" && pred(t))
        });
        trains(&st)
            .find(|t| t["state"] == "ready" && pred(t))
            .cloned()
            .expect("ready train")
    }

    fn approve(&self, train: &Value) -> Output {
        self.run(&["queue", "approve", train_id(train), "--yes"])
    }

    /// `sluice queue approve --yes` with `extra` on top of the sandbox
    /// environment (the approver's `PATH`, `CI`, ...).
    fn approve_with(&self, train: &Value, extra: &[(&str, &str)]) -> Output {
        StdCommand::new(Sandbox::binary())
            .args(["queue", "approve", train_id(train), "--yes"])
            .current_dir(&self.sb.repo)
            .env_clear()
            .envs(self.sb.env())
            .envs(extra.iter().copied())
            .output()
            .expect("approve")
    }

    fn reconcile(&self) -> String {
        let out = self.run(&["queue", "reconcile"]);
        assert!(out.status.success(), "reconcile: {}", show(&out));
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn bare_ref(&self, r: &str) -> Option<String> {
        git_dir_ref(&self.sb, &self.bare, r)
    }

    fn daemon_log(&self) -> String {
        fs::read_to_string(self.sb.home.join("state/sluice/daemon.log")).unwrap_or_default()
    }

    /// A hook of the repository (`.git/hooks/<name>`).
    fn repo_hook(&self, name: &str, body: &str) {
        hook(&self.sb.repo.join(".git/hooks").join(name), body);
    }

    /// A hook of the bare remote (`remote.git/hooks/<name>`).
    fn bare_hook(&self, name: &str, body: &str) {
        hook(&self.bare.join("hooks").join(name), body);
    }

    /// Commits `files` on `main` in a separate clone of the bare remote and pushes it.
    fn move_remote_main(&self, name: &str, files: &[(&str, &str)]) -> String {
        let other = self.root.join(name);
        let bare = self.bare.display().to_string();
        self.sb
            .git_in(&self.root, &["clone", "-q", &bare, other.to_str().unwrap()]);
        for (path, body) in files {
            fs::write(other.join(path), body).expect("write");
        }
        self.sb.git_in(&other, &["add", "-A"]);
        self.sb.git_in(
            &other,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", name],
        );
        self.sb.git_in(&other, &["push", "-q", "origin", "main"]);
        self.bare_ref("refs/heads/main").expect("moved main")
    }

    /// `sluice queue approve --yes` as a background process.
    fn spawn_approve(&self, train: &Value) -> Child {
        StdCommand::new(Sandbox::binary())
            .args(["queue", "approve", train_id(train), "--yes"])
            .current_dir(&self.sb.repo)
            .env_clear()
            .envs(self.sb.env())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn approve")
    }
}

fn hook(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).expect("hooks dir");
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("hook");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn git_dir_ref(sb: &Sandbox, git_dir: &Path, r: &str) -> Option<String> {
    let out = StdCommand::new("git")
        .args(["--git-dir", git_dir.to_str().unwrap()])
        .args(["rev-parse", "--verify", "--quiet", r])
        .env_clear()
        .envs(sb.env())
        .output()
        .expect("git");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Prints evidence for the acceptance note (`--nocapture` shows it).
fn evidence(label: &str, text: &str) {
    eprintln!("[evidence] {label}:\n{}", text.trim_end());
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

fn job_is(v: &Value, id: &str, state: &str) -> bool {
    v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|j| j["job_id"] == id && j["state"] == state)
}

fn members(t: &Value) -> Vec<String> {
    t["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str().map(str::to_owned))
        .collect()
}

fn mkfifo(path: &Path) {
    let st = StdCommand::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo");
    assert!(st.success());
}

fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !pred() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// `git merge-base --is-ancestor a b` in a git dir.
fn is_ancestor(sb: &Sandbox, git_dir: &Path, a: &str, b: &str) -> bool {
    StdCommand::new("git")
        .args(["--git-dir", git_dir.to_str().unwrap()])
        .args(["merge-base", "--is-ancestor", a, b])
        .env_clear()
        .envs(sb.env())
        .stderr(Stdio::null())
        .status()
        .expect("git")
        .success()
}

// ------------------------------------------------------------------ test 1

/// Two worktrees, B conflicts with A: A is merged and waits for approval,
/// nothing is pushed until `approve`, A lands (with the audit ref, through
/// the repository's `pre-push` hook), B is never pushed.
#[test]
fn acceptance_1_conflict_ready_no_push_before_approval_then_landing() {
    let q = Q::new(GATE);
    let witness = q.root.join("pre-push-witness");
    q.repo_hook(
        "pre-push",
        &format!("cat > '{}'\nexit 0", witness.display()),
    );
    let base = q.bare_ref("refs/heads/main").unwrap();
    // A ready train of a placeholder job holds the target while A and B
    // enqueue; cancelling it lets the runner claim A and B as one train.
    let (wx, _x) = q.worktree("feat/x", &[("x.txt", "x\n")]);
    let (wa, _a) = q.worktree("feat/a", &[("a.txt", "from A\n"), ("y.txt", "y\n")]);
    let (wb, b) = q.worktree("feat/b", &[("a.txt", "from B\n")]);
    let jx = q.enqueue(&wx, &[]);
    q.wait_ready("placeholder ready", |_| true);
    let ja = q.enqueue(&wa, &[]);
    let jb = q.enqueue(&wb, &[]);
    let out = q.run(&["queue", "cancel", &jx]);
    assert!(out.status.success(), "{}", show(&out));

    let t = q.wait_ready("train with A ready", |t| {
        members(t) == vec![ja.clone(), jb.clone()]
    });
    let st = q.status();
    assert!(job_is(&st, &ja, "ready"), "{st:#}");
    let vb = job(&st, &jb);
    assert_eq!(vb["state"], "conflict", "{st:#}");
    assert_eq!(vb["detail"]["paths"], json!(["a.txt"]));
    let text = q.status_text();
    evidence("1 status", &text);
    assert!(text.contains("READY (awaiting approval)"), "{text}");
    assert!(text.contains("CONFLICT a.txt"), "{text}");

    // Nothing is pushed without approval, however long we wait.
    thread::sleep(Duration::from_secs(1));
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);
    assert!(!witness.exists(), "no push attempted before approval");

    let out = q.approve(&t);
    evidence("1 approve", &show(&out));
    assert!(out.status.success(), "{}", show(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("sluice: landed "),
        "{}",
        show(&out)
    );
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
    assert!(job_is(&st, &ja, "landed"));
    assert!(job_is(&st, &jb, "conflict"));
    // The repository's pre-push hook ran during the push.
    let hook_input = fs::read_to_string(&witness).expect("pre-push hook ran");
    assert!(hook_input.contains("refs/heads/main"), "{hook_input}");
    // B never reached the remote.
    assert!(!is_ancestor(&q.sb, &q.bare, &b, "refs/heads/main"));
    let has_b = StdCommand::new("git")
        .args(["--git-dir", q.bare.to_str().unwrap(), "cat-file", "-e", &b])
        .env_clear()
        .envs(q.sb.env())
        .status()
        .unwrap()
        .success();
    assert!(!has_b, "B's commit must not be on the remote");
}

// ------------------------------------------------------------------ test 2

/// Reconcile row 2: once the push is over, a remote that now resolves to
/// another URL is refused (`endpoint_changed`) and nothing is decided.
fn reconcile_refuses_a_changed_endpoint(q: &Q, id: &str) {
    let other = q.root.join("elsewhere.git");
    let bare_url = q.bare.display().to_string();
    q.sb.git_in(
        &q.root,
        &["clone", "-q", "--bare", &bare_url, other.to_str().unwrap()],
    );
    q.sb.git(&["remote", "set-url", "origin", other.to_str().unwrap()]);
    let deadline = Instant::now() + WAIT;
    let out = loop {
        let out = q.reconcile();
        if !out.contains("push_in_progress") {
            break out;
        }
        assert!(Instant::now() < deadline, "push never ended: {out}");
        thread::sleep(Duration::from_millis(100));
    };
    evidence("2b reconcile after set-url", &out);
    assert!(out.contains("refused (endpoint_changed)"), "{out}");
    assert_eq!(train(&q.status(), id)["state"], "needs_reconcile");
    q.sb.git(&["remote", "set-url", "origin", &bare_url]);
}

/// Kills the daemon with SIGKILL while `git push` is blocked in the bare
/// remote's `pre-receive` hook, restarts it through the CLI, and resolves the
/// parked train with `reconcile` after the hook is released (`landed`) or
/// aborted (`ready`, approval voided).
fn kill_during_push(release: bool) {
    let q = Q::new(GATE);
    let fifo = q.root.join("fifo");
    let pid_file = q.root.join("hook.pid");
    let done_file = q.root.join("hook.done");
    mkfifo(&fifo);
    let _unblock = Unblock(fifo.clone());
    q.bare_hook(
        "pre-receive",
        &format!(
            "echo $$ > '{pid}.tmp' && mv '{pid}.tmp' '{pid}'\nv=$(cat '{fifo}')\n\
             touch '{done}'\n[ \"$v\" = go ]",
            pid = pid_file.display(),
            fifo = fifo.display(),
            done = done_file.display()
        ),
    );
    let base = q.bare_ref("refs/heads/main").unwrap();
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let ja = q.enqueue(&wa, &[]);
    let t = q.wait_ready("ready", |_| true);
    let id = train_id(&t).to_owned();
    let s = t["merge_sha"].as_str().unwrap().to_owned();
    let daemon_pid = q.sb.status_json()["daemon"]["pid"].as_u64().unwrap();

    let mut approver = q.spawn_approve(&t);
    wait_for("the push to block in pre-receive", || pid_file.exists());
    let st = q.status();
    assert_eq!(train(&st, &id)["state"], "pushing", "{st:#}");
    // The approving CLI would autostart a daemon while polling; stop it first.
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

    // Any CLI call starts a new daemon, which parks the train.
    let st = q.wait("needs_reconcile after restart", |v| {
        train(v, &id)["state"] == "needs_reconcile"
    });
    assert_ne!(
        q.sb.status_json()["daemon"]["pid"].as_u64().unwrap(),
        daemon_pid
    );
    assert!(job_is(&st, &ja, "needs_reconcile"), "{st:#}");
    let text = q.status_text();
    evidence("2 status after restart", &text);
    assert!(text.contains("NEEDS RECONCILE"), "{text}");
    // The push is still running: reconcile refuses to decide.
    let out = q.reconcile();
    evidence("2 reconcile while pushing", &out);
    assert!(out.contains("refused"), "{out}");
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);

    fs::write(&fifo, if release { "go\n" } else { "abort\n" }).expect("release the hook");
    wait_for("the hook to end", || done_file.exists());
    if !release {
        reconcile_refuses_a_changed_endpoint(&q, &id);
    }
    // Reconcile refuses while the push process lives; it then decides.
    let deadline = Instant::now() + WAIT;
    let out = loop {
        let out = q.reconcile();
        if !out.contains("refused") {
            break out;
        }
        assert!(Instant::now() < deadline, "reconcile kept refusing: {out}");
        thread::sleep(Duration::from_millis(100));
    };
    evidence("2 reconcile after the hook ended", &out);
    if release {
        assert!(out.contains(&format!("train {id}: landed")), "{out}");
        assert_eq!(q.bare_ref("refs/heads/main").unwrap(), s);
        let st = q.wait("landed", |v| train(v, &id)["state"] == "landed");
        assert!(job_is(&st, &ja, "landed"), "{st:#}");
    } else {
        assert!(out.contains(&format!("train {id}: ready")), "{out}");
        assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);
        let st = q.wait("ready again", |v| train(v, &id)["state"] == "ready");
        let tr = train(&st, &id);
        assert!(tr.get("approved_via").is_none(), "approval voided: {tr:#}");
        assert!(job_is(&st, &ja, "ready"), "{st:#}");
    }
    assert!(q.reconcile().contains("no train needs reconciling"));
}

/// On drop (also when the test panics), aborts a `pre-receive` hook still
/// blocked on the FIFO so no `git push` outlives the test.
struct Unblock(PathBuf);

impl Drop for Unblock {
    fn drop(&mut self) {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        // Non-blocking: fails at once (ENXIO) when no hook is reading.
        if let Ok(mut f) = fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(&self.0)
        {
            let _ = f.write_all(b"abort\n");
        }
    }
}

#[test]
fn acceptance_2a_sigkill_during_push_then_hook_released_reconciles_to_landed() {
    kill_during_push(true);
}

#[test]
fn acceptance_2b_sigkill_during_push_then_hook_aborted_reconciles_to_ready() {
    kill_during_push(false);
}

// Variant 2c (marker written, no push spawned) has no outside knob: it is
// covered by `sluice-gate-queue` `tests/queue.rs`
// `marker_without_push_reconciles_to_ready` and
// `marker_without_push_is_parked_at_start_then_ready`.

// ------------------------------------------------------------------ test 3

/// The agent verified the exact tree with `sluice run`; the single-member
/// (fast-forward) train hits the marker and runs no gate.
#[test]
fn acceptance_3_marked_tree_is_ready_without_running_a_gate() {
    let q = Q::new(GATE);
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let out = q.run_in(&wa, &["run", "test"]);
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(q.sb.runs(), 1);
    q.enqueue(&wa, &[]);
    let t = q.wait_ready("ready", |_| true);
    assert_eq!(t["gates"][0]["status"], "cached", "{t:#}");
    assert!(t["gates"][0].get("job_id").is_none(), "{t:#}");
    assert_eq!(q.sb.runs(), 1, "zero gate executions for the train");
}

// ------------------------------------------------------------------ test 4

/// A train's gate and a concurrent `sluice run` of the same gate on the same
/// tree share one execution.
#[test]
fn acceptance_4_train_gate_and_agent_run_coalesce() {
    let q = Q::new(
        r#"
[gates.test]
cmd = "sleep 2; echo run >> \"$SLUICE_TEST_COUNTER\""
hooks = ["pre-push"]
"#,
    );
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let mut agent = q.sb.sluice_in(&wa);
    agent.args(["run", "test"]).timeout(WAIT);
    let agent = thread::spawn(move || agent.output().expect("sluice run"));
    wait_for("the agent's gate to start", || {
        q.sb.socket().exists()
            && q.sb.status_json()["running"]
                .as_array()
                .is_some_and(|r| !r.is_empty())
    });
    q.enqueue(&wa, &[]);
    wait_for("the train to join the running gate", || {
        q.sb.status_json()["running"]
            .as_array()
            .is_some_and(|r| r.iter().any(|j| j["subscribers"] == 2))
    });
    let t = q.wait_ready("ready", |_| true);
    let out = agent.join().expect("agent thread");
    assert!(out.status.success(), "{}", show(&out));
    assert_eq!(q.sb.runs(), 1, "one execution for both requests");
    assert!(
        matches!(t["gates"][0]["status"].as_str(), Some("pass" | "cached")),
        "{t:#}"
    );
}

// ------------------------------------------------------------------ test 5

/// The remote target moves while the train is ready: the train is dissolved
/// and rebuilt, the old plan is refused, the new approval lands on top.
#[test]
fn acceptance_5_base_moved_dissolves_old_plan_is_stale_new_one_lands() {
    let q = Q::new(GATE);
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let ja = q.enqueue(&wa, &[]);
    let t1 = q.wait_ready("ready", |_| true);
    let id1 = train_id(&t1).to_owned();
    let old_hash = t1["train_hash"].as_str().unwrap().to_owned();
    let moved = q.move_remote_main("other", &[("other.txt", "o\n")]);

    let st = q.wait("rebuilt on the moved base", |v| {
        train(v, &id1)["state"] == "dissolved"
            && trains(v).any(|t| t["state"] == "ready" && t["base_sha"] == moved.as_str())
    });
    let t2 = trains(&st)
        .find(|t| t["state"] == "ready")
        .cloned()
        .unwrap();
    assert_eq!(members(&t2), vec![ja.clone()]);
    assert_eq!(job(&st, &ja)["attempts"], 2);

    // The CLI refuses the old train (exit 4) ...
    let out = q.run(&["queue", "approve", &id1, "--yes"]);
    evidence("5 approve old train", &show(&out));
    assert_eq!(out.status.code(), Some(4), "{}", show(&out));
    // ... and the daemon answers `stale-plan` to the old hash.
    let reply = sluice_gate_proto::client::connect(&q.sb.socket())
        .expect("connect")
        .request(&ClientMessage::QueueApprove {
            id: 1,
            train_id: id1.clone(),
            train_hash: old_hash,
            via: ApproveVia::Cli,
            env: std::collections::BTreeMap::new(),
        })
        .expect("reply");
    evidence("5 old hash over the socket", &format!("{reply:?}"));
    assert!(
        matches!(
            reply,
            ServerMessage::Error {
                code: ErrorCode::StalePlan,
                ..
            }
        ),
        "{reply:?}"
    );

    let out = q.approve(&t2);
    assert!(out.status.success(), "{}", show(&out));
    let s = q.bare_ref("refs/heads/main").unwrap();
    assert_eq!(Some(s.as_str()), t2["merge_sha"].as_str());
    assert!(is_ancestor(&q.sb, &q.bare, &moved, &s), "landed on top");
}

// ------------------------------------------------------------------ test 6

type McpClient = RunningService<RoleClient, ()>;

async fn mcp(sb: &Sandbox) -> McpClient {
    let mut cmd = tokio::process::Command::new(Sandbox::binary());
    cmd.arg("mcp")
        .current_dir(&sb.home)
        .env_clear()
        .envs(sb.env());
    let (transport, _stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sluice mcp");
    ().serve(transport).await.expect("initialize")
}

async fn approve_over_mcp(client: &McpClient, q: &Q, train: &str) -> CallToolResult {
    let Value::Object(args) = json!({
        "working_directory": q.sb.repo.display().to_string(),
        "train_id": train,
    }) else {
        unreachable!()
    };
    client
        .call_tool(CallToolRequestParams::new("queue_approve").with_arguments(args))
        .await
        .expect("queue_approve")
}

fn tool_text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `queue_approve` over MCP is refused by default and lands the train once
/// the user config allows agent approval (the daemon reads the user config
/// at start, so it is restarted after the edit).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acceptance_6_mcp_approval_refused_by_default_then_allowed() {
    let q = Q::new(GATE);
    let base = q.bare_ref("refs/heads/main").unwrap();
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    q.enqueue(&wa, &[]);
    let t = q.wait_ready("ready", |_| true);
    let id = train_id(&t).to_owned();

    let client = mcp(&q.sb).await;
    let r = approve_over_mcp(&client, &q, &id).await;
    evidence("6 refused", &tool_text(&r));
    assert_eq!(r.is_error, Some(true), "{}", tool_text(&r));
    assert!(
        tool_text(&r).starts_with("not-allowed: "),
        "{}",
        tool_text(&r)
    );
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);
    assert_eq!(train(&q.status(), &id)["state"], "ready");

    q.user_config("tick = \"200ms\"\nallow_agent_approval = true\n");
    let out = q.run(&["daemon", "stop"]);
    assert!(out.status.success(), "{}", show(&out));
    let r = approve_over_mcp(&client, &q, &id).await;
    evidence("6 allowed", &tool_text(&r));
    assert_ne!(r.is_error, Some(true), "{}", tool_text(&r));
    let s = t["merge_sha"].as_str().unwrap();
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), s);
    let st = q.status();
    assert_eq!(train(&st, &id)["state"], "landed");
    assert_eq!(train(&st, &id)["approved_via"], "mcp");
    client.cancel().await.expect("shutdown");
}

/// Calls an MCP tool with `args`.
async fn mcp_call(client: &McpClient, name: &'static str, args: Value) -> CallToolResult {
    let Value::Object(args) = args else {
        unreachable!()
    };
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args))
        .await
        .expect(name)
}

/// After a conflict the agent is not stuck: `queue_enqueue` answers
/// `duplicate-branch` (the job keeps the branch), `queue_status` names
/// `queue_retry`, which re-reads the rebased branch head and re-queues the
/// job; `queue_cancel` withdraws it. Every MCP request is audited as `mcp`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines, reason = "one scenario, step by step")]
async fn mcp_retry_and_cancel_after_a_conflict() {
    let q = Q::new(GATE);
    let (wa, _a) = q.worktree("feat/a", &[("a.txt", "from A\n")]);
    let (wb, _b) = q.worktree("feat/b", &[("a.txt", "from B\n")]);
    let ja = q.enqueue(&wa, &[]);
    q.wait_ready("A ready", |t| members(t) == vec![ja.clone()]);
    let client = mcp(&q.sb).await;
    let wb_dir = wb.display().to_string();
    // A lands first; B, enqueued over MCP, then conflicts with the new main.
    let out = q.approve(&q.wait_ready("A", |_| true));
    assert!(out.status.success(), "{}", show(&out));
    let r = mcp_call(
        &client,
        "queue_enqueue",
        json!({"working_directory": wb_dir}),
    )
    .await;
    assert_ne!(r.is_error, Some(true), "{}", tool_text(&r));
    let jb = structured_job_id(&r);
    q.wait("B conflicts", |v| job_is(v, &jb, "conflict"));
    let r = mcp_call(
        &client,
        "queue_status",
        json!({"working_directory": wb_dir}),
    )
    .await;
    let status = tool_text(&r);
    evidence("mcp status after conflict", &status);
    assert!(
        status.contains(&format!("call queue_retry with job_id {jb}")),
        "{status}"
    );
    let r = mcp_call(
        &client,
        "queue_enqueue",
        json!({"working_directory": wb_dir}),
    )
    .await;
    assert!(
        tool_text(&r).starts_with("duplicate-branch: "),
        "{}",
        tool_text(&r)
    );
    // Rebase B onto the landed main with a non-conflicting change.
    q.sb.git_in(&wb, &["fetch", "-q", "origin"]);
    q.sb.git_in(&wb, &["reset", "-q", "--hard", "origin/main"]);
    fs::write(wb.join("b.txt"), "b\n").expect("write");
    q.sb.git_in(&wb, &["add", "-A"]);
    q.sb.git_in(
        &wb,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "b again",
        ],
    );
    let head = q.sb.git_in(&wb, &["rev-parse", "HEAD"]);
    let r = mcp_call(
        &client,
        "queue_retry",
        json!({"working_directory": wb_dir, "job_id": jb}),
    )
    .await;
    evidence("mcp retry", &tool_text(&r));
    assert_ne!(r.is_error, Some(true), "{}", tool_text(&r));
    assert!(
        tool_text(&r).starts_with("requeued feat/b"),
        "{}",
        tool_text(&r)
    );
    let st = q.status();
    assert_eq!(job(&st, &jb)["head_sha"], head.as_str());
    // A second retry of a job that is no longer conflict/failed is refused.
    let r = mcp_call(
        &client,
        "queue_retry",
        json!({"working_directory": wb_dir, "job_id": jb}),
    )
    .await;
    assert!(
        tool_text(&r).starts_with("conflict-state: "),
        "{}",
        tool_text(&r)
    );
    let t = q.wait_ready("B ready", |t| members(t) == vec![jb.clone()]);
    let r = mcp_call(
        &client,
        "queue_cancel",
        json!({"working_directory": wb_dir, "job_id": jb}),
    )
    .await;
    evidence("mcp cancel", &tool_text(&r));
    assert_ne!(r.is_error, Some(true), "{}", tool_text(&r));
    let st = q.wait("B cancelled", |v| job_is(v, &jb, "cancelled"));
    assert_eq!(train(&st, train_id(&t))["state"], "dissolved");
    let db = rusqlite::Connection::open(q.sb.home.join("state/sluice/sluice.db")).expect("db");
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    let actors: Vec<String> = db
        .prepare(
            "SELECT actor FROM queue_events WHERE job_id = ?1 AND \
             (from_state IS NULL OR note IN ('retry', 'cancel')) ORDER BY rowid",
        )
        .unwrap()
        .query_map([&jb], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(actors, ["mcp", "mcp", "mcp"], "enqueue, retry, cancel");
    client.cancel().await.expect("shutdown");
}

/// The job id in a `queue_enqueue` result's structured content.
fn structured_job_id(r: &CallToolResult) -> String {
    r.structured_content
        .as_ref()
        .and_then(|v| v["job_id"].as_str())
        .unwrap_or_else(|| panic!("no job id: {}", tool_text(r)))
        .to_owned()
}

// ------------------------------------------------------------------ test 7

/// `git remote set-url` after `ready`: the approved train is dissolved with
/// `endpoint_changed` and rebuilt against the new URL; nothing is pushed to
/// either URL without a new approval.
#[test]
fn acceptance_7a_endpoint_changed_never_pushes() {
    let q = Q::new(GATE);
    let base = q.bare_ref("refs/heads/main").unwrap();
    let other = q.root.join("other.git");
    let bare_url = q.bare.display().to_string();
    q.sb.git_in(
        &q.root,
        &["clone", "-q", "--bare", &bare_url, other.to_str().unwrap()],
    );
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let ja = q.enqueue(&wa, &[]);
    let t = q.wait_ready("ready", |_| true);
    let id = train_id(&t).to_owned();
    q.sb.git(&["remote", "set-url", "origin", other.to_str().unwrap()]);

    let out = q.approve(&t);
    evidence("7a approve after set-url", &show(&out));
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("dissolved (endpoint_changed)"),
        "{}",
        show(&out)
    );
    let st = q.status();
    assert_eq!(train(&st, &id)["state"], "dissolved");
    assert_eq!(train(&st, &id)["note"], "endpoint_changed");
    // Rebuilt against the new URL, waiting for a new approval.
    let t2 = q.wait_ready("rebuilt", |t| t["train_id"] != id.as_str());
    assert_eq!(members(&t2), vec![ja]);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);
    assert_eq!(
        git_dir_ref(&q.sb, &other, "refs/heads/main").unwrap(),
        base,
        "never pushed to the new URL"
    );
}

/// The lease is stolen (another owner's token written into the database)
/// while a train is approved and its remote unreachable: once the remote is
/// back the runner never starts the push.
#[test]
fn acceptance_7b_lost_lease_never_pushes() {
    let q = Q::new(GATE);
    let base = q.bare_ref("refs/heads/main").unwrap();
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    q.enqueue(&wa, &[]);
    let t = q.wait_ready("ready", |_| true);
    let id = train_id(&t).to_owned();
    // Unreachable remote: the approved train waits (pre-push check retried).
    let away = q.root.join("remote.away");
    fs::rename(&q.bare, &away).expect("hide the remote");
    let mut approver = q.spawn_approve(&t);
    q.wait("approved", |v| train(v, &id)["state"] == "approved");
    let db = rusqlite::Connection::open(q.sb.home.join("state/sluice/sluice.db")).expect("db");
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    let token: String = db
        .query_row("SELECT token FROM lease WHERE name = 'daemon'", [], |r| {
            r.get(0)
        })
        .expect("lease row");
    db.execute(
        "UPDATE lease SET token = 'stolen-by-the-test' WHERE name = 'daemon'",
        [],
    )
    .unwrap();
    fs::rename(&away, &q.bare).expect("restore the remote");
    // Many runner ticks (200 ms) with a reachable remote.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base, "no push");
    let markers: i64 = db
        .query_row("SELECT count(*) FROM queue_push_markers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(markers, 0, "no push marker written");
    assert_eq!(train(&q.status(), &id)["state"], "approved");
    let _ = approver.kill();
    let _ = approver.wait();
    // Give the lease back so the sandbox's `daemon stop` can release it.
    db.execute(
        "UPDATE lease SET token = ?1 WHERE name = 'daemon'",
        [&token],
    )
    .unwrap();
}

// ------------------------------------------------------------------ test 8

/// A three-member train fails because of B: it splits, A and C land after
/// their approvals, B is `failed` with the gate name.
#[test]
fn acceptance_8_failing_member_splits_and_the_others_land() {
    // `slow.txt` (the blocker) keeps the target busy while A, B, C enqueue,
    // so the three end up in one train; `bad.txt` (B) fails the gate.
    let q = Q::new(
        r#"
[gates.test]
cmd = "test ! -e bad.txt && { test ! -e slow.txt || sleep 2; }"
hooks = ["pre-push"]

[queue]
max_train = 4
"#,
    );
    let (wx, _x) = q.worktree("feat/x", &[("slow.txt", "s\n")]);
    let (wa, _a) = q.worktree("feat/a", &[("ok-a.txt", "a\n")]);
    let (wb, _b) = q.worktree("feat/b", &[("bad.txt", "b\n")]);
    let (wc, _c) = q.worktree("feat/c", &[("ok-c.txt", "c\n")]);
    let jx = q.enqueue(&wx, &[]);
    q.wait("blocker claimed", |v| {
        trains(v).any(|t| members(t) == vec![jx.clone()])
    });
    let ja = q.enqueue(&wa, &[]);
    let jb = q.enqueue(&wb, &[]);
    let jc = q.enqueue(&wc, &[]);
    let tx = q.wait_ready("blocker ready", |t| members(t) == vec![jx.clone()]);
    assert!(q.approve(&tx).status.success());

    let abc = vec![ja.clone(), jb.clone(), jc.clone()];
    let ta = q.wait_ready("A alone", |t| members(t) == vec![ja.clone()]);
    let st = q.status();
    let first = trains(&st)
        .find(|t| members(t) == abc)
        .expect("the three-member train");
    assert_eq!(first["state"], "dissolved");
    assert_eq!(first["gates"][0]["status"], "fail");
    let out = q.approve(&ta);
    assert!(out.status.success(), "{}", show(&out));
    let tc = q.wait_ready("C alone", |t| members(t) == vec![jc.clone()]);
    let st = q.status();
    let vb = job(&st, &jb);
    assert_eq!(vb["state"], "failed", "{st:#}");
    assert_eq!(vb["reason"], "gate_failed");
    assert_eq!(vb["detail"]["gate"], "test");
    let text = q.status_text();
    evidence("8 status", &text);
    assert!(text.contains("gate_failed gate test"), "{text}");
    let out = q.approve(&tc);
    assert!(out.status.success(), "{}", show(&out));
    let st = q.status();
    for j in [&jx, &ja, &jc] {
        assert!(job_is(&st, j, "landed"), "{j}: {st:#}");
    }
    let main = q.bare_ref("refs/heads/main").unwrap();
    let files = q.sb.git_in(
        &q.root,
        &[
            "--git-dir",
            q.bare.to_str().unwrap(),
            "ls-tree",
            "--name-only",
            &main,
        ],
    );
    assert!(
        files.contains("ok-a.txt") && files.contains("ok-c.txt"),
        "{files}"
    );
    assert!(!files.contains("bad.txt"), "{files}");
}

// ------------------------------------------------------------------ extras

/// `--base integration` (a local branch) lands by ref update without the
/// network; a branch checked out in a worktree is refused.
#[test]
fn local_branch_target_lands_by_ref_update_and_refuses_checked_out() {
    let q = Q::new(GATE);
    let base = q.bare_ref("refs/heads/main").unwrap();
    q.sb.git(&["branch", "integration", "main"]);
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let out = q.run_in(&wa, &["queue", "enqueue", "--base", "refs/heads/main"]);
    evidence("local checked-out refusal", &show(&out));
    assert_eq!(out.status.code(), Some(4), "{}", show(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("target_checked_out"),
        "{}",
        show(&out)
    );
    q.enqueue(&wa, &["--base", "integration"]);
    let t = q.wait_ready("ready", |_| true);
    assert_eq!(t["remote"], "");
    assert_eq!(t["target"], "integration");
    let out = q.approve(&t);
    assert!(out.status.success(), "{}", show(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("on integration"),
        "{}",
        show(&out)
    );
    let s = t["merge_sha"].as_str().unwrap();
    assert_eq!(q.sb.git(&["rev-parse", "refs/heads/integration"]), s);
    assert_eq!(
        q.sb.git(&["rev-parse", &format!("refs/sluice/landed/{s}")]),
        s
    );
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base, "no network");
}

/// A `pre-push` hook that declines fails the train's jobs with
/// `pre_push_hook` and pushes nothing.
#[test]
fn declining_pre_push_hook_fails_the_train() {
    let q = Q::new(GATE);
    q.repo_hook("pre-push", "echo 'policy says no' >&2\nexit 1");
    let base = q.bare_ref("refs/heads/main").unwrap();
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let ja = q.enqueue(&wa, &[]);
    let t = q.wait_ready("ready", |_| true);
    let out = q.approve(&t);
    evidence("declining hook approve", &show(&out));
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("pre_push_hook"),
        "{}",
        show(&out)
    );
    let st = q.status();
    let j = job(&st, &ja);
    assert_eq!(j["state"], "failed");
    assert_eq!(j["reason"], "pre_push_hook");
    assert!(
        j["detail"]["output"]
            .as_str()
            .is_some_and(|o| o.contains("policy says no")),
        "{j:#}"
    );
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base);
}

// ------------------------------------------- the daemon vouches for its push

/// A directory with a `tool` script that prints `tool <version>`.
fn tool_dir(q: &Q, name: &str, version: &str) -> PathBuf {
    let dir = q.root.join(name);
    fs::create_dir_all(&dir).expect("tool dir");
    hook(&dir.join("tool"), &format!("echo 'tool {version}'"));
    dir
}

fn path_with(first: &Path) -> String {
    format!(
        "{}:{}",
        first.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// The repository's `pre-push` hook runs the real `sluice run --hook pre-push
/// <gate>` for each gate and appends everything it prints to `log`.
fn sluice_pre_push_hook(q: &Q, gates: &[&str], log: &Path) {
    let bin = Sandbox::binary();
    let body = gates.iter().fold(String::new(), |mut body, g| {
        // Output to the log and to stderr (what the push reports on a
        // declined hook), the exit status of `sluice` decides.
        let _ = write!(
            body,
            "out=$('{}' run --hook pre-push {g} 2>&1)\nrc=$?\n\
                 printf '%s\\n' \"$out\" >> '{}'\nprintf '%s\\n' \"$out\" >&2\n\
                 [ \"$rc\" -eq 0 ] || exit 1\n",
            bin.display(),
            log.display()
        );
        body
    });
    q.repo_hook("pre-push", &body);
}

/// Issue 4: the hook of the train's push derives another key (`key_inputs`
/// see the approver's `PATH`, `CI` is set) and would miss the cache and run
/// the whole suite again inside `push_timeout`. The daemon answers from the
/// train instead: the gate runs once, the push lands.
#[test]
fn the_pre_push_hook_of_the_trains_push_is_answered_from_the_train() {
    let q = Q::new(&format!(
        "{GATE}key_inputs = [\"tool --version\"]\n[queue]\npush_timeout = \"60s\"\n"
    ));
    let daemon_tool = tool_dir(&q, "bin-daemon", "1.0");
    let approver_tool = tool_dir(&q, "bin-approver", "2.0");
    // The daemon (autostarted by the first CLI call) sees tool 1.0.
    let out =
        q.sb.sluice_in(&q.sb.repo)
            .env("PATH", path_with(&daemon_tool))
            .args(["queue", "status", "--json"])
            .output()
            .expect("sluice");
    assert!(out.status.success(), "{}", show(&out));
    let hook_log = q.root.join("hook.log");
    sluice_pre_push_hook(&q, &["test"], &hook_log);

    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let ja = q.enqueue(&wa, &[]);
    let t = q.wait_ready("ready", |_| true);
    assert_eq!(q.sb.runs(), 1, "the train ran the gate once");

    // The approver's PATH puts another `tool` first and sets CI.
    let out = q.approve_with(&t, &[("PATH", &path_with(&approver_tool)), ("CI", "1")]);
    evidence("vouch approve", &show(&out));
    assert!(out.status.success(), "{}", show(&out));
    let st = q.status();
    assert!(job_is(&st, &ja, "landed"), "{st:#}");
    let s = train(&st, train_id(&t))["merge_sha"].as_str().unwrap();
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), s);
    assert_eq!(q.sb.runs(), 1, "the hook did not run the gate again");
    let printed = fs::read_to_string(&hook_log).expect("the hook ran");
    evidence("vouch hook output", &printed);
    assert!(printed.contains("PASS (cached"), "{printed}");
    assert!(printed.contains("verified by merge train"), "{printed}");
    assert!(
        q.daemon_log().contains("push hook vouched: train="),
        "{}",
        q.daemon_log()
    );
}

/// A gate the hook script runs but the train did not verify fails the push at
/// once as `pre_push_hook`, with the daemon's message, instead of running the
/// gate until `push_timeout`. (A member that tags a new gate `pre-push` in
/// `.sluice.toml` is caught earlier, when the train is built:
/// `hook_gate_unverified`. What is left for the push is a hook script that
/// calls a gate its configuration does not tag `pre-push`.)
#[test]
fn a_hook_gate_the_train_did_not_verify_fails_the_push_at_once() {
    let base = format!(
        "{GATE}[gates.extra]\ncmd = \"echo extra >> \\\"$SLUICE_TEST_COUNTER\\\"\"\n\
         hooks = [\"pre-commit\"]\n[queue]\npush_timeout = \"60s\"\n"
    );
    let q = Q::new(&base);
    let hook_log = q.root.join("hook.log");
    sluice_pre_push_hook(&q, &["extra"], &hook_log);
    let base_sha = q.bare_ref("refs/heads/main").unwrap();
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let ja = q.enqueue(&wa, &[]);
    let t = q.wait_ready("ready", |_| true);
    assert_eq!(q.sb.runs(), 1, "the train ran its pre-push gate only");

    let started = Instant::now();
    let out = q.approve(&t);
    let took = started.elapsed();
    evidence("unverified approve", &show(&out));
    assert_eq!(out.status.code(), Some(1), "{}", show(&out));
    assert!(
        took < Duration::from_secs(30),
        "took {took:?}, not a timeout"
    );
    let st = q.status();
    let j = job(&st, &ja);
    assert_eq!(j["state"], "failed", "{st:#}");
    assert_eq!(j["reason"], "pre_push_hook", "{j:#}");
    let output = j["detail"]["output"].as_str().unwrap_or_default();
    assert!(
        output.contains("gate 'extra' runs in the pre-push hook but merge train"),
        "{j:#}"
    );
    assert!(output.contains("[queue] gates"), "{j:#}");
    assert_eq!(q.sb.runs(), 1, "the unverified gate never ran");
    assert_eq!(q.bare_ref("refs/heads/main").unwrap(), base_sha);
}

/// A `sluice run --hook pre-push` started in the merge worktree by a process
/// that is not a descendant of the push gets no vouch: it goes the normal way
/// (here the cache is off, so the gate really runs).
#[test]
fn a_run_in_the_merge_worktree_outside_the_push_is_not_vouched() {
    let q = Q::new(GATE);
    let started = q.root.join("hook.started");
    let release = q.root.join("hook.release");
    q.repo_hook(
        "pre-push",
        &format!(
            "touch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done",
            started.display(),
            release.display()
        ),
    );
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let ja = q.enqueue(&wa, &[]);
    let t = q.wait_ready("ready", |_| true);
    let id = train_id(&t).to_owned();
    assert_eq!(q.sb.runs(), 1);

    let mut approver = q.spawn_approve(&t);
    wait_for("the push to reach its hook", || started.exists());
    assert_eq!(train(&q.status(), &id)["state"], "pushing");

    let merge_wt = q.sb.home.join("state/sluice/merge").join(&id);
    let out =
        q.sb.sluice_in(&merge_wt)
            .env("SLUICE_CACHE", "0")
            .args(["run", "--hook", "pre-push", "test"])
            .output()
            .expect("sluice run");
    evidence("foreign run", &show(&out));
    assert!(out.status.success(), "{}", show(&out));
    let text = show(&out);
    assert!(!text.contains("verified by merge train"), "{text}");
    assert_eq!(q.sb.runs(), 2, "the gate ran: no vouch");
    assert!(
        !q.daemon_log().contains("push hook vouched"),
        "{}",
        q.daemon_log()
    );

    fs::write(&release, "").expect("release the hook");
    let status = approver.wait().expect("approver");
    assert!(status.success());
    assert!(job_is(&q.status(), &ja, "landed"));
}

// ----------------------------------------------- base failures (section 7)

/// A remote URL that does not exist: the job stays `queued` with reason
/// `base_unavailable` and the git error, then fails after 3 attempts;
/// `queue retry` re-queues it and, with the URL fixed, it gets built.
#[test]
fn unreachable_base_is_reported_then_fails_after_three_attempts() {
    let q = Q::new(GATE);
    q.user_config("tick = \"1s\"\n");
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    let nowhere = q.root.join("nowhere.git");
    q.sb.git(&["remote", "set-url", "origin", nowhere.to_str().unwrap()]);
    let ja = q.enqueue(&wa, &["--base", "origin/main"]);
    let st = q.wait("reason recorded", |v| {
        job(v, &ja)["reason"] == "base_unavailable"
    });
    let j = job(&st, &ja);
    assert_eq!(j["state"], "queued", "{st:#}");
    let text = q.status_text();
    evidence("base_unavailable queued", &text);
    assert!(text.contains("waiting: base_unavailable"), "{text}");
    assert!(
        j["detail"]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{j:#}"
    );
    let st = q.wait("failed after 3 attempts", |v| job_is(v, &ja, "failed"));
    let j = job(&st, &ja);
    assert_eq!(j["reason"], "base_unavailable");
    assert_eq!(j["detail"]["failures"], 3);
    let text = q.status_text();
    evidence("base_unavailable failed", &text);
    assert!(text.contains("FAILED base_unavailable"), "{text}");

    q.sb.git(&["remote", "set-url", "origin", q.bare.to_str().unwrap()]);
    let out = q.run_in(&wa, &["queue", "retry", &ja]);
    assert!(out.status.success(), "{}", show(&out));
    let t = q.wait_ready("built after the retry", |_| true);
    assert_eq!(members(&t), vec![ja.clone()]);
    assert!(job(&q.status(), &ja).get("reason").is_none());
}

/// An invalid `.sluice.toml` on the base commit: reason
/// `base_config_invalid` with the parse error, `failed` after 3 attempts.
#[test]
fn invalid_base_config_is_reported_then_fails_after_three_attempts() {
    let q = Q::new(GATE);
    q.user_config("tick = \"1s\"\n");
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n")]);
    q.sb.write_config("bogus_key = 1\n");
    q.sb.git(&["commit", "-q", "-am", "broken config"]);
    q.sb.git(&["push", "-q", "origin", "main"]);
    let ja = q.enqueue(&wa, &[]);
    let st = q.wait("reason recorded", |v| {
        job(v, &ja)["reason"] == "base_config_invalid"
    });
    let j = job(&st, &ja);
    assert_eq!(j["state"], "queued", "{st:#}");
    let text = q.status_text();
    evidence("base_config_invalid queued", &text);
    assert!(text.contains("waiting: base_config_invalid"), "{text}");
    assert!(
        j["detail"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("bogus_key")),
        "{j:#}"
    );
    let st = q.wait("failed after 3 attempts", |v| job_is(v, &ja, "failed"));
    assert_eq!(job(&st, &ja)["reason"], "base_config_invalid");
    let out = q.run_in(&wa, &["queue", "retry", &ja]);
    assert!(out.status.success(), "{}", show(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("requeued"),
        "{}",
        show(&out)
    );
    assert_ne!(job(&q.status(), &ja)["state"], "failed");
}

/// The dead end of a target whose own configuration is broken: the branch
/// that fixes it can land only through a human's `--trust-member-config`,
/// which agents (even with `allow_agent_approval`) cannot use or approve.
#[test]
fn trusting_the_members_config_gets_past_a_broken_target_config() {
    // The body after `version = 1` (what `write_config` and `Q::new` take).
    const GOOD: &str = "[gates.test]\ncmd = \"true\"\nhooks = [\"pre-push\"]\n\n# a\n# b\n# end\n";
    let q = Q::with_user(GOOD, "allow_agent_approval = true\n");
    // The target's file breaks; the branch, forked from the broken target,
    // repairs it (and changes the gate's command). The merged tree must hold a
    // valid file: the train checks it, and a Sluice pre-push hook reads it.
    q.sb.write_config(&format!("{GOOD}bogus_key = 1\n"));
    q.sb.git(&["commit", "-q", "-am", "broken config"]);
    q.sb.git(&["push", "-q", "origin", "main"]);
    let fixed = format!(
        "version = 1\n{}",
        GOOD.replace("cmd = \"true\"", "cmd = \"echo ok\"")
    );
    let (wa, _a) = q.worktree("feat/a", &[("n.txt", "n\n"), (".sluice.toml", &fixed)]);
    let ja = q.enqueue(&wa, &[]);
    q.wait("failed on the broken base", |v| job_is(v, &ja, "failed"));

    // An agent's terminal cannot use the flag.
    let agent = StdCommand::new(Sandbox::binary())
        .args(["queue", "retry", &ja, "--trust-member-config"])
        .current_dir(&wa)
        .env_clear()
        .envs(q.sb.env())
        .env("CLAUDECODE", "1")
        .output()
        .expect("retry");
    assert_eq!(agent.status.code(), Some(4), "{}", show(&agent));
    assert!(
        String::from_utf8_lossy(&agent.stderr).contains("reserved to a human"),
        "{}",
        show(&agent)
    );
    assert!(job_is(&q.status(), &ja, "failed"));

    // A human can.
    let out = q.run_in(&wa, &["queue", "retry", &ja, "--trust-member-config"]);
    assert!(out.status.success(), "{}", show(&out));
    let t = q.wait_ready("the trusted train", |t| t["config_source"] == "member");
    let text = q.status_text();
    evidence("trusted train", &text);
    assert!(text.contains("CONFIG FROM BRANCH feat/a"), "{text}");
    assert!(text.contains("+cmd = \"echo ok\""), "{text}");
    assert_eq!(members(&t), vec![ja.clone()]);

    // Its approval is a human's too.
    let refused = q.approve_with(&t, &[("CLAUDECODE", "1")]);
    assert_eq!(refused.status.code(), Some(4), "{}", show(&refused));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("own .sluice.toml"));
    let out = q.approve(&t);
    assert!(out.status.success(), "{}", show(&out));
    q.wait("landed", |v| job_is(v, &ja, "landed"));
}
