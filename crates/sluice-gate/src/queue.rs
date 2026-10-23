//! `sluice queue ...`: the client side of the local merge queue
//! (`kb/sluice/design/merge-queue.md` sections 8 and 9).
//!
//! Every subcommand is a thin client of the daemon's `queue-*` messages. The
//! client resolves facts with git in the caller's worktree (branch, head SHA,
//! cleanliness) and sends them; it never changes the index or the working tree.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use serde_json::Value;
use sluice_gate_core::agent;
use sluice_gate_core::config::UserConfig;
use sluice_gate_core::git::{self, EnvMap, RepoPaths};
use sluice_gate_proto::client::{self, ClientError};
use sluice_gate_proto::{
    ApproveVia, Caller, ClientMessage, EnqueueRequest, ErrorCode, FEATURE_QUEUE, QueueJobState,
    QueueJobView, QueueStatusReport, QueueTrainState, QueueTrainView, ReconcileDecision,
    ServerMessage, TrainGate,
};

use crate::run::EXIT_ERROR;

/// Exit status of a usage error (also what clap uses).
const EXIT_USAGE: u8 = 2;
/// Exit status of a request refused because of the state of the queue.
const EXIT_REFUSED: u8 = 4;
/// Period of the status poll while a push finishes.
const POLL_PERIOD: Duration = Duration::from_secs(1);

/// Variables forwarded to the daemon for `git fetch` / `git push`.
const ENV_ALLOWLIST: [&str; 8] = [
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "HOME",
    "PATH",
    "TERM",
];

/// A failed subcommand: the message (printed after `sluice: `) and exit status.
#[derive(Debug)]
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn error(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_ERROR,
            message: message.into(),
        }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_USAGE,
            message: message.into(),
        }
    }
}

type Outcome = Result<(), Failure>;

fn finish(result: Outcome) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            eprintln!("sluice: {}", f.message);
            ExitCode::from(f.code)
        }
    }
}

/// Keeps only the allowlisted variables that are set.
fn filter_env(env: &EnvMap) -> BTreeMap<String, String> {
    ENV_ALLOWLIST
        .iter()
        .filter_map(|k| env.get(*k).map(|v| ((*k).to_owned(), v.clone())))
        .collect()
}

fn process_env() -> EnvMap {
    std::env::vars().collect()
}

// ---------------------------------------------------------------- transport

/// Sends one request to the daemon (starting it when needed) and returns the
/// reply; an `error` reply becomes a [`Failure`] with the matching exit code.
///
/// A read-only request (`queue-status`) whose connection drops before any
/// reply, typically because the daemon died or is restarting, is sent once
/// more after making sure a daemon runs; requests with effects never are.
fn send(env: &EnvMap, msg: &ClientMessage, wait_forever: bool) -> Result<ServerMessage, Failure> {
    match send_once(env, msg, wait_forever) {
        Err(Retryable::Lost(_)) if matches!(msg, ClientMessage::QueueStatus { .. }) => {
            send_once(env, msg, wait_forever).map_err(Retryable::into_failure)
        }
        other => other.map_err(Retryable::into_failure),
    }
}

/// A failure, and whether the connection dropped before any reply.
enum Retryable {
    Lost(Failure),
    Final(Failure),
}

impl Retryable {
    fn into_failure(self) -> Failure {
        match self {
            Self::Lost(f) | Self::Final(f) => f,
        }
    }
}

fn send_once(
    env: &EnvMap,
    msg: &ClientMessage,
    wait_forever: bool,
) -> Result<ServerMessage, Retryable> {
    let lost = |e: String| Retryable::Lost(Failure::error(e));
    let socket = client::ensure_daemon(env)
        .map_err(|e| Retryable::Final(Failure::error(format!("{e:#}"))))?;
    let conn = client::connect(&socket).map_err(|e| lost(e.to_string()))?;
    if !conn.supports(FEATURE_QUEUE) {
        return Err(Retryable::Final(Failure::error(
            "the running daemon does not support the merge queue; run 'sluice daemon stop' and retry",
        )));
    }
    let timeout = if wait_forever {
        None
    } else {
        Some(Duration::from_secs(60))
    };
    match conn.request_timeout(msg, timeout) {
        Ok(ServerMessage::Error { code, message, .. }) => Err(Retryable::Final(Failure {
            code: match code {
                ErrorCode::ConflictState
                | ErrorCode::NotAllowed
                | ErrorCode::StalePlan
                | ErrorCode::DuplicateBranch => EXIT_REFUSED,
                _ => EXIT_ERROR,
            },
            message,
        })),
        Ok(reply) => Ok(reply),
        Err(ClientError::Daemon(m)) => Err(Retryable::Final(Failure::error(m))),
        Err(e) => Err(lost(e.to_string())),
    }
}

fn unexpected(what: &str, reply: &ServerMessage) -> Failure {
    Failure::error(format!("unexpected reply to {what}: {reply:?}"))
}

fn fetch_status(env: &EnvMap, repo: Option<&Path>) -> Result<QueueStatusReport, Failure> {
    let msg = ClientMessage::QueueStatus {
        id: 1,
        repo: repo.map(Path::to_path_buf),
    };
    match send(env, &msg, false)? {
        ServerMessage::QueueStatus { status, .. } => Ok(status),
        other => Err(unexpected("queue-status", &other)),
    }
}

// ---------------------------------------------------------------------- git

/// The repository the command runs in.
struct Repo {
    paths: RepoPaths,
    /// Environment of Sluice's own git probes.
    probe: EnvMap,
}

impl Repo {
    fn discover(env: &EnvMap) -> Result<Self, Failure> {
        let cwd = std::env::current_dir().map_err(|e| Failure::error(e.to_string()))?;
        let paths = git::discover(&cwd, env).map_err(|e| Failure::error(e.to_string()))?;
        let probe = git::probe_env(env, Some(&paths));
        Ok(Self { paths, probe })
    }

    fn git(&self, args: &[&str]) -> Result<String, sluice_gate_core::error::Error> {
        git::run(args, &self.paths.toplevel, &self.probe, true)
    }

    /// Current branch, or `None` on a detached HEAD.
    fn current_branch(&self) -> Option<String> {
        self.git(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .filter(|b| !b.is_empty())
    }

    /// Commit the local branch points at.
    fn branch_head(&self, branch: &str) -> Result<String, Failure> {
        let spec = format!("refs/heads/{branch}^{{commit}}");
        self.git(&["rev-parse", "--verify", "--quiet", &spec])
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Failure::error(format!("branch '{branch}' does not exist here")))
    }

    /// True when tracked files have staged or unstaged changes.
    fn is_dirty(&self) -> Result<bool, Failure> {
        let out = self
            .git(&["status", "--porcelain", "--untracked-files=no"])
            .map_err(|e| Failure::error(e.to_string()))?;
        Ok(!out.is_empty())
    }
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn target_name(remote: &str, target: &str) -> String {
    if remote.is_empty() {
        target.to_owned()
    } else {
        format!("{remote}/{target}")
    }
}

// ------------------------------------------------------------------ enqueue

/// `sluice queue enqueue [BRANCH] [--base <ref>]`.
pub fn enqueue(branch: Option<&str>, base: Option<&str>) -> ExitCode {
    finish(do_enqueue(branch, base))
}

fn do_enqueue(branch: Option<&str>, base: Option<&str>) -> Outcome {
    let env = process_env();
    let repo = Repo::discover(&env)?;
    let branch = match branch {
        Some(b) => b.to_owned(),
        None => repo.current_branch().ok_or_else(|| {
            Failure::error(
                "HEAD is detached; check out a branch or name one: 'sluice queue enqueue <BRANCH>'",
            )
        })?,
    };
    let head_sha = repo.branch_head(&branch)?;
    if repo.is_dirty()? {
        return Err(Failure::error(format!(
            "the worktree has uncommitted changes to tracked files; commit or stash them \
             before queueing '{branch}' (only committed work is queued)"
        )));
    }
    let user = UserConfig::load(&env).unwrap_or_default();
    let caller = match agent::detect(
        &env,
        &user.agent_detection.env_any_set,
        &user.agent_detection.env_any_equals,
    ) {
        agent::Caller::Agent { .. } => Caller::Agent,
        agent::Caller::Human => Caller::Human,
    };
    let request = EnqueueRequest {
        repo: repo.paths.common_dir.clone(),
        worktree: repo.paths.toplevel.clone(),
        branch,
        head_sha,
        base: base.map(str::to_owned),
        caller,
        session: env
            .get("CLAUDE_CODE_SESSION_ID")
            .filter(|s| !s.is_empty())
            .cloned(),
        env: filter_env(&env),
        via: None,
    };
    match send(&env, &ClientMessage::QueueEnqueue { id: 1, request }, true)? {
        ServerMessage::QueueJob { job, .. } => {
            println!("{}", queued_line(&job));
            Ok(())
        }
        other => Err(unexpected("queue-enqueue", &other)),
    }
}

fn queued_line(job: &QueueJobView) -> String {
    let position = job
        .position
        .map_or_else(String::new, |p| format!(", position {p}"));
    format!(
        "sluice: queued {} (job {}) for {}{position}",
        job.branch,
        job.job_id,
        target_name(&job.remote, &job.target)
    )
}

// ------------------------------------------------------------------- status

/// `sluice queue status [--json]`.
pub fn status(json: bool) -> ExitCode {
    finish(do_status(json))
}

fn do_status(json: bool) -> Outcome {
    let env = process_env();
    // Inside a repository only its own trains and jobs are shown.
    let common = Repo::discover(&env).ok().map(|r| r.paths.common_dir);
    let report = fetch_status(&env, common.as_deref())?;
    if json {
        let text =
            serde_json::to_string_pretty(&report).map_err(|e| Failure::error(e.to_string()))?;
        println!("{text}");
    } else {
        print!("{}", render_status(&report));
    }
    Ok(())
}

fn gate_text(g: &TrainGate) -> String {
    let dur = g.duration_ms.map(|ms| {
        // Display only: sub-second precision is enough.
        #[allow(clippy::cast_precision_loss)]
        let secs = ms as f64 / 1000.0;
        format!(" {secs:.1}s")
    });
    match g.status.as_str() {
        "pass" => format!("{} PASS{}", g.gate, dur.unwrap_or_default()),
        "cached" => format!("{} PASS (cached)", g.gate),
        "fail" => format!("{} FAIL{}", g.gate, dur.unwrap_or_default()),
        "timeout" => format!("{} TIMEOUT{}", g.gate, dur.unwrap_or_default()),
        "killed" => format!("{} KILLED", g.gate),
        "error" => format!("{} ERROR", g.gate),
        other => format!("{} {other}", g.gate),
    }
}

fn train_state_label(s: QueueTrainState) -> &'static str {
    match s {
        QueueTrainState::Building => "BUILDING",
        QueueTrainState::Verifying => "VERIFYING",
        QueueTrainState::Ready => "READY (awaiting approval)",
        QueueTrainState::Approved => "APPROVED",
        QueueTrainState::Pushing => "PUSHING",
        QueueTrainState::Landed => "LANDED",
        QueueTrainState::Dissolved => "DISSOLVED",
        QueueTrainState::NeedsReconcile => "NEEDS RECONCILE (push outcome unknown)",
        QueueTrainState::Unknown => "UNKNOWN",
    }
}

fn job_state_label(s: QueueJobState) -> &'static str {
    match s {
        QueueJobState::Queued => "queued",
        QueueJobState::Merging => "merging",
        QueueJobState::Verifying | QueueJobState::Ready | QueueJobState::Approved => "merged",
        QueueJobState::Pushing => "pushing",
        QueueJobState::Landed => "landed",
        QueueJobState::Conflict => "CONFLICT",
        QueueJobState::Failed => "FAILED",
        QueueJobState::Cancelled => "cancelled",
        QueueJobState::NeedsReconcile => "needs_reconcile",
        QueueJobState::Unknown => "unknown",
    }
}

/// Conflicting paths from a job's `detail` (an array of paths or `{"paths": [...]}`).
fn conflict_paths(detail: Option<&Value>) -> Vec<String> {
    let list = match detail {
        Some(Value::Array(a)) => Some(a),
        Some(Value::Object(o)) => o.get("paths").and_then(Value::as_array),
        _ => None,
    };
    list.map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    })
    .unwrap_or_default()
}

/// One-line explanation of a failed job from its `reason` and `detail`.
fn failure_text(job: &QueueJobView) -> String {
    let mut parts = Vec::new();
    if let Some(reason) = &job.reason {
        parts.push(reason.clone());
    }
    match &job.detail {
        Some(Value::String(s)) => parts.push(s.clone()),
        Some(Value::Object(o)) => {
            if let Some(e) = o.get("error").and_then(Value::as_str) {
                parts.push(e.lines().next().unwrap_or_default().to_owned());
            }
            if let Some(g) = o.get("gate").and_then(Value::as_str) {
                parts.push(format!("gate {g}"));
            }
            if let Some(j) = o.get("job_id").and_then(Value::as_str) {
                parts.push(format!("(see 'sluice logs {j}')"));
            }
        }
        _ => {}
    }
    parts.join(" ")
}

/// The tail of a job line: what happened and what to do next.
fn job_note(job: &QueueJobView, gates: &[TrainGate]) -> String {
    match job.state {
        QueueJobState::Conflict => {
            let paths = conflict_paths(job.detail.as_ref());
            let paths = if paths.is_empty() {
                String::new()
            } else {
                format!(" {}", paths.join(", "))
            };
            format!(
                "{paths}  -> rebase, then 'sluice queue retry {}'",
                job.job_id
            )
        }
        QueueJobState::Queued if job.reason.is_some() => {
            format!("  waiting: {}", failure_text(job))
        }
        QueueJobState::Failed => format!(
            " {}  -> fix and commit, then 'sluice queue retry {}'",
            failure_text(job),
            job.job_id
        ),
        QueueJobState::Verifying
        | QueueJobState::Ready
        | QueueJobState::Approved
        | QueueJobState::Pushing
        | QueueJobState::Landed
            if !gates.is_empty() =>
        {
            let text: Vec<String> = gates.iter().map(gate_text).collect();
            format!("  gates: {}", text.join("  "))
        }
        _ => String::new(),
    }
}

fn job_line(job: &QueueJobView, gates: &[TrainGate]) -> String {
    format!(
        "  {}  job {}  {}{}",
        job.branch,
        job.job_id,
        job_state_label(job.state),
        job_note(job, gates)
    )
}

/// Human block of one train (header, members, hint), each line ending in `\n`.
fn render_train(train: &QueueTrainView, jobs: &[QueueJobView]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "train {}  {}  base {}  {}",
        train.train_id,
        target_name(&train.remote, &train.target),
        short(&train.base_sha),
        train_state_label(train.state)
    );
    for id in &train.members {
        match jobs.iter().find(|j| &j.job_id == id) {
            Some(job) => {
                let _ = writeln!(out, "{}", job_line(job, &train.gates));
            }
            None => {
                let _ = writeln!(out, "  job {id}");
            }
        }
    }
    if let Some(note) = &train.note {
        let _ = writeln!(out, "  note: {note}");
    }
    match train.state {
        QueueTrainState::Ready => {
            let _ = writeln!(
                out,
                "  approve with: sluice queue approve {}",
                train.train_id
            );
        }
        QueueTrainState::NeedsReconcile => {
            let _ = writeln!(
                out,
                "  the push may or may not have happened: run 'sluice queue reconcile'"
            );
        }
        _ => {}
    }
    out
}

/// Trains worth showing to a human: a dissolved train is hidden once a
/// newer train exists for the same (repository, remote, target); its
/// members are then shown by the newer train or on their own (`--json`
/// keeps everything).
fn visible_trains(report: &QueueStatusReport) -> Vec<&QueueTrainView> {
    let superseded = |t: &QueueTrainView| {
        t.state == QueueTrainState::Dissolved
            && report.trains.iter().any(|n| {
                n.repo == t.repo
                    && n.remote == t.remote
                    && n.target == t.target
                    && (n.created_at.as_str(), n.train_id.as_str())
                        > (t.created_at.as_str(), t.train_id.as_str())
            })
    };
    report.trains.iter().filter(|t| !superseded(t)).collect()
}

/// Renders the human form of a `queue-status` report.
fn render_status(report: &QueueStatusReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let trains = visible_trains(report);
    for train in &trains {
        out.push_str(&render_train(train, &report.jobs));
    }
    let in_train = |j: &QueueJobView| {
        trains
            .iter()
            .any(|t| t.members.iter().any(|m| m == &j.job_id))
    };
    let loose: Vec<&QueueJobView> = report
        .jobs
        .iter()
        .filter(|j| {
            !in_train(j) && !matches!(j.state, QueueJobState::Landed | QueueJobState::Cancelled)
        })
        .collect();
    let queued: Vec<&&QueueJobView> = loose
        .iter()
        .filter(|j| j.state == QueueJobState::Queued)
        .collect();
    if !queued.is_empty() {
        let _ = writeln!(out, "queued:");
        for j in queued {
            let position = j
                .position
                .map_or_else(String::new, |p| format!("  position {p}"));
            // Why no train is built yet (base unavailable, base config invalid).
            let waiting = j
                .reason
                .as_ref()
                .map_or_else(String::new, |_| format!("  waiting: {}", failure_text(j)));
            let _ = writeln!(
                out,
                "  {}  job {}  for {}{position}{waiting}",
                j.branch,
                j.job_id,
                target_name(&j.remote, &j.target)
            );
        }
    }
    let others: Vec<&&QueueJobView> = loose
        .iter()
        .filter(|j| j.state != QueueJobState::Queued)
        .collect();
    if !others.is_empty() {
        let _ = writeln!(out, "jobs needing attention:");
        for j in others {
            let _ = writeln!(out, "{}", job_line(j, &[]));
        }
    }
    if out.is_empty() {
        out.push_str("sluice: the merge queue is empty\n");
    }
    out
}

// ------------------------------------------------------------------ approve

/// `sluice queue approve <TRAIN_ID> [--yes]`.
pub fn approve(train_id: &str, yes: bool) -> ExitCode {
    finish(do_approve(train_id, yes))
}

fn do_approve(train_id: &str, yes: bool) -> Outcome {
    let tty = std::io::stdin().is_terminal();
    if !yes && !tty {
        return Err(Failure::usage(
            "approving needs a terminal to confirm; run it in a terminal or pass --yes",
        ));
    }
    let env = process_env();
    let report = fetch_status(&env, None)?;
    let train = report
        .trains
        .iter()
        .find(|t| t.train_id == train_id)
        .ok_or_else(|| {
            Failure::error(format!("no train '{train_id}' (see 'sluice queue status')"))
        })?;
    let Some(hash) = train
        .train_hash
        .clone()
        .filter(|_| train.state == QueueTrainState::Ready)
    else {
        return Err(Failure {
            code: EXIT_REFUSED,
            message: format!(
                "train {train_id} is {}, not ready for approval",
                train_state_label(train.state)
            ),
        });
    };
    println!("{}", render_plan(train, &report.jobs));
    if !yes && !confirm("Land this train? [y/N] ") {
        return Err(Failure::error("not approved"));
    }
    let msg = ClientMessage::QueueApprove {
        id: 1,
        train_id: train_id.to_owned(),
        train_hash: hash,
        via: ApproveVia::Cli,
        env: filter_env(&env),
    };
    let ServerMessage::QueueApproved { train: after, .. } = send(&env, &msg, true)? else {
        return Err(Failure::error("unexpected reply to queue-approve"));
    };
    match after.state {
        QueueTrainState::Approved | QueueTrainState::Pushing => {
            println!("sluice: approved; pushing\u{2026}");
            let user = UserConfig::load(&env).unwrap_or_default();
            wait_for_landing(&env, &after, landing_deadline(&after, user.queue.tick))
        }
        _ => report_final(&after, &report.jobs),
    }
}

/// How long `approve` waits for the landing: the base's `push_timeout`
/// plus one `network_timeout` (the pre-push checks), two runner ticks and a
/// 30 s margin. Defaults (10 min, 60 s) for a daemon that does not report them.
fn landing_deadline(train: &QueueTrainView, tick: Duration) -> Duration {
    let push = Duration::from_millis(train.push_timeout_ms.unwrap_or(600_000));
    let network = Duration::from_millis(train.network_timeout_ms.unwrap_or(60_000));
    push + network + tick * 2 + Duration::from_secs(30)
}

fn render_plan(train: &QueueTrainView, jobs: &[QueueJobView]) -> String {
    use std::fmt::Write as _;
    let mut out = render_train(train, jobs);
    let _ = writeln!(
        out,
        "  target {}  base {}  landing commit {}",
        target_name(&train.remote, &train.target),
        short(&train.base_sha),
        train.merge_sha.as_deref().map_or("?", short)
    );
    let gates: Vec<String> = train.gates.iter().map(gate_text).collect();
    let _ = writeln!(out, "  gates: {}", gates.join("  "));
    out
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

/// Prints the final line for a train and maps its state to the result.
fn report_final(train: &QueueTrainView, jobs: &[QueueJobView]) -> Outcome {
    let target = target_name(&train.remote, &train.target);
    match train.state {
        QueueTrainState::Landed => {
            println!(
                "sluice: landed {} on {target} (train {})",
                train.merge_sha.as_deref().map_or("?", short),
                train.train_id
            );
            Ok(())
        }
        QueueTrainState::NeedsReconcile => Err(Failure::error(format!(
            "train {} needs reconcile: the push outcome is unknown; run 'sluice queue reconcile'",
            train.train_id
        ))),
        QueueTrainState::Dissolved => {
            let failed: Vec<String> = jobs
                .iter()
                .filter(|j| train.members.contains(&j.job_id) && j.state == QueueJobState::Failed)
                .map(|j| format!("{}: {}", j.branch, failure_text(j)))
                .collect();
            if failed.is_empty() {
                let why = train
                    .note
                    .as_deref()
                    .map_or_else(String::new, |n| format!(" ({n})"));
                Err(Failure::error(format!(
                    "train {} was dissolved{why}; its jobs were re-queued (see 'sluice queue status')",
                    train.train_id
                )))
            } else {
                Err(Failure::error(format!(
                    "train {} failed: {}",
                    train.train_id,
                    failed.join("; ")
                )))
            }
        }
        other => Err(Failure::error(format!(
            "train {} is {}",
            train.train_id,
            train_state_label(other)
        ))),
    }
}

/// Polls `queue-status` until the train is no longer approved or pushing,
/// or `deadline` expired (exit 1 with the current state).
fn wait_for_landing(env: &EnvMap, train: &QueueTrainView, deadline: Duration) -> Outcome {
    let started = std::time::Instant::now();
    loop {
        thread::sleep(POLL_PERIOD);
        let report = fetch_status(env, None)?;
        match report.trains.iter().find(|t| t.train_id == train.train_id) {
            Some(t)
                if matches!(
                    t.state,
                    QueueTrainState::Approved | QueueTrainState::Pushing
                ) =>
            {
                if started.elapsed() >= deadline {
                    return Err(Failure::error(wait_expired_text(t, deadline)));
                }
            }
            Some(t) => return report_final(t, &report.jobs),
            None => {
                // The train left the report: judge by its members.
                let mut gone = train.clone();
                gone.state =
                    if report.jobs.iter().any(|j| {
                        train.members.contains(&j.job_id) && j.state == QueueJobState::Landed
                    }) {
                        QueueTrainState::Landed
                    } else {
                        QueueTrainState::Dissolved
                    };
                return report_final(&gone, &report.jobs);
            }
        }
    }
}

/// The message of an `approve` that stopped waiting.
fn wait_expired_text(train: &QueueTrainView, waited: Duration) -> String {
    format!(
        "stopped waiting after {}: train {} is still {}; the daemon goes on (check 'sluice queue status')",
        humantime::format_duration(Duration::from_secs(waited.as_secs())),
        train.train_id,
        train_state_label(train.state)
    )
}

// ------------------------------------------------- retry, cancel, reconcile

/// `sluice queue retry <JOB_ID>`.
pub fn retry(job_id: &str) -> ExitCode {
    finish(do_retry(job_id))
}

fn do_retry(job_id: &str) -> Outcome {
    let env = process_env();
    let repo = Repo::discover(&env)?;
    let report = fetch_status(&env, None)?;
    let job = report
        .jobs
        .iter()
        .find(|j| j.job_id == job_id)
        .ok_or_else(|| Failure::error(format!("no job '{job_id}' (see 'sluice queue status')")))?;
    if job.repo != repo.paths.common_dir {
        return Err(Failure::error(format!(
            "job {job_id} belongs to another repository ({}); run this inside it",
            job.repo.display()
        )));
    }
    let head_sha = repo.branch_head(&job.branch)?;
    let msg = ClientMessage::QueueRetry {
        id: 1,
        job_id: job_id.to_owned(),
        head_sha,
        via: None,
    };
    match send(&env, &msg, false)? {
        ServerMessage::QueueJob { job, .. } => {
            let position = job
                .position
                .map_or_else(String::new, |p| format!(", position {p}"));
            println!(
                "sluice: requeued {} (job {}) for {}{position}",
                job.branch,
                job.job_id,
                target_name(&job.remote, &job.target)
            );
            Ok(())
        }
        other => Err(unexpected("queue-retry", &other)),
    }
}

/// `sluice queue cancel <JOB_ID>`.
pub fn cancel(job_id: &str) -> ExitCode {
    finish(do_cancel(job_id))
}

fn do_cancel(job_id: &str) -> Outcome {
    let env = process_env();
    let msg = ClientMessage::QueueCancel {
        id: 1,
        job_id: job_id.to_owned(),
        via: None,
    };
    match send(&env, &msg, false)? {
        ServerMessage::QueueJob { job, .. } => {
            if job.cancel_requested && job.state == QueueJobState::Pushing {
                println!(
                    "sluice: cancel requested for {} (job {}); it is pushing and is only \
                     cancelled if 'sluice queue reconcile' proves nothing landed",
                    job.branch, job.job_id
                );
            } else {
                println!(
                    "sluice: {} (job {}) is {}",
                    job.branch,
                    job.job_id,
                    job_state_label(job.state)
                );
            }
            Ok(())
        }
        other => Err(unexpected("queue-cancel", &other)),
    }
}

/// `sluice queue reconcile`.
pub fn reconcile() -> ExitCode {
    finish(do_reconcile())
}

fn decision_name(d: ReconcileDecision) -> &'static str {
    match d {
        ReconcileDecision::Landed => "landed",
        ReconcileDecision::Ready => "ready",
        ReconcileDecision::Queued => "queued",
        ReconcileDecision::Cancelled => "cancelled",
        ReconcileDecision::Refused => "refused",
        ReconcileDecision::Unknown => "unknown",
    }
}

fn do_reconcile() -> Outcome {
    let env = process_env();
    let common = Repo::discover(&env).ok().map(|r| r.paths.common_dir);
    let msg = ClientMessage::QueueReconcile {
        id: 1,
        repo: common,
    };
    match send(&env, &msg, true)? {
        ServerMessage::QueueReconcile { results, .. } => {
            if results.is_empty() {
                println!("sluice: no train needs reconciling");
            }
            for r in results {
                println!(
                    "train {}: {} ({})",
                    r.train_id,
                    decision_name(r.decision),
                    r.reason.as_deref().unwrap_or("no reason given")
                );
            }
            Ok(())
        }
        other => Err(unexpected("queue-reconcile", &other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gate(name: &str, status: &str, ms: Option<u64>) -> TrainGate {
        TrainGate {
            gate: name.into(),
            status: status.into(),
            duration_ms: ms,
            ..TrainGate::default()
        }
    }

    fn job(id: &str, branch: &str, state: QueueJobState) -> QueueJobView {
        QueueJobView {
            job_id: id.into(),
            branch: branch.into(),
            state,
            remote: "origin".into(),
            target: "main".into(),
            ..QueueJobView::default()
        }
    }

    fn train(id: &str, state: QueueTrainState, members: &[&str]) -> QueueTrainView {
        QueueTrainView {
            train_id: id.into(),
            remote: "origin".into(),
            target: "main".into(),
            base_sha: "4e1c9a0123456".into(),
            state,
            members: members.iter().map(|m| (*m).to_owned()).collect(),
            ..QueueTrainView::default()
        }
    }

    #[test]
    fn ready_train_shows_members_gates_and_the_approve_hint() {
        let mut t = train("t-1-aaaaaa", QueueTrainState::Ready, &["j1"]);
        t.gates = vec![
            gate("test", "cached", None),
            gate("lint", "pass", Some(12_400)),
        ];
        let report = QueueStatusReport {
            schema: 1,
            trains: vec![t],
            jobs: vec![job("j1", "feat/login", QueueJobState::Ready)],
        };
        let text = render_status(&report);
        assert!(
            text.starts_with(
                "train t-1-aaaaaa  origin/main  base 4e1c9a0  READY (awaiting approval)\n"
            ),
            "{text}"
        );
        assert!(text.contains("feat/login  job j1  merged"), "{text}");
        assert!(
            text.contains("gates: test PASS (cached)  lint PASS 12.4s"),
            "{text}"
        );
        assert!(
            text.contains("approve with: sluice queue approve t-1-aaaaaa"),
            "{text}"
        );
    }

    #[test]
    fn conflict_member_lists_paths_and_the_retry_hint() {
        let mut conflicted = job("j2", "feat/ui", QueueJobState::Conflict);
        conflicted.detail = Some(json!({"paths": ["src/app.ts", "src/b.ts"]}));
        let mut t = train("t-1-aaaaaa", QueueTrainState::Ready, &["j1", "j2"]);
        t.gates = vec![gate("test", "pass", Some(1000))];
        let report = QueueStatusReport {
            schema: 1,
            trains: vec![t],
            jobs: vec![job("j1", "feat/login", QueueJobState::Ready), conflicted],
        };
        let text = render_status(&report);
        assert!(
            text.contains(
                "feat/ui  job j2  CONFLICT src/app.ts, src/b.ts  -> rebase, then 'sluice queue retry j2'"
            ),
            "{text}"
        );
        // Gates are shown for the clean member only.
        assert_eq!(text.matches("gates:").count(), 1, "{text}");
    }

    #[test]
    fn conflict_paths_accept_a_plain_array() {
        assert_eq!(conflict_paths(Some(&json!(["a", "b"]))), ["a", "b"]);
        assert_eq!(conflict_paths(None).len(), 0);
    }

    #[test]
    fn needs_reconcile_train_points_at_reconcile() {
        let report = QueueStatusReport {
            schema: 1,
            trains: vec![train("t-9", QueueTrainState::NeedsReconcile, &["j1"])],
            jobs: vec![job("j1", "feat/x", QueueJobState::NeedsReconcile)],
        };
        let text = render_status(&report);
        assert!(text.contains("NEEDS RECONCILE"), "{text}");
        assert!(text.contains("run 'sluice queue reconcile'"), "{text}");
        assert!(!text.contains("approve with"), "{text}");
    }

    #[test]
    fn queued_jobs_outside_trains_show_their_position() {
        let mut j = job("j3", "feat/api", QueueJobState::Queued);
        j.position = Some(2);
        let report = QueueStatusReport {
            schema: 1,
            trains: vec![],
            jobs: vec![j, job("j4", "old", QueueJobState::Landed)],
        };
        let text = render_status(&report);
        assert!(text.starts_with("queued:\n"), "{text}");
        assert!(
            text.contains("feat/api  job j3  for origin/main  position 2"),
            "{text}"
        );
        assert!(!text.contains("old"), "{text}");
    }

    #[test]
    fn a_queued_job_whose_base_cannot_be_prepared_says_why() {
        let mut j = job("j5", "feat/api", QueueJobState::Queued);
        j.position = Some(1);
        j.reason = Some("base_unavailable".into());
        j.detail = Some(json!({"error": "fatal: no such remote\nmore", "failures": 1}));
        let mut f = job("j6", "feat/b", QueueJobState::Failed);
        f.reason = Some("base_config_invalid".into());
        f.detail = Some(json!({"error": "TOML parse error", "failures": 3}));
        let report = QueueStatusReport {
            schema: 1,
            trains: vec![],
            jobs: vec![j, f],
        };
        let text = render_status(&report);
        assert!(
            text.contains("position 1  waiting: base_unavailable fatal: no such remote\n"),
            "{text}"
        );
        assert!(
            text.contains("FAILED base_config_invalid TOML parse error"),
            "{text}"
        );
    }

    #[test]
    fn superseded_dissolved_trains_are_hidden() {
        let mut old = train("t-1-aaaaaa", QueueTrainState::Dissolved, &["j1", "j2"]);
        old.created_at = "2026-10-22T10:00:00Z".into();
        old.gates = vec![gate("test", "fail", Some(1000))];
        let mut new = train("t-2-bbbbbb", QueueTrainState::Ready, &["j1"]);
        new.created_at = "2026-10-22T10:05:00Z".into();
        let mut other = train("t-0-cccccc", QueueTrainState::Dissolved, &["j9"]);
        other.target = "release".into();
        other.created_at = "2026-10-22T09:00:00Z".into();
        let report = QueueStatusReport {
            schema: 1,
            trains: vec![old, new, other],
            jobs: vec![
                job("j1", "feat/a", QueueJobState::Ready),
                job("j2", "feat/b", QueueJobState::Queued),
            ],
        };
        let text = render_status(&report);
        assert!(!text.contains("t-1-aaaaaa"), "{text}");
        assert!(!text.contains("FAIL"), "{text}");
        assert!(text.contains("t-2-bbbbbb"), "{text}");
        // The newest train of another target stays, even dissolved.
        assert!(text.contains("t-0-cccccc"), "{text}");
        // The hidden train's other member is listed on its own.
        assert!(text.contains("queued:\n  feat/b  job j2"), "{text}");
    }

    #[test]
    fn landing_wait_is_bounded_by_the_base_timeouts() {
        let mut t = train("t-1", QueueTrainState::Approved, &["j1"]);
        t.push_timeout_ms = Some(120_000);
        t.network_timeout_ms = Some(2_000);
        assert_eq!(
            landing_deadline(&t, Duration::from_secs(5)),
            Duration::from_secs(120 + 2 + 10 + 30)
        );
        t.push_timeout_ms = None;
        t.network_timeout_ms = None;
        assert_eq!(
            landing_deadline(&t, Duration::from_secs(5)),
            Duration::from_secs(600 + 60 + 10 + 30)
        );
        let text = wait_expired_text(&t, Duration::from_millis(700_500));
        assert!(
            text.starts_with("stopped waiting after 11m 40s: train t-1 is still APPROVED"),
            "{text}"
        );
        assert!(text.contains("sluice queue status"), "{text}");
    }

    #[test]
    fn an_empty_queue_says_so() {
        let text = render_status(&QueueStatusReport::default());
        assert_eq!(text, "sluice: the merge queue is empty\n");
    }

    #[test]
    fn env_allowlist_keeps_only_present_allowed_variables() {
        let env: EnvMap = [
            ("PATH", "/bin"),
            ("HOME", "/h"),
            ("SSH_AUTH_SOCK", "/s"),
            ("AWS_SECRET_ACCESS_KEY", "x"),
            ("GIT_DIR", "/g"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let kept = filter_env(&env);
        assert_eq!(
            kept.keys().map(String::as_str).collect::<Vec<_>>(),
            ["HOME", "PATH", "SSH_AUTH_SOCK"]
        );
    }

    #[test]
    fn queued_line_matches_the_design() {
        let mut j = job("j1", "feat/login", QueueJobState::Queued);
        j.position = Some(2);
        assert_eq!(
            queued_line(&j),
            "sluice: queued feat/login (job j1) for origin/main, position 2"
        );
        j.remote.clear();
        j.position = None;
        assert_eq!(
            queued_line(&j),
            "sluice: queued feat/login (job j1) for main"
        );
    }
}
