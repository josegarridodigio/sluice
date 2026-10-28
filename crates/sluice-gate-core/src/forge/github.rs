//! GitHub through the `gh` CLI: a read-only probe of a branch's protection.
//!
//! Calls (all `GET`, at most three): the rules that apply to the branch
//! (`rules/branches/{b}`), the branch itself (`branches/{b}`, `protected`)
//! and, only to refine a protection of unknown kind, the classic protection
//! (`branches/{b}/protection`, readable with admin rights: its 404 body
//! `Branch not protected` means none, a plain `Not Found` means no access).

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use super::exec::{self, Budget, RunError};
use super::pr::{self, PrObservation, PrSpec, PullRequest};
use super::{Forge, Protection};
use crate::git::EnvMap;

const CALL_CAP: Duration = Duration::from_secs(4);

/// Reading pull requests, and the calls that write one: a little more than
/// an API probe takes.
const PR_READ_CAP: Duration = Duration::from_secs(12);
const PR_WRITE_CAP: Duration = Duration::from_secs(25);

/// Fields of `gh pr list --json` the queue reads.
const PR_FIELDS: &str = "number,url,state,headRefOid,mergedAt,mergeCommit,autoMergeRequest";

/// Rule types of a ruleset that do not stop a fast-forward push of a
/// merge commit.
const HARMLESS_RULES: [&str; 3] = ["deletion", "non_fast_forward", "copilot_code_review"];

/// One HTTP answer of `gh api --include`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// Body (JSON text, may be empty).
    pub body: String,
}

impl Reply {
    fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }

    fn message(&self) -> String {
        self.json()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default()
    }
}

/// Splits `gh api --include` output into status and body.
#[must_use]
pub fn parse_reply(stdout: &str) -> Option<Reply> {
    let text = stdout.trim_start();
    let first = text.lines().next()?;
    let status = first
        .strip_prefix("HTTP/")?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let body = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .map_or("", |(_, b)| b);
    Some(Reply {
        status,
        body: body.trim().to_owned(),
    })
}

/// Percent-encodes a branch name for a URL path (`/` stays).
#[must_use]
pub fn encode_branch(branch: &str) -> String {
    let mut out = String::new();
    for b in branch.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Why a status other than 200 stops the probe.
fn http_reason(host: &str, r: &Reply) -> String {
    let message = r.message();
    let detail = if message.is_empty() {
        String::new()
    } else {
        format!(": {message}")
    };
    match r.status {
        401 => format!(
            "gh is not authenticated for {host} (HTTP 401); run `gh auth login --hostname {host}`"
        ),
        403 => format!("forbidden or rate limited (HTTP 403){detail}"),
        404 => "repository not found, or no access with this gh login (HTTP 404)".to_owned(),
        s => format!("HTTP {s}{detail}"),
    }
}

fn rule_types(body: &Value) -> Vec<String> {
    body.as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("type").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// The decision, over the answers of an API: `get(path)` returns the reply
/// or the reason it could not be had.
pub fn decide(
    host: &str,
    repo: &str,
    branch: &str,
    get: &mut dyn FnMut(&str) -> Result<Reply, String>,
) -> Protection {
    let b = encode_branch(branch);
    let base = format!("repos/{repo}");
    let rules = get(&format!("{base}/rules/branches/{b}"))
        .ok()
        .filter(|r| r.status == 200)
        .and_then(|r| r.json())
        .map(|v| rule_types(&v))
        .unwrap_or_default();
    if rules.iter().any(|t| t == "pull_request") {
        return Protection::PullRequestRequired(
            "a repository ruleset requires a pull request".into(),
        );
    }
    let info = match get(&format!("{base}/branches/{b}")) {
        Ok(r) => r,
        Err(e) => return Protection::Unknown(e),
    };
    if info.status == 404 && info.message() == "Branch not found" {
        return Protection::Unknown(format!("branch {branch} does not exist on {host}/{repo}"));
    }
    if info.status != 200 {
        return Protection::Unknown(http_reason(host, &info));
    }
    let protected = info
        .json()
        .and_then(|v| v.get("protected").and_then(Value::as_bool))
        .unwrap_or(false);
    let other: Vec<String> = rules
        .into_iter()
        .filter(|t| !HARMLESS_RULES.contains(&t.as_str()))
        .collect();
    if !protected && other.is_empty() {
        return Protection::Open;
    }
    refine(&base, &b, &other, get)
}

/// A protected branch whose rules do not name a pull request: ask for the
/// classic protection to tell pull-request reviews from the rest.
fn refine(
    base: &str,
    b: &str,
    other: &[String],
    get: &mut dyn FnMut(&str) -> Result<Reply, String>,
) -> Protection {
    let ruleset = (!other.is_empty()).then(|| format!("a ruleset applies ({})", other.join(", ")));
    let classic = get(&format!("{base}/branches/{b}/protection"))
        .ok()
        .filter(|r| r.status == 200)
        .and_then(|r| r.json());
    match classic {
        Some(v) if v.get("required_pull_request_reviews").is_some() => {
            Protection::PullRequestRequired(
                "branch protection requires pull request reviews".into(),
            )
        }
        Some(_) => Protection::Protected(
            ruleset
                .unwrap_or_else(|| "branch protection applies (no pull request required)".into()),
        ),
        None => Protection::Protected(ruleset.unwrap_or_else(|| {
            "the branch is protected (the rules are not readable with this login)".into()
        })),
    }
}

/// A repository on a GitHub host, reached through `gh`.
#[derive(Debug)]
pub struct GitHub<'a> {
    env: &'a EnvMap,
    cwd: &'a Path,
    host: String,
    repo: String,
}

impl<'a> GitHub<'a> {
    /// `repo` is `owner/repo`.
    #[must_use]
    pub fn new(env: &'a EnvMap, cwd: &'a Path, host: &str, repo: &str) -> Self {
        Self {
            env,
            cwd,
            host: host.to_owned(),
            repo: repo.to_owned(),
        }
    }

    fn api(&self, path: &str, budget: &Budget) -> Result<Reply, String> {
        let args = [
            "api",
            "--include",
            "--hostname",
            &self.host,
            "-H",
            "Accept: application/vnd.github+json",
            path,
        ];
        let out =
            exec::run("gh", &args, self.env, self.cwd, budget, CALL_CAP).map_err(|e| match e {
                RunError::Missing => "gh is not installed or not in PATH".to_owned(),
                RunError::Timeout => "gh did not answer in time".to_owned(),
                RunError::Failed(why) => format!("gh could not run: {why}"),
            })?;
        parse_reply(&out.stdout).ok_or_else(|| {
            let line = out.stderr.lines().find(|l| !l.trim().is_empty());
            match line {
                Some(l) if l.contains("gh auth login") => format!(
                    "gh is not logged in to {}; run `gh auth login --hostname {}`",
                    self.host, self.host
                ),
                Some(l) => format!("gh failed: {}", l.trim()),
                None => "gh printed no HTTP answer".to_owned(),
            }
        })
    }

    /// `-R` argument naming this repository on its host.
    fn repo_arg(&self) -> String {
        if self.host == "github.com" {
            self.repo.clone()
        } else {
            format!("{}/{}", self.host, self.repo)
        }
    }

    /// Runs `gh <args>`; the captured output of a run that exited 0, else
    /// the reason (gh's first line of complaint).
    fn gh(&self, args: &[&str], budget: &Budget, cap: Duration) -> Result<String, String> {
        let out = exec::run("gh", args, self.env, self.cwd, budget, cap).map_err(|e| match e {
            RunError::Missing => "gh is not installed or not in PATH".to_owned(),
            RunError::Timeout => "gh did not answer in time".to_owned(),
            RunError::Failed(why) => format!("gh could not run: {why}"),
        })?;
        if out.code == Some(0) {
            return Ok(out.stdout);
        }
        let line = out
            .stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("no message");
        if line.contains("gh auth login") || line.contains("not logged in") {
            return Err(format!(
                "gh is not logged in to {}; run `gh auth login --hostname {}`",
                self.host, self.host
            ));
        }
        Err(format!("gh failed: {line}"))
    }

    fn list_prs(&self, branch: &str, budget: &Budget) -> Result<Vec<PullRequest>, String> {
        let repo = self.repo_arg();
        let args = [
            "pr", "list", "-R", &repo, "--head", branch, "--state", "all", "--limit", "10",
            "--json", PR_FIELDS,
        ];
        pr::parse_list(&self.gh(&args, budget, PR_READ_CAP)?)
    }

    /// True when `gh` holds a login for this host (exit status of `gh auth
    /// status --hostname`).
    #[must_use]
    pub fn logged_in(env: &EnvMap, cwd: &Path, host: &str, budget: &Budget) -> bool {
        let args = ["auth", "status", "--hostname", host];
        exec::run("gh", &args, env, cwd, budget, CALL_CAP).is_ok_and(|c| c.code == Some(0))
    }
}

impl Forge for GitHub<'_> {
    fn name(&self) -> &'static str {
        "GitHub"
    }

    fn probe_protection(&self, target: &str, budget: &Budget) -> Protection {
        decide(&self.host, &self.repo, target, &mut |p| self.api(p, budget))
    }

    fn pr_state(&self, branch: &str, budget: &Budget) -> Result<Option<PullRequest>, String> {
        Ok(pr::pick(self.list_prs(branch, budget)?))
    }

    fn ensure_pr(&self, spec: &PrSpec, budget: &Budget) -> Result<(PullRequest, bool), String> {
        if let Some(found) = self.pr_state(&spec.head, budget)? {
            return Ok((found, false));
        }
        let repo = self.repo_arg();
        let args = [
            "pr",
            "create",
            "-R",
            &repo,
            "--base",
            &spec.base,
            "--head",
            &spec.head,
            "--title",
            &spec.title,
            "--body",
            &spec.body,
        ];
        let out = self.gh(&args, budget, PR_WRITE_CAP)?;
        let (number, url) = pr::number_from_url(&out).ok_or_else(|| {
            format!(
                "gh pr create printed no pull request address: {}",
                out.trim()
            )
        })?;
        // The head of a new pull request is the branch's tip; the caller
        // compares it with the commit it pushed.
        let state = self.pr_state(&spec.head, budget).ok().flatten().map_or(
            PrObservation::Open {
                head: String::new(),
            },
            |p| p.state,
        );
        Ok((
            PullRequest {
                number,
                url,
                auto_merge: false,
                state,
            },
            true,
        ))
    }

    fn arm_auto_merge(&self, number: u64, head: &str, budget: &Budget) -> Result<(), String> {
        let repo = self.repo_arg();
        let n = number.to_string();
        let args = [
            "pr",
            "merge",
            &n,
            "-R",
            &repo,
            "--auto",
            "--merge",
            "--match-head-commit",
            head,
        ];
        self.gh(&args, budget, PR_WRITE_CAP).map(|_| ())
    }

    fn close_pr(&self, number: u64, comment: &str, budget: &Budget) -> Result<(), String> {
        let repo = self.repo_arg();
        let n = number.to_string();
        let args = ["pr", "close", &n, "-R", &repo, "--comment", comment];
        self.gh(&args, budget, PR_WRITE_CAP).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.to_owned(),
        }
    }

    /// Runs `decide` against canned answers by path suffix.
    fn run(answers: &[(&str, Result<Reply, String>)]) -> Protection {
        decide("github.com", "acme/widgets", "main", &mut |path| {
            answers
                .iter()
                .find(|(suffix, _)| path.ends_with(suffix))
                .map_or_else(
                    || Err(format!("unexpected call {path}")),
                    |(_, r)| r.clone(),
                )
        })
    }

    #[test]
    fn replies_are_split_from_the_headers() {
        let text = "HTTP/2.0 404 Not Found\r\nContent-Type: application/json\r\n\r\n{\"message\":\"Branch not found\"}";
        let r = parse_reply(text).unwrap();
        assert_eq!((r.status, r.message().as_str()), (404, "Branch not found"));
        assert_eq!(parse_reply("gh: Bad credentials (HTTP 401)"), None);
    }

    #[test]
    fn branch_names_are_encoded_but_keep_their_slashes() {
        assert_eq!(encode_branch("release/1.0"), "release/1.0");
        assert_eq!(encode_branch("a b#c"), "a%20b%23c");
    }

    #[test]
    fn a_pull_request_rule_decides_at_once() {
        let rules = r#"[{"type":"deletion"},{"type":"pull_request"}]"#;
        let p = run(&[("rules/branches/main", Ok(reply(200, rules)))]);
        assert!(matches!(p, Protection::PullRequestRequired(_)), "{p:?}");
    }

    #[test]
    fn nothing_protected_is_open_and_harmless_rules_do_not_count() {
        let branch = ("branches/main", Ok(reply(200, r#"{"protected":false}"#)));
        let rules = r#"[{"type":"copilot_code_review"},{"type":"non_fast_forward"}]"#;
        let p = run(&[("rules/branches/main", Ok(reply(200, rules))), branch]);
        assert_eq!(p, Protection::Open);
    }

    #[test]
    fn classic_protection_is_told_apart_by_what_it_says() {
        let branch = ("branches/main", Ok(reply(200, r#"{"protected":true}"#)));
        let rules = ("rules/branches/main", Ok(reply(200, "[]")));
        let reviews = (
            "protection",
            Ok(reply(200, r#"{"required_pull_request_reviews":{}}"#)),
        );
        let p = run(&[rules.clone(), branch.clone(), reviews]);
        assert!(matches!(p, Protection::PullRequestRequired(_)), "{p:?}");
        let checks = (
            "protection",
            Ok(reply(200, r#"{"required_status_checks":{}}"#)),
        );
        let p = run(&[rules.clone(), branch.clone(), checks]);
        assert!(matches!(p, Protection::Protected(_)), "{p:?}");
        let no_access = ("protection", Ok(reply(404, r#"{"message":"Not Found"}"#)));
        let p = run(&[rules, branch, no_access]);
        assert!(
            matches!(&p, Protection::Protected(e) if e.contains("not readable")),
            "{p:?}"
        );
    }

    #[test]
    fn other_rules_make_the_branch_protected() {
        let branch = ("branches/main", Ok(reply(200, r#"{"protected":false}"#)));
        let rules = (
            "rules/branches/main",
            Ok(reply(200, r#"[{"type":"required_status_checks"}]"#)),
        );
        let p = run(&[rules, branch, ("protection", Err("no".into()))]);
        assert!(
            matches!(&p, Protection::Protected(e) if e.contains("required_status_checks")),
            "{p:?}"
        );
    }

    #[test]
    fn errors_are_unknown_with_a_reason() {
        let rules = ("rules/branches/main", Ok(reply(404, "{}")));
        for (status, body, word) in [
            (401, r#"{"message":"Bad credentials"}"#, "gh auth login"),
            (403, r#"{"message":"rate limit exceeded"}"#, "rate limit"),
            (404, r#"{"message":"Not Found"}"#, "no access"),
            (404, r#"{"message":"Branch not found"}"#, "does not exist"),
            (500, "{}", "HTTP 500"),
        ] {
            let p = run(&[rules.clone(), ("branches/main", Ok(reply(status, body)))]);
            assert!(
                matches!(&p, Protection::Unknown(r) if r.contains(word)),
                "{status}: {p:?}"
            );
        }
        let p = run(&[
            rules,
            ("branches/main", Err("gh did not answer in time".into())),
        ]);
        assert!(
            matches!(&p, Protection::Unknown(r) if r.contains("in time")),
            "{p:?}"
        );
    }
}
