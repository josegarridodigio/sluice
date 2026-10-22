//! Blocking client of the Sluice daemon (std only, no async runtime),
//! available with the `client` feature (ADR-0008).
//!
//! Wire protocol: `kb/sluice/design/protocol.md`. Daemon autostart:
//! "Daemon lifecycle" in `kb/sluice/design/executor.md`.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::{
    self as proto, ClientEvent, ClientMessage, ExecRequest, ExecSignal, Outcome, PROTOCOL_VERSION,
    RunRequest, ServerMessage, StatusReport,
};
use anyhow::{Context as _, Result, anyhow};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::socket::{self, AddressFamily, SockFlag, SockType, UnixAddr};
use sluice_gate_core::dirs;
use sluice_gate_core::git::EnvMap;

/// Exit status of `sluice daemon` when another daemon still holds
/// `daemon.lock` (for example, one draining its jobs after `stop`).
/// `ensure_daemon` retries on it. Same value as `EX_TEMPFAIL`.
pub const EXIT_ALREADY_RUNNING: u8 = 75;

/// Connect timeout of ordinary requests.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Budget of the best-effort hit event (connect, write, welcome).
const EVENT_BUDGET: Duration = Duration::from_millis(20);
/// Write timeout of every request.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Read timeout of short requests (`status`, `logs`, `stop`).
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a client waits for an autostarted daemon's socket.
const START_WAIT: Duration = Duration::from_secs(5);
/// Poll period while waiting for the socket.
const START_POLL: Duration = Duration::from_millis(20);
/// How long `ensure_daemon` keeps retrying while an old daemon drains.
const DRAIN_WAIT: Duration = Duration::from_secs(5);
/// Retry period while an old daemon drains.
const DRAIN_POLL: Duration = Duration::from_millis(250);

/// Why talking to the daemon failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No daemon is listening on the socket.
    #[error("no daemon is listening on {0}")]
    NotRunning(PathBuf),
    /// The connection closed before the expected reply.
    #[error("the daemon closed the connection")]
    Closed,
    /// The daemon answered with an error message.
    #[error("{0}")]
    Daemon(String),
    /// Anything else (I/O, malformed replies, protocol mismatch).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ClientError {
    /// True when the connection itself broke (end of file or an I/O error),
    /// which after `queued` means the daemon died: it never closes a
    /// subscriber's connection before `done`.
    #[must_use]
    pub fn is_disconnect(&self) -> bool {
        match self {
            Self::Closed => true,
            Self::Other(e) => e.downcast_ref::<std::io::Error>().is_some(),
            Self::NotRunning(_) | Self::Daemon(_) => false,
        }
    }
}

/// The final `done` of a `run` request.
#[derive(Debug, Clone)]
pub struct RunDone {
    /// Id of the job that produced the outcome (the shared job when coalesced).
    pub job_id: String,
    /// The outcome.
    pub outcome: Outcome,
}

/// A connection to the daemon after a successful handshake.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    daemon_pid: u32,
    features: Vec<String>,
}

/// Connects to a Unix socket without blocking longer than `timeout`
/// (a daemon with a full accept backlog makes `connect` return `EAGAIN`).
fn connect_timeout(path: &Path, timeout: Duration) -> std::io::Result<UnixStream> {
    let addr = UnixAddr::new(path)?;
    let start = Instant::now();
    loop {
        let fd = socket::socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
            None,
        )?;
        match socket::connect(fd.as_raw_fd(), &addr) {
            Ok(()) => {
                let stream = UnixStream::from(fd);
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(Errno::EAGAIN | Errno::EINPROGRESS) if start.elapsed() < timeout => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn hello() -> ClientMessage {
    ClientMessage::Hello {
        protocol: PROTOCOL_VERSION,
        client: format!("sluice-gate {}", env!("CARGO_PKG_VERSION")),
        pid: std::process::id(),
    }
}

/// Checks a handshake reply; returns the daemon pid and its features.
fn check_welcome(line: &str) -> std::result::Result<(u32, Vec<String>), ClientError> {
    match proto::decode_server(line) {
        Ok(ServerMessage::Welcome {
            protocol,
            pid,
            features,
            ..
        }) if protocol == PROTOCOL_VERSION => Ok((pid, features)),
        Ok(ServerMessage::Welcome { protocol, .. }) => Err(anyhow!(
            "the running daemon speaks protocol {protocol}, this client speaks \
             {PROTOCOL_VERSION}; run `sluice daemon stop` (the next command starts a matching daemon)"
        )
        .into()),
        Ok(ServerMessage::Error { message, .. }) => Err(anyhow!(
            "the daemon refused the handshake ({message}); run `sluice daemon stop`"
        )
        .into()),
        Ok(other) => Err(anyhow!("unexpected handshake reply: {other:?}").into()),
        Err(e) => Err(anyhow!("malformed handshake reply: {e}").into()),
    }
}

/// Connects to the daemon and performs the `hello` / `welcome` handshake
/// (same as [`Client::connect`]).
///
/// # Errors
///
/// See [`Client::connect`].
pub fn connect(socket: &Path) -> std::result::Result<Client, ClientError> {
    Client::connect(socket)
}

impl Client {
    /// Connects to the daemon and performs the `hello` / `welcome` handshake.
    ///
    /// # Errors
    ///
    /// [`ClientError::NotRunning`] when nothing listens on `socket`, otherwise
    /// I/O or protocol errors (a protocol mismatch tells the user to run
    /// `sluice daemon stop`).
    pub fn connect(socket: &Path) -> std::result::Result<Self, ClientError> {
        handshake(socket)
    }
}

fn handshake(socket: &Path) -> std::result::Result<Client, ClientError> {
    let stream = connect_timeout(socket, CONNECT_TIMEOUT)
        .map_err(|_| ClientError::NotRunning(socket.to_path_buf()))?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .context("configuring the socket")?;
    stream
        .set_read_timeout(Some(REPLY_TIMEOUT))
        .context("configuring the socket")?;
    let writer = stream.try_clone().context("cloning the socket")?;
    let mut client = Client {
        reader: BufReader::new(stream),
        writer,
        daemon_pid: 0,
        features: Vec::new(),
    };
    client.send(&hello())?;
    let line = client.read_line()?.ok_or(ClientError::Closed)?;
    (client.daemon_pid, client.features) = check_welcome(&line)?;
    Ok(client)
}

impl Client {
    /// Process id announced by the daemon in `welcome`.
    pub fn daemon_pid(&self) -> u32 {
        self.daemon_pid
    }

    /// Optional features announced by the daemon in `welcome` (empty for a
    /// daemon that predates them).
    pub fn features(&self) -> &[String] {
        &self.features
    }

    /// True when the daemon announced `feature` (e.g. [`proto::FEATURE_QUEUE`]).
    pub fn supports(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }

    fn send(&mut self, msg: &ClientMessage) -> std::result::Result<(), ClientError> {
        let line = proto::encode(msg).context("encoding a request")?;
        self.writer
            .write_all(line.as_bytes())
            .context("writing to the daemon")?;
        Ok(())
    }

    fn read_line(&mut self) -> std::result::Result<Option<String>, ClientError> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(line)),
            Err(e) => Err(anyhow::Error::from(e)
                .context("reading from the daemon")
                .into()),
        }
    }

    /// Reads the next server message; EOF is [`ClientError::Closed`].
    fn recv(&mut self) -> std::result::Result<ServerMessage, ClientError> {
        let line = self.read_line()?.ok_or(ClientError::Closed)?;
        if proto::line_too_long(&line) {
            return Err(anyhow!("the daemon sent a line longer than 1 MiB").into());
        }
        let msg = proto::decode_server(&line).context("malformed reply from the daemon")?;
        if let ServerMessage::Error { message, .. } = msg {
            return Err(ClientError::Daemon(message));
        }
        Ok(msg)
    }

    /// Sends `run` and streams its events to `on_event` until `done`;
    /// returns the job id and outcome carried by `done`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Closed`] when the connection drops before `done`,
    /// [`ClientError::Daemon`] when the daemon answers with an error.
    pub fn run(
        mut self,
        request: RunRequest,
        mut on_event: impl FnMut(&ServerMessage),
    ) -> std::result::Result<RunDone, ClientError> {
        self.send(&ClientMessage::Run { id: 1, request })?;
        // Jobs may run for a long time: no read timeout from here on.
        self.reader
            .get_ref()
            .set_read_timeout(None)
            .context("configuring the socket")?;
        loop {
            let msg = self.recv()?;
            on_event(&msg);
            if let ServerMessage::Done {
                job_id, outcome, ..
            } = msg
            {
                return Ok(RunDone { job_id, outcome });
            }
        }
    }

    /// Sends `exec` and returns the connection that streams its events.
    /// Only meaningful with a daemon announcing [`proto::FEATURE_EXEC`].
    ///
    /// # Errors
    ///
    /// I/O errors while sending.
    pub fn exec(mut self, request: ExecRequest) -> std::result::Result<ExecConn, ClientError> {
        self.send(&ClientMessage::Exec { id: 1, request })?;
        self.reader
            .get_ref()
            .set_read_timeout(None)
            .context("configuring the socket")?;
        Ok(ExecConn {
            reader: self.reader,
            writer: self.writer,
        })
    }

    /// Asks for the status report.
    ///
    /// # Errors
    ///
    /// I/O, protocol or daemon errors.
    pub fn status(mut self) -> std::result::Result<StatusReport, ClientError> {
        self.send(&ClientMessage::Status { id: 1 })?;
        match self.recv()? {
            ServerMessage::Status { status, .. } => Ok(status),
            other => Err(anyhow!("unexpected reply to status: {other:?}").into()),
        }
    }

    /// Asks for the last `tail` lines of a job log (`None`: the newest job;
    /// `tail = 0`: the daemon's default). Returns the log path and the lines.
    ///
    /// # Errors
    ///
    /// [`ClientError::Daemon`] when the job is unknown, I/O or protocol errors.
    pub fn logs(
        mut self,
        job_id: Option<&str>,
        tail: usize,
    ) -> std::result::Result<(PathBuf, Vec<String>), ClientError> {
        self.send(&ClientMessage::Logs {
            id: 1,
            job_id: job_id.map(str::to_owned),
            tail,
        })?;
        match self.recv()? {
            ServerMessage::Log { path, lines, .. } => Ok((path, lines)),
            other => Err(anyhow!("unexpected reply to logs: {other:?}").into()),
        }
    }

    /// Sends a single-reply request (for example a `queue-*` message) and
    /// returns the daemon's one reply as is. An `error` reply is returned as
    /// [`ServerMessage::Error`], not as [`ClientError::Daemon`], so the
    /// caller can map its code (`conflict-state`, `stale-plan`, ...). Not
    /// for `run`, which streams several replies (use [`Client::run`]).
    ///
    /// # Errors
    ///
    /// I/O or protocol errors, and [`ClientError::Closed`] when the daemon
    /// closes the connection without replying.
    pub fn request(self, msg: &ClientMessage) -> std::result::Result<ServerMessage, ClientError> {
        self.request_timeout(msg, Some(REPLY_TIMEOUT))
    }

    /// Like [`Client::request`] with an explicit read timeout (`None`: wait
    /// as long as the daemon needs, for requests that push or fetch).
    ///
    /// # Errors
    ///
    /// As [`Client::request`].
    pub fn request_timeout(
        mut self,
        msg: &ClientMessage,
        timeout: Option<Duration>,
    ) -> std::result::Result<ServerMessage, ClientError> {
        self.reader
            .get_ref()
            .set_read_timeout(timeout)
            .context("configuring the socket")?;
        self.send(msg)?;
        let line = self.read_line()?.ok_or(ClientError::Closed)?;
        if proto::line_too_long(&line) {
            return Err(anyhow!("the daemon sent a line longer than 1 MiB").into());
        }
        Ok(proto::decode_server(&line).context("malformed reply from the daemon")?)
    }

    /// Asks the daemon to stop; returns the number of running jobs.
    ///
    /// # Errors
    ///
    /// I/O, protocol or daemon errors.
    pub fn stop(mut self, force: bool) -> std::result::Result<usize, ClientError> {
        self.send(&ClientMessage::Stop { id: 1, force })?;
        match self.recv()? {
            ServerMessage::Stopping { running, .. } => Ok(running),
            other => Err(anyhow!("unexpected reply to stop: {other:?}").into()),
        }
    }
}

/// An `exec` request in flight: read its events with [`ExecConn::recv`];
/// forward signals or hang up from another thread with an [`ExecControl`].
#[derive(Debug)]
pub struct ExecConn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl ExecConn {
    /// Next server message (an `error` reply is returned as
    /// [`ClientError::Daemon`]); EOF is [`ClientError::Closed`].
    ///
    /// # Errors
    ///
    /// I/O or protocol errors, [`ClientError::Daemon`] or [`ClientError::Closed`].
    pub fn recv(&mut self) -> std::result::Result<ServerMessage, ClientError> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => return Err(ClientError::Closed),
            Ok(_) => {}
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context("reading from the daemon")
                    .into());
            }
        }
        if proto::line_too_long(&line) {
            return Err(anyhow!("the daemon sent a line longer than 1 MiB").into());
        }
        let msg = proto::decode_server(&line).context("malformed reply from the daemon")?;
        if let ServerMessage::Error { message, .. } = msg {
            return Err(ClientError::Daemon(message));
        }
        Ok(msg)
    }

    /// A handle to forward signals or hang up while another thread reads.
    ///
    /// # Errors
    ///
    /// When the socket cannot be duplicated.
    pub fn control(&self) -> std::result::Result<ExecControl, ClientError> {
        Ok(ExecControl {
            writer: self.writer.try_clone().context("cloning the socket")?,
        })
    }
}

/// Writer side of an [`ExecConn`], usable from a signal-handling thread.
#[derive(Debug)]
pub struct ExecControl {
    writer: UnixStream,
}

impl ExecControl {
    /// Sends `signal{sig}` for the `exec` request.
    ///
    /// # Errors
    ///
    /// I/O errors (the daemon is gone).
    pub fn signal(&self, signal: ExecSignal) -> std::result::Result<(), ClientError> {
        let line =
            proto::encode(&ClientMessage::Signal { id: 1, signal }).context("encoding a signal")?;
        (&self.writer)
            .write_all(line.as_bytes())
            .context("writing to the daemon")?;
        Ok(())
    }

    /// Closes the connection in both directions: the daemon detaches this
    /// subscriber (its cancel path kills a job left without subscribers) and
    /// a blocked [`ExecConn::recv`] returns.
    pub fn hang_up(&self) {
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
    }
}

/// Sends the best-effort `hit` event; never blocks much longer than 20 ms
/// and ignores every failure.
///
/// The client waits for `welcome` before closing: the daemon writes it
/// before reading the event, and a write to a closed peer would make it
/// drop the connection (and the event) on the floor.
pub fn send_hit_event(socket: &Path, repo: &Path, gate: &str, saved_ms: u64) {
    let start = Instant::now();
    let Ok(stream) = connect_timeout(socket, EVENT_BUDGET) else {
        return;
    };
    let remaining = EVENT_BUDGET
        .saturating_sub(start.elapsed())
        .max(Duration::from_millis(1));
    if stream.set_write_timeout(Some(remaining)).is_err()
        || stream.set_read_timeout(Some(remaining)).is_err()
    {
        return;
    }
    let event = ClientMessage::Event {
        id: 0,
        event: ClientEvent::Hit {
            repo: repo.to_path_buf(),
            gate: gate.to_owned(),
            saved_ms,
        },
    };
    let (Ok(h), Ok(e)) = (proto::encode(&hello()), proto::encode(&event)) else {
        return;
    };
    let mut writer = &stream;
    if writer.write_all(format!("{h}{e}").as_bytes()).is_err() {
        return;
    }
    let mut line = String::new();
    let _ = BufReader::new(&stream).read_line(&mut line);
}

/// Returns true when a daemon accepts connections on `socket`.
fn reachable(socket: &Path) -> bool {
    connect_timeout(socket, CONNECT_TIMEOUT).is_ok()
}

/// Makes sure a daemon is running and returns its socket path.
///
/// Tries to connect; on failure takes the exclusive start lock, re-checks,
/// spawns `sluice daemon` detached (stdin null, output appended to the
/// daemon log, own process group), polls the socket every 20 ms for up to
/// 5 s and releases the lock. When the spawned daemon exits with
/// [`EXIT_ALREADY_RUNNING`] (an old daemon is still draining after `stop`),
/// it retries every 250 ms for up to 5 s.
///
/// # Errors
///
/// When the daemon cannot be started; the message names the daemon log.
pub fn ensure_daemon(env: &EnvMap) -> Result<PathBuf> {
    let socket = dirs::socket_path(env).context("cannot locate the daemon socket")?;
    if reachable(&socket) {
        return Ok(socket);
    }
    let log_path = dirs::daemon_log_path(env);
    let fail = |why: String| {
        anyhow!(
            "the daemon could not be started ({why}); see {}",
            log_path.display()
        )
    };

    let lock_path = dirs::start_lock_path(env).map_err(|e| fail(e.to_string()))?;
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| fail(format!("cannot open {}: {e}", lock_path.display())))?;
    let _lock = Flock::lock(lock_file, FlockArg::LockExclusive)
        .map_err(|(_, e)| fail(format!("cannot lock {}: {e}", lock_path.display())))?;
    if reachable(&socket) {
        return Ok(socket);
    }

    if let Some(parent) = log_path.parent() {
        dirs::create_private_dir(parent)
            .map_err(|e| fail(format!("cannot create {}: {e}", parent.display())))?;
    }
    let drain_deadline = Instant::now() + DRAIN_WAIT;
    loop {
        match spawn_daemon(env, &socket, &log_path) {
            Spawn::Ready => return Ok(socket),
            Spawn::Failed(why) => return Err(fail(why)),
            Spawn::AlreadyRunning(why) => {
                if Instant::now() >= drain_deadline {
                    return Err(fail(why));
                }
                thread::sleep(DRAIN_POLL);
                if reachable(&socket) {
                    return Ok(socket);
                }
            }
        }
    }
}

/// Result of one autostart attempt.
enum Spawn {
    /// The socket accepts connections.
    Ready,
    /// The spawned process exited with [`EXIT_ALREADY_RUNNING`].
    AlreadyRunning(String),
    /// Any other failure (the reason).
    Failed(String),
}

/// Spawns `sluice daemon` once and waits up to 5 s for its socket.
fn spawn_daemon(env: &EnvMap, socket: &Path, log_path: &Path) -> Spawn {
    let open_log = || OpenOptions::new().create(true).append(true).open(log_path);
    let (log, log_err) = match (open_log(), open_log()) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return Spawn::Failed(format!("cannot open the log: {e}")),
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return Spawn::Failed(format!("cannot find myself: {e}")),
    };
    // The daemon outlives this hook or terminal: give it a neutral
    // environment without the per-invocation git hook variables.
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .env_clear()
        .envs(env.iter().filter(|(k, _)| !k.starts_with("GIT_")))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Spawn::Failed(format!("cannot spawn: {e}")),
    };

    let deadline = Instant::now() + START_WAIT;
    let mut reaped = false;
    loop {
        if reachable(socket) {
            return Spawn::Ready;
        }
        if !reaped {
            // Without `--foreground` the spawned process forks and exits 0
            // right away; a non-zero exit means it gave up before forking.
            match child.try_wait() {
                Ok(Some(status)) if status.code() == Some(i32::from(EXIT_ALREADY_RUNNING)) => {
                    return Spawn::AlreadyRunning(format!(
                        "another daemon still holds the lock: it exited with {status}"
                    ));
                }
                Ok(Some(status)) if !status.success() => {
                    return Spawn::Failed(format!("it exited with {status}"));
                }
                Ok(Some(_)) => reaped = true,
                Ok(None) => {}
                Err(e) => return Spawn::Failed(e.to_string()),
            }
        }
        if Instant::now() >= deadline {
            return Spawn::Failed(format!(
                "no socket at {} after {}s",
                socket.display(),
                START_WAIT.as_secs()
            ));
        }
        thread::sleep(START_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_with_the_right_protocol_is_accepted() {
        let line = proto::encode(&ServerMessage::Welcome {
            protocol: PROTOCOL_VERSION,
            daemon: "d".into(),
            instance: "i".into(),
            pid: 42,
            started_at: "t".into(),
            features: vec![proto::FEATURE_QUEUE.into()],
        })
        .unwrap();
        assert_eq!(
            check_welcome(&line).unwrap(),
            (42, vec![proto::FEATURE_QUEUE.to_owned()])
        );
        let old = r#"{"type":"welcome","protocol":1,"daemon":"d","instance":"i","pid":7,"started_at":"t"}"#;
        assert_eq!(check_welcome(old).unwrap(), (7, Vec::new()));
    }

    #[test]
    fn mismatched_protocol_and_refusals_are_errors() {
        let line = proto::encode(&ServerMessage::Welcome {
            protocol: PROTOCOL_VERSION + 1,
            daemon: "d".into(),
            instance: "i".into(),
            pid: 42,
            started_at: "t".into(),
            features: Vec::new(),
        })
        .unwrap();
        let err = check_welcome(&line).unwrap_err().to_string();
        assert!(err.contains("sluice daemon stop"), "{err}");
        assert!(check_welcome("{not json").is_err());
    }

    #[test]
    fn connecting_to_nothing_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("none.sock");
        assert!(matches!(
            Client::connect(&socket),
            Err(ClientError::NotRunning(_))
        ));
        assert!(!reachable(&socket));
        // The hit event never fails loudly.
        send_hit_event(&socket, Path::new("/r"), "g", 1);
    }
}
