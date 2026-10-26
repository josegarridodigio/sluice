//! The push of an approved train (design section 6): argument list,
//! process handling (own process group, pid + start ticks, timeout) and the
//! classification of `git push --porcelain` output.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::io::AsyncReadExt;

use crate::git::Git;

/// How a push ended, as far as Git's output tells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushVerdict {
    /// Exit 0: every ref updated.
    Landed,
    /// Definitive rejection because the target moved (`stale info`,
    /// `non-fast-forward`, `fetch first`, any `[rejected]`): nothing landed,
    /// the train is dissolved and its members re-queued.
    Moved(String),
    /// Definitive rejection by policy (`[remote rejected]`, permission
    /// denied) or a refspec git refused locally (`error: src refspec`):
    /// nothing landed, members `failed`.
    Rejected(String),
    /// The repository's `pre-push` hook declined (exit 1, no ref line, no
    /// `fatal:` and no git `error:` line besides `failed to push some
    /// refs`): nothing was sent; members `failed` with `pre_push_hook`.
    HookDeclined(String),
    /// Anything else: signal, timeout, network error, truncated or
    /// unparsable output → `needs_reconcile`.
    Ambiguous(String),
}

/// One `<flag>\t<from>:<to>\t<summary>` line of the porcelain output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RefLine {
    flag: char,
    to: String,
    summary: String,
}

fn ref_lines(stdout: &str) -> Vec<RefLine> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let flag_part = parts.next()?;
            let refs = parts.next()?;
            let summary = parts.next().unwrap_or("").to_owned();
            let mut chars = flag_part.chars();
            let flag = chars.next()?;
            if chars.next().is_some() || !matches!(flag, ' ' | '+' | '-' | '*' | '!' | '=') {
                return None;
            }
            let to = refs.rsplit_once(':').map_or(refs, |(_, t)| t).to_owned();
            Some(RefLine { flag, to, summary })
        })
        .collect()
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// True when git's output says the remote refused the credentials (or
/// there were none to give): SSH `Permission denied (publickey)`, a forge's
/// `Permission to o/r denied`, HTTP `Authentication failed` / 401 / 403,
/// `could not read Username/Password` (no askpass helper and no terminal),
/// or an unknown host key under `StrictHostKeyChecking`. The server
/// refused the connection before any ref update, and asking again with the
/// same environment gives the same answer: a definitive failure, never a
/// reason to retry or to reconcile.
#[must_use]
pub fn credentials_refused(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("permission denied")
        || (lower.contains("permission to") && lower.contains("denied"))
        || lower.contains("authentication failed")
        || lower.contains("returned error: 401")
        || lower.contains("returned error: 403")
        || lower.contains("could not read username")
        || lower.contains("could not read password")
        || lower.contains("host key verification failed")
}

/// Classifies the end of `git push --atomic --porcelain`.
/// `exit` is `None` when the process was killed by a signal; `target_ref`
/// is `refs/heads/<target>`.
#[must_use]
pub fn classify(exit: Option<i32>, stdout: &str, stderr: &str, target_ref: &str) -> PushVerdict {
    let Some(code) = exit else {
        return PushVerdict::Ambiguous("push killed by a signal".into());
    };
    let lines = ref_lines(stdout);
    let done = stdout.lines().any(|l| l.trim() == "Done");
    if code == 0 {
        if lines.iter().any(|l| l.flag == '!') {
            return PushVerdict::Ambiguous("exit 0 with a rejected ref".into());
        }
        return PushVerdict::Landed;
    }
    if lines.is_empty() {
        if credentials_refused(stderr) {
            return PushVerdict::Rejected(format!("permission denied: {}", tail(stderr, 3)));
        }
        let fatal = stderr.lines().any(|l| l.trim_start().starts_with("fatal:"));
        // git's own `error:` lines other than the final summary.
        let git_errors: Vec<&str> = stderr
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with("error:") && !l.contains("failed to push some refs"))
            .collect();
        // A refspec git refused locally: nothing was sent.
        if git_errors.iter().any(|l| {
            l.starts_with("error: src refspec")
                || l.starts_with("error: dst refspec")
                || l.starts_with("error: unable to push to unqualified destination")
        }) {
            return PushVerdict::Rejected(format!("refspec refused: {}", tail(stderr, 3)));
        }
        // The `pre-push` hook declined: exit 1, nothing but the hook's own
        // output and git's summary line. Any other git `error:` line makes
        // the end ambiguous rather than a hook's verdict.
        if code == 1
            && !fatal
            && git_errors.is_empty()
            && stderr.contains("failed to push some refs")
        {
            let out: Vec<&str> = stderr
                .lines()
                .filter(|l| !l.contains("failed to push some refs"))
                .collect();
            return PushVerdict::HookDeclined(tail(&out.join("\n"), 20));
        }
        return PushVerdict::Ambiguous(format!("push failed (exit {code}): {}", tail(stderr, 3)));
    }
    if !done {
        return PushVerdict::Ambiguous("truncated porcelain output".into());
    }
    let main = lines.iter().find(|l| l.to == target_ref);
    let pick = |l: &RefLine| -> Option<PushVerdict> {
        if l.flag != '!' {
            return None;
        }
        let s = l.summary.as_str();
        if s.contains("atomic push failed") {
            return None;
        }
        if s.contains("[remote rejected]") || s.contains("permission denied") {
            return Some(PushVerdict::Rejected(s.to_owned()));
        }
        if s.contains("[rejected]") {
            return Some(PushVerdict::Moved(s.to_owned()));
        }
        None
    };
    if let Some(v) = main.and_then(pick) {
        return v;
    }
    // The target only failed because of the atomic transaction: look at the
    // other refs. A stale audit ref means `S` may already have landed.
    for l in lines.iter().filter(|l| l.to != target_ref) {
        match pick(l) {
            Some(PushVerdict::Moved(s)) => {
                return PushVerdict::Ambiguous(format!("audit ref rejected: {s}"));
            }
            Some(v) => return v,
            None => {}
        }
    }
    PushVerdict::Ambiguous(format!(
        "push failed (exit {code}) without a definitive reason"
    ))
}

/// Arguments of the push (without the leading `git`).
#[must_use]
pub fn push_args(remote: &str, target: &str, base_sha: &str, s: &str, audit: bool) -> Vec<String> {
    let mut v = vec![
        "push".to_owned(),
        "--atomic".to_owned(),
        "--porcelain".to_owned(),
        "--no-follow-tags".to_owned(),
        "--recurse-submodules=no".to_owned(),
        format!("--force-with-lease=refs/heads/{target}:{base_sha}"),
    ];
    if audit {
        v.push(format!("--force-with-lease=refs/sluice/landed/{s}:"));
    }
    v.push("--".to_owned());
    v.push(remote.to_owned());
    v.push(format!("{s}:refs/heads/{target}"));
    if audit {
        v.push(format!("{s}:refs/sluice/landed/{s}"));
    }
    v
}

/// End of a push process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushEnd {
    /// The process exited (`code` `None` = signal).
    Exited {
        /// Exit code.
        code: Option<i32>,
        /// Captured stdout.
        stdout: String,
        /// Captured stderr.
        stderr: String,
    },
    /// `push_timeout` expired; the group was terminated. What the push (and
    /// its `pre-push` hook) printed before that, as far as the pipes were
    /// drained.
    TimedOut {
        /// Captured stdout.
        stdout: String,
        /// Captured stderr.
        stderr: String,
    },
}

/// A spawned push.
#[derive(Debug)]
pub struct PushProcess {
    child: tokio::process::Child,
    /// Process id (== process group id).
    pub pid: u32,
    /// Start ticks (`executor::proc_start_ticks`).
    pub start_ticks: Option<u64>,
    out: tokio::task::JoinHandle<String>,
    err: tokio::task::JoinHandle<String>,
}

/// Spawns `git <args>` in `cwd` (the merge worktree) in its own process
/// group, hooks **enabled** (the repository's `pre-push` runs). `program`
/// replaces `git` (tests only: an injected spawn failure).
///
/// # Errors
///
/// Spawn failure.
pub fn spawn(
    program: Option<&Path>,
    git: &Git,
    cwd: &Path,
    args: &[String],
) -> Result<PushProcess> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let std_cmd = match program {
        None => sluice_gate_core::git::command(&refs, cwd, git.env()),
        Some(p) => {
            let mut c = std::process::Command::new(p);
            c.args(&refs)
                .current_dir(cwd)
                .env_clear()
                .envs(git.env())
                .stdin(Stdio::null());
            c
        }
    };
    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(false);
    let mut child = cmd.spawn().context("spawning git push")?;
    let pid = child
        .id()
        .context("git push exited before it could be tracked")?;
    let start_ticks = sluice_gate_daemon::executor::proc_start_ticks(pid);
    let mut so = child.stdout.take().context("push stdout")?;
    let mut se = child.stderr.take().context("push stderr")?;
    let out = tokio::spawn(async move {
        let mut s = String::new();
        let _ = so.read_to_string(&mut s).await;
        s
    });
    let err = tokio::spawn(async move {
        let mut s = String::new();
        let _ = se.read_to_string(&mut s).await;
        s
    });
    Ok(PushProcess {
        child,
        pid,
        start_ticks,
        out,
        err,
    })
}

impl PushProcess {
    /// Waits up to `timeout`; on expiry terminates the process group
    /// (SIGTERM, SIGKILL after 2 s) and reaps the process.
    pub async fn wait(mut self, timeout: Duration) -> PushEnd {
        let status = tokio::time::timeout(timeout, self.child.wait()).await;
        match status {
            Ok(Ok(st)) => {
                let collect = Duration::from_secs(5);
                let stdout = tokio::time::timeout(collect, self.out)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                let stderr = tokio::time::timeout(collect, self.err)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                PushEnd::Exited {
                    code: st.code(),
                    stdout,
                    stderr,
                }
            }
            Ok(Err(e)) => PushEnd::Exited {
                code: None,
                stdout: String::new(),
                stderr: format!("cannot wait for git push: {e}"),
            },
            Err(_) => {
                let pgid = i32::try_from(self.pid).unwrap_or(i32::MAX);
                sluice_gate_daemon::executor::terminate_group(pgid, Duration::from_secs(2)).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
                // The pipes close when the group dies: a short bounded wait
                // keeps what the hook printed (a process that escaped the
                // group and holds a pipe open only costs this wait).
                let drain = Duration::from_secs(2);
                let stdout = drained(&mut self.out, drain).await;
                let stderr = drained(&mut self.err, drain).await;
                PushEnd::TimedOut { stdout, stderr }
            }
        }
    }
}

/// The output of a reader task within `limit`; the task is aborted (and the
/// output lost) when it does not finish in time.
async fn drained(task: &mut tokio::task::JoinHandle<String>, limit: Duration) -> String {
    if let Ok(joined) = tokio::time::timeout(limit, &mut *task).await {
        joined.unwrap_or_default()
    } else {
        task.abort();
        String::new()
    }
}

/// The note of a push that hit `push_timeout`: `push_timeout`, followed by
/// the last lines the push printed (stderr, then stdout) when there are any.
#[must_use]
pub fn timeout_note(stdout: &str, stderr: &str) -> String {
    let mut text = tail(stderr, TIMEOUT_TAIL_LINES);
    if text.is_empty() {
        text = tail(stdout, TIMEOUT_TAIL_LINES);
    }
    if text.is_empty() {
        return "push_timeout".to_owned();
    }
    // Bounded: the note lands in the train row, its events and `status`.
    let text: String = text.chars().take(TIMEOUT_TAIL_CHARS).collect();
    format!("push_timeout: {text}")
}

/// Lines of the killed push's output kept in the `push_timeout` note.
const TIMEOUT_TAIL_LINES: usize = 20;

/// Characters of those lines kept.
const TIMEOUT_TAIL_CHARS: usize = 4000;

/// Lands a train on a local branch: `update-ref` compare-and-swap from
/// `base_sha` to `s`, then the audit ref. Hooks disabled. `Ok(false)` when
/// the compare-and-swap failed (the branch moved).
///
/// # Errors
///
/// Spawn failure.
pub fn land_local(
    git: &Git,
    common: &Path,
    train_id: &str,
    target: &str,
    base_sha: &str,
    s: &str,
    audit: bool,
) -> Result<bool> {
    let refname = format!("refs/heads/{target}");
    let msg = format!("sluice: land {train_id}");
    let out = git.repo_out(common, &["update-ref", "-m", &msg, &refname, s, base_sha])?;
    if !out.status.success() {
        return Ok(false);
    }
    if audit {
        let audit_ref = format!("refs/sluice/landed/{s}");
        let _ = git.repo(common, &["update-ref", &audit_ref, s]);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    const T: &str = "refs/heads/main";

    #[test]
    fn success() {
        let out = "To /srv/r.git\n \tabc:refs/heads/main\td6e5f44..4f2112f\n*\tabc:refs/sluice/landed/abc\t[new reference]\nDone\n";
        assert_eq!(classify(Some(0), out, "", T), PushVerdict::Landed);
    }

    #[test]
    fn stale_info_is_moved() {
        let out = "To ../r.git\n!\tabc:refs/heads/main\t[rejected] (stale info)\n!\tabc:refs/sluice/landed/abc\t[rejected] (atomic push failed)\nDone\n";
        let err = "error: atomic push failed for ref refs/heads/main. status: 7\nerror: failed to push some refs to '../r.git'\n";
        assert!(
            matches!(classify(Some(1), out, err, T), PushVerdict::Moved(s) if s.contains("stale info"))
        );
    }

    #[test]
    fn non_fast_forward_is_moved() {
        let out = "To ../r.git\n!\tabc:refs/heads/main\t[rejected] (non-fast-forward)\n!\tabc:refs/sluice/landed/abc\t[rejected] (atomic push failed)\nDone\n";
        assert!(
            matches!(classify(Some(1), out, "", T), PushVerdict::Moved(s) if s.contains("non-fast-forward"))
        );
    }

    #[test]
    fn remote_rejected_is_policy() {
        let out = "To ../r.git\n!\tabc:refs/heads/main\t[remote rejected] (pre-receive hook declined)\n!\tabc:refs/sluice/landed/abc\t[remote rejected] (pre-receive hook declined)\nDone\n";
        assert!(matches!(
            classify(Some(1), out, "remote: no\n", T),
            PushVerdict::Rejected(_)
        ));
    }

    #[test]
    fn permission_denied_is_policy() {
        let err = "ERROR: Permission to o/r.git denied to someone.\nfatal: Could not read from remote repository.\n\nPlease make sure you have the correct access rights\nand the repository exists.\n";
        assert!(
            matches!(classify(Some(128), "", err, T), PushVerdict::Rejected(s) if s.starts_with("permission denied"))
        );
        let err = "git@host: Permission denied (publickey).\nfatal: Could not read from remote repository.\n";
        assert!(matches!(
            classify(Some(128), "", err, T),
            PushVerdict::Rejected(_)
        ));
    }

    #[test]
    fn refused_credentials_are_definitive() {
        for err in [
            "remote: authentication required\nfatal: Authentication failed for 'http://127.0.0.1:1/r.git/'\n",
            "fatal: unable to access 'https://h/r.git/': The requested URL returned error: 401\n",
            "fatal: could not read Username for 'https://h': terminal prompts disabled\n",
            "Host key verification failed.\nfatal: Could not read from remote repository.\n",
        ] {
            assert!(credentials_refused(err), "{err}");
            assert!(
                matches!(classify(Some(128), "", err, T), PushVerdict::Rejected(_)),
                "{err}"
            );
        }
        assert!(!credentials_refused(
            "fatal: unable to access 'https://h/r.git/': Could not resolve host: h\n"
        ));
        assert!(!credentials_refused(
            "ssh: connect to host h port 22: Connection timed out\n"
        ));
    }

    #[test]
    fn pre_push_hook_declined() {
        let err = "hook says no\nerror: failed to push some refs to '../r.git'\n";
        assert_eq!(
            classify(Some(1), "", err, T),
            PushVerdict::HookDeclined("hook says no".into())
        );
    }

    #[test]
    fn hook_declined_needs_no_git_error_besides_the_summary() {
        // A hook that prints a ref-shaped `error:` line itself still declines
        // only when git added nothing of its own: here git did.
        let err =
            "error: RPC failed; curl 56 recv failure\nerror: failed to push some refs to 'h'\n";
        assert!(matches!(
            classify(Some(1), "", err, T),
            PushVerdict::Ambiguous(_)
        ));
        let err = "hook output\nerror: some other git complaint\nerror: failed to push some refs to 'h'\n";
        assert!(matches!(
            classify(Some(1), "", err, T),
            PushVerdict::Ambiguous(_)
        ));
        // Exit 1 without the summary line is not a hook's verdict either.
        assert!(matches!(
            classify(Some(1), "", "something odd\n", T),
            PushVerdict::Ambiguous(_)
        ));
    }

    #[test]
    fn a_refused_refspec_is_definitive() {
        let err = "error: src refspec deadbeef does not match any\nerror: failed to push some refs to '../r.git'\n";
        assert!(
            matches!(classify(Some(1), "", err, T), PushVerdict::Rejected(s) if s.starts_with("refspec refused"))
        );
        let err = "error: dst refspec refs/heads/x matches more than one\nerror: failed to push some refs to 'r'\n";
        assert!(matches!(
            classify(Some(1), "", err, T),
            PushVerdict::Rejected(_)
        ));
    }

    #[test]
    fn network_failure_is_ambiguous() {
        let err = "fatal: unable to access 'https://h/r.git/': Could not resolve host: h\n";
        assert!(matches!(
            classify(Some(128), "", err, T),
            PushVerdict::Ambiguous(_)
        ));
        let err = "fatal: '/nonexistent/x.git' does not appear to be a git repository\nfatal: Could not read from remote repository.\n";
        assert!(matches!(
            classify(Some(128), "", err, T),
            PushVerdict::Ambiguous(_)
        ));
    }

    #[test]
    fn truncated_output_is_ambiguous() {
        let out = "To /srv/r.git\n \tabc:refs/heads/main\td6e5f44..4f2112f\n";
        assert!(
            matches!(classify(Some(1), out, "", T), PushVerdict::Ambiguous(s) if s.contains("truncated"))
        );
        assert!(matches!(
            classify(None, out, "", T),
            PushVerdict::Ambiguous(_)
        ));
    }

    #[test]
    fn stale_audit_ref_is_ambiguous() {
        let out = "To r\n!\tabc:refs/heads/main\t[rejected] (atomic push failed)\n!\tabc:refs/sluice/landed/abc\t[rejected] (stale info)\nDone\n";
        assert!(matches!(
            classify(Some(1), out, "", T),
            PushVerdict::Ambiguous(_)
        ));
    }

    #[test]
    fn the_timeout_note_keeps_the_tail_of_the_output() {
        assert_eq!(timeout_note("", ""), "push_timeout");
        assert_eq!(timeout_note("\n  \n", "\n"), "push_timeout");
        let err: String = (1..=30).fold(String::new(), |mut s, n| {
            let _ = writeln!(s, "line {n}");
            s
        });
        let note = timeout_note("stdout text\n", &err);
        assert!(note.starts_with("push_timeout: line 11\n"), "{note}");
        assert!(note.ends_with("line 30"), "{note}");
        assert!(!note.contains("line 10\n"));
        assert!(!note.contains("stdout text"));
        // With nothing on stderr, stdout is used.
        assert_eq!(timeout_note("only out\n", ""), "push_timeout: only out");
        let long = "x".repeat(10_000);
        assert!(timeout_note("", &long).chars().count() < 4100);
    }

    #[test]
    fn args_with_and_without_audit_ref() {
        let a = push_args("origin", "main", "B", "S", true);
        assert_eq!(
            a,
            [
                "push",
                "--atomic",
                "--porcelain",
                "--no-follow-tags",
                "--recurse-submodules=no",
                "--force-with-lease=refs/heads/main:B",
                "--force-with-lease=refs/sluice/landed/S:",
                "--",
                "origin",
                "S:refs/heads/main",
                "S:refs/sluice/landed/S",
            ]
        );
        let a = push_args("origin", "main", "B", "S", false);
        assert_eq!(a.len(), 9);
        assert!(!a.iter().any(|x| x.contains("landed")));
    }
}
