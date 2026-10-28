//! The client side of pull-request landing end to end with a fake `gh` on a
//! private `PATH` (ADR-0025): find-or-create, auto-merge only on request,
//! reading and closing, a forge that cannot be asked. No network, no login.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sluice_gate_core::forge::landing::{self, TrainRef};
use sluice_gate_core::forge::{AutoMerge, Member, PrFact, PrObservation, describe};

type Env = BTreeMap<String, String>;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// `gh pr list` answers `$GH_LIST` until `pr create` ran, then a pull request
/// 42 whose head is `$GH_HEAD`; every call is logged.
const FAKE_GH: &str = r#"#!/bin/sh
echo "$*" >> "$GH_LOG"
case "$1 $2" in
  'auth status') exit "${GH_AUTH_EXIT:-0}";;
  'pr list')
    if [ -n "$GH_LIST_FAIL" ]; then echo "$GH_LIST_FAIL" >&2; exit 1; fi
    if [ -f "$GH_DIR/created" ]; then
      printf '[{"number":42,"url":"https://github.com/example/repo/pull/42","state":"OPEN","headRefOid":"%s","mergedAt":null,"mergeCommit":null,"autoMergeRequest":null}]' "$GH_HEAD"
    else
      printf '%s' "${GH_LIST:-[]}"
    fi;;
  'pr create')
    if [ -n "$GH_CREATE_FAIL" ]; then echo "$GH_CREATE_FAIL" >&2; exit 1; fi
    : > "$GH_DIR/created"
    echo "https://github.com/example/repo/pull/42";;
  'pr merge')
    if [ -n "$GH_MERGE_FAIL" ]; then echo "$GH_MERGE_FAIL" >&2; exit 1; fi;;
  'pr close') ;;
esac
"#;

struct Lab {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    env: Env,
    log: PathBuf,
}

impl Lab {
    fn new(gh: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (tools, repo, log) = (root.join("tools"), root.join("repo"), root.join("gh.log"));
        fs::create_dir_all(&tools).unwrap();
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(root.join("gh")).unwrap();
        if gh {
            let path = tools.join("gh");
            fs::write(&path, FAKE_GH).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for args in [
            vec!["init", "-q"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/example/repo.git",
            ],
        ] {
            let ok = Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(ok.success());
        }
        // Only git besides the fake: the host's own `gh` must never answer.
        let git = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .map(|d| Path::new(d).join("git"))
            .find(|p| p.is_file())
            .expect("git on PATH");
        std::os::unix::fs::symlink(git, tools.join("git")).unwrap();
        let env: Env = [
            ("PATH", tools.display().to_string()),
            ("HOME", root.join("home").display().to_string()),
            ("XDG_CACHE_HOME", root.join("cache").display().to_string()),
            ("GH_LOG", log.display().to_string()),
            ("GH_DIR", root.join("gh").display().to_string()),
            ("GH_HEAD", SHA.to_owned()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        Self {
            _dir: dir,
            repo,
            env,
            log,
        }
    }

    fn with(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.to_owned(), value.to_owned());
        self
    }

    fn train(&self) -> TrainRef {
        TrainRef {
            train_id: "t-1".into(),
            repo: self.repo.clone(),
            remote: "origin".into(),
            merge_sha: SHA.into(),
        }
    }

    fn calls(&self, what: &str) -> usize {
        self.log().lines().filter(|l| l.starts_with(what)).count()
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn spec() -> sluice_gate_core::forge::PrSpec {
    let members = [Member {
        branch: "feat/a".into(),
        head: "abcdef0123".into(),
        job: "j-1".into(),
    }];
    describe("t-1", "main", SHA, &members, &["test".to_owned()])
}

fn open_head() -> PrObservation {
    PrObservation::Open { head: SHA.into() }
}

#[test]
fn the_pull_request_is_opened_once_whatever_the_number_of_runs() {
    let lab = Lab::new(true);
    let first = landing::open(&lab.env, &lab.train(), &spec(), false);
    assert!(first.created, "{first:?}");
    assert_eq!(
        (first.number, first.observation.clone()),
        (Some(42), open_head())
    );
    assert_eq!(
        first.url.as_deref(),
        Some("https://github.com/example/repo/pull/42")
    );
    let log = lab.log();
    for want in [
        "pr create -R example/repo --base main --head sluice/train/t-1 --title Land feat/a (sluice train t-1)",
        "--body Opened by `sluice queue approve` for train `t-1`",
    ] {
        assert!(log.contains(want), "{want}\n{log}");
    }
    // Again (approving the train a second time, `queue reconcile`): found.
    let second = landing::open(&lab.env, &lab.train(), &spec(), false);
    assert!(!second.created, "{second:?}");
    assert_eq!(second.number, Some(42));
    assert_eq!(lab.calls("pr create"), 1, "{}", lab.log());
}

#[test]
fn an_existing_pull_request_is_adopted_not_duplicated() {
    let existing = r#"[{"number":7,"url":"https://github.com/example/repo/pull/7","state":"OPEN","headRefOid":"abc","mergedAt":null,"mergeCommit":null,"autoMergeRequest":null}]"#;
    let lab = Lab::new(true).with("GH_LIST", existing);
    let fact = landing::open(&lab.env, &lab.train(), &spec(), false);
    assert!(!fact.created);
    assert_eq!(fact.number, Some(7));
    assert_eq!(lab.calls("pr create"), 0);
}

#[test]
fn a_crash_between_the_push_and_the_pull_request_is_finished_by_the_next_run() {
    let lab = Lab::new(true).with("GH_CREATE_FAIL", "GraphQL: boom");
    let failed = landing::open(&lab.env, &lab.train(), &spec(), false);
    match &failed.observation {
        PrObservation::CreateFailed { reason } => assert!(reason.contains("boom"), "{reason}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(failed.number, None);
    // The next run (reconcile, or approving again) has no failure.
    let lab = Lab {
        env: {
            let mut env = lab.env.clone();
            env.remove("GH_CREATE_FAIL");
            env
        },
        ..lab
    };
    let done = landing::open(&lab.env, &lab.train(), &spec(), false);
    assert!(done.created, "{done:?}");
    assert_eq!(done.number, Some(42));
}

#[test]
fn auto_merge_is_armed_only_when_asked_and_never_with_admin_rights() {
    let plain = Lab::new(true);
    let fact = landing::open(&plain.env, &plain.train(), &spec(), false);
    assert_eq!(fact.auto_merge, AutoMerge::NotRequested);
    assert_eq!(plain.calls("pr merge"), 0, "{}", plain.log());

    let lab = Lab::new(true);
    let fact = landing::open(&lab.env, &lab.train(), &spec(), true);
    assert_eq!(fact.auto_merge, AutoMerge::Armed, "{fact:?}");
    let log = lab.log();
    assert!(
        log.contains(&format!(
            "pr merge 42 -R example/repo --auto --merge --match-head-commit {SHA}"
        )),
        "{log}"
    );
    assert!(!log.contains("--admin"), "{log}");

    // The forge refusing it leaves the pull request open for a human.
    let lab = Lab::new(true).with("GH_MERGE_FAIL", "auto-merge is not allowed");
    let fact = landing::open(&lab.env, &lab.train(), &spec(), true);
    assert_eq!(fact.auto_merge, AutoMerge::Failed);
    assert_eq!(fact.number, Some(42));
    assert!(matches!(fact.observation, PrObservation::Open { .. }));
}

fn pr_json(state: &str, merge: &str) -> String {
    format!(
        r#"[{{"number":9,"url":"https://github.com/example/repo/pull/9","state":"{state}","headRefOid":"{SHA}","mergedAt":null,"mergeCommit":{merge},"autoMergeRequest":null}}]"#
    )
}

fn observed(lab: &Lab) -> PrObservation {
    landing::observe_now(&lab.env, &lab.train()).observation
}

#[test]
fn what_the_forge_says_becomes_a_fact() {
    let open = Lab::new(true).with("GH_LIST", &pr_json("OPEN", "null"));
    assert_eq!(observed(&open), open_head());
    let merged = Lab::new(true).with("GH_LIST", &pr_json("MERGED", r#"{"oid":"fedcba"}"#));
    assert_eq!(
        observed(&merged),
        PrObservation::Merged {
            merge_commit: Some("fedcba".into()),
            head: Some(SHA.into())
        }
    );
    let closed = Lab::new(true).with("GH_LIST", &pr_json("CLOSED", "null"));
    assert_eq!(
        observed(&closed),
        PrObservation::Closed {
            head: Some(SHA.into())
        }
    );
    assert_eq!(observed(&Lab::new(true)), PrObservation::Missing);
}

#[test]
fn a_forge_that_cannot_be_asked_is_unreachable_never_an_error() {
    let no_gh = observed(&Lab::new(false));
    match no_gh {
        PrObservation::Unreachable { reason } => {
            assert!(reason.contains("gh is not installed"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    let failing = Lab::new(true).with("GH_LIST_FAIL", "HTTP 502: bad gateway");
    match observed(&failing) {
        PrObservation::Unreachable { reason } => assert!(reason.contains("502"), "{reason}"),
        other => panic!("{other:?}"),
    }
    let logged_out = Lab::new(true).with(
        "GH_LIST_FAIL",
        "To get started with GitHub CLI, please run:  gh auth login",
    );
    match observed(&logged_out) {
        PrObservation::Unreachable { reason } => {
            assert!(reason.contains("gh auth login"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_local_remote_has_no_forge() {
    let lab = Lab::new(true);
    let ok = Command::new("git")
        .args(["remote", "set-url", "origin", "/srv/git/repo.git"])
        .current_dir(&lab.repo)
        .env("PATH", lab.env["PATH"].as_str())
        .status()
        .unwrap();
    assert!(ok.success());
    match observed(&lab) {
        PrObservation::Unreachable { reason } => {
            assert!(reason.contains("not a network remote"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(lab.calls("pr list"), 0);
}

#[test]
fn reads_are_cached_briefly_and_a_fresh_read_is_not() {
    let lab = Lab::new(true).with("GH_LIST", &pr_json("OPEN", "null"));
    let first = landing::observe(&lab.env, &lab.train());
    let second = landing::observe(&lab.env, &lab.train());
    assert_eq!(first, second);
    assert_eq!(lab.calls("pr list"), 1, "{}", lab.log());
    let _ = landing::observe_now(&lab.env, &lab.train());
    assert_eq!(lab.calls("pr list"), 2, "{}", lab.log());
}

#[test]
fn closing_names_the_pull_request_and_comments() {
    let lab = Lab::new(true);
    landing::close(&lab.env, &lab.train(), 42, "withdrawn").unwrap();
    assert!(
        lab.log()
            .contains("pr close 42 -R example/repo --comment withdrawn"),
        "{}",
        lab.log()
    );
    let _: PrFact = landing::observe(&lab.env, &lab.train());
}
