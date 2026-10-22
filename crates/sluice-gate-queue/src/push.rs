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
    /// denied): nothing landed, members `failed`.
    Rejected(String),
    /// The repository's `pre-push` hook declined (exit 1, no ref line, no
    /// `fatal:`): nothing was sent; members `failed` with `pre_push_hook`.
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
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("permission denied")
            || (lower.contains("permission to") && lower.contains("denied"))
            || lower.contains("returned error: 403")
        {
            return PushVerdict::Rejected(format!("permission denied: {}", tail(stderr, 3)));
        }
        let fatal = stderr.lines().any(|l| l.trim_start().starts_with("fatal:"));
        if code == 1 && !fatal && stderr.contains("failed to push some refs") {
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
    /// `push_timeout` expired; the group was terminated.
    TimedOut,
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
/// group, hooks **enabled** (the repository's `pre-push` runs).
///
/// # Errors
///
/// Spawn failure.
pub fn spawn(git: &Git, cwd: &Path, args: &[String]) -> Result<PushProcess> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd =
        tokio::process::Command::from(sluice_gate_core::git::command(&refs, cwd, git.env()));
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
                self.out.abort();
                self.err.abort();
                PushEnd::TimedOut
            }
        }
    }
}

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
    fn pre_push_hook_declined() {
        let err = "hook says no\nerror: failed to push some refs to '../r.git'\n";
        assert_eq!(
            classify(Some(1), "", err, T),
            PushVerdict::HookDeclined("hook says no".into())
        );
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
                "origin",
                "S:refs/heads/main",
                "S:refs/sluice/landed/S",
            ]
        );
        let a = push_args("origin", "main", "B", "S", false);
        assert_eq!(a.len(), 8);
        assert!(!a.iter().any(|x| x.contains("landed")));
    }
}
