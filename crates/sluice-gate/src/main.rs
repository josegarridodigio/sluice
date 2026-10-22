//! The `sluice` binary.
//!
//! One binary, several modes: thin client for hooks and terminals
//! (`sluice run`), MCP server for agents (`sluice mcp`), and the daemon
//! itself (`sluice daemon`). Phase 0 only defines the command surface;
//! every command reports that it is not implemented yet.

use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

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
    Init,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum HookManager {
    Husky,
    Lefthook,
    PreCommit,
    Plain,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let name = match cli.command {
        Command::Run { .. } => "run",
        Command::Mcp => "mcp",
        Command::Daemon => "daemon",
        Command::Status { .. } => "status",
        Command::Cache { .. } => "cache",
        Command::Init => "init",
        Command::Hook { .. } => "hook",
        Command::Logs { .. } => "logs",
    };
    eprintln!(
        "sluice: '{name}' is not implemented yet (cache format v{})",
        sluice_gate_core::CACHE_FORMAT_VERSION
    );
    ExitCode::from(EXIT_NOT_IMPLEMENTED)
}
