//! Shared fixtures: a repository with a local bare remote, and a real daemon
//! running the queue extension on a temporary socket and state directory.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use sluice_gate_daemon::server::{
    self, AdmissionConfig, DaemonConfig, LastSubscriberGone, UserConfig,
};
use sluice_gate_proto::{
    ApproveVia, Caller, ClientMessage, EnqueueRequest, QueueJobState, QueueJobView,
    QueueStatusReport, QueueTrainState, QueueTrainView, ReconcileResult, ServerMessage,
    decode_server,
};
use sluice_gate_queue::{QueueExtension, QueueOptions};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub type EnvMap = BTreeMap<String, String>;

pub const WAIT: Duration = Duration::from_secs(20);

/// Environment for the test's own git calls.
pub fn git_env(home: &Path) -> EnvMap {
    let mut env: EnvMap = std::env::vars()
        .filter(|(k, _)| !k.starts_with("GIT_") && !k.starts_with("CLAUDE"))
        .collect();
    for (k, v) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_AUTHOR_NAME", "Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
        ("LC_ALL", "C"),
    ] {
        env.insert(k.into(), v.into());
    }
    env.insert("HOME".into(), home.display().to_string());
    env
}

pub fn git_in(dir: &Path, env: &EnvMap, args: &[&str]) -> String {
    // No (detached) auto-maintenance from the test's own commits and pushes.
    let out = Command::new("git")
        .args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(env)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository `repo` (main worktree on `main`) with `origin` = a local
/// bare repository, `.sluice.toml` committed on `main`.
pub struct Fixture {
    pub tmp: TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub repo: PathBuf,
    pub common: PathBuf,
    pub bare: PathBuf,
    pub env: EnvMap,
}

impl Fixture {
    pub fn new(sluice_toml: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let env = git_env(&home);
        let bare = root.join("remote.git");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_in(
            &root,
            &env,
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git_in(&repo, &env, &["init", "-q", "-b", "main"]);
        for (k, v) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
        ] {
            git_in(&repo, &env, &["config", k, v]);
        }
        let hooks = repo.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        git_in(
            &repo,
            &env,
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        std::fs::write(repo.join(".sluice.toml"), sluice_toml).unwrap();
        std::fs::write(repo.join("a.txt"), "base\n").unwrap();
        git_in(&repo, &env, &["add", "-A"]);
        git_in(&repo, &env, &["commit", "-q", "-m", "initial"]);
        git_in(
            &repo,
            &env,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        git_in(&repo, &env, &["push", "-q", "origin", "main"]);
        git_in(&repo, &env, &["fetch", "-q", "origin"]);
        git_in(&repo, &env, &["remote", "set-head", "origin", "main"]);
        let common = repo.join(".git");
        Self {
            tmp,
            root,
            home,
            repo,
            common,
            bare,
            env,
        }
    }

    pub fn git(&self, args: &[&str]) -> String {
        git_in(&self.repo, &self.env, args)
    }

    pub fn bare_ref(&self, r: &str) -> Option<String> {
        let out = Command::new("git")
            .args([
                "--git-dir",
                self.bare.to_str().unwrap(),
                "rev-parse",
                "--verify",
                "--quiet",
                r,
            ])
            .env_clear()
            .envs(&self.env)
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    pub fn local_ref(&self, r: &str) -> Option<String> {
        let out = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", r])
            .current_dir(&self.repo)
            .env_clear()
            .envs(&self.env)
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Creates branch `name` from `from` in its own worktree (an agent's),
    /// writes `files`, commits; returns (worktree, head sha).
    pub fn branch(&self, name: &str, from: &str, files: &[(&str, &str)]) -> (PathBuf, String) {
        let wt = self.root.join(format!("wt-{}", name.replace('/', "-")));
        self.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            name,
            wt.to_str().unwrap(),
            from,
        ]);
        for (path, content) in files {
            let p = wt.join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        git_in(&wt, &self.env, &["add", "-A"]);
        git_in(
            &wt,
            &self.env,
            &["commit", "-q", "-m", &format!("work on {name}")],
        );
        let sha = git_in(&wt, &self.env, &["rev-parse", "HEAD"]);
        (wt, sha)
    }

    /// Pushes a new commit to the bare remote's `main` from a scratch clone.
    pub fn move_remote_main(&self, file: &str) -> String {
        let clone = self.root.join(format!("clone-{file}"));
        git_in(
            &self.root,
            &self.env,
            &[
                "clone",
                "-q",
                self.bare.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        std::fs::write(clone.join(file), "moved\n").unwrap();
        git_in(&clone, &self.env, &["add", "-A"]);
        git_in(&clone, &self.env, &["commit", "-q", "-m", "moved"]);
        git_in(&clone, &self.env, &["push", "-q", "origin", "HEAD:main"]);
        git_in(&clone, &self.env, &["rev-parse", "HEAD"])
    }

    pub fn write_hook(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.common.join("hooks").join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Base env given to the queue (no user git config).
    pub fn queue_env(&self) -> EnvMap {
        let mut env = EnvMap::new();
        for k in ["PATH", "LANG"] {
            if let Ok(v) = std::env::var(k) {
                env.insert(k.into(), v);
            }
        }
        env.insert("HOME".into(), self.home.display().to_string());
        env.insert(
            "XDG_CONFIG_HOME".into(),
            self.home.join(".config").display().to_string(),
        );
        let run = self.root.join("run");
        std::fs::create_dir_all(&run).unwrap();
        env.insert("XDG_RUNTIME_DIR".into(), run.display().to_string());
        env
    }

    /// The approving / enqueuing client's environment.
    pub fn client_env(&self) -> EnvMap {
        let mut env = EnvMap::new();
        env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        env.insert("HOME".into(), self.home.display().to_string());
        env
    }
}

pub struct Daemon {
    pub state: PathBuf,
    pub socket: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<sluice_gate_daemon::Result<()>>,
}

pub fn daemon_config(state: &Path, user: UserConfig, base_env: EnvMap) -> DaemonConfig {
    daemon_config_with(state, user, base_env, None)
}

pub fn daemon_config_with(
    state: &Path,
    user: UserConfig,
    base_env: EnvMap,
    push_program: Option<PathBuf>,
) -> DaemonConfig {
    daemon_config_full(state, user, base_env, push_program, None)
}

/// Like [`daemon_config_with`]; `login_shell` is resolved for the trains' tool
/// environment (ADR-0020). Without it the harness never starts a login shell:
/// with a `base_env` the trains' tools run in that environment, so the tests
/// do not depend on the machine's shell.
pub fn daemon_config_full(
    state: &Path,
    user: UserConfig,
    base_env: EnvMap,
    push_program: Option<PathBuf>,
    login_shell: Option<PathBuf>,
) -> DaemonConfig {
    DaemonConfig {
        socket_path: state.join("d.sock"),
        lock_path: state.join("daemon.lock"),
        db_path: state.join("sluice.db"),
        logs_dir: state.join("logs"),
        budget: 2,
        head_reserve_after: Duration::from_secs(30),
        repo_caps: BTreeMap::new(),
        nice: 0,
        ionice: None,
        last_subscriber_gone: LastSubscriberGone::Cancel,
        log_retention: Duration::from_hours(24),
        tail_lines: 20,
        marker_ttl: Duration::from_hours(24),
        version: "0.0.0-test".to_owned(),
        admission: AdmissionConfig {
            psi_enabled: false,
            // With the harness's budget of 2 every first run is estimated at
            // half the host's usable memory, so whether two gates may run
            // together would depend on the machine running the tests. Memory
            // admission has its own deterministic tests in the daemon crate.
            memory_aware: false,
            ..AdmissionConfig::default()
        },
        psi_root: state.join("pressure"),
        proc_root: std::path::PathBuf::from("/proc"),
        user,
        log: sluice_gate_daemon::server::LogSettings::default(),
        extensions: vec![Arc::new(QueueExtension::new(QueueOptions {
            base_env: Some(base_env),
            push_program,
            login_shell,
        }))],
        reaper: None,
        containment: None,
    }
}

pub fn user_config(allow_agent_approval: bool) -> UserConfig {
    let mut user = UserConfig::default();
    user.queue.tick = Duration::from_millis(100);
    user.queue.allow_agent_approval = allow_agent_approval;
    user
}

impl Daemon {
    pub async fn start(fx: &Fixture, user: UserConfig) -> Self {
        Self::start_with(fx, user, None).await
    }

    /// Like [`Daemon::start`]; `push_program` replaces `git` for the push.
    pub async fn start_with(fx: &Fixture, user: UserConfig, push_program: Option<PathBuf>) -> Self {
        Self::start_policy(fx, user, push_program, LastSubscriberGone::Cancel).await
    }

    /// Like [`Daemon::start_with`] under the given `last_subscriber_gone`.
    pub async fn start_policy(
        fx: &Fixture,
        user: UserConfig,
        push_program: Option<PathBuf>,
        policy: LastSubscriberGone,
    ) -> Self {
        Self::start_full(fx, user, push_program, policy, None).await
    }

    /// Like [`Daemon::start`]; the trains' tool environment is resolved through
    /// the fake login shell `login_shell`.
    pub async fn start_login_shell(fx: &Fixture, user: UserConfig, login_shell: PathBuf) -> Self {
        let policy = LastSubscriberGone::Cancel;
        Self::start_full(fx, user, None, policy, Some(login_shell)).await
    }

    async fn start_full(
        fx: &Fixture,
        user: UserConfig,
        push_program: Option<PathBuf>,
        policy: LastSubscriberGone,
        login_shell: Option<PathBuf>,
    ) -> Self {
        let state = fx.root.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let mut cfg = daemon_config_full(&state, user, fx.queue_env(), push_program, login_shell);
        cfg.last_subscriber_gone = policy;
        let socket = cfg.socket_path.clone();
        let _ = std::fs::remove_file(&socket);
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(server::run(cfg, async move {
            let _ = rx.await;
        }));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !socket.exists() {
            assert!(tokio::time::Instant::now() < deadline, "no socket");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Self {
            state,
            socket,
            stop: Some(tx),
            task,
        }
    }

    pub async fn shutdown(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        tokio::time::timeout(WAIT, self.task)
            .await
            .expect("daemon exits")
            .unwrap()
            .unwrap();
    }

    pub async fn ask(&self, msg: &ClientMessage) -> ServerMessage {
        let (_w, mut lines) = server::connect(&self.socket, msg).await.unwrap();
        let line = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .expect("a reply in time")
            .unwrap()
            .expect("a reply");
        decode_server(&line).unwrap()
    }

    pub async fn enqueue(
        &self,
        fx: &Fixture,
        wt: &Path,
        branch: &str,
        sha: &str,
        base: Option<&str>,
    ) -> ServerMessage {
        self.ask(&ClientMessage::QueueEnqueue {
            id: 1,
            request: EnqueueRequest {
                repo: fx.common.clone(),
                worktree: wt.to_path_buf(),
                branch: branch.into(),
                head_sha: sha.into(),
                base: base.map(str::to_owned),
                caller: Caller::Agent,
                session: Some("s1".into()),
                env: fx.client_env(),
                via: None,
                trust_member_config: false,
            },
        })
        .await
    }

    pub async fn enqueue_ok(
        &self,
        fx: &Fixture,
        wt: &Path,
        branch: &str,
        sha: &str,
        base: Option<&str>,
    ) -> QueueJobView {
        match self.enqueue(fx, wt, branch, sha, base).await {
            ServerMessage::QueueJob { job, .. } => job,
            other => panic!("enqueue failed: {other:?}"),
        }
    }

    /// The daemon's general status report (`sluice status`).
    pub async fn daemon_status(&self) -> sluice_gate_proto::StatusReport {
        match self.ask(&ClientMessage::Status { id: 3 }).await {
            ServerMessage::Status { status, .. } => *status,
            other => panic!("unexpected {other:?}"),
        }
    }

    pub async fn status(&self) -> QueueStatusReport {
        match self
            .ask(&ClientMessage::QueueStatus { id: 2, repo: None })
            .await
        {
            ServerMessage::QueueStatus { status, .. } => status,
            other => panic!("unexpected {other:?}"),
        }
    }

    pub async fn approve(
        &self,
        fx: &Fixture,
        train: &str,
        hash: &str,
        via: ApproveVia,
    ) -> ServerMessage {
        self.ask(&ClientMessage::QueueApprove {
            id: 3,
            train_id: train.into(),
            train_hash: hash.into(),
            via,
            env: fx.client_env(),
        })
        .await
    }

    pub async fn cancel(&self, job: &str) -> ServerMessage {
        self.ask(&ClientMessage::QueueCancel {
            id: 4,
            job_id: job.into(),
            via: None,
        })
        .await
    }

    pub async fn retry(&self, job: &str, head_sha: &str) -> ServerMessage {
        self.ask(&ClientMessage::QueueRetry {
            id: 6,
            job_id: job.into(),
            head_sha: head_sha.into(),
            via: None,
            trust_member_config: false,
            env: EnvMap::new(),
        })
        .await
    }

    pub async fn reconcile(&self) -> Vec<ReconcileResult> {
        match self
            .ask(&ClientMessage::QueueReconcile { id: 5, repo: None })
            .await
        {
            ServerMessage::QueueReconcile { results, .. } => results,
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Polls `queue-status` until `pred` holds.
    pub async fn wait_for(
        &self,
        what: &str,
        pred: impl Fn(&QueueStatusReport) -> bool,
    ) -> QueueStatusReport {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let st = self.status().await;
            if pred(&st) {
                return st;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}: {st:#?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn wait_train(
        &self,
        what: &str,
        pred: impl Fn(&QueueTrainView) -> bool,
    ) -> QueueTrainView {
        let st = self.wait_for(what, |s| s.trains.iter().any(&pred)).await;
        st.trains.into_iter().find(|t| pred(t)).unwrap()
    }

    pub async fn wait_job(&self, job_id: &str, state: QueueJobState) -> QueueJobView {
        let st = self
            .wait_for(&format!("job {job_id} {state:?}"), |s| {
                s.jobs
                    .iter()
                    .any(|j| j.job_id == job_id && j.state == state)
            })
            .await;
        st.jobs.into_iter().find(|j| j.job_id == job_id).unwrap()
    }
}

pub fn ready(t: &QueueTrainView) -> bool {
    t.state == QueueTrainState::Ready
}
