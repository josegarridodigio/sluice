//! Path-scoped cache keys end to end (`paths` / `paths_ignore`): design
//! `kb/sluice/design/path-scoped-keys.md`, section 9, cases 8 to 23.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::Sandbox;
use predicates::prelude::*;
use sluice_gate_core::marker::Marker;

/// A gate counting its runs, scoped by `extra` (TOML lines).
fn counting_gate(name: &str, extra: &str) -> String {
    format!("\n[gates.{name}]\ncmd = \"echo run >> \\\"$SLUICE_TEST_COUNTER\\\"\"\n{extra}\n")
}

/// Writes `files` (creating directories) and commits them.
fn commit(sb: &Sandbox, files: &[(&str, &str)]) {
    for (path, content) in files {
        let p = sb.repo.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "files"]);
}

/// Runs `gate` and returns whether it was a cache hit (asserting success).
fn hit(sb: &Sandbox, gate: &str) -> bool {
    let before = sb.runs();
    let out = sb.sluice().args(["run", gate]).output().expect("run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{gate}: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cached = stdout.contains("PASS (cached");
    assert_eq!(cached, sb.runs() == before, "{stdout}");
    cached
}

fn write(sb: &Sandbox, path: &str, content: &str) {
    let p = sb.repo.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn src_sandbox(extra: &str) -> Sandbox {
    let sb = Sandbox::new(&counting_gate("s", extra));
    commit(
        &sb,
        &[
            ("src/lib.rs", "lib\n"),
            ("src/notes.md", "notes\n"),
            ("docs/a.md", "a\n"),
        ],
    );
    sb
}

/// Cases 8 and 9: out-of-scope edits hit, in-scope edits miss, reverting hits.
#[test]
fn doc_only_changes_hit_and_source_changes_miss() {
    let sb = src_sandbox("paths = [\"src/**\"]");
    assert!(!hit(&sb, "s"));
    write(&sb, "docs/a.md", "edited\n");
    write(&sb, "a.txt", "edited\n");
    assert!(hit(&sb, "s"), "doc-only change");
    write(&sb, "src/lib.rs", "changed\n");
    assert!(!hit(&sb, "s"), "source change");
    write(&sb, "src/lib.rs", "lib\n");
    assert!(hit(&sb, "s"), "reverted");
}

/// Case 10: untracked non-ignored files count like tracked ones; ignored
/// files never do.
#[test]
fn untracked_files_follow_the_scope() {
    let sb = src_sandbox("paths = [\"src\"]");
    assert!(!hit(&sb, "s"));
    write(&sb, "docs/new.md", "n\n");
    assert!(hit(&sb, "s"), "untracked outside the scope");
    write(&sb, "src/ignored.txt", "ignored by .gitignore\n");
    assert!(hit(&sb, "s"), "ignored inside the scope");
    write(&sb, "src/new.rs", "n\n");
    assert!(!hit(&sb, "s"), "untracked inside the scope");
}

/// Case 11: any change of `.sluice.toml` misses a scoped gate.
#[test]
fn config_changes_miss() {
    let sb = src_sandbox("paths = [\"src\"]");
    assert!(!hit(&sb, "s"));
    let base = fs::read_to_string(sb.repo.join(".sluice.toml")).unwrap();
    fs::write(sb.repo.join(".sluice.toml"), format!("{base}# a comment\n")).unwrap();
    assert!(!hit(&sb, "s"), "comment");
    fs::write(
        sb.repo.join(".sluice.toml"),
        format!("{base}[gates.other]\ncmd = \"true\"\n"),
    )
    .unwrap();
    assert!(!hit(&sb, "s"), "another gate");
    fs::write(
        sb.repo.join(".sluice.toml"),
        format!("{base}[gates.other]\ncmd = \"false\"\n"),
    )
    .unwrap();
    assert!(!hit(&sb, "s"), "another gate's cmd");
}

/// Case 12: `key_inputs` and `key_files` stay in the key.
#[test]
fn key_inputs_and_key_files_still_miss() {
    let sb = src_sandbox(
        "paths = [\"src\"]\nkey_inputs = [\"echo $KEYIN\"]\nkey_files = [\"ignored.txt\"]",
    );
    let run = |v: &str| {
        let before = sb.runs();
        sb.sluice()
            .env("KEYIN", v)
            .args(["run", "s"])
            .assert()
            .success();
        sb.runs() == before
    };
    assert!(!run("1"));
    assert!(run("1"));
    assert!(!run("2"), "key_inputs output changed");
    write(&sb, "ignored.txt", "secret\n");
    assert!(!run("2"), "key_files content changed");
    assert!(run("2"));
}

/// Case 13: an ignored in-scope file does not invalidate.
#[test]
fn paths_ignore_removes_files_from_the_scope() {
    let sb = src_sandbox("paths = [\"src\"]\npaths_ignore = [\"src/**/*.md\"]");
    assert!(!hit(&sb, "s"));
    write(&sb, "src/notes.md", "edited\n");
    assert!(hit(&sb, "s"));
    write(&sb, "src/lib.rs", "edited\n");
    assert!(!hit(&sb, "s"));
}

/// Case 14: renames across the boundary miss, renames outside hit.
#[test]
fn renames_across_the_boundary_miss() {
    let sb = Sandbox::new(&counting_gate("s", "paths = [\"src\"]"));
    commit(
        &sb,
        &[
            ("docs/a.rs", "a\n"),
            ("docs/c.rs", "c\n"),
            ("src/b.rs", "b\n"),
        ],
    );
    assert!(!hit(&sb, "s"));
    fs::rename(sb.repo.join("docs/c.rs"), sb.repo.join("docs/d.rs")).unwrap();
    assert!(hit(&sb, "s"), "rename within docs/");
    fs::rename(sb.repo.join("docs/a.rs"), sb.repo.join("src/a.rs")).unwrap();
    assert!(!hit(&sb, "s"), "docs/a.rs -> src/a.rs");
    fs::rename(sb.repo.join("src/b.rs"), sb.repo.join("docs/b.rs")).unwrap();
    assert!(!hit(&sb, "s"), "src/b.rs -> docs/b.rs");
}

/// Case 15: modes and symlink targets are part of the scope.
#[test]
fn chmod_and_symlink_retarget_miss() {
    let sb = src_sandbox("paths = [\"src\"]");
    std::os::unix::fs::symlink("../docs/a.md", sb.repo.join("src/link")).unwrap();
    assert!(!hit(&sb, "s"));
    assert!(hit(&sb, "s"));
    fs::set_permissions(
        sb.repo.join("src/lib.rs"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(!hit(&sb, "s"), "chmod +x");
    fs::remove_file(sb.repo.join("src/link")).unwrap();
    std::os::unix::fs::symlink("../a.txt", sb.repo.join("src/link")).unwrap();
    assert!(!hit(&sb, "s"), "symlink retarget");
    // The target's content is not followed (documented).
    write(&sb, "a.txt", "edited target\n");
    assert!(hit(&sb, "s"));
}

/// An unscoped gate, its tree and its key as computed by Sluice 0.2 (before
/// path scoping existed) for `Sandbox::new(OLD_GATE)`.
const OLD_GATE: &str = "\n[gates.old]\ncmd = \"echo run >> \\\"$SLUICE_TEST_COUNTER\\\"\"\n";
const OLD_TREE: &str = "0276d2ead96bbf36c5be97b7e490232c8d4636a2";
const OLD_KEY: &str = "37598a0d5013cfd46b28db0e5eec6bf54a2ad1f1ff14f004d60b8adb36bef9ea";

/// Case 16: a marker written by 0.2 for an unscoped gate still hits.
#[test]
fn markers_written_by_0_2_still_hit() {
    let sb = Sandbox::new(OLD_GATE);
    sb.sluice()
        .args(["run", "--explain", "old"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains(format!("tree:         {OLD_TREE}"))
                .and(predicate::str::contains(format!("key:          {OLD_KEY}")))
                .and(predicate::str::contains("scope:").not()),
        );
    fs::create_dir_all(sb.markers_dir()).unwrap();
    let marker = serde_json::json!({
        "format": 1,
        "key": OLD_KEY,
        "tree": OLD_TREE,
        "gate": "old",
        "cmd": "echo run >> \"$SLUICE_TEST_COUNTER\"",
        "passed_at": "2026-10-20T10:00:00Z",
        "duration_ms": 1234,
        "exit_code": 0,
        "job_id": "written-by-0.2",
        "sluice_version": "0.2.0",
        "committed_tree": null,
    });
    fs::write(
        sb.markers_dir().join(format!("{OLD_KEY}.json")),
        serde_json::to_vec_pretty(&marker).unwrap(),
    )
    .unwrap();
    assert!(hit(&sb, "old"));
    assert_eq!(sb.runs(), 0);
}

/// Case 17: a scoped pass writes an ordinary format-1 marker whose `tree`
/// is the root tree.
#[test]
fn a_scoped_marker_is_an_ordinary_marker_of_the_root_tree() {
    let sb = src_sandbox("paths = [\"src\"]");
    assert!(!hit(&sb, "s"));
    let head_tree = sb.git(&["rev-parse", "HEAD^{tree}"]);
    let file = fs::read_dir(sb.markers_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| !p.to_string_lossy().ends_with(".miss.json"))
        .expect("a marker");
    let text = fs::read_to_string(file).unwrap();
    let marker: Marker = serde_json::from_str(&text).expect("strict v0.2 schema");
    assert_eq!(marker.tree.as_str(), head_tree);
    assert_eq!(marker.format, 1);
    let fields: Vec<String> = serde_json::from_str::<serde_json::Value>(&text)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert!(!fields.iter().any(|f| f.contains("scope")), "{fields:?}");
}

/// Case 18 (CLI side): a scoped hit after an out-of-scope edit logs the
/// current root tree, not the marker's.
#[test]
fn a_scoped_hit_reports_the_current_tree() {
    let sb = src_sandbox("paths = [\"src\"]");
    assert!(!hit(&sb, "s"));
    write(&sb, "docs/a.md", "edited\n");
    assert!(hit(&sb, "s"));
    let explain = sb
        .sluice()
        .args(["run", "--explain", "s"])
        .output()
        .unwrap();
    let explain = String::from_utf8_lossy(&explain.stdout);
    let current = explain
        .lines()
        .find_map(|l| l.strip_prefix("tree:         "))
        .unwrap()
        .to_owned();
    assert_ne!(current, sb.git(&["rev-parse", "HEAD^{tree}"]));
    let log = fs::read_to_string(sb.home.join("state/sluice/logs/client.log")).unwrap();
    let last_hit = log.lines().rfind(|l| l.contains("decision=hit")).unwrap();
    assert!(
        last_hit.contains(&format!("tree={}", &current[..12])),
        "{last_hit} vs {current}"
    );
}

/// Case 19: a pre-commit pass with unstaged changes outside the scope lets
/// pre-push hit on its own; inside the scope it does not, and `cache
/// status` shows the evidence note.
#[test]
fn pre_commit_with_unstaged_changes_outside_the_scope() {
    let sb = src_sandbox("paths = [\"src\"]");
    write(&sb, "src/lib.rs", "staged\n");
    sb.git(&["add", "src/lib.rs"]);
    write(&sb, "docs/a.md", "unstaged\n");
    sb.sluice()
        .env("SLUICE_AGENT", "0")
        .args(["run", "--hook", "pre-commit", "s"])
        .assert()
        .success();
    sb.git(&["commit", "-q", "-m", "staged only"]);
    sb.git(&["checkout", "-q", "--", "docs/a.md"]);
    assert!(hit(&sb, "s"), "pre-push on the committed checkout");

    write(&sb, "src/lib.rs", "staged again\n");
    sb.git(&["add", "src/lib.rs"]);
    write(&sb, "src/extra.rs", "unstaged in scope\n");
    sb.sluice()
        .env("SLUICE_AGENT", "0")
        .args(["run", "--hook", "pre-commit", "s"])
        .assert()
        .success();
    sb.git(&["commit", "-q", "-m", "staged again"]);
    fs::remove_file(sb.repo.join("src/extra.rs")).unwrap();
    sb.sluice()
        .args(["cache", "status"])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(
                r"s\s+MISS \[scoped: \d+ of \d+ files, [0-9a-f]{12}\]; passed with unstaged changes present",
            )
            .unwrap(),
        );
    assert!(!hit(&sb, "s"));
}

/// Case 20: rewriting an out-of-scope tracked file still records a miss.
#[test]
fn a_scoped_gate_that_modifies_the_tree_is_not_cached() {
    let sb = Sandbox::new(
        "\n[gates.fix]\ncmd = \"echo run >> \\\"$SLUICE_TEST_COUNTER\\\"; echo x >> a.txt\"\n\
         paths = [\"src\"]\n",
    );
    sb.sluice()
        .args(["run", "fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "not cached: the gate modified the tree",
        ));
    assert_eq!(sb.marker_count(), 0);
    assert_eq!(sb.miss_count(), 1);
}

/// Case 21: a gitlink anywhere, even outside the scope, makes the tree
/// unknown.
#[test]
fn a_gitlink_outside_the_scope_runs_uncached() {
    let sb = src_sandbox("paths = [\"src\"]");
    let nested = sb.repo.join("vendor/nested");
    fs::create_dir_all(&nested).unwrap();
    sb.git_in(&nested, &["init", "-q", "-b", "main"]);
    fs::write(nested.join("n.txt"), "n\n").unwrap();
    sb.git_in(&nested, &["add", "-A"]);
    sb.git_in(
        &nested,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "n",
        ],
    );
    for _ in 0..2 {
        sb.sluice()
            .args(["run", "s"])
            .assert()
            .success()
            .stderr(predicate::str::contains("submodules are not supported"));
    }
    assert_eq!(sb.runs(), 2);
}

/// Case 22: `cache status` shows the scope.
#[test]
fn cache_status_shows_the_scope() {
    let sb = Sandbox::new(&format!(
        "{}{}",
        counting_gate("s", "paths = [\"src\"]"),
        counting_gate("u", "")
    ));
    commit(&sb, &[("src/a.rs", "a\n"), ("src/b.rs", "b\n")]);
    assert!(!hit(&sb, "s"));
    // .gitignore, a.txt, .sluice.toml, src/a.rs, src/b.rs
    sb.sluice()
        .args(["cache", "status"])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(r"\ns\s+HIT .*\[scoped: 2 of 5 files, [0-9a-f]{12}\]\n")
                .unwrap()
                .and(predicate::str::is_match(r"\nu\s+MISS\n").unwrap()),
        );
}

fn doctor_scoped(sb: &Sandbox) -> Vec<serde_json::Value> {
    let out = sb
        .sluice()
        .args(["doctor", "--json"])
        .output()
        .expect("doctor");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["name"] == "scoped-gates")
        .cloned()
        .collect()
}

/// Case 23: `doctor` warns for a manifest outside the scope and for an
/// empty scope, not when the manifest is a key file.
#[test]
fn doctor_warns_about_scopes_that_look_too_small() {
    let sb = Sandbox::new(&format!(
        "{}{}{}",
        counting_gate("test", "paths = [\"crates/**\"]"),
        counting_gate("empty", "paths = [\"nothing\"]"),
        counting_gate(
            "keyed",
            "paths = [\"crates\"]\nkey_files = [\"Cargo.lock\", \"Cargo.toml\"]"
        ),
    ));
    commit(
        &sb,
        &[
            ("Cargo.toml", "[workspace]\n"),
            ("Cargo.lock", "# lock\n"),
            ("crates/a/Cargo.toml", "[package]\n"),
            ("crates/a/src/lib.rs", "\n"),
            ("docs/Makefile", "all:\n"),
        ],
    );
    let checks = doctor_scoped(&sb);
    assert_eq!(checks.len(), 3, "{checks:?}");
    let detail = |i: usize| checks[i]["detail"].as_str().unwrap().to_owned();
    // Sorted by gate name: empty, keyed, test.
    assert_eq!(checks[0]["status"], "warn");
    assert!(
        detail(0).contains("gate 'empty': paths[0] matches no file"),
        "{}",
        detail(0)
    );
    assert!(
        detail(0).contains("its scope matches no file"),
        "{}",
        detail(0)
    );
    assert_eq!(checks[1]["status"], "ok", "{}", detail(1));
    assert_eq!(checks[2]["status"], "warn");
    assert!(
        detail(2).contains(
            "gate 'test' is scoped by paths but Cargo.lock is outside its scope; edits to it \
             will not invalidate the cache"
        ),
        "{}",
        detail(2)
    );
    assert!(detail(2).contains("Cargo.toml is outside"), "{}", detail(2));
    // docs/ holds no selected path: its Makefile is not reported.
    assert!(!detail(2).contains("docs/Makefile"), "{}", detail(2));
}
