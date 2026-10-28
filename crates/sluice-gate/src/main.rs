//! The `sluice` binary.
//!
//! One binary, several modes: thin client for hooks and terminals
//! (`sluice run`), MCP server for agents (`sluice mcp`), and the daemon
//! itself (`sluice daemon`).

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sluice_gate_core::config::{REPO_CONFIG_FILE, RepoConfig};

mod budget;
mod cache;
mod cg_exec;
mod clientlog;
mod daemon_cmd;
mod dirs;
mod doctor;
mod exec;
mod gate_list;
mod guard;
mod hookentry;
mod init;
mod logging;
mod logs;
mod queue;
mod run;
mod session_contain;
mod session_start;
mod shim;
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
                  once. Git hooks run gates through `sluice run`, or through `sluice hooks on`, which runs the repository's own hook commands through the same queue and cache; coding agents call the MCP server started by \
                  `sluice mcp` (tools verify, status, gate_list, logs and the merge-queue tools \
                  queue_enqueue, queue_status, queue_approve, queue_retry, queue_cancel).\n\n\
                  On a cache miss `sluice run` hands the gate to the per-user daemon \
                  (`sluice daemon`), starting it on demand; when the daemon cannot be reached \
                  or started the gate runs in-process. `sluice status` shows the queue. \
                  `sluice exec -- <cmd>` runs any other heavy command under the same budget, \
                  without cache.\n\n\
                  Exit codes:\n  \
                  0    pass or cache hit\n  \
                  N    the gate's own code when it fails (`sluice run`, `sluice exec`)\n  \
                  1    Sluice error\n  \
                  2    usage error (also `sluice queue` and `sluice update`)\n  \
                  3    refused by agent_hook_policy = \"fail-fast\"\n  \
                  4    `sluice queue` request refused by the state of the queue\n  \
                  75   `sluice daemon` found a daemon already holding the lock\n  \
                  124  gate timeout\n  \
                  125  `sluice exec` failed after the daemon accepted the request\n  \
                  141  `sluice exec` stdout is a closed pipe\n  \
                  143  gate killed (signal or `sluice daemon stop --force`)\n\n\
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
        /// Gate name from `.sluice.toml`, or, without one, a check unit of a hook that Sluice runs
        /// (see `sluice gate list`).
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
    /// Run any command under the budget, without cache (builds, test runs, docker builds).
    ///
    /// The command (an argv, no shell: use `sluice exec -- sh -c '...'` for a script)
    /// is queued in the daemon like a gate and runs in its own process group with the
    /// caller's environment and directory, stdin `/dev/null` and no terminal. Its stdout
    /// and stderr are replayed byte for byte to this process's stdout and stderr;
    /// SIGINT, SIGTERM and SIGHUP are forwarded to the whole process group.
    ///
    /// Exit codes: the command's own code; 128+N when it was killed by signal N; 124
    /// on --timeout; 143 when the daemon stopped and killed it; 127 command not found;
    /// 126 not executable; 125 Sluice error after the request was accepted.
    ///
    /// Without a usable daemon (not running and not startable, or older than 0.3.0) the
    /// command runs in-process under the same budget semaphore with inherited stdio.
    /// Inside a Sluice job (`SLUICE_JOB` set) it runs directly, without queueing.
    /// `SLUICE_DISABLE=1` always runs it in-process (with a terminal, if any).
    Exec {
        /// Budget units the command takes; default half the budget, rounded up.
        #[arg(long, value_name = "N")]
        weight: Option<u32>,
        /// Label shown in `sluice status` and messages; default the first two words.
        #[arg(long, value_name = "L")]
        label: Option<String>,
        /// Share one run between simultaneous requests with this key and the same
        /// command and directory (environment differences are ignored).
        #[arg(long, value_name = "K")]
        coalesce_key: Option<String>,
        /// Kill the command after this long (for example `90s`, `20m`); exit 124.
        #[arg(long, value_name = "DUR", value_parser = humantime::parse_duration)]
        timeout: Option<std::time::Duration>,
        /// Do not print Sluice's informational lines (errors are still printed).
        #[arg(short, long)]
        quiet: bool,
        /// The command and its arguments (after `--`).
        #[arg(
            required = true,
            num_args = 1..,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "CMD"
        )]
        argv: Vec<String>,
    },
    /// `PreToolUse` hook of the agent plugin: read the hook JSON on stdin, print a decision.
    ///
    /// Denies `sluice queue approve` (reserved to humans). Rewrites a gate command
    /// to `sluice run <gate> -v` and a heavy command (`[agent] heavy_commands`) to
    /// `sluice exec --label <L> -- <cmd>` by inserting that prefix into the
    /// unchanged original, answering `allow` or `ask` as the agent client's
    /// permission settings permit, or not at all. Otherwise denies raw gate
    /// commands as the shell guard does. Never fails closed: any error prints
    /// nothing and exits 0. Never contacts the daemon. `SLUICE_GUARD=0` disables
    /// it; `SLUICE_GUARD=deny` denies with the rewritten command as a suggestion.
    Guard {
        /// Accepted for the plugin's hook script (the hook JSON is always read on stdin).
        #[arg(long)]
        hook: bool,
    },
    /// Internal: hooks of the agent plugin.
    #[command(hide = true)]
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Shell shims: route heavy commands of agents that only have a shell through the queue.
    ///
    /// For agents other than Claude Code (Codex, Cursor, Aider, ...): small `sh`
    /// wrappers named after the heavy commands (`make`, `cargo`, `pnpm`, `docker`,
    /// ...) that, put first in an agent shell's PATH, run those commands as
    /// `sluice exec --label <words> -- <real binary> <args>` in agent sessions
    /// (agent detection or `SLUICE_SHIMS=1`) and exec the real binary unchanged
    /// otherwise (human shells, `SLUICE_SHIMS=0`, inside a Sluice job, subcommands
    /// that are not heavy such as `docker ps` or `npm install`).
    Shim {
        #[command(subcommand)]
        command: ShimCommand,
    },
    /// Start the MCP stdio server (spawned by the coding agent).
    ///
    /// Tools: `verify` (run gates through the cache and the daemon, blocking with
    /// progress notifications), `status`, `gate_list`, `logs`, and the merge-queue
    /// tools `queue_enqueue`, `queue_status`, `queue_approve`, `queue_retry`,
    /// `queue_cancel`. Logs go to stderr only. Register `sluice mcp` as a stdio MCP server in the agent's config.
    Mcp,
    /// Internal: write this process's pid to `<LEAF>/cgroup.procs`, raise its
    /// `oom_score_adj`, then exec the command (containment placement helper;
    /// runs the command even when the move or the adj fails).
    #[command(hide = true)]
    CgExec {
        /// Cgroup directory to move into (`-`: no move, only the adj).
        leaf: std::path::PathBuf,
        /// Raise this process's `oom_score_adj` to at least this value
        /// (never lowers it).
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u16).range(0..=1000))]
        oom_score_adj: Option<u16>,
        /// The command and its arguments (after `--`).
        #[arg(last = true, required = true)]
        argv: Vec<std::ffi::OsString>,
    },
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
    /// List the gates of the current repository.
    Gate {
        #[command(subcommand)]
        command: GateCommand,
    },
    /// Inspect or clear cache markers for the current repository.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Write a `.sluice.toml` that fits the repository and wire its git hooks.
    ///
    /// Detects the stack at the repository root and the commands the existing
    /// hooks (husky, lefthook, pre-commit, plain) already run, writes the gates
    /// and rewrites those hook lines to `sluice run --hook <hook> <gate>`.
    /// Nothing is staged or committed.
    Init {
        /// Overwrite an existing `.sluice.toml`.
        #[arg(long)]
        force: bool,
        /// Print the gates, the files and the diff of the hook edits; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// How existing hooks are handled: `rewire` edits the hook files (default);
        /// `entry` leaves them untouched and makes Sluice the hooks entry point of
        /// this clone (`sluice hooks on`).
        #[arg(long, value_enum, default_value_t = init::HooksMode::Rewire)]
        hooks: init::HooksMode,
    },
    /// Print hook integration snippets.
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// Make Sluice the git hooks entry point of this clone, without touching the repository.
    ///
    /// `on` gives the clone a hooks directory of its own (under its git directory) and points
    /// `core.hooksPath` at it through a file included by the last `[include]` of the clone's
    /// git config, so husky's own `core.hooksPath` writes do not displace it. Every hook runs
    /// `sluice hooks run`: when Sluice understands the repository's hook (husky, lefthook,
    /// pre-commit or plain) it runs the hook's commands one by one through the queue and the
    /// cache; otherwise it executes the hook untouched, with the same arguments, stdin,
    /// directory, environment and exit code. `status` shows which. `off` removes the include
    /// and the managed files.
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
    /// Show the log of a job.
    Logs {
        /// Job id; defaults to the most recent job.
        job_id: Option<String>,
        /// Only print the last N lines.
        #[arg(short = 'n', long)]
        lines: Option<usize>,
        /// Print the scheduler timeline instead of the log: one line per job
        /// (requested, queued, admitted, started, finished, wait reasons,
        /// demand, requesters).
        #[arg(long)]
        timeline: bool,
        /// With --timeline and no job id: the jobs queued within this period.
        #[arg(
            long,
            value_name = "DURATION",
            default_value = "10m",
            requires = "timeline"
        )]
        since: String,
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
enum ShimCommand {
    /// Write one wrapper per command of the heavy list into the shim directory.
    ///
    /// Default directory `$XDG_DATA_HOME/sluice/shims` or `~/.local/share/sluice/shims`
    /// (created with mode 0700; wrappers 0755, written atomically). Idempotent:
    /// existing Sluice shims are refreshed and shims of commands no longer in the list
    /// are removed. A file Sluice did not write is never overwritten unless --force.
    Install {
        /// Shim directory to use instead of the default.
        #[arg(long, value_name = "DIR")]
        dir: Option<std::path::PathBuf>,
        /// Replace files with a shim's name that Sluice did not write.
        #[arg(long)]
        force: bool,
    },
    /// Remove the shims (only files Sluice wrote; the directory only when empty).
    Uninstall {
        /// Shim directory to use instead of the default.
        #[arg(long, value_name = "DIR")]
        dir: Option<std::path::PathBuf>,
    },
    /// Show the shims, whether their directory is in PATH before the real binaries,
    /// which real binary each one runs, and what this shell would do.
    Status {
        /// Shim directory to use instead of the default.
        #[arg(long, value_name = "DIR")]
        dir: Option<std::path::PathBuf>,
        /// Print machine-readable JSON (`schema: 1`).
        #[arg(long)]
        json: bool,
    },
    /// Run <NAME> as a shim would (called by the wrappers).
    #[command(hide = true)]
    Exec {
        /// The command name (the shim's file name).
        name: String,
        /// Its arguments (after `--`).
        #[arg(last = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// `SessionStart` hook: install the shims, put them first in the agent's PATH through
    /// `$CLAUDE_ENV_FILE` and print the rules as additional context. Reads the hook JSON on
    /// stdin. Never fails: any error prints nothing and exits 0.
    SessionStart {
        /// Version of the plugin that calls this (informational).
        #[arg(long, value_name = "X.Y.Z")]
        plugin_version: String,
    },
}

#[derive(Debug, Subcommand)]
enum QueueCommand {
    /// Queue a committed branch for the next train.
    ///
    /// Defaults to the current branch of this worktree. Refuses a detached HEAD, a
    /// worktree with staged or unstaged changes to tracked files (untracked files do
    /// not matter), a branch that is already queued and a branch equal to the target.
    ///
    /// Warns (on stderr) when the target refuses direct pushes, as far as your `gh` login
    /// can tell or as an earlier refused push taught the queue; the branch is queued
    /// anyway.
    Enqueue {
        /// Branch to queue; defaults to the current branch.
        branch: Option<String>,
        /// Where the train lands: `<remote>/<branch>` (pushed), `<branch>` (a local
        /// branch, updated in place) or a bare name tried first as a branch of the
        /// configured remote and then as a local branch. Defaults to `[queue] remote`
        /// and `target`. Tags, raw SHAs, HEAD and checked-out local branches are refused.
        #[arg(long)]
        base: Option<String>,
        /// Human only. Build this job's train from the branch's own `.sluice.toml`
        /// (gates and `[queue]`) instead of the target's, to fix a configuration
        /// that makes every train fail. The train is alone, shows the file's diff and
        /// can only be approved by a human at the CLI. Agents are refused.
        #[arg(long)]
        trust_member_config: bool,
    },
    /// Show trains, their members, gates, conflicts and queued jobs.
    ///
    /// Inside a repository only its own trains and jobs are listed. With --json the
    /// `queue-status` payload is printed verbatim (it carries `schema: 1`). For a
    /// train that waits for its pull request (`awaiting_merge`) it first reads the
    /// pull request with your own `gh` (read-only, a few seconds at most, cached
    /// briefly) and tells the daemon, so a merged or closed pull request shows up.
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
    ///
    /// A train that lands through a pull request (`[queue] land = "pull-request"`) is
    /// pushed to `sluice/train/<id>` and this command then opens its pull request with
    /// your own `gh` (finding it when it already exists). Running it again on such a
    /// train whose pull request could not be opened finishes the job. Sluice never
    /// merges: the forge and you do.
    Approve {
        /// Train id, as shown by `sluice queue status`.
        train_id: String,
        /// Do not ask for confirmation (required without a terminal).
        #[arg(long)]
        yes: bool,
        /// Pull-request landing only: also arm the forge's auto-merge on the pull
        /// request (`gh pr merge --auto --merge --match-head-commit <S>`). The forge
        /// merges when its own rules are met; admin rights are never used.
        #[arg(long)]
        auto_merge: bool,
    },
    /// Re-queue a conflicted or failed job with the current head of its branch.
    ///
    /// Run it in the repository of the job after rebasing or fixing the branch; the
    /// branch head is re-read here.
    Retry {
        /// Job id, as shown by `sluice queue status`.
        job_id: String,
        /// Human only. Build the job's train from the branch's own `.sluice.toml`
        /// (see `enqueue --trust-member-config`). A plain retry goes back to the
        /// target's configuration.
        #[arg(long)]
        trust_member_config: bool,
    },
    /// Cancel a job that has not been pushed.
    ///
    /// A job that is pushing is only cancelled if `sluice queue reconcile` proves
    /// that nothing landed. A job whose train waits for its pull request
    /// (`awaiting_merge`) is withdrawn too: the train is dissolved, the pull request
    /// Sluice opened is closed with your own `gh` and the `sluice/train/<id>` branch
    /// it pushed is deleted (a pull request somebody else opened is left alone). The MCP
    /// `queue_cancel` is refused for such a job.
    Cancel {
        /// Job id, as shown by `sluice queue status`.
        job_id: String,
    },
    /// Resolve trains whose push outcome is unknown (`needs_reconcile`).
    ///
    /// Reads the remote and prints one line per train: the decision (landed, ready,
    /// queued, cancelled or refused) and the reason. It never pushes. For trains that
    /// land through a pull request (`awaiting_merge`) it also reads each pull request
    /// with your own `gh` and tells the daemon (merged, closed, head changed, ...); a
    /// human running it opens a missing pull request too.
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
    /// Internal: the gate reaper started by the daemon (kills the gates of a
    /// daemon that died).
    #[command(hide = true)]
    Reaper {
        /// Start the reaper in the background and print its pid.
        #[arg(long)]
        detach: bool,
        /// Pid of the daemon the reaper works for (informational, for `ps`).
        #[arg(long, value_name = "PID")]
        daemon: Option<u32>,
    },
    /// Internal: the holder process of `sluice-agents.scope` (containment). It
    /// blocks reading its stdin, which the daemon keeps open, and exits when
    /// the daemon is gone. With `--lifetime` (the session hook's stopgap when
    /// no daemon runs) it ignores stdin and exits after that many seconds.
    #[command(hide = true)]
    Holder {
        /// Exit after this many seconds instead of following the daemon.
        #[arg(long, value_name = "SECONDS")]
        lifetime: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
enum GateCommand {
    /// List the gates: those of `.sluice.toml` or, without one, the check units of the
    /// repository's git hooks (named `<manager>:<hook>#<n>`), and the hooks and units
    /// that are not gates, with the reason.
    List {
        /// Print machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
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
enum HooksCommand {
    /// Activate the entry point in this clone (idempotent).
    On,
    /// Remove the include and the managed files; the previous `core.hooksPath` applies again.
    Off,
    /// Show whether the entry point is on, what it delegates to and whether it is effective.
    Status {
        /// Print machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Run a hook of the repository through Sluice (called by the managed scripts).
    ///
    /// Runs the hook's commands through the queue when Sluice understands the hook; otherwise
    /// executes the repository's own hook with the same arguments and stdin, replacing this
    /// process. A hook that is not installed exits 0.
    #[command(hide = true)]
    Run {
        /// The clone's git common directory (written into the scripts).
        #[arg(long, value_name = "DIR")]
        common_dir: Option<std::path::PathBuf>,
        /// The hook's name.
        hook: String,
        /// The hook's arguments (after `--`).
        #[arg(last = true)]
        args: Vec<std::ffi::OsString>,
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

#[allow(clippy::too_many_lines, reason = "one arm per subcommand")]
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
        Command::Exec {
            weight,
            label,
            coalesce_key,
            timeout,
            quiet,
            argv,
        } => exec::exec(&exec::ExecArgs {
            weight,
            label,
            coalesce_key,
            timeout,
            quiet,
            argv,
        }),
        Command::Gate {
            command: GateCommand::List { json },
        } => gate_list::list(json),
        Command::Cache {
            command: CacheCommand::Status { all },
        } => cache::status(all),
        Command::Cache {
            command: CacheCommand::Clear { gate },
        } => cache::clear(gate.as_deref()),
        Command::Init {
            force,
            dry_run,
            hooks,
        } => init::run(force, dry_run, hooks),
        Command::Hook {
            command: HookCommand::Print { manager },
        } => hook_print(manager),
        Command::Hooks { command } => match command {
            HooksCommand::On => hookentry::on(),
            HooksCommand::Off => hookentry::off(),
            HooksCommand::Status { json } => hookentry::status(json),
            HooksCommand::Run {
                common_dir,
                hook,
                args,
            } => hookentry::run(&hook, &args, common_dir.as_deref()),
        },
        Command::Logs {
            job_id,
            timeline: true,
            since,
            ..
        } => logs::timeline(job_id.as_deref(), &since),
        Command::Logs { job_id, lines, .. } => logs::show(job_id.as_deref(), lines),
        Command::Guard { hook: _ } => guard::run_hook(),
        Command::Agent {
            command: AgentCommand::SessionStart { plugin_version },
        } => session_start::run_hook(&plugin_version),
        Command::Shim { command } => match command {
            ShimCommand::Install { dir, force } => shim::install(dir.as_deref(), force),
            ShimCommand::Uninstall { dir } => shim::uninstall(dir.as_deref()),
            ShimCommand::Status { dir, json } => shim::status(dir.as_deref(), json),
            ShimCommand::Exec { name, args } => shim::exec(&name, &args),
        },
        Command::Mcp => mcp(),
        Command::CgExec {
            leaf,
            oom_score_adj,
            argv,
        } => cg_exec::run(&leaf, oom_score_adj, &argv),
        Command::Daemon {
            command: Some(DaemonCommand::Stop { force }),
            ..
        } => daemon_cmd::stop(force),
        Command::Daemon {
            command: Some(DaemonCommand::Restart { debug, force }),
            ..
        } => daemon_cmd::restart(debug, force),
        Command::Daemon {
            command: Some(DaemonCommand::Reaper { detach, .. }),
            ..
        } => daemon_cmd::reaper(detach),
        Command::Daemon {
            command: Some(DaemonCommand::Holder { lifetime }),
            ..
        } => daemon_cmd::holder(lifetime),
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
            QueueCommand::Enqueue {
                branch,
                base,
                trust_member_config,
            } => queue::enqueue(branch.as_deref(), base.as_deref(), trust_member_config),
            QueueCommand::Status { json } => queue::status(json),
            QueueCommand::Approve {
                train_id,
                yes,
                auto_merge,
            } => queue::approve(&train_id, yes, auto_merge),
            QueueCommand::Retry {
                job_id,
                trust_member_config,
            } => queue::retry(&job_id, trust_member_config),
            QueueCommand::Cancel { job_id } => queue::cancel(&job_id),
            QueueCommand::Reconcile => queue::reconcile(),
        },
    }
}

/// `sluice mcp`: the MCP stdio server on a multi-threaded tokio runtime
/// (only this subcommand builds one; the `sluice run` hit path stays synchronous).
/// The gates the MCP server derives from the hooks of a repository without
/// `.sluice.toml`.
fn hook_gate_source() -> sluice_gate_core::hookgate::HookGateSource {
    std::sync::Arc::new(|paths, env| hookentry::gates::derive(paths, env, None))
}

fn mcp() -> ExitCode {
    let env = sluice_gate_core::config::process_env();
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
    match runtime.block_on(sluice_gate_mcp::serve(env, hook_gate_source())) {
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
            Ok(config) => {
                if let Some(warning) = doctor::queue_only_hook_warning(&config) {
                    eprintln!("sluice: warning: {warning}");
                }
                (String::new(), hook_gates(&config))
            }
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

#[cfg(test)]
mod exit_code_tests {
    use clap::CommandFactory;

    use super::Cli;

    /// Every exit code of the CLI: the number, the constant the program uses
    /// for it (where reachable from here) and a phrase `--help` must put next
    /// to it. The numbers are a compatibility promise (docs/compatibility.md).
    #[test]
    fn exit_codes_are_pinned_and_documented() {
        let table: [(u8, Option<u8>, &str); 11] = [
            (0, None, "pass"),
            (1, Some(crate::run::EXIT_ERROR), "Sluice error"),
            (2, None, "usage error"),
            (3, Some(crate::run::EXIT_FAIL_FAST), "agent_hook_policy"),
            (4, None, "queue"),
            (
                75,
                Some(sluice_gate_proto::client::EXIT_ALREADY_RUNNING),
                "daemon",
            ),
            (124, Some(crate::run::EXIT_TIMEOUT), "timeout"),
            (125, Some(crate::exec::EXIT_SLUICE), "exec"),
            (141, None, "closed pipe"),
            (143, Some(crate::run::EXIT_KILLED), "killed"),
            (7, None, "gate's own code"),
        ];
        let help = Cli::command().render_long_help().to_string();
        for (code, constant, phrase) in table {
            if let Some(used) = constant {
                assert_eq!(used, code, "the constant for exit code {code} changed");
            }
            // The gate's own code is listed as `N`, not as a number.
            let label = if code == 7 {
                "N".to_owned()
            } else {
                code.to_string()
            };
            let listed = help
                .lines()
                .any(|l| l.split_whitespace().next() == Some(label.as_str()) && l.contains(phrase));
            assert!(
                listed,
                "`sluice --help` must document exit code {label} ({phrase})"
            );
        }
    }
}
