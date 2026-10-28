//! `sluice gate list`: the gates of the repository, from `.sluice.toml` or,
//! without one, from its git hooks (ADR-0021 decision 7).

use std::process::ExitCode;

use sluice_gate_core::config::{EnvMap, RepoConfig, UserConfig};
use sluice_gate_core::git;
use sluice_gate_core::hookgate::{HookExclusion, HookGates};
use sluice_gate_core::lookup;

use crate::hookentry::gates;
use crate::run::EXIT_ERROR;

fn fail(message: &str) -> ExitCode {
    eprintln!("sluice: error: {message}");
    ExitCode::from(EXIT_ERROR)
}

/// The configuration to list and, for hook gates, the manager of each.
struct Listing {
    config: RepoConfig,
    managers: std::collections::BTreeMap<String, String>,
    excluded: Vec<HookExclusion>,
}

fn load(paths: &git::RepoPaths, env: &EnvMap) -> Result<Listing, String> {
    match RepoConfig::load(&paths.toplevel) {
        Ok(config) => Ok(Listing {
            config,
            managers: std::collections::BTreeMap::new(),
            excluded: Vec::new(),
        }),
        Err(sluice_gate_core::Error::ConfigNotFound(root)) => {
            let derived: HookGates = gates::derive(paths, env, None);
            if derived.gates.is_empty() && derived.excluded.is_empty() {
                return Err(sluice_gate_core::Error::ConfigNotFound(root).to_string());
            }
            let managers = derived
                .gates
                .iter()
                .map(|g| (g.name.clone(), g.manager.clone()))
                .collect();
            let excluded = derived.excluded.clone();
            Ok(Listing {
                config: lookup::hook_config(derived),
                managers,
                excluded,
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

fn json_of(paths: &git::RepoPaths, listing: &Listing, budget: u32) -> serde_json::Value {
    let gates: Vec<_> = listing
        .config
        .gates
        .iter()
        .map(|(name, g)| {
            let mut gate = serde_json::json!({
                "name": name,
                "cmd": g.cmd.trim(),
                "weight": g.weight.ceiling(budget),
                "timeout_seconds": g.timeout.as_secs(),
                "hooks": g.hooks,
                "cache": g.cache,
                "source": if listing.managers.contains_key(name) { "hook" } else { "config" },
            });
            if let Some(manager) = listing.managers.get(name) {
                gate["manager"] = manager.clone().into();
            }
            gate
        })
        .collect();
    let excluded: Vec<_> = listing
        .excluded
        .iter()
        .map(|e| {
            serde_json::json!({
                "hook": e.hook,
                "unit": e.unit,
                "command": e.command,
                "reason": e.reason,
            })
        })
        .collect();
    serde_json::json!({
        "schema": 1,
        "repository": paths.toplevel,
        "gates": gates,
        "excluded": excluded,
    })
}

fn print_text(listing: &Listing, budget: u32) {
    let width = listing
        .config
        .gates
        .keys()
        .map(String::len)
        .max()
        .unwrap_or(0);
    for (name, g) in &listing.config.gates {
        let origin = listing
            .managers
            .get(name)
            .map_or(String::new(), |m| format!(", from the {m} hook"));
        println!(
            "{name:<width$}  {}  (weight {}, timeout {}, hooks: {}, cache {}{origin})",
            g.normalized_cmd(),
            g.weight.ceiling(budget),
            humantime::format_duration(g.timeout),
            if g.hooks.is_empty() {
                "-".to_owned()
            } else {
                g.hooks.join(", ")
            },
            if g.cache { "on" } else { "off" },
        );
    }
    if !listing.excluded.is_empty() {
        println!("not gates:");
    }
    for e in &listing.excluded {
        let what = match (&e.unit, &e.command) {
            (Some(unit), Some(cmd)) => format!("{unit} `{cmd}`"),
            _ => format!("hook {}", e.hook),
        };
        println!("  {what}: {}", e.reason);
    }
}

/// `sluice gate list [--json]`.
pub fn list(json: bool) -> ExitCode {
    let env: EnvMap = sluice_gate_core::config::process_env();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return fail(&format!("cannot read the current directory: {e}")),
    };
    let paths = match git::discover(&cwd, &env) {
        Ok(p) => p,
        Err(e) => return fail(&e.to_string()),
    };
    let listing = match load(&paths, &env) {
        Ok(l) => l,
        Err(e) => return fail(&e),
    };
    let budget = UserConfig::load(&env).map_or_else(
        |_| sluice_gate_core::cpus::default_budget(),
        |u| u.daemon.budget,
    );
    if json {
        println!("{:#}", json_of(&paths, &listing, budget));
    } else {
        print_text(&listing, budget);
    }
    ExitCode::SUCCESS
}
