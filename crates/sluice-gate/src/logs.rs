//! `sluice logs [<job-id>] [-n <lines>]`: print a job log.
//!
//! The daemon is asked first (it knows every job it ran); without a daemon,
//! or for a job it does not know (in-process fallback runs), the log
//! directory is scanned as in Phase 1.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sluice_gate_core::dirs as core_dirs;
use sluice_gate_core::git::EnvMap;

use crate::dirs;
use crate::run::EXIT_ERROR;
use sluice_gate_daemon::executor::spool;
use sluice_gate_proto::client;

/// Prints the log of `job_id`, or of the newest job when `None`.
/// With `lines`, only the last that many lines.
pub fn show(job_id: Option<&str>, lines: Option<usize>) -> ExitCode {
    let env: EnvMap = std::env::vars().collect();
    if let Some((path, tail)) = from_daemon(&env, job_id, lines) {
        // Without `-n` the whole file is printed; the daemon only sends a tail.
        let all = read_lines(&path).unwrap_or(tail);
        print_lines(&path, &all, lines);
        return ExitCode::SUCCESS;
    }
    let dir = dirs::logs_dir(&env);
    let path = if let Some(id) = job_id {
        let log = dir.join(format!("{id}.log"));
        let spool = dir.join(format!("{id}.exec"));
        if !log.exists() && spool.exists() {
            spool
        } else {
            log
        }
    } else {
        let newest = fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(sluice_gate_core::logfile::is_job_log_name)
            })
            .map(|e| e.path())
            .max();
        let Some(newest) = newest else {
            eprintln!("sluice: no job logs in {}", dir.display());
            return ExitCode::from(EXIT_ERROR);
        };
        newest
    };
    match read_lines(&path) {
        Ok(all) => {
            print_lines(&path, &all, lines);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("sluice: cannot open {}: {e}", path.display());
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Asks a running daemon (never starts one) for the job's log.
fn from_daemon(
    env: &EnvMap,
    job_id: Option<&str>,
    lines: Option<usize>,
) -> Option<(PathBuf, Vec<String>)> {
    let socket = core_dirs::socket_path(env).ok()?;
    if !socket.exists() {
        return None;
    }
    let client = client::connect(&socket).ok()?;
    client.logs(job_id, lines.unwrap_or(0)).ok()
}

/// Lines of a job log; the output spool of an exec job (`<job>.exec`) is
/// decoded with stdout and stderr merged in arrival order.
fn read_lines(path: &Path) -> std::io::Result<Vec<String>> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    if reader.fill_buf()?.starts_with(spool::MAGIC) {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        let merged = spool::merged(&bytes).unwrap_or_default();
        return Ok(String::from_utf8_lossy(&merged)
            .lines()
            .map(str::to_owned)
            .collect());
    }
    Ok(reader.lines().map_while(Result::ok).collect())
}

fn print_lines(path: &Path, all: &[String], lines: Option<usize>) {
    let start = lines.map_or(0, |n| all.len().saturating_sub(n));
    eprintln!("# {}", path.display());
    for line in &all[start..] {
        println!("{line}");
    }
}
