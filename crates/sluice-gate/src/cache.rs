//! `sluice cache status|clear`: inspect and remove markers of the current repository.

use std::process::ExitCode;
use std::time::Duration;

use sluice_gate_core::config::{RepoConfig, UserConfig};
use sluice_gate_core::git::{self, EnvMap};
use sluice_gate_core::key::{self, KeyMaterial};
use sluice_gate_core::marker::{Lookup, MarkerStore};
use sluice_gate_core::tree::{self, Snapshot};

use crate::run::{EXIT_ERROR, age};

fn fail(message: &str) -> ExitCode {
    eprintln!("sluice: error: {message}");
    ExitCode::from(EXIT_ERROR)
}

/// `sluice cache status [--all]`.
///
/// Prints, for every gate of `.sluice.toml`, whether the current tree has a
/// marker (and how old it is) or a recorded miss. With `all`, also lists
/// every marker stored for the repository.
pub fn status(all: bool) -> ExitCode {
    let env: EnvMap = std::env::vars().collect();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return fail(&format!("cannot read the current directory: {e}")),
    };
    let paths = match git::discover(&cwd, &env) {
        Ok(p) => p,
        Err(e) => return fail(&e.to_string()),
    };
    let config = match RepoConfig::load(&paths.toplevel) {
        Ok(c) => c,
        Err(e) => return fail(&e.to_string()),
    };
    let user = match UserConfig::load(&env) {
        Ok(u) => u,
        Err(e) => return fail(&e.to_string()),
    };
    let ttl = Duration::from_hours(u64::from(user.cache.ttl_days) * 24);
    let store = MarkerStore::new(&paths.common_dir);
    println!("markers: {}", store.dir().display());

    let probe_env = git::probe_env(&env, Some(&paths));
    let scratch = match tree::scratch_root(&env) {
        Ok(s) => s,
        Err(e) => return fail(&e.to_string()),
    };
    match tree::snapshot(&paths, &probe_env, &scratch) {
        Ok(Snapshot::Known(tree)) => {
            println!("tree:    {tree}");
            let child_env = match git::child_env(&env) {
                Ok(e) => e,
                Err(e) => return fail(&e.to_string()),
            };
            for (name, gate) in &config.gates {
                if !gate.cache {
                    println!("{name:<16} always runs (cache = false)");
                    continue;
                }
                let mut gate_env = child_env.clone();
                gate_env.extend(gate.env.iter().map(|(k, v)| (k.clone(), v.clone())));
                let line = match key::collect_inputs(
                    &gate.key_inputs,
                    &gate.key_files,
                    &paths.toplevel,
                    &gate_env,
                ) {
                    Ok((key_inputs, key_files)) => {
                        let material = KeyMaterial {
                            tree: tree.clone(),
                            gate_name: name.clone(),
                            cmd: gate.normalized_cmd(),
                            env: gate.env.clone(),
                            key_inputs,
                            key_files,
                        };
                        let cache_key = key::derive(&material);
                        match store.lookup(&cache_key, ttl) {
                            Ok(Lookup::Hit(m)) => {
                                format!("HIT  (passed {} ago, job {})", age(&m.passed_at), m.job_id)
                            }
                            Ok(Lookup::Miss(Some(m))) => format!(
                                "MISS (last run {} ago: {}{})",
                                age(&m.recorded_at),
                                m.reason,
                                m.exit_code.map_or(String::new(), |c| format!(", exit {c}"))
                            ),
                            Ok(Lookup::Miss(None)) => "MISS".to_owned(),
                            Err(e) => format!("ERROR ({e})"),
                        }
                    }
                    Err(e) => format!("ERROR ({e})"),
                };
                println!("{name:<16} {line}");
            }
        }
        Ok(Snapshot::Unknown { reason }) => {
            println!("tree:    unknown ({reason}); every gate runs uncached");
        }
        Err(e) => println!("tree:    error ({e})"),
    }

    if all {
        match store.list() {
            Ok(list) if list.is_empty() => println!("\nno markers stored"),
            Ok(list) => {
                println!("\nall markers ({}):", list.len());
                for m in list {
                    println!(
                        "  {:<16} {} ago  tree {}  key {}",
                        m.gate,
                        age(&m.passed_at),
                        &m.tree.as_str()[..12],
                        &m.key.as_str()[..12]
                    );
                }
            }
            Err(e) => println!("\ncannot list markers: {e}"),
        }
    }
    ExitCode::SUCCESS
}

/// `sluice cache clear [--gate <name>]`.
pub fn clear(gate: Option<&str>) -> ExitCode {
    let env: EnvMap = std::env::vars().collect();
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return fail(&format!("cannot read the current directory: {e}")),
    };
    let paths = match git::discover(&cwd, &env) {
        Ok(p) => p,
        Err(e) => return fail(&e.to_string()),
    };
    let store = MarkerStore::new(&paths.common_dir);
    match store.clear(gate) {
        Ok(n) => {
            println!(
                "sluice: removed {n} cache entr{} for {}",
                if n == 1 { "y" } else { "ies" },
                gate.map_or_else(|| "all gates".to_owned(), |g| format!("gate '{g}'"))
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e.to_string()),
    }
}
