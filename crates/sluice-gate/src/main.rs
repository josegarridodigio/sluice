//! The `sluice` binary.
//!
//! One binary, several modes: thin client for hooks and terminals
//! (`sluice run`), MCP server for agents (`sluice mcp`), and the daemon
//! itself (`sluice daemon`). Phase 0 only defines the command surface;
//! every command reports that it is not implemented yet.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod snippets;

use snippets::{HookManager, STARTER_CONFIG, snippet};

/// Exit code used while a command is not implemented yet.
const EXIT_NOT_IMPLEMENTED: u8 = 70;

#[derive(Debug, Parser)]
#[command(
    name = "sluice",
    version,
    about = "Verification queue and cache for parallel coding agents",
    long_about = "Sluice runs named gates (tests, linters, type checks) with a bounded global \
                  budget, caches results per git tree so each tree is verified once, and \
                  coalesces duplicate requests. Git hooks call `sluice run`; agents call the \
                  MCP server started by `sluice mcp`."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a gate, using the cache and the queue.
    Run {
        /// Gate name as defined in `.sluice.toml`.
        gate: String,
        /// Extra arguments appended to the gate command.
        #[arg(last = true)]
        extra: Vec<String>,
    },
    /// Start the MCP stdio server (spawned by the coding agent).
    Mcp,
    /// Run the per-user daemon in the foreground.
    Daemon,
    /// Show queue, in-flight jobs and cache statistics.
    Status {
        /// Print machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Inspect or clear cache markers for the current repository.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Write a starter `.sluice.toml` and print hook snippets.
    Init {
        /// Overwrite an existing `.sluice.toml`.
        #[arg(long)]
        force: bool,
    },
    /// Print hook integration snippets.
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// Show the log of a job.
    Logs {
        /// Job id; defaults to the most recent job of the current repository.
        job_id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// List markers for the current tree.
    Status,
    /// Remove markers for the current repository.
    Clear {
        /// Only clear markers of this gate.
        #[arg(long)]
        gate: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// Print the snippet for a hook manager.
    Print {
        /// Hook manager to print the snippet for.
        manager: HookManager,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Command::Init { force } => return init(*force),
        Command::Hook {
            command: HookCommand::Print { manager },
        } => return hook_print(*manager),
        _ => {}
    }
    let name = match cli.command {
        Command::Run { .. } => "run",
        Command::Mcp => "mcp",
        Command::Daemon => "daemon",
        Command::Status { .. } => "status",
        Command::Cache { .. } => "cache",
        Command::Init { .. } => "init",
        Command::Hook { .. } => "hook",
        Command::Logs { .. } => "logs",
    };
    eprintln!(
        "sluice: '{name}' is not implemented yet (cache format v{})",
        sluice_gate_core::CACHE_FORMAT_VERSION
    );
    ExitCode::from(EXIT_NOT_IMPLEMENTED)
}

const CONFIG_FILE: &str = ".sluice.toml";

/// Gates for the hooks `(pre-commit, pre-push)`.
type HookGates = (Vec<String>, Vec<String>);

/// Gates of the starter config.
fn starter_gates() -> HookGates {
    (
        vec!["lint".to_owned(), "test".to_owned()],
        vec!["test".to_owned()],
    )
}

/// Temporary line scanner for `.sluice.toml`: collects gate names from
/// `[gates.<name>]` headers and assigns hooks from a `hooks = [...]` line
/// inside the block. To be replaced by the core config loader once the lead
/// wires it in.
fn scan_hook_gates(text: &str) -> HookGates {
    let (mut pre_commit, mut pre_push) = (Vec::new(), Vec::new());
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("[gates.") {
            let name = rest.split(']').next().unwrap_or_default();
            let valid = name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
                && rest[name.len()..].starts_with(']');
            current = valid.then(|| name.to_owned());
        } else if line.starts_with('[') {
            current = None;
        } else if let Some(gate) = &current
            && line.starts_with("hooks")
        {
            if line.contains("\"pre-commit\"") {
                pre_commit.push(gate.clone());
            }
            if line.contains("\"pre-push\"") {
                pre_push.push(gate.clone());
            }
        }
    }
    (pre_commit, pre_push)
}

fn hook_print(manager: HookManager) -> ExitCode {
    let (text, gates) = match fs::read_to_string(CONFIG_FILE) {
        Ok(text) => (String::new(), scan_hook_gates(&text)),
        Err(_) => (
            "# No .sluice.toml found here; example gates shown. Run 'sluice init' first.\n"
                .to_owned(),
            starter_gates(),
        ),
    };
    print!("{text}{}", snippet(manager, &gates.0, &gates.1));
    ExitCode::SUCCESS
}

fn init(force: bool) -> ExitCode {
    if Path::new(CONFIG_FILE).exists() && !force {
        eprintln!("sluice: {CONFIG_FILE} already exists; not overwriting");
        return ExitCode::from(1);
    }
    if let Err(err) = fs::write(CONFIG_FILE, STARTER_CONFIG) {
        eprintln!("sluice: cannot write {CONFIG_FILE}: {err}");
        return ExitCode::from(1);
    }
    println!("sluice: wrote {CONFIG_FILE}");
    println!();
    let (pre_commit, pre_push) = starter_gates();
    print!("{}", snippet(HookManager::Plain, &pre_commit, &pre_push));
    ExitCode::SUCCESS
}
