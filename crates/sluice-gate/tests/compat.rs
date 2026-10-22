//! Compatibility golden files (ADR-0013) for the command line, the exit codes,
//! the MCP tool list and the plugin surface.
//!
//! Fixtures: `tests/fixtures/compat/<release>/cli-help.txt` and
//! `mcp-tools.json`, recorded from the release binaries by
//! `scripts/compat/generate.sh` and never edited to make a test pass. Tests
//! tagged `#[ignore = "gap Gn: ..."]` state a promise the code does not keep
//! yet; task H3b removes the attribute once the code is fixed.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::Sandbox;
use serde_json::{Value, json};

const RELEASES: [&str; 2] = ["v0.1.0", "v0.2.0"];

fn fixtures(release: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compat")
        .join(release)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// ---------------------------------------------------------------------------
// CLI surface (G12)
// ---------------------------------------------------------------------------

/// Commands, options, positionals and possible values of one `--help` text,
/// as `kind:name` strings. Descriptions and wording are not part of it.
fn surface(help: &str) -> BTreeSet<String> {
    #[derive(PartialEq)]
    enum In {
        Nothing,
        Commands,
        Options,
        Arguments,
    }
    let mut out = BTreeSet::new();
    let mut mode = In::Nothing;
    let mut last_item = String::new();
    for line in help.lines() {
        if !line.starts_with(' ') && line.ends_with(':') {
            mode = match line {
                "Commands:" => In::Commands,
                "Options:" => In::Options,
                "Arguments:" => In::Arguments,
                _ => In::Nothing,
            };
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        // Options without a short flag are indented to line up with the others.
        let option_without_short = mode == In::Options && indent == 6 && text.starts_with("--");
        if indent == 2 || option_without_short {
            match mode {
                In::Commands => {
                    let name = text.split_whitespace().next().unwrap();
                    if name != "help" {
                        out.insert(format!("command:{name}"));
                    }
                }
                In::Options => {
                    let spec = text.split("  ").next().unwrap();
                    for token in spec.split(", ") {
                        let flag = token.split_whitespace().next().unwrap();
                        if flag.starts_with('-') && flag != "-h" && flag != "--help" {
                            out.insert(format!("option:{flag}"));
                            flag.clone_into(&mut last_item);
                        }
                    }
                }
                In::Arguments => {
                    let name = text
                        .split_whitespace()
                        .next()
                        .unwrap()
                        .trim_matches(|c| "<>[].".contains(c));
                    out.insert(format!("argument:{name}"));
                    name.clone_into(&mut last_item);
                }
                In::Nothing => {}
            }
        } else if indent > 2
            && mode != In::Nothing
            && let Some(value) = text.strip_prefix("- ")
            && let Some((name, _)) = value.split_once(':')
        {
            out.insert(format!("value:{last_item}={name}"));
        }
    }
    out
}

fn sections(file: &str) -> Vec<(Vec<String>, String)> {
    let mut out: Vec<(Vec<String>, String)> = Vec::new();
    for line in file.lines() {
        if let Some(title) = line
            .strip_prefix("===== ")
            .and_then(|l| l.strip_suffix(" ====="))
        {
            let path = title
                .split_whitespace()
                .skip(1)
                .map(str::to_owned)
                .collect();
            out.push((path, String::new()));
        } else if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

fn current_help(path: &[String]) -> String {
    let out = Command::new(Sandbox::binary())
        .args(path)
        .arg("--help")
        .env_clear()
        .output()
        .expect("spawn sluice");
    assert!(out.status.success(), "sluice {path:?} --help failed");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn cli_surface_is_additive() {
    for release in RELEASES {
        let file = fs::read_to_string(fixtures(release).join("cli-help.txt")).unwrap();
        let old = sections(&file);
        assert!(old.len() >= 13, "{release}: cli-help.txt went missing");
        // The extractor must see real items, or the comparison below proves nothing.
        let run = old.iter().find(|(p, _)| p == &["run".to_owned()]).unwrap();
        let items = surface(&run.1);
        for expected in ["option:--hook", "option:--explain", "argument:GATE"] {
            assert!(
                items.contains(expected),
                "{release}: `{expected}` not extracted: {items:?}"
            );
        }
        for (path, body) in old {
            let name = format!("{release}: sluice {}", path.join(" "));
            let want = surface(&body);
            let have = surface(&current_help(&path));
            let missing: Vec<_> = want.difference(&have).collect();
            assert!(missing.is_empty(), "{name} lost {missing:?}");
        }
    }
}

#[test]
fn every_released_subcommand_is_listed_by_the_top_level_help() {
    for release in RELEASES {
        let file = fs::read_to_string(fixtures(release).join("cli-help.txt")).unwrap();
        let (_, top) = &sections(&file)[0];
        let have = surface(&current_help(&[]));
        for item in surface(top).iter().filter(|i| i.starts_with("command:")) {
            assert!(have.contains(item), "{release}: {item} disappeared");
        }
    }
}

// ---------------------------------------------------------------------------
// Exit codes (G11)
// ---------------------------------------------------------------------------

const GATES: &str = r#"
[gates.test]
cmd = "echo run >> \"$SLUICE_TEST_COUNTER\""
hooks = ["pre-commit", "pre-push"]

[gates.lint]
cmd = "echo lint >> \"$SLUICE_TEST_COUNTER\""

[gates.fail]
cmd = "exit 7"

[gates.slow]
cmd = "sleep 5"
timeout = "1s"

[gates.sig]
cmd = "kill -TERM $$"
"#;

/// What is checked, the extra environment, the arguments, the exit code.
type ExitCase<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [&'a str], i32);

fn exit_code(sb: &Sandbox, extra_env: &[(&str, &str)], args: &[&str]) -> i32 {
    let out = sb
        .sluice()
        .envs(extra_env.iter().copied())
        .args(args)
        .timeout(Duration::from_secs(30))
        .output()
        .unwrap();
    out.status.code().expect("exited normally")
}

#[test]
fn exit_codes_are_frozen() {
    let sb = Sandbox::new(GATES);
    // (what, environment, arguments, code): the table of `sluice --help`.
    let table: [ExitCase; 8] = [
        ("pass", &[], &["run", "lint"], 0),
        ("the gate's own code", &[], &["run", "fail"], 7),
        ("Sluice error", &[], &["run", "no-such-gate"], 1),
        ("usage error", &[], &["run"], 2),
        (
            "refused by agent_hook_policy = fail-fast",
            &[("CLAUDECODE", "1")],
            &["run", "--hook", "pre-commit", "test"],
            3,
        ),
        ("gate timeout", &[], &["run", "slow"], 124),
        ("gate killed by a signal", &[], &["run", "sig"], 143),
        (
            "another daemon holds the lock",
            &[],
            &["daemon", "--foreground"],
            75,
        ),
    ];
    for (what, env, args, code) in table {
        assert_eq!(exit_code(&sb, env, args), code, "{what}: sluice {args:?}");
    }
}

#[test]
fn queue_requests_refused_by_the_queue_state_exit_4() {
    let sb = Sandbox::new(GATES);
    let remote = sb.root.path().join("remote.git");
    sb.git_in(
        sb.root.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    sb.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    sb.git(&["push", "-q", "origin", "main"]);
    sb.git(&["fetch", "-q", "origin"]);
    sb.git(&["remote", "set-head", "origin", "main"]);
    sb.git(&["checkout", "-q", "-b", "feature"]);
    fs::write(sb.repo.join("f.txt"), "feature\n").unwrap();
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "feature"]);
    assert_eq!(
        exit_code(&sb, &[], &["queue", "enqueue"]),
        0,
        "first enqueue"
    );
    assert_eq!(
        exit_code(&sb, &[], &["queue", "enqueue"]),
        4,
        "the same branch twice is refused by the state of the queue"
    );
}

#[test]
fn long_help_documents_every_exit_code() {
    let help = current_help(&[]);
    for (code, what) in [
        ("0", "pass"),
        ("1", "error"),
        ("2", "usage"),
        ("3", "agent_hook_policy"),
        ("4", "queue"),
        ("75", "daemon"),
        ("124", "timeout"),
        ("143", "killed"),
    ] {
        let listed = help
            .lines()
            .any(|l| l.split(|c: char| !c.is_ascii_digit()).any(|w| w == code) && l.contains(what));
        assert!(
            listed,
            "`sluice --help` must document exit code {code} ({what})"
        );
    }
}

// ---------------------------------------------------------------------------
// MCP tools (G13)
// ---------------------------------------------------------------------------

/// `tools/list` of the current `sluice mcp` over stdio.
fn current_tools() -> Value {
    let sb = Sandbox::new("");
    let mut child = Command::new(Sandbox::binary())
        .arg("mcp")
        .current_dir(&sb.home)
        .env_clear()
        .envs(sb.env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sluice mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut send = |msg: Value| writeln!(stdin, "{msg}").unwrap();
    let mut reply = |id: u64| loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "server closed stdout"
        );
        let v: Value = serde_json::from_str(&line).unwrap();
        if v["id"] == id && v.get("method").is_none() {
            return v;
        }
    };
    send(
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},
        "clientInfo":{"name":"compat-test","version":"1"}}}),
    );
    reply(1);
    send(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}));
    send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let tools = reply(2)["result"].clone();
    drop(stdin);
    let _ = child.wait();
    tools
}

fn by_name(tools: &Value) -> std::collections::BTreeMap<String, Value> {
    tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["name"].as_str().unwrap().to_owned(), t.clone()))
        .collect()
}

fn strings(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Old properties still exist with the same `type`; nothing new is required.
fn assert_schema_additive(what: &str, old: &Value, new: &Value) {
    let (Some(old_props), new_props) = (old["properties"].as_object(), &new["properties"]) else {
        return;
    };
    for (name, old_prop) in old_props {
        let new_prop = &new_props[name];
        assert!(!new_prop.is_null(), "{what}: property `{name}` was removed");
        if let Some(t) = old_prop.get("type") {
            assert_eq!(&new_prop["type"], t, "{what}: type of `{name}` changed");
        }
    }
    let old_required = strings(&old["required"]);
    let new_required = strings(&new["required"]);
    let newly: Vec<_> = new_required.difference(&old_required).collect();
    assert!(
        newly.is_empty(),
        "{what}: new required arguments {newly:?} break old callers"
    );
}

#[test]
fn tools_are_additive() {
    let now = by_name(&current_tools());
    for release in RELEASES {
        let old: Value =
            serde_json::from_slice(&fs::read(fixtures(release).join("mcp-tools.json")).unwrap())
                .unwrap();
        let old = by_name(&old);
        assert!(old.len() >= 4, "{release}: mcp-tools.json went missing");
        for (name, tool) in &old {
            let new = now
                .get(name)
                .unwrap_or_else(|| panic!("{release}: tool `{name}` disappeared"));
            assert_schema_additive(
                &format!("{release}/{name} input"),
                &tool["inputSchema"],
                &new["inputSchema"],
            );
            if !tool["outputSchema"].is_null() {
                assert_schema_additive(
                    &format!("{release}/{name} output"),
                    &tool["outputSchema"],
                    &new["outputSchema"],
                );
            }
        }
    }
}

#[test]
fn queue_tools_exist_since_v0_2_0() {
    let now = by_name(&current_tools());
    for tool in [
        "verify",
        "status",
        "gate_list",
        "logs",
        "queue_enqueue",
        "queue_status",
        "queue_approve",
        "queue_retry",
        "queue_cancel",
    ] {
        assert!(now.contains_key(tool), "tool {tool}");
    }
}

// ---------------------------------------------------------------------------
// Plugin surface (G14)
// ---------------------------------------------------------------------------

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .unwrap()
}

#[test]
fn plugin_version_equals_the_workspace_version() {
    let plugin = read_json(&repo_root().join("plugin/.claude-plugin/plugin.json"));
    assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn plugin_contract_is_frozen() {
    let root = repo_root();
    let plugin = read_json(&root.join("plugin/.claude-plugin/plugin.json"));
    assert_eq!(plugin["name"], "sluice");
    let mcp = read_json(&root.join("plugin/.mcp.json"));
    assert_eq!(mcp["mcpServers"]["sluice"]["command"], "sluice");
    assert_eq!(mcp["mcpServers"]["sluice"]["args"], json!(["mcp"]));
    let hooks = read_json(&root.join("plugin/hooks/hooks.json"));
    let pre = &hooks["hooks"]["PreToolUse"][0];
    assert_eq!(pre["matcher"], "Bash");
    assert!(
        pre["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("hooks/guard.sh")
    );
    let market = read_json(&root.join(".claude-plugin/marketplace.json"));
    assert_eq!(market["name"], "sluice");
    assert_eq!(market["plugins"][0]["name"], "sluice");
    let skill = fs::read_to_string(root.join("plugin/skills/sluice/SKILL.md")).unwrap();
    assert!(skill.lines().any(|l| l == "name: sluice"));
}
