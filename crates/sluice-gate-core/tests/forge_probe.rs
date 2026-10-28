//! The forge probe end to end with a fake `gh` and a fake `ssh` on a
//! private `PATH`: no network, no real login.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sluice_gate_core::forge::{Protection, probe_target};

type Env = BTreeMap<String, String>;

const FAKE_SSH: &str = "#!/bin/sh\nfor a; do last=$a; done\n\
    case \"$last\" in\n\
      gh-alias) echo 'hostname github.com';;\n\
      ghe-alias) echo 'hostname ghe.example.com';;\n\
      *) echo \"hostname $last\";;\n\
    esac\n";

/// Answers `gh api` by path from the `GH_*` variables, logs every call.
const FAKE_GH: &str = "#!/bin/sh\necho \"$*\" >> \"$GH_LOG\"\n\
    reply() { printf 'HTTP/2.0 %s\\r\\nContent-Type: application/json\\r\\n\\r\\n%s' \"$1\" \"$2\"; }\n\
    case \"$1 $2\" in\n\
      'auth status') exit \"${GH_AUTH_EXIT:-0}\";;\n\
    esac\n\
    case \"$*\" in\n\
      *branches/main/protection) reply \"$GH_PROT_STATUS\" \"$GH_PROT\";;\n\
      *rules/branches/main) reply '200 OK' \"$GH_RULES\";;\n\
      *branches/main) reply '200 OK' \"$GH_BRANCH\";;\n\
    esac\n";

struct Lab {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    env: Env,
    log: PathBuf,
}

fn script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn lab(url: &str, gh: bool) -> Lab {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (tools, repo, log) = (root.join("tools"), root.join("repo"), root.join("gh.log"));
    fs::create_dir_all(&tools).unwrap();
    fs::create_dir_all(&repo).unwrap();
    script(&tools.join("ssh"), FAKE_SSH);
    if gh {
        script(&tools.join("gh"), FAKE_GH);
    }
    for args in [vec!["init", "-q"], vec!["remote", "add", "origin", url]] {
        let ok = Command::new("git")
            .args(&args)
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(ok.success());
    }
    // Only git besides the fakes: the host's own `gh` must never answer.
    let git = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(|d| Path::new(d).join("git"))
        .find(|p| p.is_file())
        .expect("git on PATH");
    std::os::unix::fs::symlink(git, tools.join("git")).unwrap();
    let path = tools.display().to_string();
    let env: Env = [
        ("PATH", path),
        ("HOME", root.join("home").display().to_string()),
        ("XDG_CACHE_HOME", root.join("cache").display().to_string()),
        ("GH_LOG", log.display().to_string()),
        ("GH_RULES", "[]".into()),
        ("GH_BRANCH", r#"{"name":"main","protected":false}"#.into()),
        ("GH_PROT_STATUS", "404 Not Found".into()),
        ("GH_PROT", r#"{"message":"Branch not protected"}"#.into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    Lab {
        _dir: dir,
        repo,
        env,
        log,
    }
}

fn calls(l: &Lab) -> Vec<String> {
    fs::read_to_string(&l.log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_pull_request_rule_behind_an_ssh_alias_requires_a_pull_request() {
    let mut l = lab("git@gh-alias:acme/widgets.git", true);
    l.env.insert(
        "GH_RULES".into(),
        r#"[{"type":"pull_request","parameters":{}}]"#.into(),
    );
    let p = probe_target(&l.env, &l.repo, "origin", "main");
    assert!(matches!(p, Protection::PullRequestRequired(_)), "{p:?}");
    let log = calls(&l);
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("--hostname github.com"), "{log:?}");
    assert!(
        log[0].contains("repos/acme/widgets/rules/branches/main"),
        "{log:?}"
    );
}

#[test]
fn an_unprotected_branch_is_open_and_the_answer_is_cached() {
    let l = lab("https://github.com/acme/widgets.git", true);
    assert_eq!(
        probe_target(&l.env, &l.repo, "origin", "main"),
        Protection::Open
    );
    let after_first = calls(&l).len();
    assert_eq!(after_first, 2, "{:?}", calls(&l));
    assert_eq!(
        probe_target(&l.env, &l.repo, "origin", "main"),
        Protection::Open
    );
    assert_eq!(calls(&l).len(), after_first);
}

#[test]
fn a_protected_branch_with_unreadable_rules_is_protected_not_open() {
    let mut l = lab("https://github.com/acme/widgets.git", true);
    l.env.insert(
        "GH_BRANCH".into(),
        r#"{"name":"main","protected":true}"#.into(),
    );
    l.env
        .insert("GH_PROT_STATUS".into(), "404 Not Found".into());
    l.env
        .insert("GH_PROT".into(), r#"{"message":"Not Found"}"#.into());
    let p = probe_target(&l.env, &l.repo, "origin", "main");
    assert!(matches!(p, Protection::Protected(_)), "{p:?}");
}

#[test]
fn classic_protection_requiring_reviews_requires_a_pull_request() {
    let mut l = lab("https://github.com/acme/widgets.git", true);
    l.env.insert(
        "GH_BRANCH".into(),
        r#"{"name":"main","protected":true}"#.into(),
    );
    l.env.insert("GH_PROT_STATUS".into(), "200 OK".into());
    l.env.insert(
        "GH_PROT".into(),
        r#"{"required_pull_request_reviews":{"required_approving_review_count":1}}"#.into(),
    );
    let p = probe_target(&l.env, &l.repo, "origin", "main");
    assert!(matches!(p, Protection::PullRequestRequired(_)), "{p:?}");
}

#[test]
fn no_gh_means_unknown_never_an_error() {
    let l = lab("https://github.com/acme/widgets.git", false);
    let p = probe_target(&l.env, &l.repo, "origin", "main");
    assert!(
        matches!(&p, Protection::Unknown(r) if r.contains("gh is not installed")),
        "{p:?}"
    );
}

#[test]
fn an_enterprise_host_needs_a_gh_login_for_it() {
    let mut l = lab("git@ghe-alias:acme/widgets.git", true);
    l.env.insert("GH_AUTH_EXIT".into(), "1".into());
    let p = probe_target(&l.env, &l.repo, "origin", "main");
    assert!(
        matches!(&p, Protection::Unknown(r) if r.contains("ghe.example.com")),
        "{p:?}"
    );
    l.env.insert("GH_AUTH_EXIT".into(), "0".into());
    assert_eq!(
        probe_target(&l.env, &l.repo, "origin", "main"),
        Protection::Open
    );
    let last = calls(&l).pop().unwrap();
    assert!(last.contains("--hostname ghe.example.com"), "{last}");
}

#[test]
fn other_forges_and_local_remotes_are_unknown() {
    let l = lab("https://gitlab.com/acme/widgets.git", true);
    let p = probe_target(&l.env, &l.repo, "origin", "main");
    assert!(
        matches!(&p, Protection::Unknown(r) if r.contains("gitlab.com")),
        "{p:?}"
    );
    let l = lab("/srv/git/widgets.git", true);
    let p = probe_target(&l.env, &l.repo, "origin", "main");
    assert!(
        matches!(&p, Protection::Unknown(r) if r.contains("not a network remote")),
        "{p:?}"
    );
    assert_eq!(calls(&l), Vec::<String>::new());
}
