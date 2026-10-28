//! Throwaway authenticated git servers for the transport tests: an
//! unprivileged `sshd` with a test `ssh-agent`, and a smart HTTP server
//! (`git_http_server.py`: `git http-backend` behind Python's `http.server`
//! with basic auth). Every process is a child of the test, owned by a
//! guard that kills exactly that pid on drop (also when the test panics).

use std::fs;
use std::io::{BufRead as _, BufReader};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Variables of a credentials environment (the approver's or enqueuer's
/// extra env on top of the sandbox env).
pub type Creds = Vec<(String, String)>;

/// A child process killed (and reaped) on drop.
pub struct Proc {
    child: Child,
}

impl Proc {
    fn new(child: Child) -> Self {
        Self { child }
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `PATH` of the test process.
fn path_env() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// The first of `candidates` that exists, else `name` found on `PATH`.
fn find_program(name: &str, candidates: &[&str]) -> Option<PathBuf> {
    for c in candidates {
        if Path::new(c).is_file() {
            return Some(PathBuf::from(c));
        }
    }
    std::env::split_paths(&path_env())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// A port that was free a moment ago (bind to port 0 and release it).
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("a free port")
}

fn write_exec(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
}

fn current_user() -> String {
    let out = Command::new("id").arg("-un").output().expect("id -un");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Lines of a private key file that are key material (its base64 body), to
/// grep for leaks.
fn key_material(private_key: &str) -> Vec<String> {
    private_key
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("-----") && l.len() >= 16)
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------- ssh

/// An `ssh-agent -D` holding some keys.
pub struct Agent {
    pub socket: PathBuf,
    _proc: Proc,
}

impl Agent {
    fn start(ssh_agent: &Path, ssh_add: &Path, socket: PathBuf, keys: &[&Path]) -> Self {
        let child = Command::new(ssh_agent)
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .env_clear()
            .env("PATH", path_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ssh-agent");
        let mut proc_ = Proc::new(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(!proc_.exited(), "ssh-agent exited at start");
            assert!(Instant::now() < deadline, "ssh-agent socket never appeared");
            thread::sleep(Duration::from_millis(20));
        }
        for key in keys {
            let out = Command::new(ssh_add)
                .arg("-q")
                .arg(key)
                .env_clear()
                .env("PATH", path_env())
                .env("SSH_AUTH_SOCK", &socket)
                .stdin(Stdio::null())
                .output()
                .expect("ssh-add");
            assert!(
                out.status.success(),
                "ssh-add {}: {}",
                key.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Self {
            socket,
            _proc: proc_,
        }
    }
}

/// An unprivileged `sshd -D` on a high port of 127.0.0.1.
pub struct Sshd {
    pub log: PathBuf,
    stall_pids: PathBuf,
    _proc: Proc,
}

impl Drop for Sshd {
    /// Kills the `sleep 600` of stalled sessions (pids recorded by the
    /// forced command, checked against `/proc/<pid>/cmdline`); `_proc`
    /// then kills the listener.
    fn drop(&mut self) {
        let pids = fs::read_to_string(&self.stall_pids).unwrap_or_default();
        for pid in pids.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
            let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if cmd.starts_with(b"sleep\x00600") {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }
}

/// A bare repository reached over SSH, with three credentials sets.
pub struct SshRemote {
    pub url: String,
    /// Agent with the authorised key; `GIT_SSH_COMMAND` offers only it.
    pub good: Creds,
    /// Agent with a key the server does not know.
    pub wrong: Creds,
    /// The authorised "stall" key: its `git-receive-pack` sleeps 600 s.
    pub stall: Creds,
    /// Private key material of every client key (must never leak).
    pub secrets: Vec<String>,
    pub sshd: Sshd,
    _agents: (Agent, Agent),
}

/// Starts `sshd` and two agents for the bare repository `bare`; `None`
/// (with the reason printed) when OpenSSH is not installed or `sshd`
/// cannot run unprivileged here: the SSH tests are then skipped.
#[allow(clippy::too_many_lines, reason = "one setup, step by step")]
pub fn ssh_remote(dir: &Path, bare: &Path) -> Option<SshRemote> {
    let tools = (
        find_program("sshd", &["/usr/sbin/sshd", "/usr/bin/sshd"]),
        find_program("ssh", &[]),
        find_program("ssh-keygen", &[]),
        find_program("ssh-agent", &[]),
        find_program("ssh-add", &[]),
    );
    let (Some(sshd), Some(_ssh), Some(keygen), Some(agent), Some(add)) = tools else {
        eprintln!("skipped: OpenSSH (sshd, ssh, ssh-keygen, ssh-agent, ssh-add) is not installed");
        return None;
    };
    fs::create_dir_all(dir).expect("ssh dir");
    let keygen_key = |name: &str| -> PathBuf {
        let p = dir.join(name);
        let out = Command::new(&keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
            .arg(&p)
            .env_clear()
            .env("PATH", path_env())
            .output()
            .expect("ssh-keygen");
        assert!(out.status.success(), "ssh-keygen {name}");
        p
    };
    let host = keygen_key("host_key");
    let good = keygen_key("good");
    let wrong = keygen_key("wrong");
    let stall = keygen_key("stall");
    let pub_of = |k: &Path| fs::read_to_string(k.with_extension("pub")).expect("pub key");
    let stall_pids = dir.join("stall.pids");
    let stall_cmd = dir.join("stall.sh");
    write_exec(
        &stall_cmd,
        &format!(
            "#!/bin/sh\ncase \"$SSH_ORIGINAL_COMMAND\" in\n\
             git-receive-pack*) echo $$ >> '{}'; exec sleep 600 ;;\n\
             *) exec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\" ;;\nesac\n",
            stall_pids.display()
        ),
    );
    let authorized = dir.join("authorized_keys");
    fs::write(
        &authorized,
        format!(
            "{}command=\"{}\",no-pty,no-port-forwarding,no-agent-forwarding {}",
            pub_of(&good),
            stall_cmd.display(),
            pub_of(&stall)
        ),
    )
    .expect("authorized_keys");
    let user = current_user();
    let mut secrets = Vec::new();
    for k in [&good, &wrong, &stall] {
        let text = fs::read_to_string(k).expect("private key");
        secrets.extend(key_material(&text));
    }
    // The agents hold the keys; the private key files go away so the
    // agent is the only way to authenticate.
    let agent_good = Agent::start(&agent, &add, dir.join("agent-good.sock"), &[&good, &stall]);
    let agent_wrong = Agent::start(&agent, &add, dir.join("agent-wrong.sock"), &[&wrong]);
    for k in [&good, &wrong, &stall] {
        fs::remove_file(k).expect("remove private key");
    }

    let log = dir.join("sshd.log");
    let mut started = None;
    for _ in 0..3 {
        let port = free_port();
        let cfg = dir.join("sshd_config");
        fs::write(
            &cfg,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\n\
                 AllowUsers {user}\nStrictModes no\nUsePAM no\nPasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\nPermitRootLogin no\nPidFile none\n\
                 LogLevel VERBOSE\n",
                host.display(),
                authorized.display()
            ),
        )
        .expect("sshd_config");
        let child = Command::new(&sshd)
            .arg("-D")
            .arg("-e")
            .arg("-f")
            .arg(&cfg)
            .env_clear()
            .env("PATH", path_env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).expect("sshd log"))
            .spawn()
            .expect("spawn sshd");
        let mut proc_ = Proc::new(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                started = Some((port, proc_));
                break;
            }
            if proc_.exited() || Instant::now() > deadline {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
        if started.is_some() {
            break;
        }
    }
    let Some((port, proc_)) = started else {
        eprintln!(
            "skipped: sshd cannot run unprivileged here:\n{}",
            fs::read_to_string(&log).unwrap_or_default()
        );
        return None;
    };
    let known_hosts = dir.join("known_hosts");
    let ssh_command = |key: &Path| {
        format!(
            "ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -i {} \
             -o UserKnownHostsFile={} -o GlobalKnownHostsFile=/dev/null \
             -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5",
            key.with_extension("pub").display(),
            known_hosts.display()
        )
    };
    let creds = |agent: &Agent, key: &Path| -> Creds {
        vec![
            (
                "SSH_AUTH_SOCK".to_owned(),
                agent.socket.display().to_string(),
            ),
            ("GIT_SSH_COMMAND".to_owned(), ssh_command(key)),
        ]
    };
    Some(SshRemote {
        url: format!("ssh://{user}@127.0.0.1:{port}{}", bare.display()),
        good: creds(&agent_good, &good),
        wrong: creds(&agent_wrong, &wrong),
        stall: creds(&agent_good, &stall),
        secrets,
        sshd: Sshd {
            log,
            stall_pids,
            _proc: proc_,
        },
        _agents: (agent_good, agent_wrong),
    })
}

// ---------------------------------------------------------------- http

const SERVER_PY: &str = include_str!("git_http_server.py");

/// The smart HTTP server.
pub struct HttpRemote {
    pub url: String,
    /// `GIT_ASKPASS` answering user `alice` with her password.
    pub good: Creds,
    /// `GIT_ASKPASS` answering `alice` with a wrong password.
    pub wrong: Creds,
    /// `GIT_ASKPASS` answering `bob`, whose pushes stall for 600 s.
    pub stall: Creds,
    /// Every password (must never leak).
    pub secrets: Vec<String>,
    /// Password of `carol`, for URLs with embedded credentials.
    pub carol_password: String,
    pub port: u16,
    config: PathBuf,
    _proc: Proc,
}

pub const ALICE_PASSWORD: &str = "alice-pw-7f3a9c41e2";
pub const WRONG_PASSWORD: &str = "wrong-pw-0b8d55e6a1";
pub const BOB_PASSWORD: &str = "bob-pw-4c2e19d7f0";
pub const CAROL_PASSWORD: &str = "carol-pw-91e0a7c3b5";

impl HttpRemote {
    /// Lets fetches and `ls-remote` through without credentials (pushes
    /// still need them).
    pub fn set_anonymous_read(&self, on: bool) {
        write_http_config(&self.config, on);
    }
}

fn write_http_config(path: &Path, anonymous_read: bool) {
    let cfg = serde_json::json!({
        "anonymous_read": anonymous_read,
        "users": {
            "alice": {"password": ALICE_PASSWORD, "mode": "ok"},
            "bob": {"password": BOB_PASSWORD, "mode": "stall"},
            "carol": {"password": CAROL_PASSWORD, "mode": "ok"},
        }
    });
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, cfg.to_string()).expect("http config");
    fs::rename(&tmp, path).expect("http config");
}

/// True when the `git` under test has the HTTP client transport
/// (`git-remote-http` in `git --exec-path`). A git built with `NO_CURL`
/// (the git 2.31.0 job of the matrix) has none.
fn git_has_http_transport() -> bool {
    Command::new("git")
        .arg("--exec-path")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| {
            Path::new(String::from_utf8_lossy(&o.stdout).trim())
                .join("git-remote-http")
                .is_file()
        })
}

/// True when `git http-backend` (the CGI program the test server runs) is
/// installed. Some distributions ship it in a separate package (Alpine:
/// `git-daemon`); without it the server answers an empty advertisement and a
/// fetch fails with `couldn't find remote ref`.
fn git_has_http_backend() -> bool {
    Command::new("git")
        .arg("--exec-path")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| {
            Path::new(String::from_utf8_lossy(&o.stdout).trim())
                .join("git-http-backend")
                .is_file()
        })
}

/// Serves the bare repositories under `root` (`<url>/<name>.git`); `None`
/// (reason printed) without `python3` or without git's HTTP transport.
pub fn http_remote(dir: &Path, root: &Path, repo: &str) -> Option<HttpRemote> {
    let Some(python) = find_program("python3", &[]) else {
        eprintln!("skipped: python3 is not installed");
        return None;
    };
    if !git_has_http_transport() {
        eprintln!(
            "skipped: this git has no HTTP transport (no git-remote-http, built without curl)"
        );
        return None;
    }
    if !git_has_http_backend() {
        eprintln!("skipped: git http-backend is not installed (Alpine: package git-daemon)");
        return None;
    }
    fs::create_dir_all(dir).expect("http dir");
    let script = dir.join("git_http_server.py");
    fs::write(&script, SERVER_PY).expect("server script");
    let config = dir.join("http.json");
    write_http_config(&config, false);
    let mut child = Command::new(python)
        .arg("-I")
        .arg(&script)
        .arg(root)
        .arg(&config)
        .env_clear()
        .env("PATH", path_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(dir.join("http.log")).expect("http log"))
        .spawn()
        .expect("spawn the http server");
    let stdout = child.stdout.take().expect("stdout");
    let proc_ = Proc::new(child);
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read the port");
    let port: u16 = line.trim().parse().unwrap_or_else(|_| {
        panic!(
            "http server did not start: {}",
            fs::read_to_string(dir.join("http.log")).unwrap_or_default()
        )
    });
    let askpass = |name: &str, user: &str, password: &str| -> Creds {
        let p = dir.join(format!("askpass-{name}.sh"));
        write_exec(
            &p,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\nUsername*) echo '{user}' ;;\n*) echo '{password}' ;;\nesac\n"
            ),
        );
        vec![("GIT_ASKPASS".to_owned(), p.display().to_string())]
    };
    Some(HttpRemote {
        url: format!("http://127.0.0.1:{port}/{repo}"),
        good: askpass("good", "alice", ALICE_PASSWORD),
        wrong: askpass("wrong", "alice", WRONG_PASSWORD),
        stall: askpass("stall", "bob", BOB_PASSWORD),
        secrets: [ALICE_PASSWORD, WRONG_PASSWORD, BOB_PASSWORD, CAROL_PASSWORD]
            .map(str::to_owned)
            .to_vec(),
        carol_password: CAROL_PASSWORD.to_owned(),
        port,
        config,
        _proc: proc_,
    })
}
