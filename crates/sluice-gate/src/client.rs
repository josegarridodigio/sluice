//! Blocking client of the Sluice daemon (std only, no async runtime).
//!
//! Wire protocol: `kb/sluice/design/protocol.md`. Daemon autostart:
//! "Daemon lifecycle" in `kb/sluice/design/executor.md`.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::socket::{self, AddressFamily, SockFlag, SockType, UnixAddr};
use sluice_gate_core::dirs;
use sluice_gate_core::git::EnvMap;
use sluice_gate_proto::{
    self as proto, ClientEvent, ClientMessage, Outcome, PROTOCOL_VERSION, RunRequest,
    ServerMessage, StatusReport,
};

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

/// A connection to the daemon after a successful handshake.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    daemon_pid: u32,
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

/// Checks a handshake reply; returns the daemon pid.
fn check_welcome(line: &str) -> std::result::Result<u32, ClientError> {
    match proto::decode_server(line) {
        Ok(ServerMessage::Welcome { protocol, pid, .. }) if protocol == PROTOCOL_VERSION => Ok(pid),
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

/// Connects to the daemon and performs the `hello` / `welcome` handshake.
///
/// # Errors
///
/// [`ClientError::NotRunning`] when nothing listens on `socket`, otherwise
/// I/O or protocol errors (a protocol mismatch tells the user to run
/// `sluice daemon stop`).
pub fn connect(socket: &Path) -> std::result::Result<Client, ClientError> {
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
    };
    client.send(&hello())?;
    let line = client.read_line()?.ok_or(ClientError::Closed)?;
    client.daemon_pid = check_welcome(&line)?;
    Ok(client)
}

impl Client {
    /// Process id announced by the daemon in `welcome`.
    pub fn daemon_pid(&self) -> u32 {
        self.daemon_pid
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

    /// Sends `run` and streams its events to `on_event` until `done`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Closed`] when the connection drops before `done`,
    /// [`ClientError::Daemon`] when the daemon answers with an error.
    pub fn run(
        mut self,
        request: RunRequest,
        mut on_event: impl FnMut(&ServerMessage),
    ) -> std::result::Result<Outcome, ClientError> {
        self.send(&ClientMessage::Run { id: 1, request })?;
        // Jobs may run for a long time: no read timeout from here on.
        self.reader
            .get_ref()
            .set_read_timeout(None)
            .context("configuring the socket")?;
        loop {
            let msg = self.recv()?;
            on_event(&msg);
            if let ServerMessage::Done { outcome, .. } = msg {
                return Ok(outcome);
            }
        }
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
/// 5 s and releases the lock.
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
        fs::create_dir_all(parent)
            .map_err(|e| fail(format!("cannot create {}: {e}", parent.display())))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| fail(format!("cannot open the log: {e}")))?;
    let log_err = log
        .try_clone()
        .map_err(|e| fail(format!("cannot open the log: {e}")))?;
    let exe = std::env::current_exe().map_err(|e| fail(format!("cannot find myself: {e}")))?;
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
    let mut child = cmd
        .spawn()
        .map_err(|e| fail(format!("cannot spawn: {e}")))?;

    let deadline = Instant::now() + START_WAIT;
    let mut reaped = false;
    loop {
        if reachable(&socket) {
            return Ok(socket);
        }
        if !reaped {
            // Without `--foreground` the spawned process forks and exits 0
            // right away; a non-zero exit means it gave up before forking.
            match child.try_wait() {
                Ok(Some(status)) if !status.success() => {
                    return Err(fail(format!("it exited with {status}")));
                }
                Ok(Some(_)) => reaped = true,
                Ok(None) => {}
                Err(e) => return Err(fail(e.to_string())),
            }
        }
        if Instant::now() >= deadline {
            return Err(fail(format!(
                "no socket at {} after {}s",
                socket.display(),
                START_WAIT.as_secs()
            )));
        }
        thread::sleep(START_POLL);
    }
}
