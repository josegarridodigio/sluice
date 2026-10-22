//! One `run` request to the daemon over an async Unix socket.
//!
//! The blocking client of `sluice-gate-proto` cannot be interrupted from
//! another task while it waits for `done`, so `verify` speaks the `run`
//! exchange itself (same wire format, `kb/sluice/design/protocol.md`): on
//! cancellation it sends `cancel` and closes the connection, and the
//! daemon applies `daemon.last_subscriber_gone` to the job. Autostart and
//! the short requests (`status`, `logs`) still use the blocking client.

use std::path::Path;
use std::time::Duration;

use sluice_gate_proto::{
    self as proto, ClientMessage, Outcome, PROTOCOL_VERSION, RunRequest, ServerMessage,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio_util::sync::CancellationToken;

/// Handshake budget (connect, `hello`, `welcome`).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How a `run` request ended.
#[derive(Debug)]
#[allow(clippy::large_enum_variant, reason = "one short-lived value per gate")]
pub(crate) enum RunEnd {
    /// The daemon sent `done`.
    Done {
        /// Job id carried by `done`.
        job_id: String,
        /// Final outcome.
        outcome: Outcome,
        /// Whether `queued` said the request joined an existing job.
        coalesced: bool,
    },
    /// The MCP request was cancelled; the subscription was dropped.
    Cancelled {
        /// Job id, when `queued` arrived before the cancellation.
        job_id: Option<String>,
    },
    /// A sibling gate of the same `verify` failed first (early verdict,
    /// ADR-0011 M4); the subscription was dropped with reason
    /// `early-verdict`.
    EarlyVerdict {
        /// Job id, when `queued` arrived before the verdict.
        job_id: Option<String>,
    },
}

/// Why a `run` request failed.
#[derive(Debug)]
pub(crate) enum RunError {
    /// Before the job was queued (connect, handshake, refusal).
    NotQueued(String),
    /// After `queued`: the connection broke or the reply was malformed.
    Lost(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotQueued(why) => write!(f, "the daemon did not accept the request: {why}"),
            Self::Lost(why) => write!(f, "lost the connection to the daemon: {why}"),
        }
    }
}

async fn send(writer: &mut OwnedWriteHalf, msg: &ClientMessage) -> Result<(), String> {
    let line = proto::encode(msg).map_err(|e| e.to_string())?;
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())
}

/// Sends `request` and follows its events until `done`, cancellation of the
/// MCP request (`ct`) or an early verdict of the `verify` call (`early`).
///
/// `on_event` sees every server message (`queued`, `started`, `progress`).
#[allow(clippy::too_many_lines, reason = "the run exchange, in protocol order")]
pub(crate) async fn run(
    socket: &Path,
    request: RunRequest,
    ct: &CancellationToken,
    early: &CancellationToken,
    mut on_event: impl FnMut(&ServerMessage) + Send,
) -> Result<RunEnd, RunError> {
    let handshake = async {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| format!("cannot connect to {}: {e}", socket.display()))?;
        let (read, mut writer) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        send(
            &mut writer,
            &ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                client: format!("sluice-gate-mcp {}", env!("CARGO_PKG_VERSION")),
                pid: std::process::id(),
            },
        )
        .await?;
        let line = lines
            .next_line()
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the daemon closed the connection".to_owned())?;
        match proto::decode_server(&line) {
            Ok(ServerMessage::Welcome { protocol, .. }) if protocol == PROTOCOL_VERSION => {}
            Ok(ServerMessage::Welcome { protocol, .. }) => {
                return Err(format!(
                    "the running daemon speaks protocol {protocol}, this server speaks \
                     {PROTOCOL_VERSION}; run `sluice daemon stop`"
                ));
            }
            Ok(ServerMessage::Error { message, .. }) => return Err(message),
            Ok(other) => return Err(format!("unexpected handshake reply: {other:?}")),
            Err(e) => return Err(format!("malformed handshake reply: {e}")),
        }
        Ok::<_, String>((lines, writer))
    };
    let (mut lines, mut writer) = tokio::select! {
        () = ct.cancelled() => return Ok(RunEnd::Cancelled { job_id: None }),
        () = early.cancelled() => return Ok(RunEnd::EarlyVerdict { job_id: None }),
        r = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake) => match r {
            Ok(Ok(v)) => v,
            Ok(Err(why)) => return Err(RunError::NotQueued(why)),
            Err(_) => return Err(RunError::NotQueued("the handshake timed out".to_owned())),
        },
    };
    send(&mut writer, &ClientMessage::Run { id: 1, request })
        .await
        .map_err(RunError::NotQueued)?;

    let mut job: Option<String> = None;
    let mut coalesced = false;
    loop {
        let line = tokio::select! {
            () = ct.cancelled() => {
                // Detach explicitly, then close; either alone is enough for the daemon.
                let _ = send(&mut writer, &ClientMessage::Cancel { id: 1, reason: None }).await;
                let _ = writer.shutdown().await;
                return Ok(RunEnd::Cancelled { job_id: job });
            }
            () = early.cancelled() => {
                // The daemon records `killed_by = early-verdict` when this was
                // the job's last subscriber; other subscribers keep it running.
                let cancel = ClientMessage::Cancel {
                    id: 1,
                    reason: Some(proto::CANCEL_EARLY_VERDICT.to_owned()),
                };
                let _ = send(&mut writer, &cancel).await;
                let _ = writer.shutdown().await;
                return Ok(RunEnd::EarlyVerdict { job_id: job });
            }
            line = lines.next_line() => line,
        };
        let fail = |why: String| {
            if job.is_some() {
                RunError::Lost(why)
            } else {
                RunError::NotQueued(why)
            }
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) => return Err(fail("the daemon closed the connection".to_owned())),
            Err(e) => return Err(fail(e.to_string())),
        };
        if proto::line_too_long(&line) {
            return Err(fail("the daemon sent a line longer than 1 MiB".to_owned()));
        }
        let msg = proto::decode_server(&line).map_err(|e| fail(format!("malformed reply: {e}")))?;
        on_event(&msg);
        match msg {
            ServerMessage::Queued {
                job_id,
                coalesced: c,
                ..
            } => {
                job = Some(job_id);
                coalesced = c;
            }
            ServerMessage::Done {
                job_id, outcome, ..
            } => {
                return Ok(RunEnd::Done {
                    job_id,
                    outcome,
                    coalesced,
                });
            }
            ServerMessage::Error { message, .. } => return Err(fail(message)),
            _ => {}
        }
    }
}
