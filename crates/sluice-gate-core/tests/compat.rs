//! Compatibility golden files (ADR-0013): configs, markers and miss records
//! written by released binaries must be read by the current code.
//!
//! Fixtures live in `tests/fixtures/compat/<release>/` and are recorded from
//! the release binaries by `scripts/compat/generate.sh`. They are never edited
//! to make a test pass. Tests tagged `#[ignore = "gap Gn: ..."]` state a
//! promise the code does not keep yet; task H3b removes the attribute once
//! the code is fixed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use sluice_gate_core::config::{RepoConfig, ToolEnv, UserConfig};
use sluice_gate_core::key::KeyTree;
use sluice_gate_core::key::{self, CacheKey, KeyMaterial};
use sluice_gate_core::marker::{Lookup, MarkerStore};
use sluice_gate_core::tree::TreeHash;

const RELEASES: [&str; 6] = ["v0.1.0", "v0.2.0", "v0.3.0", "v0.4.0", "v0.4.1", "v0.5.0"];

/// Longer than any marker can be old.
const CENTURY: Duration = Duration::from_hours(100 * 365 * 24);

fn fixtures(release: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compat")
        .join(release)
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn key_of(s: &str) -> CacheKey {
    CacheKey::try_from(s.to_owned()).unwrap()
}

// ---------------------------------------------------------------------------
// Repository configs
// ---------------------------------------------------------------------------

fn hook_policy(p: sluice_gate_core::config::HookPolicy) -> Value {
    serde_json::to_value(p).unwrap()
}

/// The gates and defaults of a parsed repository config, in the shape of
/// `config/expected.json` (computed independently, from the TOML, by the
/// generator).
fn repo_summary(cfg: &RepoConfig, with_queue: bool) -> Value {
    let gates: BTreeMap<_, _> = cfg
        .gates
        .iter()
        .map(|(name, g)| {
            (
                name.clone(),
                json!({
                    "cmd": g.cmd,
                    "weight": g.weight,
                    "timeout_secs": g.timeout.as_secs_f64(),
                    "hooks": g.hooks,
                    "key_inputs": g.key_inputs,
                    "key_files": g.key_files.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
                    "env": g.env,
                    "depends_on": g.depends_on,
                    "cache": g.cache,
                }),
            )
        })
        .collect();
    let mut out = json!({
        "version": cfg.version,
        "defaults": {
            "agent_hook_policy": hook_policy(cfg.defaults.agent_hook_policy),
            "human_hook_policy": hook_policy(cfg.defaults.human_hook_policy),
            "tail_lines": cfg.defaults.tail_lines,
        },
        "gates": gates,
    });
    if with_queue {
        let q = &cfg.queue;
        out["queue"] = json!({
            "remote": q.remote,
            "target": q.target,
            "gates": q.gates,
            "max_train": q.max_train,
            "audit_ref": q.audit_ref,
            "push_timeout_secs": q.push_timeout.as_secs_f64(),
            "network_timeout_secs": q.network_timeout.as_secs_f64(),
        });
    }
    out
}

#[test]
fn configs_parse() {
    for release in RELEASES {
        let dir = fixtures(release).join("config");
        let expected = read_json(&dir.join("expected.json"));
        let expected = expected.as_object().unwrap();
        assert!(expected.len() >= 4, "{release}: fixtures went missing");
        for (file, want) in expected {
            let path = dir.join(file);
            let text = fs::read_to_string(&path).unwrap();
            let cfg = RepoConfig::parse(&text, &path)
                .unwrap_or_else(|e| panic!("{release}/{file} no longer parses: {e}"));
            let got = repo_summary(&cfg, want.get("queue").is_some());
            assert_eq!(&got, want, "{release}/{file}: effective gate table changed");
        }
    }
}

#[test]
fn v0_2_0_ships_a_queue_config() {
    let dir = fixtures("v0.2.0").join("config");
    let path = dir.join("queue.toml");
    let cfg = RepoConfig::parse(&fs::read_to_string(&path).unwrap(), &path).unwrap();
    assert_eq!(cfg.queue.remote, "origin");
    assert_eq!(cfg.queue.max_train, 4);
}

// ---------------------------------------------------------------------------
// User configs
// ---------------------------------------------------------------------------

fn secs(d: Duration) -> Value {
    json!(d.as_secs_f64())
}

/// Every key the generator recorded (explicitly set scalars of the file),
/// read back from the parsed config.
#[allow(clippy::too_many_lines, reason = "one arm per documented key")]
fn user_value(cfg: &UserConfig, key: &str) -> Value {
    if let Some(rest) = key.strip_prefix("repos|") {
        let (path, field) = rest.rsplit_once('|').unwrap();
        assert_eq!(field, "budget_share_max", "unknown repos key {key}");
        let ov = cfg
            .repos
            .get(Path::new(path))
            .unwrap_or_else(|| panic!("no [repos.{path}]"));
        return json!(ov.budget_share_max);
    }
    match key {
        "version" => json!(cfg.version),
        "daemon.budget" => json!(cfg.daemon.budget),
        "daemon.nice" => json!(cfg.daemon.nice),
        "daemon.ionice_class" => serde_json::to_value(cfg.daemon.ionice_class).unwrap(),
        "daemon.ionice_level" => json!(cfg.daemon.ionice_level),
        "daemon.log_retention_days" => json!(cfg.daemon.log_retention_days),
        "daemon.idle_exit_secs" => secs(cfg.daemon.idle_exit),
        "daemon.last_subscriber_gone" => {
            serde_json::to_value(cfg.daemon.last_subscriber_gone).unwrap()
        }
        "queue.enabled" => json!(cfg.queue.enabled),
        "queue.allow_agent_approval" => json!(cfg.queue.allow_agent_approval),
        "queue.tick_secs" => secs(cfg.queue.tick),
        "queue.retention_secs" => secs(cfg.queue.retention),
        "admission.psi_enabled" => json!(cfg.admission.psi_enabled),
        "admission.cpu_some_avg10_max" => json!(cfg.admission.cpu_some_avg10_max),
        "admission.memory_some_avg10_max" => json!(cfg.admission.memory_some_avg10_max),
        "admission.psi_poll_interval_secs" => secs(cfg.admission.psi_poll_interval),
        "admission.psi_max_wait_secs" => secs(cfg.admission.psi_max_wait),
        "mcp.progress_interval_secs" => secs(cfg.mcp.progress_interval),
        "debug.level" => json!(cfg.debug.level),
        "debug.daemon_log_max_mib" => json!(cfg.debug.daemon_log_max_mib),
        "cache.ttl_days" => json!(cfg.cache.ttl_days),
        "cache.tail_lines" => json!(cfg.cache.tail_lines),
        "scheduler.head_reserve_after_secs" => secs(cfg.scheduler.head_reserve_after),
        "agent_detection.env_any_set" => json!(cfg.agent_detection.env_any_set),
        "agent_detection.env_any_equals" => json!(cfg.agent_detection.env_any_equals),
        // Since v0.3.0 (the generator records these as written in the file).
        "admission.cpu_some_avg10_low" => json!(cfg.admission.cpu_some_avg10_low),
        "admission.memory_some_avg10_low" => json!(cfg.admission.memory_some_avg10_low),
        "admission.overcommit" => json!(cfg.admission.overcommit),
        "admission.overcommit_interval" => {
            json!(format!("{}s", cfg.admission.overcommit_interval.as_secs()))
        }
        "agent.exec_weight" => json!(cfg.agent.exec_weight),
        "agent.heavy_commands_extra" => json!(cfg.agent.heavy_commands_extra),
        "mcp.verify_parallel" => json!(cfg.mcp.verify_parallel),
        "scheduler.fairness" => serde_json::to_value(cfg.scheduler.fairness).unwrap(),
        "scheduler.human_first" => json!(cfg.scheduler.human_first),
        "scheduler.learned_weights" => json!(cfg.scheduler.learned_weights),
        "scheduler.order" => serde_json::to_value(cfg.scheduler.order).unwrap(),
        other => panic!("fixture key {other} has no reader in compat.rs: add it"),
    }
}

#[test]
fn user_configs_parse() {
    for release in RELEASES {
        let dir = fixtures(release).join("user-config");
        let expected = read_json(&dir.join("expected.json"));
        for (file, want) in expected.as_object().unwrap() {
            let path = dir.join(file);
            let cfg = UserConfig::parse(&fs::read_to_string(&path).unwrap(), &path)
                .unwrap_or_else(|e| panic!("{release}/{file} no longer parses: {e}"));
            for (key, value) in want.as_object().unwrap() {
                assert_eq!(&user_value(&cfg, key), value, "{release}/{file}: {key}");
            }
        }
    }
}

#[test]
fn an_empty_user_config_means_defaults() {
    for release in RELEASES {
        let path = fixtures(release).join("user-config/empty.toml");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.is_empty(), "the empty fixture must be empty");
        let cfg = UserConfig::parse(&text, &path).unwrap();
        assert_eq!(cfg.version, 1);
        assert!(cfg.daemon.budget >= 1);
    }
}

// ---------------------------------------------------------------------------
// Markers and miss records
// ---------------------------------------------------------------------------

/// A marker store whose directory holds a copy of the fixture files.
fn store_with(release: &str) -> (tempfile::TempDir, MarkerStore, PathBuf) {
    let common = tempfile::tempdir().unwrap();
    let store = MarkerStore::new(common.path());
    let dir = store.dir().to_path_buf();
    fs::create_dir_all(&dir).unwrap();
    let src = fixtures(release).join("markers");
    for entry in fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
    }
    (common, store, dir)
}

fn marker_files(release: &str) -> Vec<(CacheKey, Value)> {
    let src = fixtures(release).join("markers");
    let mut out = Vec::new();
    for entry in fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if let Some(stem) = name.strip_suffix(".json")
            && stem.strip_suffix(".miss").is_none()
        {
            out.push((key_of(stem), read_json(&path)));
        }
    }
    out.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    out
}

fn miss_files(release: &str) -> Vec<(CacheKey, Value)> {
    let src = fixtures(release).join("markers");
    let mut out = Vec::new();
    for entry in fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if let Some(stem) = name.strip_suffix(".miss.json") {
            out.push((key_of(stem), read_json(&path)));
        }
    }
    out.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    out
}

#[test]
fn markers_hit() {
    for release in RELEASES {
        let (_guard, store, _dir) = store_with(release);
        let markers = marker_files(release);
        assert!(
            markers.len() >= 4,
            "{release}: marker fixtures went missing"
        );
        for (key, body) in &markers {
            let Lookup::Hit(m) = store.lookup(key, CENTURY).unwrap() else {
                panic!("{release}: marker {key} is no longer a hit");
            };
            assert_eq!(m.key, *key);
            assert_eq!(m.format, 1);
            assert_eq!(m.gate, body["gate"]);
            assert_eq!(m.cmd, body["cmd"]);
            assert_eq!(m.exit_code, 0);
            assert_eq!(m.sluice_version, release.trim_start_matches('v'));
            assert_eq!(m.tree.as_str(), body["tree"].as_str().unwrap());
        }
    }
}

#[test]
fn markers_written_inside_pre_commit_keep_committed_tree() {
    for release in RELEASES {
        let (_guard, store, _dir) = store_with(release);
        let with_tree: Vec<_> = marker_files(release)
            .into_iter()
            .filter(|(_, b)| b["committed_tree"].is_string())
            .collect();
        assert_eq!(with_tree.len(), 1, "{release}: one hook marker expected");
        let (key, body) = &with_tree[0];
        let Lookup::Hit(m) = store.lookup(key, CENTURY).unwrap() else {
            panic!("{release}: hook marker is not a hit");
        };
        assert_eq!(
            m.committed_tree.as_ref().map(TreeHash::as_str),
            body["committed_tree"].as_str()
        );
    }
}

#[test]
fn old_keys_are_derived_identically() {
    for release in RELEASES {
        let inputs = read_json(&fixtures(release).join("inputs.json"));
        let entries = inputs.as_array().unwrap();
        assert!(entries.len() >= 7, "{release}: inputs.json went missing");
        let mut keyed_checked = false;
        for e in entries {
            let material = KeyMaterial {
                tree: KeyTree::Root(
                    TreeHash::try_from(e["tree"].as_str().unwrap().to_owned()).unwrap(),
                ),
                gate_name: e["gate"].as_str().unwrap().to_owned(),
                cmd: key::normalize_cmd(e["cmd"].as_str().unwrap()),
                env: e["env"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect(),
                key_inputs: pairs(&e["key_inputs"]),
                key_files: pairs(&e["key_files"]),
            };
            keyed_checked |= e["gate"] == "keyed";
            let derived = key::derive(&material);
            assert_eq!(
                derived.as_str(),
                e["key"].as_str().unwrap(),
                "{release}: key of {} changed; the cache layout is frozen",
                e["file"]
            );
            assert_eq!(
                e["file"].as_str().unwrap().split('.').next().unwrap(),
                derived.as_str()
            );
        }
        assert!(
            keyed_checked,
            "{release}: the gate with env, inputs and files is missing"
        );
    }
}

fn pairs(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p[0].as_str().unwrap().to_owned(),
                p[1].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn miss_records_read() {
    // gate -> (reason, exit code) as recorded by the release.
    let expected = [
        ("fail", "gate failed", Some(1)),
        ("timeout", "timed out", None),
        ("modify", "the gate modified the tree", Some(0)),
    ];
    for release in RELEASES {
        let (_guard, store, _dir) = store_with(release);
        let misses = miss_files(release);
        assert_eq!(misses.len(), expected.len(), "{release}: miss fixtures");
        for (key, body) in &misses {
            let gate = body["gate"].as_str().unwrap();
            let (_, reason, code) = expected.iter().find(|(g, ..)| *g == gate).unwrap();
            let rec = store
                .read_miss(key)
                .unwrap()
                .unwrap_or_else(|| panic!("{release}: {gate} miss unreadable"));
            assert_eq!(rec.reason, *reason);
            assert_eq!(rec.exit_code, *code);
            assert_eq!(rec.gate, gate);
            assert!(rec.log_path.is_some());
            // No marker exists for that key, so the lookup reports it.
            match store.lookup(key, CENTURY).unwrap() {
                Lookup::Miss(Some(m)) => assert_eq!(m.reason, *reason),
                other => panic!("{release}: {gate}: {other:?}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Damaged markers
// ---------------------------------------------------------------------------

fn bad_case(release: &str, case: &str) -> (tempfile::TempDir, MarkerStore, CacheKey, PathBuf) {
    let dir = fixtures(release).join("markers-bad");
    let cases = read_json(&dir.join("cases.json"));
    let c = &cases[case];
    let key = key_of(c["file_key"].as_str().unwrap());
    let common = tempfile::tempdir().unwrap();
    let store = MarkerStore::new(common.path());
    fs::create_dir_all(store.dir()).unwrap();
    let target = store.dir().join(format!("{key}.json"));
    fs::copy(dir.join(c["body"].as_str().unwrap()), &target).unwrap();
    (common, store, key, target)
}

#[test]
fn foreign_format_and_wrong_key_markers_are_misses() {
    for release in RELEASES {
        for case in ["format-2", "wrong-key"] {
            let (_g, store, key, path) = bad_case(release, case);
            assert!(
                matches!(store.lookup(&key, CENTURY).unwrap(), Lookup::Miss(_)),
                "{release}/{case}"
            );
            assert!(
                !path.exists(),
                "{release}/{case}: a damaged marker is deleted"
            );
        }
    }
}

#[test]
fn a_marker_with_an_unknown_field_is_still_a_hit() {
    for release in RELEASES {
        let (_g, store, key, path) = bad_case(release, "unknown-field");
        assert!(
            matches!(store.lookup(&key, CENTURY).unwrap(), Lookup::Hit(_)),
            "{release}: a marker with a field added by a newer release must stay usable"
        );
        assert!(path.exists(), "{release}: it must not be deleted");
    }
}

#[test]
fn prune_removes_orphaned_miss_records_and_foreign_formats() {
    for release in RELEASES {
        let (_g, store, dir) = store_with(release);
        // A marker of a future cache format, for example after a key bump.
        let (_g2, _s2, _k, bad) = bad_case(release, "format-2");
        fs::copy(&bad, dir.join(bad.file_name().unwrap())).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        store.prune(Duration::from_millis(10)).unwrap();
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(
            left.is_empty(),
            "{release}: prune must drop expired markers, miss records and foreign formats, left {left:?}"
        );
    }
}

#[test]
fn prune_keeps_fresh_markers() {
    for release in RELEASES {
        let (_g, store, dir) = store_with(release);
        let before = fs::read_dir(&dir).unwrap().count();
        store.prune(CENTURY).unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), before, "{release}");
    }
}

// ---------------------------------------------------------------------------
// Integrity of all compat fixtures
// ---------------------------------------------------------------------------

/// Every `SHA256SUMS` of every crate's `tests/fixtures/compat/<release>/`
/// still matches its files: a golden file is a record of a release, so an
/// edit (to make a test pass, say) is caught here.
#[test]
fn compat_fixtures_match_their_checksums() {
    use sha2::{Digest, Sha256};

    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut checked_files = 0;
    let mut dirs = 0;
    for krate in fs::read_dir(&crates).unwrap() {
        let compat = krate.unwrap().path().join("tests/fixtures/compat");
        let Ok(releases) = fs::read_dir(&compat) else {
            continue;
        };
        for release in releases {
            let dir = release.unwrap().path();
            let sums = fs::read_to_string(dir.join("SHA256SUMS"))
                .unwrap_or_else(|e| panic!("{}: SHA256SUMS: {e}", dir.display()));
            dirs += 1;
            for line in sums.lines() {
                let (want, name) = line.split_once("  ").unwrap();
                let got = hex::encode(Sha256::digest(fs::read(dir.join(name)).unwrap()));
                assert_eq!(got, want, "{}/{name} was modified", dir.display());
                checked_files += 1;
            }
        }
    }
    assert!(dirs >= 10, "expected 5 crates x 2 releases, found {dirs}");
    assert!(checked_files >= 90, "only {checked_files} files checked");
}

// ---------------------------------------------------------------------------
// Post-release fixtures (not recorded from a release binary)
// ---------------------------------------------------------------------------

/// ADR-0015: `weight = "auto"` parses next to integers and an omitted weight,
/// and serialises back to the same spellings.
#[test]
fn auto_weight_fixture_parses() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unreleased/auto-weight.toml");
    let text = fs::read_to_string(&path).unwrap();
    let cfg = RepoConfig::parse(&text, &path).unwrap();
    let weights: BTreeMap<_, _> = cfg
        .gates
        .iter()
        .map(|(name, g)| (name.clone(), serde_json::to_value(g.weight).unwrap()))
        .collect();
    assert_eq!(
        serde_json::to_value(weights).unwrap(),
        json!({"build": 8, "lint": 1, "test": "auto"})
    );
    assert_eq!(cfg.gates["test"].weight.ceiling(20), 10);
    assert_eq!(cfg.gates["build"].weight.ceiling(20), 8);
}

/// ADR-0018: the `[queue] prepare*` keys parse, and a config without them
/// keeps the defaults.
#[test]
fn queue_prepare_fixture_parses() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unreleased/queue-prepare.toml");
    let text = fs::read_to_string(&path).unwrap();
    let cfg = RepoConfig::parse(&text, &path).unwrap();
    assert_eq!(
        cfg.queue.prepare.as_deref(),
        Some("pnpm install --frozen-lockfile --prefer-offline")
    );
    assert_eq!(cfg.queue.prepare_timeout, Duration::from_mins(15));
    assert_eq!(
        serde_json::to_value(cfg.queue.prepare_weight).unwrap(),
        json!(2)
    );
    let plain = RepoConfig::parse("version = 1\n", &path).unwrap();
    assert!(plain.queue.prepare.is_none());
    assert!(plain.queue.prepare_weight.is_auto());
}

/// ADR-0020: `[queue] tool_env` and `tool_path` parse; a user config without
/// them resolves trains' tools in the login environment.
#[test]
fn queue_tool_env_fixture_parses() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unreleased/queue-tool-env.toml");
    let text = fs::read_to_string(&path).unwrap();
    let cfg = UserConfig::parse(&text, &path).unwrap();
    assert_eq!(cfg.queue.tool_env, ToolEnv::Daemon);
    assert_eq!(
        cfg.queue.tool_path.as_deref(),
        Some("/opt/tools/bin:/usr/bin")
    );
    let plain = UserConfig::parse("version = 1\n", &path).unwrap();
    assert_eq!(plain.queue.tool_env, ToolEnv::LoginShell);
    assert!(plain.queue.tool_path.is_none());
}

/// ADR-0019: `[containment] gates_oom_score_adj` parses as `"auto"` or a
/// number; a user config without it keeps `"auto"` (800).
#[test]
fn containment_gates_oom_score_adj_fixture_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/unreleased/containment-gates-oom-score-adj.toml");
    let text = fs::read_to_string(&path).unwrap();
    let cfg = UserConfig::parse(&text, &path).unwrap();
    assert_eq!(cfg.containment.gates_oom_score_adj.resolve(), 600);
    let plain = UserConfig::parse("version = 1\n", &path).unwrap();
    assert_eq!(plain.containment.gates_oom_score_adj.resolve(), 800);
}

/// ADR-0019 piece 5: the optional ceiling of `jobs/` parses as sizes and is
/// unset by default.
#[test]
fn containment_gates_memory_max_fixture_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/unreleased/containment-gates-memory-max.toml");
    let text = fs::read_to_string(&path).unwrap();
    let cfg = UserConfig::parse(&text, &path).unwrap();
    assert_eq!(cfg.containment.gates_memory_max_bytes(), Some(20 << 30));
    assert_eq!(cfg.containment.gates_memory_swap_max_bytes(), Some(2 << 30));
    let plain = UserConfig::parse("version = 1\n", &path).unwrap();
    assert_eq!(plain.containment.gates_memory_max_bytes(), None);
    assert_eq!(plain.containment.gates_memory_swap_max_bytes(), None);
}

/// ADR-0019: `[admission] memory_aware` and `memory_reserve` parse; a user
/// config without them keeps memory-aware admission on with an `"auto"`
/// reserve (a tenth of the machine's memory).
#[test]
fn admission_memory_fixture_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/unreleased/admission-memory.toml");
    let text = fs::read_to_string(&path).unwrap();
    let cfg = UserConfig::parse(&text, &path).unwrap();
    assert!(cfg.admission.memory_aware);
    assert_eq!(cfg.admission.memory_reserve_bytes(30 << 30), 3 << 30);
    let plain = UserConfig::parse("version = 1\n", &path).unwrap();
    assert!(plain.admission.memory_aware);
    assert_eq!(plain.admission.memory_reserve_bytes(30 << 30), 3 << 30);
    let off = UserConfig::parse(
        "version = 1\n[admission]\nmemory_reserve = \"auto\"\n",
        &path,
    )
    .unwrap();
    assert_eq!(off.admission.memory_reserve_bytes(20 << 30), 2 << 30);
}

/// ADR-0019 piece 7: `[admission] memory_guard` parses and is off without
/// the key.
#[test]
fn admission_memory_guard_fixture_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/unreleased/admission-memory-guard.toml");
    let text = fs::read_to_string(&path).unwrap();
    let cfg = UserConfig::parse(&text, &path).unwrap();
    assert!(cfg.admission.memory_guard);
    assert_eq!(cfg.admission.memory_reserve_bytes(30 << 30), 3 << 30);
    let plain = UserConfig::parse("version = 1\n", &path).unwrap();
    assert!(!plain.admission.memory_guard);
}

/// ADR-0023: `[scheduler] backfill` parses, and is on without the key.
#[test]
fn scheduler_backfill_fixture_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/unreleased/scheduler-backfill.toml");
    let text = fs::read_to_string(&path).unwrap();
    let cfg = UserConfig::parse(&text, &path).unwrap();
    assert!(!cfg.scheduler.backfill);
    let plain = UserConfig::parse("version = 1\n", &path).unwrap();
    assert!(plain.scheduler.backfill);
}

/// ADR-0024: a gate's `stall_timeout` parses (a duration, `"off"`, or the
/// default), has a wire form that round-trips, and rejects nonsense.
#[test]
fn gate_stall_timeout_fixture_parses() {
    use sluice_gate_core::config::StallTimeout;
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/unreleased/gate-stall-timeout.toml");
    let text = fs::read_to_string(&path).unwrap();
    let cfg = RepoConfig::parse(&text, &path).unwrap();
    assert_eq!(cfg.gates["lint"].stall_timeout, StallTimeout::Auto);
    assert_eq!(
        cfg.gates["test"].stall_timeout,
        StallTimeout::After(std::time::Duration::from_mins(10))
    );
    assert_eq!(cfg.gates["serve"].stall_timeout, StallTimeout::Off);
    for t in [
        StallTimeout::Auto,
        StallTimeout::Off,
        cfg.gates["test"].stall_timeout,
    ] {
        assert_eq!(StallTimeout::from_wire(t.to_wire()), t);
    }
    for bad in ["\"soon\"", "\"0s\"", "5"] {
        let toml = format!("version = 1\n[gates.g]\ncmd = \"x\"\nstall_timeout = {bad}\n");
        let err = RepoConfig::parse(&toml, &path).unwrap_err().to_string();
        assert!(
            err.contains("stall_timeout") || err.contains("invalid type"),
            "{err}"
        );
    }
}

/// ADR-0025: `[queue] land` parses in the repository config and in the user
/// config; without it the queue pushes. The target commit's value wins over
/// the user's, and nothing else (a queued branch's file) is an input.
#[test]
fn queue_land_fixtures_parse_and_follow_the_precedence() {
    use sluice_gate_core::config::Land;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unreleased");
    let repo_path = dir.join("queue-land.toml");
    let repo = RepoConfig::parse(&fs::read_to_string(&repo_path).unwrap(), &repo_path).unwrap();
    assert_eq!(repo.queue.land, Some(Land::PullRequest));
    let user_path = dir.join("queue-land-user.toml");
    let user = UserConfig::parse(&fs::read_to_string(&user_path).unwrap(), &user_path).unwrap();
    assert_eq!(user.queue.land, Some(Land::PullRequest));
    let plain = RepoConfig::parse("version = 1\n", &repo_path).unwrap();
    assert_eq!(plain.queue.land, None);
    assert_eq!(Land::effective(plain.queue.land, None), Land::Push);
    // The user's default applies when the repository does not say.
    assert_eq!(
        Land::effective(plain.queue.land, user.queue.land),
        Land::PullRequest
    );
    // The repository's own value wins in both directions.
    let push = RepoConfig::parse("version = 1\n[queue]\nland = \"push\"\n", &repo_path).unwrap();
    assert_eq!(
        Land::effective(push.queue.land, user.queue.land),
        Land::Push
    );
    assert_eq!(
        Land::effective(repo.queue.land, Some(Land::Push)),
        Land::PullRequest
    );
    for bad in ["\"merge\"", "true", "1"] {
        let toml = format!("version = 1\n[queue]\nland = {bad}\n");
        assert!(RepoConfig::parse(&toml, &repo_path).is_err(), "{bad}");
    }
}
