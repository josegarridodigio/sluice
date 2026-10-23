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
