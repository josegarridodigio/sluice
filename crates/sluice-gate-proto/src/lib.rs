//! IPC protocol types between the Sluice daemon and its clients.
//!
//! # Framing
//!
//! Messages travel over a Unix stream socket as newline-delimited JSON: one
//! JSON object per line, UTF-8, compact (no pretty printing). Use [`encode`]
//! to produce a line (it appends the trailing `\n`) and [`decode_client`] /
//! [`decode_server`] to parse one. Lines longer than [`MAX_LINE_BYTES`] must
//! be rejected; [`line_too_long`] performs that check. This crate does no I/O.
//!
//! # Versioning
//!
//! The integer [`PROTOCOL_VERSION`] is exchanged in the `hello` / `welcome`
//! handshake; any incompatible change bumps it. Within a version, new optional
//! fields may be added to any message: every wire type tolerates unknown
//! fields (no `deny_unknown_fields`) and gives optional fields a
//! `#[serde(default)]`. [`STATUS_SCHEMA_VERSION`] versions the `status` reply
//! payload separately and is bumped when a field changes meaning.
//!
//! Both top-level enums are internally tagged with `type` and use kebab-case
//! spellings, for example `{"type":"run","id":1,...}`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sluice_gate_core::key::CacheKey;
use sluice_gate_core::tree::TreeHash;

/// Version of the daemon <-> client protocol.
pub const PROTOCOL_VERSION: u32 = 1;

/// Version of the `status` reply schema ([`StatusReport::schema`]).
pub const STATUS_SCHEMA_VERSION: u32 = 1;

/// Maximum accepted length of one protocol line, in bytes (1 MiB).
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// Message sent from a client to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[allow(
    clippy::large_enum_variant,
    reason = "short-lived wire value; keeps the documented unboxed API"
)]
pub enum ClientMessage {
    /// Handshake, always the first line from the client.
    Hello {
        /// Protocol version the client speaks.
        protocol: u32,
        /// Client name and version, for diagnostics.
        client: String,
        /// Client process id.
        pid: u32,
    },
    /// Request to run (or coalesce into) a gate job.
    Run {
        /// Client-chosen request id, echoed in every reply.
        id: u64,
        /// The run request payload.
        #[serde(flatten)]
        request: RunRequest,
    },
    /// Request a status report.
    Status {
        /// Client-chosen request id.
        id: u64,
    },
    /// Request the tail of a job log.
    Logs {
        /// Client-chosen request id.
        id: u64,
        /// Job to read; `None` selects the newest job.
        #[serde(default)]
        job_id: Option<String>,
        /// Number of trailing lines wanted.
        #[serde(default)]
        tail: usize,
    },
    /// Fire-and-forget telemetry from a client that did not need the daemon.
    Event {
        /// Client-chosen request id (conventionally 0).
        id: u64,
        /// The event payload.
        #[serde(flatten)]
        event: ClientEvent,
    },
    /// Ask the daemon to stop.
    Stop {
        /// Client-chosen request id.
        id: u64,
        /// Kill running jobs instead of letting them finish.
        #[serde(default)]
        force: bool,
    },
    /// Detach this subscriber from a running `run` request.
    Cancel {
        /// Id of the `run` request to detach from.
        id: u64,
    },
}

/// Payload of a `run` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRequest {
    /// Repository location.
    pub repo: RepoInfo,
    /// Gate definition as resolved by the client.
    pub gate: GateSpec,
    /// Cache key, `None` when the tree is unknown or caching is off.
    #[serde(default)]
    pub key: Option<CacheKey>,
    /// Working tree hash, `None` when unknown.
    #[serde(default)]
    pub tree: Option<TreeHash>,
    /// Committed tree hash, `None` when unknown.
    #[serde(default)]
    pub committed_tree: Option<TreeHash>,
    /// Command line to execute.
    pub cmd: String,
    /// Extra arguments appended to the command.
    #[serde(default)]
    pub extra: Vec<String>,
    /// Working directory for the gate process.
    pub cwd: PathBuf,
    /// Sanitised environment for the gate process.
    #[serde(default)]
    pub child_env: BTreeMap<String, String>,
    /// Environment used by the daemon to probe git state.
    #[serde(default)]
    pub probe_env: BTreeMap<String, String>,
    /// Who triggered the run.
    pub caller: Caller,
    /// Agent session identifier, if any.
    #[serde(default)]
    pub session: Option<String>,
    /// Client process id.
    pub client_pid: u32,
    /// Git hook that triggered the run, if any.
    #[serde(default)]
    pub hook: Option<String>,
}

/// Repository paths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoInfo {
    /// Absolute path of the working tree root.
    pub toplevel: PathBuf,
    /// Absolute path of the common git directory.
    pub common_dir: PathBuf,
    /// Absolute path of the git directory of this worktree.
    pub git_dir: PathBuf,
}

/// Gate definition sent with a run request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateSpec {
    /// Gate name.
    pub name: String,
    /// Configured command.
    pub cmd: String,
    /// Scheduler weight (budget units).
    #[serde(default)]
    pub weight: u32,
    /// Timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: u64,
    /// Extra environment configured for the gate.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Whether results of this gate are cached.
    #[serde(default)]
    pub cache: bool,
}

/// Origin of a run request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Caller {
    /// An AI agent.
    Agent,
    /// A person at a terminal.
    #[default]
    Human,
}

/// Telemetry event payload carried by [`ClientMessage::Event`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum ClientEvent {
    /// The client served a cache hit without contacting the daemon.
    Hit {
        /// Repository the hit happened in.
        repo: PathBuf,
        /// Gate name.
        gate: String,
        /// Estimated time saved, in milliseconds.
        #[serde(default)]
        saved_ms: u64,
    },
}

/// Message sent from the daemon to a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerMessage {
    /// Handshake reply.
    Welcome {
        /// Protocol version the daemon speaks.
        protocol: u32,
        /// Daemon name and version.
        daemon: String,
        /// Unique id of this daemon instance.
        instance: String,
        /// Daemon process id.
        pid: u32,
        /// Daemon start time (RFC 3339).
        started_at: String,
    },
    /// The request entered the scheduler; always the first `run` event.
    Queued {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Position in the queue.
        position: usize,
        /// True when attached to an existing job.
        #[serde(default)]
        coalesced: bool,
        /// Weight of the job.
        #[serde(default)]
        weight: u32,
    },
    /// The gate process started.
    Started {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Start time (RFC 3339).
        started_at: String,
    },
    /// Periodic progress while queued or running.
    Progress {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Milliseconds since the job was queued or started.
        #[serde(default)]
        elapsed_ms: u64,
        /// Bytes written to the log so far.
        #[serde(default)]
        log_bytes: u64,
        /// Queue position while queued.
        #[serde(default)]
        position: Option<usize>,
    },
    /// The job finished; the server closes afterwards.
    Done {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Final outcome.
        outcome: Outcome,
    },
    /// Reply to a status request.
    Status {
        /// Request id.
        id: u64,
        /// The report.
        #[serde(flatten)]
        status: StatusReport,
    },
    /// Reply to a logs request.
    Log {
        /// Request id.
        id: u64,
        /// Job id.
        job_id: String,
        /// Path of the log file.
        path: PathBuf,
        /// Trailing log lines.
        #[serde(default)]
        lines: Vec<String>,
    },
    /// Reply to a stop request.
    Stopping {
        /// Request id.
        id: u64,
        /// Number of jobs still running.
        #[serde(default)]
        running: usize,
    },
    /// Request failed.
    Error {
        /// Request id (0 when unknown).
        #[serde(default)]
        id: u64,
        /// Machine-readable code.
        code: ErrorCode,
        /// Human-readable description.
        #[serde(default)]
        message: String,
    },
}

/// Final result of a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Outcome {
    /// Result category.
    pub status: OutcomeStatus,
    /// Process exit code, when the process exited normally.
    pub exit_code: Option<i32>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Path of the job log.
    pub log_path: Option<PathBuf>,
    /// Last lines of output.
    pub tail: Vec<String>,
    /// Whether a cache marker was written.
    pub marker_written: bool,
    /// Why no marker was written, if relevant.
    pub not_cached_reason: Option<String>,
    /// Job id this request was coalesced with.
    pub coalesced_with: Option<String>,
    /// Human-readable one-liner.
    pub message: String,
}

/// Category of an [`Outcome`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutcomeStatus {
    /// Exit code 0.
    Pass,
    /// Non-zero exit code.
    Fail,
    /// Timeout exceeded.
    Timeout,
    /// Killed (forced stop or last subscriber gone).
    Killed,
    /// A marker appeared while queued; nothing ran.
    Cached,
    /// Sluice itself failed.
    #[default]
    Error,
}

/// Code carried by [`ServerMessage::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// Handshake or protocol version problem.
    Protocol,
    /// Malformed or unknown request.
    BadRequest,
    /// Requested job or resource does not exist.
    NotFound,
    /// Unexpected daemon failure.
    Internal,
}

/// Payload of the `status` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StatusReport {
    /// Schema version ([`STATUS_SCHEMA_VERSION`]).
    pub schema: u32,
    /// Daemon identity.
    pub daemon: DaemonInfo,
    /// Resource budget.
    pub budget: BudgetInfo,
    /// Running jobs.
    pub running: Vec<RunningJob>,
    /// Queued jobs.
    pub queued: Vec<QueuedJob>,
    /// Cumulative counters.
    pub totals: Totals,
}

/// Daemon identity in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DaemonInfo {
    /// Unique instance id.
    pub instance: String,
    /// Process id.
    pub pid: u32,
    /// Start time (RFC 3339).
    pub started_at: String,
    /// Daemon version.
    pub version: String,
    /// Socket path.
    pub socket: PathBuf,
}

/// Budget usage in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BudgetInfo {
    /// Total budget units.
    pub total: u32,
    /// Units in use.
    pub used: u32,
    /// Pressure stall information.
    pub psi: PsiInfo,
}

/// Linux pressure stall information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PsiInfo {
    /// Whether PSI gating is enabled.
    pub enabled: bool,
    /// CPU "some" 10 s average, if available.
    pub cpu_some_avg10: Option<f64>,
    /// Memory "some" 10 s average, if available.
    pub memory_some_avg10: Option<f64>,
    /// Whether PSI currently blocks new jobs.
    pub blocking: bool,
}

/// A running job in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RunningJob {
    /// Job id.
    pub job_id: String,
    /// Repository path.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Weight.
    pub weight: u32,
    /// Start time (RFC 3339).
    pub started_at: String,
    /// Milliseconds since start.
    pub elapsed_ms: u64,
    /// Attached subscribers.
    pub subscribers: usize,
    /// Who requested it.
    pub caller: Caller,
}

/// A queued job in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct QueuedJob {
    /// Job id.
    pub job_id: String,
    /// Repository path.
    pub repo: PathBuf,
    /// Gate name.
    pub gate: String,
    /// Weight.
    pub weight: u32,
    /// Queue time (RFC 3339).
    pub queued_at: String,
    /// Milliseconds waiting so far.
    pub waiting_ms: u64,
    /// Attached subscribers.
    pub subscribers: usize,
    /// Why it is still waiting.
    pub reason: WaitReason,
}

/// Why a queued job is not running yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WaitReason {
    /// Not enough budget units free.
    #[default]
    Budget,
    /// Blocked by system pressure.
    Psi,
    /// Per-repository concurrency cap reached.
    RepoCap,
    /// Capacity reserved for an older queued job.
    HeadReserved,
}

/// Cumulative counters in a status report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Totals {
    /// Start of the counting period (RFC 3339).
    pub since: String,
    /// Jobs run.
    pub runs: u64,
    /// Jobs that passed.
    pub passes: u64,
    /// Jobs that failed.
    pub failures: u64,
    /// Jobs that timed out.
    pub timeouts: u64,
    /// Cache hits.
    pub hits: u64,
    /// Requests coalesced into existing jobs.
    pub coalesced: u64,
    /// Total queue wait, in milliseconds.
    pub wait_ms_total: u64,
    /// Total run time, in milliseconds.
    pub run_ms_total: u64,
    /// Estimated time saved by caching and coalescing, in milliseconds.
    pub time_saved_ms: u64,
}

/// Serialises a message as compact JSON followed by `\n`.
///
/// # Errors
///
/// Returns an error if serialisation fails.
pub fn encode<T: Serialize>(msg: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    Ok(line)
}

/// Parses one client line (trailing newline optional).
///
/// # Errors
///
/// Returns an error for malformed JSON or an unknown `type`.
pub fn decode_client(line: &str) -> Result<ClientMessage, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
}

/// Parses one server line (trailing newline optional).
///
/// # Errors
///
/// Returns an error for malformed JSON or an unknown `type`.
pub fn decode_server(line: &str) -> Result<ServerMessage, serde_json::Error> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
}

/// Returns true when `line` exceeds [`MAX_LINE_BYTES`].
#[must_use]
pub fn line_too_long(line: &str) -> bool {
    line.len() > MAX_LINE_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TREE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn run_request() -> RunRequest {
        let key: CacheKey = serde_json::from_str(&format!("\"{KEY}\"")).unwrap();
        let tree: TreeHash = serde_json::from_str(&format!("\"{TREE}\"")).unwrap();
        RunRequest {
            repo: RepoInfo {
                toplevel: "/abs".into(),
                common_dir: "/abs/.git".into(),
                git_dir: "/abs/.git".into(),
            },
            gate: GateSpec {
                name: "test".into(),
                cmd: "npm test".into(),
                weight: 4,
                timeout_ms: 1_200_000,
                env: BTreeMap::from([("NODE_ENV".into(), "test".into())]),
                cache: true,
            },
            key: Some(key),
            tree: Some(tree.clone()),
            committed_tree: Some(tree),
            cmd: "npm test".into(),
            extra: vec!["--grep".into(), "x".into()],
            cwd: "/abs/worktree".into(),
            child_env: BTreeMap::from([("HOME".into(), "/h".into())]),
            probe_env: BTreeMap::from([("GIT_DIR".into(), "/abs/.git".into())]),
            caller: Caller::Agent,
            session: Some("s1".into()),
            client_pid: 4242,
            hook: Some("pre-commit".into()),
        }
    }

    fn outcome() -> Outcome {
        Outcome {
            status: OutcomeStatus::Pass,
            exit_code: Some(0),
            duration_ms: 32100,
            log_path: Some("/l/j.log".into()),
            tail: vec!["a".into()],
            marker_written: true,
            not_cached_reason: None,
            coalesced_with: Some("j0".into()),
            message: "ok".into(),
        }
    }

    fn status_report() -> StatusReport {
        StatusReport {
            schema: STATUS_SCHEMA_VERSION,
            daemon: DaemonInfo {
                instance: "i".into(),
                pid: 1,
                started_at: "t".into(),
                version: "0.1.0".into(),
                socket: "/s".into(),
            },
            budget: BudgetInfo {
                total: 8,
                used: 5,
                psi: PsiInfo {
                    enabled: true,
                    cpu_some_avg10: Some(12.3),
                    memory_some_avg10: None,
                    blocking: false,
                },
            },
            running: vec![RunningJob {
                job_id: "j".into(),
                repo: "/abs".into(),
                gate: "test".into(),
                weight: 4,
                started_at: "t".into(),
                elapsed_ms: 1200,
                subscribers: 2,
                caller: Caller::Human,
            }],
            queued: vec![QueuedJob {
                job_id: "k".into(),
                repo: "/abs".into(),
                gate: "lint".into(),
                weight: 1,
                queued_at: "t".into(),
                waiting_ms: 300,
                subscribers: 1,
                reason: WaitReason::HeadReserved,
            }],
            totals: Totals {
                runs: 120,
                ..Totals::default()
            },
        }
    }

    #[test]
    fn client_round_trip() {
        let msgs = vec![
            ClientMessage::Hello {
                protocol: 1,
                client: "c".into(),
                pid: 7,
            },
            ClientMessage::Run {
                id: 1,
                request: run_request(),
            },
            ClientMessage::Status { id: 2 },
            ClientMessage::Logs {
                id: 3,
                job_id: None,
                tail: 40,
            },
            ClientMessage::Logs {
                id: 3,
                job_id: Some("j".into()),
                tail: 5,
            },
            ClientMessage::Event {
                id: 0,
                event: ClientEvent::Hit {
                    repo: "/abs".into(),
                    gate: "test".into(),
                    saved_ms: 9,
                },
            },
            ClientMessage::Stop { id: 4, force: true },
            ClientMessage::Cancel { id: 1 },
        ];
        for m in msgs {
            let line = encode(&m).unwrap();
            assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
            assert_eq!(decode_client(&line).unwrap(), m);
        }
    }

    #[test]
    fn server_round_trip() {
        let msgs = vec![
            ServerMessage::Welcome {
                protocol: 1,
                daemon: "d".into(),
                instance: "i".into(),
                pid: 1,
                started_at: "t".into(),
            },
            ServerMessage::Queued {
                id: 1,
                job_id: "j".into(),
                position: 0,
                coalesced: true,
                weight: 4,
            },
            ServerMessage::Started {
                id: 1,
                job_id: "j".into(),
                started_at: "t".into(),
            },
            ServerMessage::Progress {
                id: 1,
                job_id: "j".into(),
                elapsed_ms: 5000,
                log_bytes: 10,
                position: Some(2),
            },
            ServerMessage::Done {
                id: 1,
                job_id: "j".into(),
                outcome: outcome(),
            },
            ServerMessage::Status {
                id: 2,
                status: status_report(),
            },
            ServerMessage::Log {
                id: 3,
                job_id: "j".into(),
                path: "/l".into(),
                lines: vec!["x".into()],
            },
            ServerMessage::Stopping { id: 4, running: 2 },
            ServerMessage::Error {
                id: 5,
                code: ErrorCode::BadRequest,
                message: "m".into(),
            },
        ];
        for m in msgs {
            assert_eq!(decode_server(&encode(&m).unwrap()).unwrap(), m);
        }
        for code in [
            ErrorCode::Protocol,
            ErrorCode::BadRequest,
            ErrorCode::NotFound,
            ErrorCode::Internal,
        ] {
            let m = ServerMessage::Error {
                id: 0,
                code,
                message: String::new(),
            };
            assert_eq!(decode_server(&encode(&m).unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn spec_hello_welcome() {
        let c = decode_client(
            r#"{"type":"hello","protocol":1,"client":"sluice-gate 0.1.0","pid":4242}"#,
        )
        .unwrap();
        assert_eq!(
            c,
            ClientMessage::Hello {
                protocol: 1,
                client: "sluice-gate 0.1.0".into(),
                pid: 4242
            }
        );
        let s = decode_server(
            r#"{"type":"welcome","protocol":1,"daemon":"sluice-gate 0.1.0","instance":"u","pid":1234,"started_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert!(matches!(s, ServerMessage::Welcome { pid: 1234, .. }));
    }

    #[test]
    fn spec_queued_done() {
        let q = decode_server(
            r#"{"type":"queued","id":1,"job_id":"j","position":0,"coalesced":false,"weight":4}"#,
        )
        .unwrap();
        assert_eq!(
            q,
            ServerMessage::Queued {
                id: 1,
                job_id: "j".into(),
                position: 0,
                coalesced: false,
                weight: 4
            }
        );
        let d = decode_server(
            r#"{"type":"done","id":1,"job_id":"j","outcome":{"status":"pass","exit_code":0,"duration_ms":32100,"log_path":"/x/j.log","tail":["last","lines"],"marker_written":true,"not_cached_reason":null,"coalesced_with":null,"message":"ok"}}"#,
        )
        .unwrap();
        let ServerMessage::Done { outcome, .. } = d else {
            panic!("not done");
        };
        assert_eq!(outcome.status, OutcomeStatus::Pass);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.duration_ms, 32100);
        assert_eq!(outcome.tail, ["last", "lines"]);
        assert!(outcome.marker_written);
        assert_eq!(outcome.coalesced_with, None);
    }

    #[test]
    fn spec_status() {
        let line = r#"{"type":"status","id":2,"schema":1,
 "daemon":{"instance":"i","pid":1,"started_at":"t","version":"0.1.0","socket":"/s"},
 "budget":{"total":8,"used":5,"psi":{"enabled":true,"cpu_some_avg10":12.3,"memory_some_avg10":0.4,"blocking":false}},
 "running":[{"job_id":"j","repo":"/abs","gate":"test","weight":4,"started_at":"t","elapsed_ms":1200,"subscribers":2,"caller":"agent"}],
 "queued":[{"job_id":"k","repo":"/abs","gate":"lint","weight":1,"queued_at":"t","waiting_ms":300,"subscribers":1,"reason":"repo-cap"}],
 "totals":{"since":"t","runs":120,"passes":110,"failures":8,"timeouts":2,"hits":430,"coalesced":37,
           "wait_ms_total":1,"run_ms_total":2,"time_saved_ms":3}}"#;
        let ServerMessage::Status { id, status } = decode_server(line).unwrap() else {
            panic!("not status");
        };
        assert_eq!(id, 2);
        assert_eq!(status.schema, 1);
        assert_eq!(status.budget.used, 5);
        assert_eq!(status.budget.psi.cpu_some_avg10, Some(12.3));
        assert_eq!(status.running[0].caller, Caller::Agent);
        assert_eq!(status.queued[0].reason, WaitReason::RepoCap);
        assert_eq!(status.totals.hits, 430);
        assert_eq!(status.totals.time_saved_ms, 3);
    }

    #[test]
    fn spec_run_with_nulls() {
        let line = r#"{"type":"run","id":1,
 "repo":{"toplevel":"/abs","common_dir":"/abs/.git","git_dir":"/abs/.git"},
 "gate":{"name":"test","cmd":"npm test","weight":4,"timeout_ms":1200000,"env":{"NODE_ENV":"test"},"cache":true},
 "key":null,"tree":null,"committed_tree":null,
 "cmd":"npm test","extra":["--grep","x"],
 "cwd":"/abs/worktree","child_env":{"PATH":"p"},"probe_env":{"GIT_DIR":"d"},
 "caller":"human","session":null,"client_pid":4242,"hook":null}"#;
        let ClientMessage::Run { id, request } = decode_client(line).unwrap() else {
            panic!("not run");
        };
        assert_eq!(id, 1);
        assert!(
            request.key.is_none() && request.tree.is_none() && request.committed_tree.is_none()
        );
        assert_eq!(request.caller, Caller::Human);
        assert_eq!(request.gate.timeout_ms, 1_200_000);
    }

    #[test]
    fn spec_event() {
        let m = decode_client(
            r#"{"type":"event","id":0,"event":"hit","repo":"/abs","gate":"test","saved_ms":32100}"#,
        )
        .unwrap();
        assert_eq!(
            m,
            ClientMessage::Event {
                id: 0,
                event: ClientEvent::Hit {
                    repo: "/abs".into(),
                    gate: "test".into(),
                    saved_ms: 32100
                }
            }
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let m = decode_client(r#"{"type":"status","id":2,"future":{"a":1}}"#).unwrap();
        assert_eq!(m, ClientMessage::Status { id: 2 });
        let o: Outcome = serde_json::from_str(r#"{"status":"fail","new_field":true}"#).unwrap();
        assert_eq!(o.status, OutcomeStatus::Fail);
        let s = decode_server(r#"{"type":"stopping","id":4,"running":2,"extra":1}"#).unwrap();
        assert_eq!(s, ServerMessage::Stopping { id: 4, running: 2 });
    }

    #[test]
    fn unknown_type_is_an_error() {
        assert!(decode_client(r#"{"type":"bogus","id":1}"#).is_err());
        assert!(decode_server(r#"{"type":"bogus"}"#).is_err());
        assert!(decode_client("not json").is_err());
    }

    #[test]
    fn spellings() {
        let run = encode(&ClientMessage::Run {
            id: 1,
            request: run_request(),
        })
        .unwrap();
        assert!(run.contains(r#""type":"run""#));
        assert!(run.contains(r#""caller":"agent""#));
        let done = encode(&ServerMessage::Done {
            id: 1,
            job_id: "j".into(),
            outcome: outcome(),
        })
        .unwrap();
        assert!(done.contains(r#""type":"done""#));
        assert!(done.contains(r#""status":"pass""#));
        let err = encode(&ServerMessage::Error {
            id: 1,
            code: ErrorCode::BadRequest,
            message: String::new(),
        })
        .unwrap();
        assert!(err.contains(r#""code":"bad-request""#));
        let st = encode(&OutcomeStatus::Cached).unwrap();
        assert_eq!(st, "\"cached\"\n");
        assert!(encode(&WaitReason::RepoCap).unwrap().contains("repo-cap"));
    }

    #[test]
    fn line_limit() {
        assert!(!line_too_long("short"));
        assert!(!line_too_long(&"a".repeat(MAX_LINE_BYTES)));
        assert!(line_too_long(&"a".repeat(MAX_LINE_BYTES + 1)));
    }
}
