//! The `sluice` binary.
//!
//! One binary, several modes: thin client for hooks and terminals
//! (`sluice run`), MCP server for agents (`sluice mcp`), and the daemon
//! itself (`sluice daemon`). Phase 1 implements the cache-only commands;
//! `mcp`, `daemon` and `status` arrive with Phases 2 and 3.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sluice_gate_core::config::{REPO_CONFIG_FILE, RepoConfig, starter_repo_config};

mod cache;
mod dirs;
mod logs;
mod run;
mod snippets;

use snippets::{HookManager, snippet};

/// Exit code used while a command is not implemented yet.
const EXIT_NOT_IMPLEMENTED: u8 = 70;

#[derive(Debug, Parser)]
#[command(
    name = "sluice",
    version,
    about = "Verification queue and cache for parallel coding agents",
    long_about = "Sluice runs named gates (tests, linters, type checks) under a global \
                  concurrency budget and caches results per git tree so each tree is verified \
                  once. Git hooks call `sluice run`; agents will call the MCP server started by \
                  `sluice mcp` once Phase 3 lands.\n\n\
                  Phase 1 ships `run`, `cache`, `init`, `hook print` and `logs`; `daemon` and \
                  `status` (Phase 2) and `mcp` (Phase 3) exit 70 until then.\n\n\
                  Exit codes of `sluice run`: 0 pass or cache hit; the gate's own code on \
                  failure; 3 refused by agent_hook_policy = \"fail-fast\"; 124 gate timeout; \
                  143 gate killed by a signal; 1 Sluice error; 2 usage error.\n\n\
                  Environment: SLUICE_DISABLE=1 runs the gate directly (no cache, no queue); \
                  SLUICE_CACHE=0|1 forces the cache off or on (on overrides CI); CI disables \
                  the cache; SLUICE_AGENT=1|0 forces agent or human detection; SLUICE_CONFIG \
                  points at the user config."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a gate: cache hit in milliseconds, otherwise run it under the concurrency budget.
    Run {
        /// Gate name as defined in `.sluice.toml`.
        gate: String,
        /// Name of the git hook invoking Sluice (pre-commit, pre-push, ...). Enables the
        /// hook policy on a cache miss and, for pre-commit, records the committed tree.
        #[arg(long)]
        hook: Option<String>,
        /// Stream the gate output to stderr while it runs (it is always written to the log).
        #[arg(short, long)]
        verbose: bool,
        /// Explain what Sluice would do (caller detection, policy, tree, key, cache lookup) and exit.
        #[arg(long)]
        explain: bool,
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
        /// Job id; defaults to the most recent job.
        job_id: Option<String>,
        /// Only print the last N lines.
        #[arg(short = 'n', long)]
        lines: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// Show, per gate, whether the current tree has a cached pass.
    Status {
        /// Also list every marker stored for the repository.
        #[arg(long)]
        all: bool,
    },
    /// Remove markers and miss records for the current repository.
    Clear {
        /// Only clear entries of this gate.
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
    match cli.command {
        Command::Run {
            gate,
            hook,
            verbose,
            explain,
            extra,
        } => run::run(&run::RunArgs {
            gate,
            extra,
            hook: hook.filter(|h| !h.is_empty()),
            verbose,
            explain,
        }),
        Command::Cache {
            command: CacheCommand::Status { all },
        } => cache::status(all),
        Command::Cache {
            command: CacheCommand::Clear { gate },
        } => cache::clear(gate.as_deref()),
        Command::Init { force } => init(force),
        Command::Hook {
            command: HookCommand::Print { manager },
        } => hook_print(manager),
        Command::Logs { job_id, lines } => logs::show(job_id.as_deref(), lines),
        Command::Mcp => not_implemented("mcp"),
        Command::Daemon => not_implemented("daemon"),
        Command::Status { .. } => not_implemented("status"),
    }
}

fn not_implemented(name: &str) -> ExitCode {
    eprintln!(
        "sluice: '{name}' is not implemented yet (it arrives with the daemon and MCP phases)"
    );
    ExitCode::from(EXIT_NOT_IMPLEMENTED)
}

/// Gates for the hooks `(pre-commit, pre-push)`.
type HookGates = (Vec<String>, Vec<String>);

/// Gates of the starter config.
fn starter_gates() -> HookGates {
    (
        vec!["lint".to_owned(), "test".to_owned()],
        vec!["test".to_owned()],
    )
}

fn hook_gates(config: &RepoConfig) -> HookGates {
    let names = |hook: &str| {
        config
            .gates_for_hook(hook)
            .into_iter()
            .map(|(name, _)| name.to_owned())
            .collect()
    };
    (names("pre-commit"), names("pre-push"))
}

fn hook_print(manager: HookManager) -> ExitCode {
    let (preface, gates) = match fs::read_to_string(REPO_CONFIG_FILE) {
        Ok(text) => match RepoConfig::parse(&text, Path::new(REPO_CONFIG_FILE)) {
            Ok(config) => (String::new(), hook_gates(&config)),
            Err(e) => {
                eprintln!("sluice: error: {e}");
                return ExitCode::from(run::EXIT_ERROR);
            }
        },
        Err(_) => (
            "# No .sluice.toml found here; example gates shown. Run 'sluice init' first.\n"
                .to_owned(),
            starter_gates(),
        ),
    };
    print!("{preface}{}", snippet(manager, &gates.0, &gates.1));
    ExitCode::SUCCESS
}

fn init(force: bool) -> ExitCode {
    if Path::new(REPO_CONFIG_FILE).exists() && !force {
        eprintln!("sluice: {REPO_CONFIG_FILE} already exists; not overwriting (use --force)");
        return ExitCode::from(run::EXIT_ERROR);
    }
    if let Err(err) = fs::write(REPO_CONFIG_FILE, starter_repo_config()) {
        eprintln!("sluice: cannot write {REPO_CONFIG_FILE}: {err}");
        return ExitCode::from(run::EXIT_ERROR);
    }
    println!("sluice: wrote {REPO_CONFIG_FILE}");
    println!();
    let (pre_commit, pre_push) = starter_gates();
    print!("{}", snippet(HookManager::Plain, &pre_commit, &pre_push));
    ExitCode::SUCCESS
}
