//! `sluice logs [<job-id>] [-n <lines>]`: print a job log.

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::ExitCode;

use sluice_gate_core::git::EnvMap;

use crate::dirs;
use crate::run::EXIT_ERROR;

/// Prints the log of `job_id`, or of the newest job when `None`.
/// With `lines`, only the last that many lines.
pub fn show(job_id: Option<&str>, lines: Option<usize>) -> ExitCode {
    let env: EnvMap = std::env::vars().collect();
    let dir = dirs::logs_dir(&env);
    let path = if let Some(id) = job_id {
        dir.join(format!("{id}.log"))
    } else {
        let newest = fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "log"))
            .max();
        let Some(newest) = newest else {
            eprintln!("sluice: no job logs in {}", dir.display());
            return ExitCode::from(EXIT_ERROR);
        };
        newest
    };
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("sluice: cannot open {}: {e}", path.display());
            return ExitCode::from(EXIT_ERROR);
        }
    };
    let all: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
    let start = lines.map_or(0, |n| all.len().saturating_sub(n));
    eprintln!("# {}", path.display());
    for line in &all[start..] {
        println!("{line}");
    }
    ExitCode::SUCCESS
}
