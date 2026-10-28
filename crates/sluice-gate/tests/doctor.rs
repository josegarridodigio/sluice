//! `sluice doctor`: the zero-config checks (`shims`, `plugin`, `weights`,
//! `init`) in text-independent JSON form.

mod common;

use std::fs;

use common::Sandbox;

fn checks(sb: &Sandbox) -> Vec<serde_json::Value> {
    let out = sb
        .sluice()
        .args(["doctor", "--json"])
        .output()
        .expect("doctor");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    report["checks"].as_array().expect("checks").clone()
}

fn find<'a>(checks: &'a [serde_json::Value], name: &str) -> Vec<&'a serde_json::Value> {
    checks.iter().filter(|c| c["name"] == name).collect()
}

#[test]
fn doctor_reports_shims_plugin_and_weights_in_a_configured_repository() {
    let sb = Sandbox::standard();
    let all = checks(&sb);
    for name in ["shims", "plugin", "weights"] {
        let found = find(&all, name);
        assert_eq!(found.len(), 1, "{name}: {all:?}");
        assert_ne!(found[0]["status"], "fail", "{name}: {all:?}");
    }
    // A config exists: no init hint. The sandbox has no agent plugin list.
    assert!(find(&all, "init").is_empty(), "{all:?}");
    assert_eq!(find(&all, "plugin")[0]["status"], "ok");
}

#[test]
fn doctor_warns_when_the_agent_plugin_version_differs() {
    let sb = Sandbox::standard();
    let plugins = sb.home.join(".claude/plugins");
    fs::create_dir_all(&plugins).unwrap();
    fs::write(
        plugins.join("installed_plugins.json"),
        r#"{"version":2,"plugins":{"sluice@sluice":[{"version":"0.0.1"}]}}"#,
    )
    .unwrap();
    let all = checks(&sb);
    let plugin = find(&all, "plugin")[0];
    assert_eq!(plugin["status"], "warn", "{plugin}");
    assert!(plugin["detail"].as_str().unwrap().contains("0.0.1"));
}

#[test]
fn doctor_suggests_init_in_a_repository_without_config() {
    let sb = Sandbox::standard();
    fs::remove_file(sb.repo.join(".sluice.toml")).unwrap();
    let all = checks(&sb);
    let init = find(&all, "init");
    assert_eq!(init.len(), 1, "{all:?}");
    assert_eq!(init[0]["status"], "ok");
    assert!(
        init[0]["detail"]
            .as_str()
            .unwrap()
            .contains("sluice init --dry-run")
    );
    assert!(find(&all, "weights").is_empty(), "{all:?}");
}

#[test]
fn doctor_warns_when_index_entries_disable_the_cache() {
    let sb = Sandbox::standard();
    let all = checks(&sb);
    assert_eq!(find(&all, "cache")[0]["status"], "ok", "{all:?}");

    sb.git(&["update-index", "--skip-worktree", "a.txt"]);
    let all = checks(&sb);
    let cache = find(&all, "cache");
    assert_eq!(cache.len(), 1, "{all:?}");
    assert_eq!(cache[0]["status"], "warn", "{cache:?}");
    let detail = cache[0]["detail"].as_str().unwrap();
    assert!(detail.contains("a.txt"), "{detail}");
    assert!(detail.contains("cache is disabled"), "{detail}");
    assert!(detail.contains("--no-skip-worktree"), "{detail}");
    assert!(detail.contains("--no-assume-unchanged"), "{detail}");

    sb.git(&["update-index", "--no-skip-worktree", "a.txt"]);
    sb.git(&["update-index", "--assume-unchanged", "a.txt"]);
    let all = checks(&sb);
    assert_eq!(find(&all, "cache")[0]["status"], "warn", "{all:?}");
    sb.git(&["update-index", "--no-assume-unchanged", "a.txt"]);
    let all = checks(&sb);
    assert_eq!(find(&all, "cache")[0]["status"], "ok", "{all:?}");
}

fn husky_v9(sb: &Sandbox, hook_body: &str) {
    let husky = sb.repo.join(".husky");
    fs::create_dir_all(husky.join("_")).unwrap();
    fs::write(
        husky.join("_/h"),
        "#!/usr/bin/env sh\nsh -e \"$(dirname \"$(dirname \"$0\")\")/$(basename \"$0\")\"\n",
    )
    .unwrap();
    fs::write(
        husky.join("_/pre-commit"),
        "#!/usr/bin/env sh\n. \"$(dirname \"$0\")/h\"\n",
    )
    .unwrap();
    fs::write(husky.join("pre-commit"), hook_body).unwrap();
    sb.git(&["config", "core.hooksPath", ".husky/_"]);
}

#[test]
fn doctor_reads_the_scripts_a_husky_v9_runner_executes() {
    let sb = Sandbox::standard();
    husky_v9(&sb, "sluice run --hook pre-commit lint\n");
    let all = checks(&sb);
    let hooks = find(&all, "hooks");
    assert_eq!(hooks[0]["status"], "ok", "{hooks:?}");

    let sb = Sandbox::standard();
    husky_v9(&sb, "npm run lint\n");
    let all = checks(&sb);
    let hooks = find(&all, "hooks");
    // A hook manager whose hooks do not call sluice: a failure (ADR-0021).
    assert_eq!(hooks[0]["status"], "fail", "{hooks:?}");
    assert!(
        hooks[0]["detail"]
            .as_str()
            .unwrap()
            .contains("sluice hooks on"),
        "{hooks:?}"
    );
}
