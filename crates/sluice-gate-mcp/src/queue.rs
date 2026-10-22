//! Merge-queue tools: thin clients of the `queue-*` messages.
//!
//! The server resolves the repository, branch, commit and cleanliness with
//! git (through `sluice_gate_core::git`, read-only probes) and sends facts to
//! the daemon, like `sluice queue enqueue` does. Daemon errors become tool
//! error results whose text starts with the error code. Design:
//! `kb/sluice/design/mcp-tools.md` and `kb/sluice/design/merge-queue.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sluice_gate_core::git::{self, EnvMap, RepoPaths};
use sluice_gate_proto::client::{self, ClientError};
use sluice_gate_proto::{
    ApproveVia, Caller, ClientMessage, EnqueueRequest, ErrorCode, QueueJobState, QueueJobView,
    QueueStatusReport, QueueTrainState, QueueTrainView, ServerMessage,
};

/// Environment variables forwarded to the daemon (credentials for fetches
/// and pushes); only those present are sent.
pub(crate) const ENV_ALLOWLIST: [&str; 8] = [
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "HOME",
    "PATH",
    "TERM",
];

/// Refusal returned when agent approval is not enabled in the user config.
pub(crate) const NOT_ALLOWED_TEXT: &str = "not-allowed: approval is reserved to humans (sluice queue approve); set [queue] allow_agent_approval = true in the user config to allow agents";

/// The allowlisted subset of `env`.
pub(crate) fn allowlisted_env(env: &EnvMap) -> BTreeMap<String, String> {
    ENV_ALLOWLIST
        .iter()
        .filter_map(|k| env.get(*k).map(|v| ((*k).to_owned(), v.clone())))
        .collect()
}

// ---------------------------------------------------------------- schemas
//
// The protocol views do not derive `JsonSchema` (the proto crate has no
// schemars). These mirrors exist only to publish `output_schema`s; the
// structured content is the protocol view itself, and a test keeps the
// two in step. Fields skipped on the wire are optional here.

/// One merge-queue job.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct JobSchema {
    /// Job id.
    pub job_id: String,
    /// Git common dir of the repository.
    pub repo: PathBuf,
    /// Worktree the job was enqueued from.
    pub worktree: PathBuf,
    /// Queued branch.
    pub branch: String,
    /// Commit captured at enqueue.
    pub head_sha: String,
    /// Remote of the target; empty for a local target.
    pub remote: String,
    /// Target branch.
    pub target: String,
    /// `queued`, `merging`, `verifying`, `ready`, `approved`, `pushing`,
    /// `landed`, `conflict`, `failed`, `cancelled`, `needs_reconcile`.
    pub state: String,
    /// Train the job belongs to, once claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub train_id: Option<String>,
    /// 1-based position among the queued jobs of its target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    /// `agent` or `human`.
    pub caller: String,
    /// Agent session identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Machine-readable reason of the last side exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Structured detail (conflicting paths, failing gate, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// A cancel was requested while pushing.
    pub cancel_requested: bool,
    /// Attempts so far.
    pub attempts: u32,
    /// Enqueue time (RFC 3339).
    pub enqueued_at: String,
    /// Last state change (RFC 3339).
    pub updated_at: String,
    /// Terminal time (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

/// One gate of a train.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct GateSchema {
    /// Gate name.
    pub gate: String,
    /// Cache key on the train's tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Sluice job id of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// `pending`, `running`, `pass`, `cached`, `fail`, `timeout`, `killed`, `error`.
    pub status: String,
    /// Run time in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// One merge-queue train.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct TrainSchema {
    /// Train id.
    pub train_id: String,
    /// Git common dir of the repository.
    pub repo: PathBuf,
    /// Remote of the target; empty for a local target.
    pub remote: String,
    /// Target branch.
    pub target: String,
    /// Commit the members were merged onto.
    pub base_sha: String,
    /// `building`, `verifying`, `ready`, `approved`, `pushing`, `landed`,
    /// `dissolved`, `needs_reconcile`.
    pub state: String,
    /// Member job ids, in merge order.
    pub members: Vec<String>,
    /// Landing commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_sha: Option<String>,
    /// Gates of the train.
    pub gates: Vec<GateSchema>,
    /// Hash that approves the train.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub train_hash: Option<String>,
    /// `cli` or `mcp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_via: Option<String>,
    /// Approval time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
    /// Creation time.
    pub created_at: String,
    /// Last state change.
    pub updated_at: String,
    /// Landing time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_at: Option<String>,
    /// Free-form note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `queue_status` result (`sluice queue status --json`).
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub(crate) struct StatusSchema {
    /// Schema version (1).
    pub schema: u32,
    /// Trains, oldest first.
    pub trains: Vec<TrainSchema>,
    /// Jobs, in queue order.
    pub jobs: Vec<JobSchema>,
}

/// `queue_approve` result.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ApproveReport {
    /// Train that was approved.
    pub train_id: String,
    /// Last known train state (`landed`, `dissolved`, `needs_reconcile`, or
    /// `approved`/`pushing` when the wait timed out).
    pub state: String,
    /// True when the train landed on its target.
    pub landed: bool,
    /// Last known view of the train.
    pub train: TrainSchema,
}

// ---------------------------------------------------------------- daemon

/// A failure to report as a tool error result: `code: message`.
pub(crate) fn failure(code: &str, message: impl std::fmt::Display) -> String {
    format!("{code}: {message}")
}

fn code_name(code: ErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "internal".to_owned())
}

/// Sends one `queue-*` request (autostarting the daemon) and returns its
/// reply; an `error` reply becomes `code: message`. Blocking.
pub(crate) fn request(env: &EnvMap, msg: &ClientMessage) -> Result<ServerMessage, String> {
    let socket = client::ensure_daemon(env).map_err(|e| {
        failure(
            "unavailable",
            format!("the Sluice daemon is unavailable: {e:#}"),
        )
    })?;
    let client = client::connect(&socket).map_err(|e| match e {
        ClientError::NotRunning(p) => {
            failure("unavailable", format!("no daemon on {}", p.display()))
        }
        other => failure("unavailable", other),
    })?;
    match client.request(msg) {
        Ok(ServerMessage::Error { code, message, .. }) => Err(failure(&code_name(code), message)),
        Ok(other) => Ok(other),
        Err(e) => Err(failure("unavailable", e)),
    }
}

/// `queue-status` for one repository (`None`: all).
pub(crate) fn fetch_status(
    env: &EnvMap,
    repo: Option<PathBuf>,
) -> Result<QueueStatusReport, String> {
    match request(env, &ClientMessage::QueueStatus { id: 1, repo })? {
        ServerMessage::QueueStatus { status, .. } => Ok(status),
        other => Err(failure(
            "internal",
            format!("unexpected reply to queue-status: {other:?}"),
        )),
    }
}

/// Fetches the train, takes its hash (only a `ready` train has one to
/// approve) and sends `queue-approve {via: mcp}`. Returns the repository's
/// common dir and the train as the daemon reports it after approval.
/// Blocking.
pub(crate) fn send_approval(
    env: &EnvMap,
    dir: &Path,
    train_id: &str,
) -> Result<(PathBuf, QueueTrainView), String> {
    let paths = discover(env, dir)?;
    let status = fetch_status(env, Some(paths.common_dir.clone()))?;
    let train = status
        .trains
        .into_iter()
        .find(|t| t.train_id == train_id)
        .ok_or_else(|| failure("not-found", format!("no train '{train_id}'")))?;
    let hash = train
        .train_hash
        .clone()
        .filter(|_| train.state == QueueTrainState::Ready)
        .ok_or_else(|| {
            failure(
                "conflict-state",
                format!(
                    "train {train_id} is {}, not ready for approval",
                    train_state_name(train.state)
                ),
            )
        })?;
    let msg = ClientMessage::QueueApprove {
        id: 1,
        train_id: train_id.to_owned(),
        train_hash: hash,
        via: ApproveVia::Mcp,
        env: allowlisted_env(env),
    };
    match request(env, &msg)? {
        ServerMessage::QueueApproved { train, .. } => Ok((paths.common_dir, train)),
        other => Err(failure(
            "internal",
            format!("unexpected reply to queue-approve: {other:?}"),
        )),
    }
}

/// Repository paths of `dir`.
pub(crate) fn discover(env: &EnvMap, dir: &Path) -> Result<RepoPaths, String> {
    git::discover(dir, env).map_err(|e| failure("not-a-repository", e))
}

// ---------------------------------------------------------------- enqueue

/// Facts of an enqueue, resolved with read-only git probes.
///
/// Without `branch` the worktree's current branch is used (a detached HEAD is
/// refused). Tracked changes in the worktree are always refused: the queue
/// merges commits, and a dirty tree means the agent has not committed yet.
pub(crate) fn enqueue_facts(
    env: &EnvMap,
    dir: &Path,
    branch: Option<&str>,
    base: Option<String>,
) -> Result<EnqueueRequest, String> {
    let paths = discover(env, dir)?;
    let probe = git::probe_env(env, Some(&paths));
    let git_run = |args: &[&str]| git::run(args, &paths.toplevel, &probe, true);

    let dirty = git_run(&["status", "--porcelain", "--untracked-files=no"])
        .map_err(|e| failure("git", e))?;
    if !dirty.is_empty() {
        return Err(failure(
            "dirty-worktree",
            "the worktree has uncommitted changes to tracked files; commit them (after `verify` \
             passes) before calling queue_enqueue",
        ));
    }
    let branch = match branch.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => b.trim_start_matches("refs/heads/").to_owned(),
        None => git_run(&["symbolic-ref", "--short", "-q", "HEAD"])
            .ok()
            .filter(|b| !b.is_empty())
            .ok_or_else(|| {
                failure(
                    "detached-head",
                    "HEAD is detached; check out the branch to queue or pass `branch`",
                )
            })?,
    };
    let reference = format!("refs/heads/{branch}^{{commit}}");
    let head_sha = git_run(&["rev-parse", "--verify", "--quiet", &reference])
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| failure("unknown-branch", format!("no local branch '{branch}'")))?;
    Ok(EnqueueRequest {
        repo: paths.common_dir,
        worktree: paths.toplevel,
        branch,
        head_sha,
        base: base.map(|b| b.trim().to_owned()).filter(|b| !b.is_empty()),
        caller: Caller::Agent,
        session: env
            .get("CLAUDE_CODE_SESSION_ID")
            .filter(|s| !s.is_empty())
            .cloned(),
        env: allowlisted_env(env),
    })
}

/// `remote/target`, or just `target` for a local target.
fn target_name(remote: &str, target: &str) -> String {
    if remote.is_empty() {
        target.to_owned()
    } else {
        format!("{remote}/{target}")
    }
}

/// Text of a successful enqueue.
pub(crate) fn enqueue_text(job: &QueueJobView) -> String {
    let mut out = format!(
        "queued {} (job {}) for {}",
        job.branch,
        job.job_id,
        target_name(&job.remote, &job.target)
    );
    match job.position {
        Some(p) => {
            let _ = write!(out, ", position {p}");
        }
        None => {
            let _ = write!(out, ", state {}", job_state_name(job.state));
        }
    }
    out.push('\n');
    out.push_str(
        "Stop here and tell the human the job id (`queue_status` shows the train id once there \
         is one). A human approves with `sluice queue approve <train id>`.\n",
    );
    out
}

// ---------------------------------------------------------------- text

pub(crate) fn job_state_name(s: QueueJobState) -> &'static str {
    match s {
        QueueJobState::Queued => "queued",
        QueueJobState::Merging => "merging",
        QueueJobState::Verifying => "verifying",
        QueueJobState::Ready => "ready",
        QueueJobState::Approved => "approved",
        QueueJobState::Pushing => "pushing",
        QueueJobState::Landed => "landed",
        QueueJobState::Conflict => "conflict",
        QueueJobState::Failed => "failed",
        QueueJobState::Cancelled => "cancelled",
        QueueJobState::NeedsReconcile => "needs_reconcile",
        QueueJobState::Unknown => "unknown",
    }
}

pub(crate) fn train_state_name(s: QueueTrainState) -> &'static str {
    match s {
        QueueTrainState::Building => "building",
        QueueTrainState::Verifying => "verifying",
        QueueTrainState::Ready => "ready",
        QueueTrainState::Approved => "approved",
        QueueTrainState::Pushing => "pushing",
        QueueTrainState::Landed => "landed",
        QueueTrainState::Dissolved => "dissolved",
        QueueTrainState::NeedsReconcile => "needs_reconcile",
        QueueTrainState::Unknown => "unknown",
    }
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Conflicting paths recorded in a job's `detail` (`{"paths": [...]}` or a
/// bare array of strings).
fn conflict_paths(job: &QueueJobView) -> Vec<String> {
    let list = match &job.detail {
        Some(serde_json::Value::Array(a)) => Some(a),
        Some(serde_json::Value::Object(o)) => o.get("paths").and_then(|p| p.as_array()),
        _ => None,
    };
    list.into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// The `error` field of a job's `detail` (base failures), first line only.
fn detail_error(job: &QueueJobView) -> Option<&str> {
    job.detail
        .as_ref()
        .and_then(|d| d.get("error"))
        .and_then(serde_json::Value::as_str)
        .and_then(|e| e.lines().next())
}

fn job_line(job: &QueueJobView, target: &str, out: &mut String) {
    let state = job_state_name(job.state);
    let _ = write!(out, "  {}  job {}  {state}", job.branch, job.job_id);
    if let (QueueJobState::Queued, Some(p)) = (job.state, job.position) {
        let _ = write!(out, " (position {p})");
    }
    match job.state {
        QueueJobState::Conflict => {
            let paths = conflict_paths(job);
            if paths.is_empty() {
                out.push_str("  CONFLICT");
            } else {
                let _ = write!(out, "  CONFLICT {}", paths.join(", "));
            }
            let _ = write!(
                out,
                "\n    rebase '{}' onto {target}, then call queue_enqueue again",
                job.branch
            );
        }
        QueueJobState::Failed | QueueJobState::NeedsReconcile => {
            if let Some(r) = &job.reason {
                let _ = write!(out, "  ({r})");
            }
            if let Some(e) = detail_error(job) {
                let _ = write!(out, "  {e}");
            }
        }
        // A queued job that cannot be built yet (base unavailable, base
        // config invalid): say why.
        QueueJobState::Queued => {
            if let Some(r) = &job.reason {
                let _ = write!(out, "  waiting: {r}");
                if let Some(e) = detail_error(job) {
                    let _ = write!(out, ": {e}");
                }
            }
        }
        _ => {}
    }
    out.push('\n');
}

fn gates_line(train: &QueueTrainView) -> Option<String> {
    if train.gates.is_empty() {
        return None;
    }
    let parts: Vec<String> = train
        .gates
        .iter()
        .map(|g| {
            let mut s = format!("{} {}", g.gate, g.status.to_uppercase());
            if let Some(ms) = g.duration_ms.filter(|_| g.status != "cached") {
                let _ = write!(s, " {}", crate::text::dur(ms));
            }
            s
        })
        .collect();
    Some(format!("  gates: {}\n", parts.join("  ")))
}

/// Agent-facing summary of a `queue-status` report.
pub(crate) fn render_status(report: &QueueStatusReport) -> String {
    if report.trains.is_empty() && report.jobs.is_empty() {
        return "the merge queue is empty for this repository\n".to_owned();
    }
    let mut out = String::new();
    for t in &report.trains {
        let target = target_name(&t.remote, &t.target);
        let _ = writeln!(
            out,
            "train {}  {target}  base {}  {}",
            t.train_id,
            short(&t.base_sha),
            train_state_name(t.state).to_uppercase()
        );
        for id in &t.members {
            match report.jobs.iter().find(|j| &j.job_id == id) {
                Some(j) => job_line(j, &target, &mut out),
                None => {
                    let _ = writeln!(out, "  job {id}");
                }
            }
        }
        if let Some(g) = gates_line(t) {
            out.push_str(&g);
        }
        match t.state {
            QueueTrainState::Ready => {
                let _ = writeln!(
                    out,
                    "  ready; a human approves with `sluice queue approve {}`",
                    t.train_id
                );
            }
            QueueTrainState::Approved | QueueTrainState::Pushing => {
                out.push_str("  approved; the daemon is pushing\n");
            }
            QueueTrainState::Landed => {
                if let Some(s) = &t.merge_sha {
                    let _ = writeln!(out, "  landed {}", short(s));
                }
            }
            QueueTrainState::NeedsReconcile => {
                out.push_str("  push outcome unknown; a human runs `sluice queue reconcile`\n");
            }
            _ => {}
        }
    }
    let loose: Vec<&QueueJobView> = report
        .jobs
        .iter()
        .filter(|j| !report.trains.iter().any(|t| t.members.contains(&j.job_id)))
        .collect();
    if !loose.is_empty() {
        out.push_str("jobs outside a train:\n");
        for j in loose {
            job_line(j, &target_name(&j.remote, &j.target), &mut out);
        }
    }
    out
}

/// Text of a finished `queue_approve`.
pub(crate) fn approve_text(train: &QueueTrainView, timed_out: bool) -> String {
    let target = target_name(&train.remote, &train.target);
    match train.state {
        QueueTrainState::Landed => format!(
            "landed {} on {target} (train {})\n",
            train.merge_sha.as_deref().map_or("?", short),
            train.train_id
        ),
        QueueTrainState::NeedsReconcile => format!(
            "needs_reconcile: the push outcome of train {} is unknown; a human runs `sluice queue reconcile`\n",
            train.train_id
        ),
        QueueTrainState::Dissolved => format!(
            "failed: train {} was dissolved before it landed; see `queue_status` for the members\n",
            train.train_id
        ),
        s if timed_out => format!(
            "timeout: train {} is still {} after waiting; check `queue_status`\n",
            train.train_id,
            train_state_name(s)
        ),
        s => format!("train {} is {}\n", train.train_id, train_state_name(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_gate_proto::TrainGate;

    fn job(state: QueueJobState) -> QueueJobView {
        QueueJobView {
            job_id: "j1".into(),
            branch: "feat/a".into(),
            remote: "origin".into(),
            target: "main".into(),
            state,
            position: Some(2),
            ..QueueJobView::default()
        }
    }

    fn to_json<T: Serialize>(v: &T) -> serde_json::Value {
        serde_json::to_value(v).expect("json")
    }

    #[test]
    fn enqueue_text_names_branch_job_target_and_position() {
        let t = enqueue_text(&job(QueueJobState::Queued));
        assert!(
            t.starts_with("queued feat/a (job j1) for origin/main, position 2\n"),
            "{t}"
        );
    }

    #[test]
    fn status_text_covers_ready_conflict_and_hint() {
        let mut conflict = job(QueueJobState::Conflict);
        conflict.job_id = "j2".into();
        conflict.branch = "feat/ui".into();
        conflict.detail = Some(serde_json::json!({"paths": ["src/app.ts"]}));
        let report = QueueStatusReport {
            schema: 1,
            trains: vec![QueueTrainView {
                train_id: "t-1".into(),
                remote: "origin".into(),
                target: "main".into(),
                base_sha: "4e1c9a0ffff".into(),
                state: QueueTrainState::Ready,
                members: vec!["j1".into(), "j2".into()],
                gates: vec![TrainGate {
                    gate: "test".into(),
                    status: "cached".into(),
                    ..TrainGate::default()
                }],
                ..QueueTrainView::default()
            }],
            jobs: vec![job(QueueJobState::Ready), conflict],
        };
        let t = render_status(&report);
        assert!(
            t.contains("train t-1  origin/main  base 4e1c9a0  READY"),
            "{t}"
        );
        assert!(t.contains("CONFLICT src/app.ts"), "{t}");
        assert!(t.contains("call queue_enqueue again"), "{t}");
        assert!(t.contains("gates: test CACHED"), "{t}");
        assert!(
            t.contains("ready; a human approves with `sluice queue approve t-1`"),
            "{t}"
        );
        assert!(render_status(&QueueStatusReport::default()).contains("empty"));
    }

    #[test]
    fn status_text_says_why_a_queued_job_waits() {
        let mut j = job(QueueJobState::Queued);
        j.reason = Some("base_unavailable".into());
        j.detail = Some(serde_json::json!({"error": "fatal: gone\nhint", "failures": 2}));
        let report = QueueStatusReport {
            schema: 1,
            trains: vec![],
            jobs: vec![j],
        };
        let t = render_status(&report);
        assert!(t.contains("waiting: base_unavailable: fatal: gone"), "{t}");
        assert!(!t.contains("hint"), "{t}");
    }

    #[test]
    fn allowlist_keeps_only_listed_present_variables() {
        let env: EnvMap = [
            ("HOME", "/h"),
            ("PATH", "/bin"),
            ("SECRET_TOKEN", "x"),
            ("SSH_AUTH_SOCK", "/s"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let got = allowlisted_env(&env);
        assert_eq!(
            got.keys().map(String::as_str).collect::<Vec<_>>(),
            ["HOME", "PATH", "SSH_AUTH_SOCK"]
        );
    }

    #[test]
    fn mirrors_accept_the_protocol_views() {
        let mut j = job(QueueJobState::Failed);
        j.reason = Some("gate".into());
        let _: JobSchema = serde_json::from_value(to_json(&j)).expect("job mirror");
        let t = QueueTrainView {
            gates: vec![TrainGate::default()],
            ..QueueTrainView::default()
        };
        let _: TrainSchema = serde_json::from_value(to_json(&t)).expect("train mirror");
        let s = QueueStatusReport {
            schema: 1,
            trains: vec![t],
            jobs: vec![j],
        };
        let _: StatusSchema = serde_json::from_value(to_json(&s)).expect("status mirror");
    }
}
