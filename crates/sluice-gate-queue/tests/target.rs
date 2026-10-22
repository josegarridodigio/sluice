//! Target resolution (design section 2) on a real repository.

mod common;

use common::{Fixture, git_in};
use sluice_gate_core::config::QueueConfig;
use sluice_gate_queue::git::Git;
use sluice_gate_queue::target::{self, Target};

fn setup() -> (Fixture, Git) {
    let fx = Fixture::new("version = 1\n");
    // origin/release (remote only), local `integration`, tag `v1`,
    // `feature` (the job's branch), `wip` checked out in a linked worktree.
    fx.git(&["push", "-q", "origin", "main:release"]);
    fx.git(&["fetch", "-q", "origin"]);
    fx.git(&["branch", "integration", "main"]);
    fx.git(&["branch", "feature", "main"]);
    fx.git(&["tag", "v1"]);
    let wip = fx.root.join("wip");
    fx.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "wip",
        wip.to_str().unwrap(),
        "main",
    ]);
    let git = Git::new(&fx.queue_env());
    (fx, git)
}

fn remote(r: &str, b: &str) -> Target {
    Target {
        remote: r.into(),
        branch: b.into(),
    }
}

fn local(b: &str) -> Target {
    Target {
        remote: String::new(),
        branch: b.into(),
    }
}

#[test]
fn resolves_remote_local_bare_and_default() {
    let (fx, git) = setup();
    let d = QueueConfig::default();
    let r = |base: Option<&str>| target::resolve(&git, &fx.common, "feature", base, &d);
    assert_eq!(r(Some("origin/main")).unwrap(), remote("origin", "main"));
    assert_eq!(
        r(Some("refs/remotes/origin/release")).unwrap(),
        remote("origin", "release")
    );
    // Bare name: a branch of [queue] remote first ...
    assert_eq!(r(Some("release")).unwrap(), remote("origin", "release"));
    assert_eq!(r(Some("main")).unwrap(), remote("origin", "main"));
    // ... then a local branch.
    assert_eq!(r(Some("integration")).unwrap(), local("integration"));
    assert_eq!(
        r(Some("refs/heads/integration")).unwrap(),
        local("integration")
    );
    // Default: [queue] remote and the remote's HEAD branch.
    assert_eq!(r(None).unwrap(), remote("origin", "main"));
    let explicit = QueueConfig {
        target: Some("release".into()),
        ..QueueConfig::default()
    };
    assert_eq!(
        target::resolve(&git, &fx.common, "feature", None, &explicit).unwrap(),
        remote("origin", "release")
    );
    assert_eq!(remote("origin", "main").display(), "origin/main");
    assert!(local("x").is_local());
}

#[test]
fn refusals() {
    let (fx, git) = setup();
    let d = QueueConfig::default();
    let code = |base: &str, job: &str| {
        target::resolve(&git, &fx.common, job, Some(base), &d)
            .unwrap_err()
            .code
    };
    let sha = git_in(&fx.repo, &fx.env, &["rev-parse", "main"]);
    assert_eq!(code("v1", "feature"), "tag");
    assert_eq!(code("refs/tags/v1", "feature"), "tag");
    assert_eq!(code(&sha, "feature"), "sha");
    assert_eq!(code(&sha[..10], "feature"), "sha");
    assert_eq!(code("HEAD", "feature"), "head");
    assert_eq!(code("origin/HEAD", "feature"), "head");
    assert_eq!(code("feature", "feature"), "own_branch");
    assert_eq!(code("origin/main", "main"), "own_branch");
    let e = target::resolve(&git, &fx.common, "feature", Some("wip"), &d).unwrap_err();
    assert_eq!(e.code, "target_checked_out");
    assert!(e.is_policy());
    assert_eq!(code("nope", "feature"), "unknown_base");
    assert_eq!(code("origin/nope", "feature"), "unknown_base");
}

#[test]
fn checked_out_main_is_refused_as_a_local_target() {
    let fx = Fixture::new("version = 1\n");
    let git = Git::new(&fx.queue_env());
    let e = target::resolve(
        &git,
        &fx.common,
        "feature",
        Some("refs/heads/main"),
        &QueueConfig::default(),
    )
    .unwrap_err();
    assert_eq!(e.code, "target_checked_out");
}

#[test]
fn endpoint_hash_tracks_the_url() {
    let fx = Fixture::new("version = 1\n");
    let git = Git::new(&fx.queue_env());
    let a = target::endpoint_hash(&git, &fx.common, "origin").unwrap();
    assert_eq!(
        a,
        target::endpoint_hash(&git, &fx.common, "origin").unwrap()
    );
    fx.git(&["remote", "set-url", "origin", "/elsewhere.git"]);
    assert_ne!(
        a,
        target::endpoint_hash(&git, &fx.common, "origin").unwrap()
    );
    let l = target::endpoint_hash(&git, &fx.common, "").unwrap();
    assert_eq!(l.len(), 64);
}
