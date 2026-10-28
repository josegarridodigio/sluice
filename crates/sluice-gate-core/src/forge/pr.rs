//! Pull requests as the merge queue's landing vehicle (ADR-0025): what a
//! forge reports about the pull request of a train, the facts a client sends
//! the daemon, and the text of the request Sluice opens.
//!
//! Nothing here merges. The types are plain data (they travel in the daemon
//! protocol); the calls that fill them are in [`super::github`].

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// What the forge says about the pull request of a train branch, or why it
/// could not say. Sent to the daemon as a fact; the daemon decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PrObservation {
    /// Open. `head` is the head commit of the pull request.
    Open {
        /// Head commit of the pull request.
        head: String,
    },
    /// Merged by the forge (or a human). `merge_commit` is what the forge
    /// calls the merge commit: the merge commit for a merge, the squashed
    /// commit for a squash, the commit the target was updated to for a rebase.
    Merged {
        /// The merge commit, when the forge names one.
        #[serde(default)]
        merge_commit: Option<String>,
        /// Head commit of the pull request when it merged.
        #[serde(default)]
        head: Option<String>,
    },
    /// Closed without merging.
    Closed {
        /// Head commit of the pull request.
        #[serde(default)]
        head: Option<String>,
    },
    /// No pull request exists for the train branch.
    Missing,
    /// The forge could not be asked (no CLI, no login, offline, rate limit).
    Unreachable {
        /// Why.
        reason: String,
    },
    /// The client tried to open the pull request and failed.
    CreateFailed {
        /// Why.
        reason: String,
    },
}

/// A pull request as the forge lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// Number on the forge.
    pub number: u64,
    /// Web address.
    pub url: String,
    /// The forge's auto-merge is armed on it.
    #[serde(default)]
    pub auto_merge: bool,
    /// Its state (never `Missing`, `Unreachable` or `CreateFailed`).
    pub state: PrObservation,
}

/// What the approving client did about auto-merge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoMerge {
    /// The approver did not ask for it.
    #[default]
    NotRequested,
    /// Armed (`gh pr merge --auto --match-head-commit`).
    Armed,
    /// Asked for and refused by the forge (the pull request stays open).
    Failed,
}

impl AutoMerge {
    /// True for [`AutoMerge::NotRequested`] (serde skip).
    #[must_use]
    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde skip_serializing_if signature"
    )]
    pub fn is_default(&self) -> bool {
        *self == Self::NotRequested
    }
}

/// One client report about the pull request of a train: sent with
/// `queue-pr-facts`. A read of a fact (and, from `approve`, the number of the
/// pull request just opened), never an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrFact {
    /// Train the report is about.
    pub train_id: String,
    /// Pull request number, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    /// Pull request address, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// This client opened the pull request (it did not exist before).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub created: bool,
    /// What was done about auto-merge.
    #[serde(default, skip_serializing_if = "AutoMerge::is_default")]
    pub auto_merge: AutoMerge,
    /// What the forge says.
    pub observation: PrObservation,
}

impl PrFact {
    /// A fact that carries no pull request, only an observation.
    #[must_use]
    pub fn of(train_id: &str, observation: PrObservation) -> Self {
        Self {
            train_id: train_id.to_owned(),
            number: None,
            url: None,
            created: false,
            auto_merge: AutoMerge::NotRequested,
            observation,
        }
    }

    /// A fact about a pull request the forge listed.
    #[must_use]
    pub fn from_pr(train_id: &str, pr: &PullRequest) -> Self {
        Self {
            number: Some(pr.number),
            url: Some(pr.url.clone()),
            ..Self::of(train_id, pr.state.clone())
        }
    }
}

/// One member of a train, for the text of the pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Queued branch.
    pub branch: String,
    /// Its head when it was queued.
    pub head: String,
    /// Sluice job id.
    pub job: String,
}

/// What `gh pr create` needs, and what the pull request says about the train.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSpec {
    /// Branch the pull request is opened from (`sluice/train/<id>`).
    pub head: String,
    /// Branch it targets.
    pub base: String,
    /// Title.
    pub title: String,
    /// Body.
    pub body: String,
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The pull request of a train: head `sluice/train/<id>`, base `target`, a
/// title naming the members and a body listing them, the gates the train
/// passed, the landing commit and the train id. No personal data: only what
/// the train itself holds.
#[must_use]
pub fn describe(
    train_id: &str,
    target: &str,
    landing: &str,
    members: &[Member],
    gates: &[String],
) -> PrSpec {
    let title = match members {
        [one] => format!("Land {} (sluice train {train_id})", one.branch),
        many => format!("Land {} branches (sluice train {train_id})", many.len()),
    };
    let mut body = String::new();
    let _ = writeln!(
        body,
        "Opened by `sluice queue approve` for train `{train_id}`. The head of this pull request \
         is the commit Sluice verified (`{}`); merge it as it is.\n",
        short(landing)
    );
    body.push_str("Members:\n");
    for m in members {
        let _ = writeln!(
            body,
            "- `{}` at `{}` (job `{}`)",
            m.branch,
            short(&m.head),
            m.job
        );
    }
    if gates.is_empty() {
        body.push_str("\nGates: none recorded.\n");
    } else {
        let _ = writeln!(body, "\nGates passed on this commit: {}.", gates.join(", "));
    }
    let _ = writeln!(
        body,
        "\nIf `{target}` moved since the train was built, the forge's own rules decide; Sluice \
         does not rebuild or merge."
    );
    PrSpec {
        head: format!("sluice/train/{train_id}"),
        base: target.to_owned(),
        title,
        body,
    }
}

/// The pull request number at the end of a `.../pull/<n>` address.
#[must_use]
pub fn number_from_url(text: &str) -> Option<(u64, String)> {
    let url = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.contains("/pull/"))?;
    let tail = url.rsplit_once("/pull/")?.1;
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    Some((digits.parse().ok()?, url.to_owned()))
}

/// Parses `gh pr list --json number,url,state,headRefOid,mergedAt,mergeCommit,
/// autoMergeRequest`.
///
/// # Errors
///
/// The text is not that JSON.
pub fn parse_list(stdout: &str) -> Result<Vec<PullRequest>, String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("gh printed something that is not JSON: {e}"))?;
    let list = value
        .as_array()
        .ok_or_else(|| "gh did not print a JSON list".to_owned())?;
    Ok(list.iter().filter_map(parse_one).collect())
}

fn parse_one(v: &serde_json::Value) -> Option<PullRequest> {
    let text = |key: &str| v.get(key).and_then(serde_json::Value::as_str);
    let head = text("headRefOid").map(str::to_owned);
    let state = match text("state")? {
        "OPEN" => PrObservation::Open {
            head: head.unwrap_or_default(),
        },
        "MERGED" => PrObservation::Merged {
            merge_commit: v
                .get("mergeCommit")
                .and_then(|m| m.get("oid"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            head,
        },
        "CLOSED" => PrObservation::Closed { head },
        _ => return None,
    };
    Some(PullRequest {
        number: v.get("number")?.as_u64()?,
        url: text("url")?.to_owned(),
        auto_merge: v.get("autoMergeRequest").is_some_and(|a| !a.is_null()),
        state,
    })
}

/// Among the pull requests of one head branch, the one that counts: an open
/// one, else a merged one, else the most recent.
#[must_use]
pub fn pick(mut list: Vec<PullRequest>) -> Option<PullRequest> {
    let rank = |p: &PullRequest| match p.state {
        PrObservation::Open { .. } => 2,
        PrObservation::Merged { .. } => 1,
        _ => 0,
    };
    list.sort_by_key(|p| (rank(p), p.number));
    list.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_names_members_gates_and_the_train() {
        let members = [
            Member {
                branch: "feat/a".into(),
                head: "1234567890".into(),
                job: "j-1".into(),
            },
            Member {
                branch: "feat/b".into(),
                head: "abcdef0123".into(),
                job: "j-2".into(),
            },
        ];
        let spec = describe(
            "t-1-abc",
            "main",
            "fedcba9876",
            &members,
            &["test".to_owned(), "lint".to_owned()],
        );
        assert_eq!(spec.head, "sluice/train/t-1-abc");
        assert_eq!(spec.base, "main");
        assert_eq!(spec.title, "Land 2 branches (sluice train t-1-abc)");
        for want in [
            "`feat/a` at `1234567` (job `j-1`)",
            "`feat/b` at `abcdef0` (job `j-2`)",
            "Gates passed on this commit: test, lint.",
            "`fedcba9`",
            "train `t-1-abc`",
        ] {
            assert!(spec.body.contains(want), "{want}\n{}", spec.body);
        }
        let one = describe("t-2", "main", "ffff", &members[..1], &[]);
        assert_eq!(one.title, "Land feat/a (sluice train t-2)");
        assert!(one.body.contains("Gates: none recorded."));
    }

    #[test]
    fn the_number_is_read_from_the_address() {
        assert_eq!(
            number_from_url("\nhttps://github.com/example/repo/pull/42\n"),
            Some((42, "https://github.com/example/repo/pull/42".to_owned()))
        );
        assert_eq!(number_from_url("no address"), None);
    }

    #[test]
    fn the_list_is_parsed_and_the_right_one_picked() {
        let json = r#"[
          {"number":3,"url":"https://h/o/r/pull/3","state":"CLOSED","headRefOid":"aaa",
           "mergedAt":null,"mergeCommit":null,"autoMergeRequest":null},
          {"number":4,"url":"https://h/o/r/pull/4","state":"MERGED","headRefOid":"bbb",
           "mergedAt":"2026-10-28T09:58:30Z","mergeCommit":{"oid":"ccc"},"autoMergeRequest":null},
          {"number":5,"url":"https://h/o/r/pull/5","state":"OPEN","headRefOid":"ddd",
           "mergedAt":null,"mergeCommit":null,"autoMergeRequest":{"enabledAt":"x"}}
        ]"#;
        let list = parse_list(json).unwrap();
        assert_eq!(list.len(), 3);
        let best = pick(list.clone()).unwrap();
        assert_eq!(best.number, 5);
        assert!(best.auto_merge);
        assert_eq!(best.state, PrObservation::Open { head: "ddd".into() });
        let merged: Vec<_> = list.into_iter().filter(|p| p.number != 5).collect();
        let best = pick(merged).unwrap();
        assert_eq!(
            best.state,
            PrObservation::Merged {
                merge_commit: Some("ccc".into()),
                head: Some("bbb".into())
            }
        );
        assert_eq!(parse_list("[]").unwrap(), Vec::new());
        assert!(parse_list("nope").is_err());
    }

    #[test]
    fn facts_round_trip_and_stay_small() {
        let fact = PrFact::of("t-1", PrObservation::Missing);
        let text = serde_json::to_string(&fact).unwrap();
        assert_eq!(
            text,
            r#"{"train_id":"t-1","observation":{"state":"missing"}}"#
        );
        let back: PrFact = serde_json::from_str(&text).unwrap();
        assert_eq!(back, fact);
        let open = PrFact {
            created: true,
            auto_merge: AutoMerge::Armed,
            number: Some(7),
            ..PrFact::of("t-1", PrObservation::Open { head: "s".into() })
        };
        let back: PrFact = serde_json::from_str(&serde_json::to_string(&open).unwrap()).unwrap();
        assert_eq!(back, open);
    }
}
