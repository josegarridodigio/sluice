//! The `sluice` binary.
//!
//! One binary, several modes: thin client for hooks and terminals
//! (`sluice run`), MCP server for agents (`sluice mcp`), and the daemon
//! itself (`sluice daemon`).

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sluice_gate_core::config::{REPO_CONFIG_FILE, RepoConfig, starter_repo_config};

mod cache;
mod clientlog;
mod daemon_cmd;
mod dirs;
mod doctor;
mod logging;
mod logs;
mod queue;
mod run;
mod snippets;
mod status;
mod update;

use snippets::{HookManager, snippet};

#[derive(Debug, Parser)]
#[command(
    name = "sluice",
    version,
    about = "Verification queue and cache for parallel coding agents",
    long_about = "Sluice runs named gates (tests, linters, type checks) under a global \
                  concurrency budget and caches results per git tree so each tree is verified \
                  once. Git hooks call `sluice run`; coding agents call the MCP server started by \
                  `sluice mcp` (tools verify, status, gate_list, logs).\n\n\
                  On a cache miss `sluice run` hands the gate to the per-user daemon \
                  (`sluice daemon`), starting it on demand; when the daemon cannot be reached \
                  or started the gate runs in-process. `sluice status` shows the queue.\n\n\
                  Exit codes of `sluice run`: 0 pass or cache hit; the gate's own code on \
                  failure; 3 refused by agent_hook_policy = \"fail-fast\"; 124 gate timeout; \
                  143 gate killed (signal or `sluice daemon stop --force`); 1 Sluice error; \
                  2 usage error.\n\n\
                  Environment: SLUICE_DISABLE=1 runs the gate directly (no cache, no queue); \
                  SLUICE_CACHE=0|1 forces the cache off or on (on overrides CI); CI disables \
                  the cache; SLUICE_AGENT=1|0 forces agent or human detection; SLUICE_CONFIG \
                  points at the user config; SLUICE_SOCKET overrides the daemon socket path; \
                  SLUICE_LOG sets the log filter of the daemon, `sluice mcp` and `sluice run` \
                  (then `[debug] level` of the user config, then info)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a gate: cache hit in milliseconds, otherwise queue it in the daemon under the budget.
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
    ///
    /// Tools: `verify` (run gates through the cache and the daemon, blocking with
    /// progress notifications), `status`, `gate_list`, `logs`. Logs go to stderr
    /// only. Register `sluice mcp` as a stdio MCP server in the agent's config.
    Mcp,
    /// Start the per-user daemon (detached unless --foreground), or stop it.
    ///
    /// Exits 75 when another daemon still holds the daemon lock.
    Daemon {
        /// Stay attached to the terminal instead of detaching.
        #[arg(long)]
        foreground: bool,
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
    /// Check the installation (git, PSI, daemon, lease, configs, hooks) and optionally
    /// collect a redacted diagnostics bundle for bug reports.
    ///
    /// Exits 1 when a check fails, 0 otherwise. Never uses the network. `--bundle`
    /// writes a `.tar.gz` with the doctor report, versions, configs, `status --json`,
    /// `queue status --json`, recent daemon, client and MCP logs and the logs of the
    /// last failed jobs. Home paths, secret-looking values and URL credentials are
    /// redacted; review the file before attaching it to an issue.
    Doctor {
        /// Print machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Write a diagnostics bundle (default `./sluice-doctor-<timestamp>.tar.gz`).
        #[arg(long, value_name = "PATH", num_args = 0..=1)]
        #[allow(clippy::option_option)]
        bundle: Option<Option<std::path::PathBuf>>,
    },
    /// Show the daemon, budget, system pressure (PSI), running and queued jobs, and totals.
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
    /// Local merge queue: enqueue committed branches, approve trains, land them.
    ///
    /// Agents enqueue committed branches; a daemon task combines them on top of the
    /// target branch in a disposable worktree, runs the gates once on the combined
    /// tree and shows the plan. Nothing is pushed until a human runs
    /// `sluice queue approve`. Exit codes: 0 ok; 1 error; 2 usage; 4 refused by the
    /// state of the queue (conflict-state, not-allowed, stale-plan, duplicate-branch).
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Update sluice to the latest (or a given) release.
    ///
    /// Downloads the release archive for this architecture with `curl` (or `wget`),
    /// verifies it against `SHA256SUMS`, checks that the new binary runs and replaces
    /// the running executable atomically. Then, unless --no-restart, it stops an idle
    /// daemon so the next `sluice` call starts the new version; a daemon running gates
    /// is left alone. `sluice mcp` servers of agent sessions keep the old binary until
    /// the agent reconnects. On a terminal it asks `Update sluice X -> Y? [y/N]`;
    /// without one it requires --yes (exit 2). Exit codes: 0 updated, up to date or
    /// --check; 1 error (network, checksum, permissions); 2 usage. Environment:
    /// `SLUICE_UPDATE_BASE_URL` points at a directory or `file://` URL holding the
    /// archives, `SHA256SUMS` and a `LATEST` file (or pass --version).
    Update {
        /// Only report whether an update is available; change nothing.
        #[arg(long)]
        check: bool,
        /// Install this release tag (for example v0.2.0), also an older one.
        #[arg(long, value_name = "vX.Y.Z")]
        version: Option<String>,
        /// Do not ask for confirmation (required without a terminal).
        #[arg(long)]
        yes: bool,
        /// Do not stop the daemon after updating.
        #[arg(long)]
        no_restart: bool,
    },
}

#[derive(Debug, Subcommand)]
enum QueueCommand {
    /// Queue a committed branch for the next train.
    ///
    /// Defaults to the current branch of this worktree. Refuses a detached HEAD, a
    /// worktree with staged or unstaged changes to tracked files (untracked files do
    /// not matter), a branch that is already queued and a branch equal to the target.
    Enqueue {
        /// Branch to queue; defaults to the current branch.
        branch: Option<String>,
        /// Where the train lands: `<remote>/<branch>` (pushed), `<branch>` (a local
        /// branch, updated in place) or a bare name tried first as a branch of the
        /// configured remote and then as a local branch. Defaults to `[queue] remote`
        /// and `target`. Tags, raw SHAs, HEAD and checked-out local branches are refused.
        #[arg(long)]
        base: Option<String>,
    },
    /// Show trains, their members, gates, conflicts and queued jobs.
    ///
    /// Inside a repository only its own trains and jobs are listed. With --json the
    /// `queue-status` payload is printed verbatim (it carries `schema: 1`).
    Status {
        /// Print machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Approve a ready train: print the plan and land it.
    ///
    /// On a terminal it asks `Land this train? [y/N]`; without one it requires --yes.
    /// The train hash that was displayed is sent along, so a train that changed in the
    /// meantime is refused (stale-plan, exit 4). Agents must not run this command.
    Approve {
        /// Train id, as shown by `sluice queue status`.
        train_id: String,
        /// Do not ask for confirmation (required without a terminal).
        #[arg(long)]
        yes: bool,
    },
    /// Re-queue a conflicted or failed job with the current head of its branch.
    ///
    /// Run it in the repository of the job after rebasing or fixing the branch; the
    /// branch head is re-read here.
    Retry {
        /// Job id, as shown by `sluice queue status`.
        job_id: String,
    },
    /// Cancel a job that has not been pushed.
    ///
    /// A job that is pushing is only cancelled if `sluice queue reconcile` proves
    /// that nothing landed.
    Cancel {
        /// Job id, as shown by `sluice queue status`.
        job_id: String,
    },
    /// Resolve trains whose push outcome is unknown (`needs_reconcile`).
    ///
    /// Reads the remote and prints one line per train: the decision (landed, ready,
    /// queued, cancelled or refused) and the reason. It never pushes.
    Reconcile,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Ask the running daemon to stop (queued jobs are dropped, running jobs finish).
    ///
    /// Without --force, waits (up to 60 s) until the running jobs finished and the
    /// daemon released its lock, so the next command starts a fresh daemon.
    Stop {
        /// Kill running jobs instead of letting them finish.
        #[arg(long)]
        force: bool,
    },
    /// Stop the daemon and start a fresh detached one.
    ///
    /// Refuses (exit 1) while gates are running unless --force. With --debug the new
    /// daemon logs at `debug` level (`SLUICE_LOG=debug`) until it exits; `sluice status`
    /// shows the active level and the log path.
    Restart {
        /// Run the new daemon with `SLUICE_LOG=debug` (only that instance).
        #[arg(long)]
        debug: bool,
        /// Kill running gates instead of refusing.
        #[arg(long)]
        force: bool,
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
        Command::Mcp => mcp(),
        Command::Daemon {
            command: Some(DaemonCommand::Stop { force }),
            ..
        } => daemon_cmd::stop(force),
        Command::Daemon {
            command: Some(DaemonCommand::Restart { debug, force }),
            ..
        } => daemon_cmd::restart(debug, force),
        Command::Doctor { json, bundle } => doctor::run(&doctor::DoctorArgs { json, bundle }),
        Command::Daemon {
            foreground,
            command: None,
        } => daemon_cmd::start(foreground),
        Command::Status { json } => status::show(json),
        Command::Update {
            check,
            version,
            yes,
            no_restart,
        } => update::run(&update::UpdateArgs {
            check,
            version,
            yes,
            no_restart,
        }),
        Command::Queue { command } => match command {
            QueueCommand::Enqueue { branch, base } => {
                queue::enqueue(branch.as_deref(), base.as_deref())
            }
            QueueCommand::Status { json } => queue::status(json),
            QueueCommand::Approve { train_id, yes } => queue::approve(&train_id, yes),
            QueueCommand::Retry { job_id } => queue::retry(&job_id),
            QueueCommand::Cancel { job_id } => queue::cancel(&job_id),
            QueueCommand::Reconcile => queue::reconcile(),
        },
    }
}

/// `sluice mcp`: the MCP stdio server on a multi-threaded tokio runtime
/// (only this subcommand builds one; the `sluice run` hit path stays synchronous).
fn mcp() -> ExitCode {
    let env = std::env::vars().collect();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("sluice: error: cannot start the async runtime: {e}");
            return ExitCode::from(run::EXIT_ERROR);
        }
    };
    match runtime.block_on(sluice_gate_mcp::serve(env)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sluice: error: {e:#}");
            ExitCode::from(run::EXIT_ERROR)
        }
    }
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
