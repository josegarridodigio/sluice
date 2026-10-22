//! The gate reaper: a companion process that terminates the daemon's gate
//! process groups when the daemon dies.
//!
//! Design: `kb/sluice/design/executor.md`, section "Daemon death". The
//! daemon starts one reaper with a pipe on its stdin and tells it about
//! every gate it spawns (`+ <pgid> <ticks>`) and every gate that ended
//! (`- <pgid>`). The reaper is started through a short-lived intermediate
//! process ([`ReaperCommand`], `sluice daemon reaper --detach`) that
//! starts the real reaper ([`detach`]) and exits, so the reaper is not a
//! descendant of the daemon: it belongs to the gates' supervision, not to
//! the daemon's process tree, and is reparented to init (or the session's
//! subreaper). The write end of the pipe lives only
//! in the daemon (close-on-exec), so the kernel closes it when the daemon
//! exits for any reason, SIGKILL included. On end of file the reaper
//! terminates every group still registered (SIGTERM, grace, SIGKILL),
//! checking each with [`executor::group_is_ours`] so that a reused pid is
//! never signalled, and exits. A clean daemon exit finds nothing left to
//! kill. No `unsafe` and no `PR_SET_PDEATHSIG` (see the design note for
//! why).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::executor;

/// How a process is started: the daemon's intermediate step
/// (`sluice daemon reaper --detach`) and, from there, the reaper itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaperCommand {
    /// Executable (the CLI uses `/proc/self/exe`, which keeps working after
    /// `sluice update` replaced the file).
    pub program: PathBuf,
    /// `argv[0]` shown by `ps`.
    pub arg0: OsString,
    /// Arguments.
    pub args: Vec<OsString>,
}

impl ReaperCommand {
    /// The command with an empty environment, `/` as working directory and
    /// its own process group (terminal signals sent to a foreground daemon
    /// do not reach it).
    fn command(&self) -> Command {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new(&self.program);
        c.arg0(&self.arg0)
            .args(&self.args)
            .env_clear()
            .current_dir("/")
            .process_group(0);
        c
    }
}

/// Body of the intermediate process: starts `reaper` with this process's
/// stdin (the daemon's pipe) and stderr, prints its pid on stdout and
/// returns without waiting, so the reaper is reparented when the caller
/// exits.
///
/// # Errors
///
/// The spawn error.
pub fn detach(reaper: &ReaperCommand) -> std::io::Result<u32> {
    let child = reaper
        .command()
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child.id())
}

/// The daemon's end of a running reaper: the write end of its pipe.
#[derive(Debug)]
pub struct Reaper {
    pid: Option<u32>,
    stdin: Mutex<Option<ChildStdin>>,
}

impl Reaper {
    /// Runs the intermediate `cmd` with a pipe on its stdin and waits for it
    /// (it only starts the reaper and prints the reaper's pid).
    ///
    /// # Errors
    ///
    /// The spawn error, or the intermediate failing.
    pub fn spawn(cmd: &ReaperCommand) -> std::io::Result<Self> {
        let mut child = cmd
            .command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take();
        let mut out = String::new();
        if let Some(stdout) = child.stdout.take() {
            let _ = std::io::Read::read_to_string(&mut std::io::BufReader::new(stdout), &mut out);
        }
        let status = child.wait()?;
        let pid = out.trim().parse::<u32>().ok();
        if !status.success() || pid.is_none() {
            return Err(std::io::Error::other(format!(
                "the reaper launcher ended with {status}"
            )));
        }
        Ok(Self {
            pid,
            stdin: Mutex::new(stdin),
        })
    }

    /// Pid of the reaper process.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn send(&self, line: &str) {
        let Ok(mut guard) = self.stdin.lock() else {
            return;
        };
        if let Some(stdin) = guard.as_mut()
            && let Err(e) = stdin.write_all(line.as_bytes())
        {
            tracing::warn!(
                "the gate reaper is gone ({e}); gates of this daemon will only be \
                 reconciled by the next daemon if it dies"
            );
            *guard = None;
        }
    }

    /// Registers a gate's process group.
    pub fn track(&self, pgid: i32, leader_ticks: Option<u64>) {
        if let Some(ticks) = leader_ticks {
            self.send(&format!("+ {pgid} {ticks}\n"));
        }
    }

    /// Forgets a gate's process group (the job ended).
    pub fn untrack(&self, pgid: i32) {
        self.send(&format!("- {pgid}\n"));
    }

    /// Closes the pipe: the reaper finds nothing registered and exits.
    pub fn close(&self) {
        if let Ok(mut guard) = self.stdin.lock() {
            guard.take();
        }
    }
}

/// Body of the reaper process: reads `+ <pgid> <ticks>` / `- <pgid>` lines
/// until end of file (or a read error), then terminates every registered
/// group that still holds processes of its run and returns how many it
/// signalled. Malformed lines are ignored.
pub fn serve(input: impl BufRead, grace: Duration) -> usize {
    let mut groups: BTreeMap<i32, u64> = BTreeMap::new();
    for line in input.lines() {
        let Ok(line) = line else { break };
        let mut parts = line.split_whitespace();
        match (
            parts.next(),
            parts.next().and_then(|p| p.parse::<i32>().ok()),
        ) {
            (Some("+"), Some(pgid)) => {
                if let Some(ticks) = parts.next().and_then(|t| t.parse::<u64>().ok()) {
                    groups.insert(pgid, ticks);
                }
            }
            (Some("-"), Some(pgid)) => {
                groups.remove(&pgid);
            }
            _ => {}
        }
    }
    let alive: Vec<i32> = groups
        .into_iter()
        .filter(|&(pgid, ticks)| executor::group_is_ours(pgid, Some(ticks)))
        .map(|(pgid, _)| pgid)
        .collect();
    if !alive.is_empty() {
        eprintln!(
            "sluice reaper: the daemon is gone; terminating {} gate process group(s): {alive:?}",
            alive.len()
        );
        executor::terminate_groups_blocking(&alive, grace);
    }
    alive.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    /// Pids whose process group is `pgid` (zombies excluded).
    fn members(pgid: i32) -> Vec<u32> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(member) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(i) = stat.rfind(')') else { continue };
            let f: Vec<&str> = stat[i + 1..].split_whitespace().collect();
            if f.first() != Some(&"Z") && f.get(2) == Some(&pgid.to_string().as_str()) {
                out.push(member);
            }
        }
        out
    }

    fn wait_until(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + limit;
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    #[test]
    fn end_of_input_terminates_registered_groups_with_their_children() {
        let mut gate = Command::new("sh")
            .args(["-c", "sleep 307.25 & sleep 307.5; wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = i32::try_from(gate.id()).unwrap();
        assert!(wait_until(Duration::from_secs(10), || members(pgid).len() == 3));
        let ticks = executor::proc_start_ticks(gate.id()).unwrap();
        // A second, unrelated group that was untracked must survive.
        let mut other = Command::new("sleep")
            .arg("307.75")
            .process_group(0)
            .spawn()
            .unwrap();
        let other_pgid = i32::try_from(other.id()).unwrap();
        let other_ticks = executor::proc_start_ticks(other.id()).unwrap();
        let input =
            format!("+ {pgid} {ticks}\n+ {other_pgid} {other_ticks}\n- {other_pgid}\nnonsense\n");
        let n = serve(input.as_bytes(), Duration::from_secs(5));
        assert_eq!(n, 1);
        let _ = gate.wait();
        assert!(
            wait_until(Duration::from_secs(10), || members(pgid).is_empty()),
            "the gate's children must die too: {:?}",
            members(pgid)
        );
        assert!(
            other.try_wait().unwrap().is_none(),
            "untracked group killed"
        );
        other.kill().unwrap();
        other.wait().unwrap();
    }

    #[test]
    fn a_reused_leader_pid_is_never_signalled() {
        let mut victim = Command::new("sleep")
            .arg("308.25")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = i32::try_from(victim.id()).unwrap();
        let ticks = executor::proc_start_ticks(victim.id()).unwrap();
        // Recorded ticks that differ from the live process: a reused pid.
        let input = format!("+ {pgid} {}\n", ticks + 1);
        assert_eq!(serve(input.as_bytes(), Duration::from_secs(1)), 0);
        assert!(victim.try_wait().unwrap().is_none());
        victim.kill().unwrap();
        victim.wait().unwrap();
    }
}
