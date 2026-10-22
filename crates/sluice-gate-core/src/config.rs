//! Configuration: the repository `.sluice.toml` and the per-user config.
//!
//! Both formats are parsed strictly: unknown fields are errors so typos
//! cannot silently change behaviour.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// File name of the repository configuration, at the repository root.
pub const REPO_CONFIG_FILE: &str = ".sluice.toml";

/// Snapshot of environment variables, injected so callers and tests never
/// touch the process environment.
pub type EnvMap = BTreeMap<String, String>;

/// The only config format version this build understands.
const SUPPORTED_VERSION: u32 = 1;

/// Maximum length of a gate name.
const MAX_GATE_NAME_LEN: usize = 64;

/// Maximum number of patterns in `paths` or `paths_ignore`.
const MAX_PATH_PATTERNS: usize = 64;

/// What a hook does when it misses the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookPolicy {
    /// Fail immediately with a retry hint.
    FailFast,
    /// Queue and wait for a slot.
    Wait,
    /// Run the gate right away, bypassing the queue.
    RunUnqueued,
}

fn config_err(path: &Path, message: impl Into<String>) -> Error {
    Error::Config {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn check_version(version: u32, path: &Path) -> Result<()> {
    if version == SUPPORTED_VERSION {
        Ok(())
    } else {
        Err(config_err(
            path,
            format!(
                "unsupported version {version}: `version` must be {SUPPORTED_VERSION} \
                 (supported versions: {SUPPORTED_VERSION})"
            ),
        ))
    }
}

// ---------------------------------------------------------------------------
// Repository config
// ---------------------------------------------------------------------------

/// Parsed `.sluice.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// Config format version (must be 1).
    pub version: u32,
    /// Repository-wide defaults.
    #[serde(default)]
    pub defaults: Defaults,
    /// Gates by name.
    #[serde(default)]
    pub gates: BTreeMap<String, Gate>,
    /// Merge queue settings (`[queue]`).
    #[serde(default)]
    pub queue: QueueConfig,
}

/// The `[queue]` table of `.sluice.toml` (merge queue, read from the base
/// commit by the daemon, never from a queued branch).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QueueConfig {
    /// Remote the default target lives on.
    pub remote: String,
    /// Default target branch; `None` = the remote's HEAD branch.
    pub target: Option<String>,
    /// Gates a train must pass; `None` = gates with a `pre-push` hook, else
    /// every gate.
    pub gates: Option<Vec<String>>,
    /// Maximum jobs combined into one train (1 disables batching).
    pub max_train: u32,
    /// Push `refs/sluice/landed/<sha>` atomically with the target.
    pub audit_ref: bool,
    /// Time limit for the push, including the repository's `pre-push` hook.
    #[serde(with = "humantime_serde")]
    pub push_timeout: Duration,
    /// Time limit of every network call of the queue other than the push
    /// (`fetch` of the base, `ls-remote` checks); on expiry git's process
    /// group is killed and the call counts as failed.
    #[serde(with = "humantime_serde")]
    pub network_timeout: Duration,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            remote: "origin".to_owned(),
            target: None,
            gates: None,
            max_train: 4,
            audit_ref: true,
            push_timeout: Duration::from_secs(600),
            network_timeout: DEFAULT_NETWORK_TIMEOUT,
        }
    }
}

/// Default `[queue] network_timeout`.
pub const DEFAULT_NETWORK_TIMEOUT: Duration = Duration::from_secs(60);

/// Why `remote` cannot be a `[queue] remote`: empty, starting with `-` (git
/// would read it as an option) or containing whitespace or control
/// characters. `None` when it is acceptable.
#[must_use]
pub fn invalid_remote_name(remote: &str) -> Option<&'static str> {
    if remote.is_empty() {
        Some("must not be empty")
    } else if remote.starts_with('-') {
        Some("must not start with '-'")
    } else if remote.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("must not contain whitespace or control characters")
    } else {
        None
    }
}

/// The `[defaults]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    /// Policy for hooks invoked by an agent on a cache miss.
    pub agent_hook_policy: HookPolicy,
    /// Policy for hooks invoked by a human on a cache miss.
    pub human_hook_policy: HookPolicy,
    /// Lines of log shown on failure; overrides the user config's `cache.tail_lines`.
    pub tail_lines: Option<usize>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            agent_hook_policy: HookPolicy::FailFast,
            human_hook_policy: HookPolicy::Wait,
            tail_lines: None,
        }
    }
}

fn default_weight() -> u32 {
    1
}

fn default_timeout() -> Duration {
    Duration::from_mins(30)
}

fn default_true() -> bool {
    true
}

/// One `[gates.<name>]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    /// Command run with `sh -c` in the worktree root.
    pub cmd: String,
    /// Budget units consumed while running (at least 1).
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Maximum execution time; queue wait does not count.
    #[serde(with = "humantime_serde", default = "default_timeout")]
    pub timeout: Duration,
    /// Hook names this gate belongs to.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Commands whose stdout is folded into the cache key.
    #[serde(default)]
    pub key_inputs: Vec<String>,
    /// Repo-relative files whose content is folded into the cache key.
    #[serde(default)]
    pub key_files: Vec<PathBuf>,
    /// Extra environment for the gate; also part of the key.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Gates that must pass first (reserved, unimplemented in v1).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// `false` makes the gate always run.
    #[serde(default = "default_true")]
    pub cache: bool,
    /// `gate` (default) or `queue-only`: a queue-only gate always runs under
    /// the budget and is never cached (its `cache` is forced to `false` at
    /// parse time). Needs sluice >= 0.3.0.
    #[serde(default)]
    pub mode: GateMode,
    /// Path scope: when set, the cache key hashes only the tree entries
    /// matching one of these patterns (plus `.sluice.toml`). Requires
    /// Sluice >= 0.3.0. See `kb/sluice/design/path-scoped-keys.md`.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Entries matching any of these patterns are removed from the scope.
    #[serde(default)]
    pub paths_ignore: Vec<String>,
}

/// The `mode` of a gate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateMode {
    /// A cached gate (the default).
    #[default]
    Gate,
    /// Always runs under the budget: no marker lookup, no marker, no miss
    /// record; simultaneous identical requests still coalesce.
    QueueOnly,
}

impl Gate {
    /// True for `mode = "queue-only"`.
    pub fn is_queue_only(&self) -> bool {
        self.mode == GateMode::QueueOnly
    }

    /// The command with surrounding whitespace trimmed and every whitespace
    /// run collapsed to a single space; this is what enters the cache key.
    pub fn normalized_cmd(&self) -> String {
        crate::key::normalize_cmd(&self.cmd)
    }
}

/// Why `pattern` cannot be a `paths`/`paths_ignore` entry, `None` when it
/// is acceptable (see `path-scoped-keys.md`, section 3).
fn invalid_path_pattern(pattern: &str) -> Option<&'static str> {
    if pattern.is_empty() {
        return Some("must not be empty");
    }
    if pattern.starts_with(':') {
        return Some("magic pathspecs are not supported; use paths_ignore");
    }
    if pattern.starts_with('!') {
        return Some("negation is not supported; use paths_ignore");
    }
    if pattern.starts_with('/') {
        return Some("must be relative to the repository root (no leading '/')");
    }
    if pattern.starts_with("./") {
        return Some("must not start with './'");
    }
    if pattern.contains('\\') {
        return Some("backslashes are not supported (no escapes)");
    }
    if pattern.chars().any(char::is_control) {
        return Some("must not contain NUL or control characters");
    }
    // A trailing '/' is allowed (`docs/`); any other empty, `.` or `..`
    // component would be normalised by git and is rejected instead.
    let body = pattern.strip_suffix('/').unwrap_or(pattern);
    for component in body.split('/') {
        match component {
            ".." => return Some("must not contain '..' components"),
            "." => return Some("must not contain '.' components"),
            "" => return Some("must not contain empty components ('//')"),
            _ => {}
        }
    }
    None
}

/// Validates one `paths`/`paths_ignore` list of gate `name`.
fn check_path_patterns(path: &Path, name: &str, field: &str, patterns: &[String]) -> Result<()> {
    if patterns.len() > MAX_PATH_PATTERNS {
        return Err(config_err(
            path,
            format!("gates.{name}.{field}: at most {MAX_PATH_PATTERNS} patterns are allowed"),
        ));
    }
    for (i, pattern) in patterns.iter().enumerate() {
        if let Some(why) = invalid_path_pattern(pattern) {
            return Err(config_err(
                path,
                format!("gates.{name}.{field}[{i}]: {why}"),
            ));
        }
    }
    Ok(())
}

fn valid_gate_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    first_ok
        && name.len() <= MAX_GATE_NAME_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

impl RepoConfig {
    /// Parse and validate `.sluice.toml` contents; `path` is used in errors.
    ///
    /// # Errors
    /// [`Error::Config`] on syntax errors, unknown fields or failed validation.
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let mut cfg: Self = toml::from_str(text).map_err(|e| config_err(path, e.to_string()))?;
        cfg.validate(path)?;
        cfg.check_queue_only(text, path)?;
        Ok(cfg)
    }

    /// Rejects `mode = "queue-only"` combined with an explicit `cache =
    /// true`, then forces `cache = false` on every queue-only gate so every
    /// cache path treats it as uncached.
    fn check_queue_only(&mut self, text: &str, path: &Path) -> Result<()> {
        if !self.gates.values().any(Gate::is_queue_only) {
            return Ok(());
        }
        // `cache` defaults to true, so only the raw document tells whether it
        // was written explicitly.
        let raw: toml::Table = toml::from_str(text).map_err(|e| config_err(path, e.to_string()))?;
        for (name, gate) in &mut self.gates {
            if !gate.is_queue_only() {
                continue;
            }
            let explicit_cache = raw
                .get("gates")
                .and_then(|g| g.get(name.as_str()))
                .and_then(|g| g.get("cache"))
                .and_then(toml::Value::as_bool);
            if explicit_cache == Some(true) {
                return Err(config_err(
                    path,
                    format!(
                        "gates.{name}: mode = \"queue-only\" never caches; remove cache = true"
                    ),
                ));
            }
            gate.cache = false;
        }
        Ok(())
    }

    /// Load `<repo_root>/.sluice.toml`.
    ///
    /// # Errors
    /// [`Error::ConfigNotFound`] when the file is missing, [`Error::Io`] when
    /// it cannot be read, [`Error::Config`] when it is invalid.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let path = repo_root.join(REPO_CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::ConfigNotFound(repo_root.to_path_buf()))
            }
            Err(e) => Err(Error::io("reading repository config", path, e)),
        }
    }

    /// Look a gate up by name.
    ///
    /// # Errors
    /// [`Error::UnknownGate`] when no such gate exists.
    pub fn gate(&self, name: &str) -> Result<&Gate> {
        self.gates
            .get(name)
            .ok_or_else(|| Error::UnknownGate(name.to_owned()))
    }

    /// Gates whose `hooks` contain `hook`, sorted by name.
    pub fn gates_for_hook(&self, hook: &str) -> Vec<(&str, &Gate)> {
        self.gates
            .iter()
            .filter(|(_, g)| g.hooks.iter().any(|h| h == hook))
            .map(|(n, g)| (n.as_str(), g))
            .collect()
    }

    fn validate(&self, path: &Path) -> Result<()> {
        check_version(self.version, path)?;
        if self.defaults.human_hook_policy == HookPolicy::FailFast {
            return Err(config_err(
                path,
                "defaults.human_hook_policy cannot be \"fail-fast\" (use \"wait\" or \"run-unqueued\")",
            ));
        }
        if self.queue.max_train < 1 {
            return Err(config_err(path, "queue.max_train must be >= 1"));
        }
        if let Some(why) = invalid_remote_name(&self.queue.remote) {
            return Err(config_err(path, format!("queue.remote {why}")));
        }
        if self.queue.network_timeout.is_zero() {
            return Err(config_err(path, "queue.network_timeout must be > 0"));
        }
        if let Some(gates) = &self.queue.gates {
            for gate in gates {
                if !self.gates.contains_key(gate) {
                    return Err(config_err(
                        path,
                        format!("queue.gates names an unknown gate \"{gate}\""),
                    ));
                }
            }
        }
        for (name, gate) in &self.gates {
            if !valid_gate_name(name) {
                return Err(config_err(
                    path,
                    format!(
                        "gates.{name}: invalid gate name (must match [a-z0-9][a-z0-9_-]* \
                         and be at most {MAX_GATE_NAME_LEN} characters)"
                    ),
                ));
            }
            if gate.cmd.trim().is_empty() {
                return Err(config_err(
                    path,
                    format!("gates.{name}.cmd must not be empty"),
                ));
            }
            if gate.weight < 1 {
                return Err(config_err(
                    path,
                    format!("gates.{name}.weight must be >= 1"),
                ));
            }
            for file in &gate.key_files {
                let escapes = file.is_absolute()
                    || file.components().any(|c| {
                        matches!(
                            c,
                            Component::ParentDir | Component::RootDir | Component::Prefix(_)
                        )
                    });
                if escapes {
                    return Err(config_err(
                        path,
                        format!(
                            "gates.{name}.key_files: {} must be relative to the repository \
                             root and contain no `..` components",
                            file.display()
                        ),
                    ));
                }
            }
            if let Some(paths) = &gate.paths {
                if paths.is_empty() {
                    return Err(config_err(
                        path,
                        format!(
                            "gates.{name}.paths: must not be empty (remove the field to use \
                             the whole tree)"
                        ),
                    ));
                }
                check_path_patterns(path, name, "paths", paths)?;
            }
            check_path_patterns(path, name, "paths_ignore", &gate.paths_ignore)?;
            for dep in &gate.depends_on {
                if dep == name {
                    return Err(config_err(
                        path,
                        format!("gates.{name}.depends_on cannot reference the gate itself"),
                    ));
                }
                if !self.gates.contains_key(dep) {
                    return Err(config_err(
                        path,
                        format!("gates.{name}.depends_on references unknown gate '{dep}'"),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// A commented `.sluice.toml` template for `sluice init`.
pub fn starter_repo_config() -> &'static str {
    r#"# Sluice repository configuration.
version = 1

[defaults]
# Behaviour when a hook invoked by an agent misses the cache.
agent_hook_policy = "fail-fast"   # fail-fast | wait | run-unqueued
# Behaviour when a hook invoked by a human misses the cache.
human_hook_policy = "wait"        # wait | run-unqueued
# Lines of log shown on failure.
tail_lines = 40

[gates.lint]
cmd = "npm run lint"
weight = 1
hooks = ["pre-commit"]

[gates.test]
cmd = "npm test"
weight = 4
timeout = "20m"
hooks = ["pre-commit", "pre-push"]
# Stdout of each command is folded into the cache key.
# key_inputs = ["node --version"]
# Ignored/untracked files whose content must invalidate the cache.
# key_files = [".env.test"]
# Extra environment for the gate (also part of the key).
# env = { NODE_ENV = "test" }
"#
}

// ---------------------------------------------------------------------------
// User config
// ---------------------------------------------------------------------------

/// What to do with a running job when its last subscriber disconnects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LastSubscriberGone {
    /// Cancel the job.
    Cancel,
    /// Let the job finish.
    Finish,
}

/// I/O scheduling class applied to gate processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IonicClass {
    /// Leave the I/O class untouched.
    None,
    /// Best-effort class.
    BestEffort,
    /// Idle class.
    Idle,
}

/// Parsed user config (`~/.config/sluice/config.toml`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UserConfig {
    /// Config format version (must be 1).
    pub version: u32,
    /// Daemon settings.
    pub daemon: DaemonConfig,
    /// Admission control (PSI) settings.
    pub admission: AdmissionConfig,
    /// Cache settings.
    pub cache: CacheConfig,
    /// Agent detection settings.
    pub agent_detection: AgentDetectionConfig,
    /// Scheduler settings.
    pub scheduler: SchedulerConfig,
    /// MCP server settings.
    pub mcp: McpConfig,
    /// Merge queue settings.
    pub queue: UserQueueConfig,
    /// Logging settings.
    pub debug: DebugConfig,
    /// Per-repository overrides keyed by repository path.
    pub repos: BTreeMap<PathBuf, RepoOverride>,
}

/// The `[queue]` table of the user config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UserQueueConfig {
    /// User-level kill switch for the merge queue runner.
    pub enabled: bool,
    /// Let the MCP `queue_approve` tool approve trains (off: humans only,
    /// through the CLI).
    pub allow_agent_approval: bool,
    /// Period of the runner's housekeeping tick.
    #[serde(with = "humantime_serde")]
    pub tick: Duration,
    /// Finished queue rows (landed or cancelled jobs, landed or dissolved
    /// trains, their markers and audit events) older than this are pruned at
    /// daemon start and once per hour.
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
}

impl Default for UserQueueConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_agent_approval: false,
            tick: Duration::from_secs(5),
            retention: Duration::from_hours(30 * 24),
        }
    }
}

/// The `[daemon]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonConfig {
    /// Total budget units; default `max(2, cpu_count - 1)`.
    pub budget: u32,
    /// Niceness (0..=19) for gate processes.
    pub nice: u8,
    /// I/O scheduling class for gate processes.
    pub ionice_class: IonicClass,
    /// I/O priority level (0..=7).
    pub ionice_level: u8,
    /// Days to keep logs.
    pub log_retention_days: u32,
    /// Exit after this much idle time; zero means never.
    #[serde(with = "humantime_serde")]
    pub idle_exit: Duration,
    /// Behaviour when the last subscriber of a job goes away.
    pub last_subscriber_gone: LastSubscriberGone,
}

/// The `[admission]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AdmissionConfig {
    /// Whether PSI-based admission is on.
    pub psi_enabled: bool,
    /// CPU `some avg10` percentage above which no job is admitted.
    pub cpu_some_avg10_max: f64,
    /// Memory `some avg10` percentage above which no job is admitted.
    pub memory_some_avg10_max: f64,
    /// How often PSI is sampled.
    #[serde(with = "humantime_serde")]
    pub psi_poll_interval: Duration,
    /// Starvation guard: once the queue head has waited this long while
    /// blocked only by PSI, one job is admitted anyway.
    #[serde(with = "humantime_serde")]
    pub psi_max_wait: Duration,
}

/// The `[cache]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    /// Days before a cache entry expires.
    pub ttl_days: u32,
    /// Lines of log kept for the tail shown on failure.
    pub tail_lines: usize,
}

/// The `[agent_detection]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentDetectionConfig {
    /// Any of these variables set and non-empty means "agent".
    pub env_any_set: Vec<String>,
    /// Any variable here equal to its value means "agent".
    pub env_any_equals: BTreeMap<String, String>,
}

/// The `[scheduler]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SchedulerConfig {
    /// Once the head of the queue has waited this long, nothing behind it
    /// starts until it does (head-of-line reservation).
    #[serde(with = "humantime_serde")]
    pub head_reserve_after: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            head_reserve_after: Duration::from_secs(30),
        }
    }
}

/// The `[mcp]` table (the `sluice mcp` server).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct McpConfig {
    /// Period of the progress notifications `verify` sends while a gate is
    /// queued or running (when the client supplied a `progressToken`).
    #[serde(with = "humantime_serde")]
    pub progress_interval: Duration,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            progress_interval: Duration::from_secs(15),
        }
    }
}

/// Default log filter directive.
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// The `[debug]` table: logging of the daemon, `sluice mcp` and the CLI.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DebugConfig {
    /// Log filter in `RUST_LOG` syntax (`info`, `debug`, `sluice_gate_daemon=debug`).
    /// `SLUICE_LOG` overrides it.
    pub level: String,
    /// Size in MiB above which `daemon.log` is rotated (at daemon start and
    /// by the daily pruning task); zero disables the rotation.
    pub daemon_log_max_mib: u64,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            level: DEFAULT_LOG_LEVEL.to_owned(),
            daemon_log_max_mib: 20,
        }
    }
}

/// The effective log filter directive: `SLUICE_LOG` (when set, non-empty and
/// valid) beats the configured `[debug] level`, which beats `info`.
#[must_use]
pub fn resolve_log_level(env: &EnvMap, configured: Option<&str>) -> String {
    let valid = |v: &str| !v.is_empty() && tracing_subscriber::EnvFilter::try_new(v).is_ok();
    env.get("SLUICE_LOG")
        .map(String::as_str)
        .filter(|v| valid(v))
        .or_else(|| configured.filter(|v| valid(v)))
        .unwrap_or(DEFAULT_LOG_LEVEL)
        .to_owned()
}

/// A `[repos."<path>"]` table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoOverride {
    /// Optional cap on the budget a single repository may hold.
    #[serde(default)]
    pub budget_share_max: Option<u32>,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            version: SUPPORTED_VERSION,
            daemon: DaemonConfig::default(),
            admission: AdmissionConfig::default(),
            cache: CacheConfig::default(),
            agent_detection: AgentDetectionConfig::default(),
            scheduler: SchedulerConfig::default(),
            mcp: McpConfig::default(),
            queue: UserQueueConfig::default(),
            debug: DebugConfig::default(),
            repos: BTreeMap::new(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(2, std::num::NonZero::get);
        Self {
            budget: u32::try_from(cpus.saturating_sub(1).max(2)).unwrap_or(u32::MAX),
            nice: 10,
            ionice_class: IonicClass::BestEffort,
            ionice_level: 7,
            log_retention_days: 7,
            idle_exit: Duration::ZERO,
            last_subscriber_gone: LastSubscriberGone::Cancel,
        }
    }
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            psi_enabled: true,
            cpu_some_avg10_max: 60.0,
            memory_some_avg10_max: 20.0,
            psi_poll_interval: Duration::from_secs(1),
            psi_max_wait: Duration::from_mins(5),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl_days: 30,
            tail_lines: 40,
        }
    }
}

impl Default for AgentDetectionConfig {
    fn default() -> Self {
        Self {
            env_any_set: vec!["CLAUDECODE".to_owned()],
            env_any_equals: BTreeMap::new(),
        }
    }
}

impl UserConfig {
    /// Location of the user config: `$SLUICE_CONFIG` when set and non-empty,
    /// else `<XDG config dir>/sluice/config.toml`.
    pub fn path(env: &EnvMap) -> PathBuf {
        if let Some(p) = env.get("SLUICE_CONFIG").filter(|p| !p.is_empty()) {
            return PathBuf::from(p);
        }
        let base = directories::BaseDirs::new().map_or_else(
            || {
                let home = env.get("HOME").map_or(".", String::as_str);
                PathBuf::from(home).join(".config")
            },
            |d| d.config_dir().to_path_buf(),
        );
        base.join("sluice").join("config.toml")
    }

    /// Load the user config; a missing file yields the defaults.
    ///
    /// # Errors
    /// [`Error::Config`] when the file is invalid, [`Error::Io`] when it
    /// exists but cannot be read.
    pub fn load(env: &EnvMap) -> Result<Self> {
        let path = Self::path(env);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::io("reading user config", path, e)),
        }
    }

    /// Parse and validate user config contents; `path` is used in errors.
    ///
    /// # Errors
    /// [`Error::Config`] on syntax errors, unknown fields or failed validation.
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let cfg: Self = toml::from_str(text).map_err(|e| config_err(path, e.to_string()))?;
        check_version(cfg.version, path)?;
        if cfg.daemon.budget < 1 {
            return Err(config_err(path, "daemon.budget must be >= 1"));
        }
        if cfg.daemon.nice > 19 {
            return Err(config_err(path, "daemon.nice must be in 0..=19"));
        }
        if cfg.daemon.ionice_level > 7 {
            return Err(config_err(path, "daemon.ionice_level must be in 0..=7"));
        }
        if cfg.admission.psi_poll_interval.is_zero() {
            return Err(config_err(
                path,
                "admission.psi_poll_interval must be greater than zero",
            ));
        }
        if cfg.admission.psi_max_wait.is_zero() {
            return Err(config_err(
                path,
                "admission.psi_max_wait must be greater than zero",
            ));
        }
        for (name, value) in [
            (
                "admission.cpu_some_avg10_max",
                cfg.admission.cpu_some_avg10_max,
            ),
            (
                "admission.memory_some_avg10_max",
                cfg.admission.memory_some_avg10_max,
            ),
        ] {
            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                return Err(config_err(
                    path,
                    format!("{name} must be a percentage between 0 and 100, got {value}"),
                ));
            }
        }
        if let Err(e) = tracing_subscriber::EnvFilter::try_new(&cfg.debug.level) {
            return Err(config_err(
                path,
                format!(
                    "debug.level {:?} is not a valid log filter ({e}); use for example \"info\" or \"debug\"",
                    cfg.debug.level
                ),
            ));
        }
        for (repo, over) in &cfg.repos {
            if over.budget_share_max == Some(0) {
                return Err(config_err(
                    path,
                    format!("repos.\"{}\".budget_share_max must be >= 1", repo.display()),
                ));
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC_REPO: &str = r#"
version = 1

[defaults]
agent_hook_policy = "fail-fast"
human_hook_policy = "wait"
tail_lines = 28

[gates.lint]
cmd = "npm run lint"
weight = 1
timeout = "5m"
hooks = ["pre-commit"]

[gates.test]
cmd = "npm test -- --maxWorkers=2"
weight = 4
timeout = "20m"
hooks = ["pre-commit", "pre-push"]
key_inputs = ["node --version"]
key_files  = [".env.test"]
env        = { NODE_ENV = "test" }
depends_on = ["lint"]
"#;

    fn p() -> &'static Path {
        Path::new("/r/.sluice.toml")
    }

    fn parse(text: &str) -> Result<RepoConfig> {
        RepoConfig::parse(text, p())
    }

    fn msg(text: &str) -> String {
        match parse(text) {
            Err(Error::Config { message, .. }) => message,
            other => panic!("expected Error::Config, got {other:?}"),
        }
    }

    fn gate_with(body: &str) -> String {
        format!("version = 1\n[gates.g]\n{body}\n")
    }

    #[test]
    fn parses_doc_example() {
        let c = parse(DOC_REPO).unwrap();
        assert_eq!(c.defaults.tail_lines, Some(28));
        assert_eq!(c.gates.len(), 2);
        let t = c.gate("test").unwrap();
        assert_eq!(t.weight, 4);
        assert_eq!(t.timeout, Duration::from_mins(20));
        assert_eq!(t.hooks, ["pre-commit", "pre-push"]);
        assert_eq!(t.key_inputs, ["node --version"]);
        assert_eq!(t.key_files, [PathBuf::from(".env.test")]);
        assert_eq!(t.env["NODE_ENV"], "test");
        assert_eq!(t.depends_on, ["lint"]);
        assert_eq!(c.gate("lint").unwrap().timeout, Duration::from_mins(5));
    }

    #[test]
    fn gate_defaults() {
        let c = parse(&gate_with("cmd = \"true\"")).unwrap();
        let g = c.gate("g").unwrap();
        assert_eq!(g.weight, 1);
        assert_eq!(g.timeout, Duration::from_mins(30));
        assert!(g.hooks.is_empty() && g.key_inputs.is_empty() && g.key_files.is_empty());
        assert!(g.env.is_empty() && g.depends_on.is_empty());
        assert!(g.cache);
        assert_eq!(g.mode, GateMode::Gate);
        assert!(!g.is_queue_only());
    }

    #[test]
    fn queue_only_mode() {
        let c = parse(&gate_with("cmd = \"make\"\nmode = \"queue-only\"")).unwrap();
        let g = c.gate("g").unwrap();
        assert!(g.is_queue_only());
        assert!(!g.cache, "a queue-only gate is never cached");
        let c = parse(&gate_with(
            "cmd = \"make\"\nmode = \"queue-only\"\ncache = false",
        ))
        .unwrap();
        assert!(c.gate("g").unwrap().is_queue_only());
        let m = msg(&gate_with(
            "cmd = \"make\"\nmode = \"queue-only\"\ncache = true",
        ));
        assert!(
            m.contains("gates.g: mode = \"queue-only\" never caches; remove cache = true"),
            "{m}"
        );
        let c = parse(&gate_with("cmd = \"make\"\nmode = \"gate\"\ncache = true")).unwrap();
        assert!(c.gate("g").unwrap().cache);
        assert!(msg(&gate_with("cmd = \"make\"\nmode = \"other\"")).contains("mode"));
    }

    #[test]
    fn repo_defaults() {
        let c = parse("version = 1").unwrap();
        assert_eq!(c.defaults.agent_hook_policy, HookPolicy::FailFast);
        assert_eq!(c.defaults.human_hook_policy, HookPolicy::Wait);
        assert_eq!(c.defaults.tail_lines, None);
        assert!(c.gates.is_empty());
    }

    #[test]
    fn zero_gates_valid_but_gate_fails() {
        let c = parse("version = 1").unwrap();
        assert!(matches!(c.gate("x"), Err(Error::UnknownGate(n)) if n == "x"));
    }

    #[test]
    fn version_required_and_checked() {
        assert!(msg("[gates]").contains("version"));
        let m = msg("version = 2");
        assert!(m.contains("version") && m.contains('1'));
    }

    #[test]
    fn gate_name_rules() {
        for bad in ["Lint", "-a", "_a", "a b", "a.b"] {
            let text = format!("version = 1\n[gates.\"{bad}\"]\ncmd = \"x\"\n");
            assert!(msg(&text).contains("gates."), "{bad}");
        }
        let long = "a".repeat(65);
        let text = format!("version = 1\n[gates.{long}]\ncmd = \"x\"\n");
        assert!(msg(&text).contains("gate name"));
        let ok = "a".repeat(64);
        let text = format!("version = 1\n[gates.{ok}]\ncmd = \"x\"\n");
        parse(&text).unwrap();
        parse("version = 1\n[gates.a0_b-c]\ncmd = \"x\"\n").unwrap();
    }

    #[test]
    fn empty_cmd_rejected() {
        assert!(msg(&gate_with("cmd = \"  \\t \"")).contains("gates.g.cmd"));
        assert!(msg(&gate_with("")).contains("cmd"));
    }

    #[test]
    fn weight_must_be_positive() {
        assert!(msg(&gate_with("cmd = \"x\"\nweight = 0")).contains("gates.g.weight"));
    }

    #[test]
    fn human_policy_cannot_fail_fast() {
        let m = msg("version = 1\n[defaults]\nhuman_hook_policy = \"fail-fast\"");
        assert!(m.contains("human_hook_policy"));
        parse("version = 1\n[defaults]\nhuman_hook_policy = \"run-unqueued\"").unwrap();
        parse("version = 1\n[defaults]\nagent_hook_policy = \"wait\"").unwrap();
    }

    #[test]
    fn invalid_policy_name() {
        assert!(
            msg("version = 1\n[defaults]\nagent_hook_policy = \"nope\"")
                .contains("agent_hook_policy")
        );
    }

    #[test]
    fn key_files_must_be_relative_and_inside() {
        for bad in ["/etc/passwd", "../x", "a/../../x"] {
            let m = msg(&gate_with(&format!("cmd = \"x\"\nkey_files = [\"{bad}\"]")));
            assert!(m.contains("key_files"), "{bad}");
        }
        parse(&gate_with(
            "cmd = \"x\"\nkey_files = [\"a/b.env\", \"./c\"]",
        ))
        .unwrap();
    }

    #[test]
    fn path_scope_fields_parse_and_default_to_unscoped() {
        let c = parse(&gate_with("cmd = \"x\"")).unwrap();
        let g = c.gate("g").unwrap();
        assert!(g.paths.is_none() && g.paths_ignore.is_empty());

        let c = parse(&gate_with(
            "cmd = \"x\"\npaths = [\"crates/**\", \"docs/\", \"Cargo.toml\"]\n\
             paths_ignore = [\"crates/**/*.md\"]",
        ))
        .unwrap();
        let g = c.gate("g").unwrap();
        assert_eq!(
            g.paths.as_deref(),
            Some(&["crates/**".to_owned(), "docs/".into(), "Cargo.toml".into()][..])
        );
        assert_eq!(g.paths_ignore, ["crates/**/*.md"]);

        // paths_ignore alone, and both with cache = false (accepted, no effect).
        parse(&gate_with("cmd = \"x\"\npaths_ignore = [\"docs\"]")).unwrap();
        parse(&gate_with(
            "cmd = \"x\"\ncache = false\npaths = [\"src\"]\npaths_ignore = [\"a\"]",
        ))
        .unwrap();
    }

    #[test]
    fn path_scope_validation_errors() {
        let full = match parse(&gate_with("cmd = \"x\"\npaths = []")) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("empty paths accepted"),
        };
        assert!(
            full.contains("/r/.sluice.toml")
                && full.contains(
                    "gates.g.paths: must not be empty (remove the field to use the whole tree)"
                ),
            "{full}"
        );
        // paths_ignore = [] is the default and therefore fine.
        parse(&gate_with("cmd = \"x\"\npaths_ignore = []")).unwrap();

        let cases = [
            ("", "must not be empty"),
            ("/src", "no leading '/'"),
            ("./src", "must not start with './'"),
            ("src/../x", "'..' components"),
            ("..", "'..' components"),
            ("src/./x", "'.' components"),
            ("src//x", "empty components"),
            (
                ":(exclude)src",
                "magic pathspecs are not supported; use paths_ignore",
            ),
            (
                ":!src",
                "magic pathspecs are not supported; use paths_ignore",
            ),
            ("!src", "negation is not supported; use paths_ignore"),
            ("src\\\\x", "backslashes are not supported"),
            ("src\\u0001x", "control characters"),
            ("src\\u0000x", "control characters"),
        ];
        for field in ["paths", "paths_ignore"] {
            for (bad, why) in cases {
                let m = msg(&gate_with(&format!(
                    "cmd = \"x\"\n{field} = [\"ok\", \"{bad}\"]"
                )));
                assert!(
                    m.starts_with(&format!("gates.g.{field}[1]: ")) && m.contains(why),
                    "{field} {bad:?}: {m}"
                );
            }
            let many: Vec<String> = (0..65).map(|i| format!("\"p{i}\"")).collect();
            let m = msg(&gate_with(&format!(
                "cmd = \"x\"\n{field} = [{}]",
                many.join(", ")
            )));
            assert!(m.contains("at most 64 patterns"), "{m}");
            let ok: Vec<String> = (0..64).map(|i| format!("\"p{i}\"")).collect();
            parse(&gate_with(&format!(
                "cmd = \"x\"\n{field} = [{}]",
                ok.join(", ")
            )))
            .unwrap();
        }
        // Trailing slash, wildcards, unicode and spaces are fine.
        parse(&gate_with(
            "cmd = \"x\"\npaths = [\"docs/\", \"src/**/*.rs\", \"a b/ñ?[0-9]\"]",
        ))
        .unwrap();
    }

    #[test]
    fn depends_on_rules() {
        let m = msg(&gate_with("cmd = \"x\"\ndepends_on = [\"g\"]"));
        assert!(m.contains("depends_on") && m.contains("itself"));
        let m = msg(&gate_with("cmd = \"x\"\ndepends_on = [\"zzz\"]"));
        assert!(m.contains("depends_on") && m.contains("zzz"));
    }

    #[test]
    fn queue_section_defaults_and_validation() {
        let c = parse("version = 1").unwrap();
        assert_eq!(c.queue.remote, "origin");
        assert_eq!(c.queue.max_train, 4);
        assert!(c.queue.audit_ref);
        assert_eq!(c.queue.push_timeout, Duration::from_secs(600));
        assert_eq!(c.queue.network_timeout, Duration::from_secs(60));
        assert!(c.queue.target.is_none() && c.queue.gates.is_none());
        let text = format!(
            "{}\n[queue]\nremote = \"up\"\ntarget = \"dev\"\ngates = [\"g\"]\nmax_train = 1\n\
             audit_ref = false\npush_timeout = \"2m\"\nnetwork_timeout = \"2s\"",
            gate_with("cmd = \"x\"")
        );
        let c = parse(&text).unwrap();
        assert_eq!(c.queue.target.as_deref(), Some("dev"));
        assert_eq!(c.queue.gates.as_deref(), Some(&["g".to_owned()][..]));
        assert_eq!(c.queue.push_timeout, Duration::from_secs(120));
        assert_eq!(c.queue.network_timeout, Duration::from_secs(2));
        assert!(msg("version = 1\n[queue]\nmax_train = 0").contains("max_train"));
        assert!(msg("version = 1\n[queue]\nremote = \" \"").contains("remote"));
        assert!(msg("version = 1\n[queue]\nnetwork_timeout = \"0s\"").contains("network_timeout"));
        // A remote reaches git as an argument: never an option, never split.
        let m = msg("version = 1\n[queue]\nremote = \"--upload-pack=touch /tmp/x #\"");
        assert!(m.contains("queue.remote must not start with '-'"), "{m}");
        let m = msg("version = 1\n[queue]\nremote = \"up stream\"");
        assert!(m.contains("whitespace"), "{m}");
        assert!(msg("version = 1\n[queue]\ngates = [\"nope\"]").contains("nope"));
        assert!(msg("version = 1\n[queue]\nbogus = 1").contains("bogus"));
    }

    #[test]
    fn user_queue_section() {
        let c = UserConfig::parse("version = 1", Path::new("u.toml")).unwrap();
        assert!(c.queue.enabled && !c.queue.allow_agent_approval);
        assert_eq!(c.queue.tick, Duration::from_secs(5));
        assert_eq!(c.queue.retention, Duration::from_hours(30 * 24));
        let c = UserConfig::parse(
            "version = 1\n[queue]\nenabled = false\nallow_agent_approval = true\ntick = \"1s\"\n\
             retention = \"7d\"",
            Path::new("u.toml"),
        )
        .unwrap();
        assert!(!c.queue.enabled && c.queue.allow_agent_approval);
        assert_eq!(c.queue.tick, Duration::from_secs(1));
        assert_eq!(c.queue.retention, Duration::from_hours(7 * 24));
    }

    #[test]
    fn unknown_fields_rejected() {
        assert!(msg("version = 1\nbogus = 1").contains("bogus"));
        assert!(msg("version = 1\n[defaults]\nbogus = 1").contains("bogus"));
        assert!(msg(&gate_with("cmd = \"x\"\nbogus = 1")).contains("bogus"));
    }

    #[test]
    fn durations() {
        for (s, secs) in [("90s", 90), ("5m", 300), ("1h", 3600)] {
            let c = parse(&gate_with(&format!("cmd = \"x\"\ntimeout = \"{s}\""))).unwrap();
            assert_eq!(c.gate("g").unwrap().timeout, Duration::from_secs(secs));
        }
        assert!(
            msg(&gate_with("cmd = \"x\"\ntimeout = \"soon\"")).contains("timeout")
                || msg(&gate_with("cmd = \"x\"\ntimeout = \"soon\"")).contains("duration")
        );
    }

    #[test]
    fn normalized_cmd_collapses_whitespace() {
        let c = parse(&gate_with("cmd = \"  npm   run\\t lint \\n\"")).unwrap();
        assert_eq!(c.gate("g").unwrap().normalized_cmd(), "npm run lint");
    }

    #[test]
    fn hook_filtering_is_sorted() {
        let text = "version = 1
[gates.zeta]
cmd = \"x\"
hooks = [\"pre-commit\"]
[gates.alpha]
cmd = \"x\"
hooks = [\"pre-commit\", \"pre-push\"]
[gates.mid]
cmd = \"x\"
";
        let c = parse(text).unwrap();
        let names: Vec<_> = c
            .gates_for_hook("pre-commit")
            .iter()
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert_eq!(c.gates_for_hook("pre-push").len(), 1);
        assert!(c.gates_for_hook("other").is_empty());
    }

    #[test]
    fn load_reads_file_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            RepoConfig::load(dir.path()),
            Err(Error::ConfigNotFound(_))
        ));
        std::fs::write(dir.path().join(REPO_CONFIG_FILE), DOC_REPO).unwrap();
        assert_eq!(RepoConfig::load(dir.path()).unwrap().gates.len(), 2);
    }

    #[test]
    fn starter_config_parses() {
        let c = parse(starter_repo_config()).unwrap();
        let lint = c.gate("lint").unwrap();
        assert_eq!(lint.weight, 1);
        assert_eq!(lint.hooks, ["pre-commit"]);
        let test = c.gate("test").unwrap();
        assert_eq!(test.weight, 4);
        assert_eq!(test.timeout, Duration::from_mins(20));
        assert_eq!(test.hooks, ["pre-commit", "pre-push"]);
    }

    // ---- user config ----

    const DOC_USER: &str = r#"
version = 1

[daemon]
budget = 8
nice = 10
ionice_class = "best-effort"
ionice_level = 7
log_retention_days = 7
idle_exit = "0s"
last_subscriber_gone = "cancel"

[admission]
psi_enabled = true
cpu_some_avg10_max = 60.0
memory_some_avg10_max = 20.0
psi_poll_interval = "1s"
psi_max_wait = "5m"

[cache]
ttl_days = 30
tail_lines = 40

[agent_detection]
env_any_set = ["CLAUDECODE", "SLUICE_AGENT"]
env_any_equals = { CLAUDE_CODE_ENTRYPOINT = "cli" }

[repos."/home/jose/Escritorio/Proyectos/foo"]
budget_share_max = 4
"#;

    fn user_err(text: &str) -> String {
        match UserConfig::parse(text, Path::new("/u.toml")) {
            Err(Error::Config { message, .. }) => message,
            other => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[test]
    fn user_defaults() {
        let c = UserConfig::default();
        let cpus = std::thread::available_parallelism().unwrap().get();
        assert_eq!(c.daemon.budget as usize, cpus.saturating_sub(1).max(2));
        assert_eq!(c.daemon.nice, 10);
        assert_eq!(c.daemon.ionice_class, IonicClass::BestEffort);
        assert_eq!(c.daemon.ionice_level, 7);
        assert_eq!(c.daemon.log_retention_days, 7);
        assert_eq!(c.daemon.idle_exit, Duration::ZERO);
        assert_eq!(c.daemon.last_subscriber_gone, LastSubscriberGone::Cancel);
        assert!(c.admission.psi_enabled);
        assert!((c.admission.cpu_some_avg10_max - 60.0).abs() < f64::EPSILON);
        assert!((c.admission.memory_some_avg10_max - 20.0).abs() < f64::EPSILON);
        assert_eq!(c.admission.psi_poll_interval, Duration::from_secs(1));
        assert_eq!(c.admission.psi_max_wait, Duration::from_mins(5));
        assert_eq!(c.cache.ttl_days, 30);
        assert_eq!(c.cache.tail_lines, 40);
        assert_eq!(c.agent_detection.env_any_set, ["CLAUDECODE"]);
        assert!(c.agent_detection.env_any_equals.is_empty());
        assert!(c.repos.is_empty());
    }

    #[test]
    fn empty_user_file_equals_defaults() {
        let c = UserConfig::parse("version = 1", Path::new("/u.toml")).unwrap();
        assert_eq!(c.daemon.budget, UserConfig::default().daemon.budget);
        assert_eq!(c.cache.ttl_days, 30);
    }

    #[test]
    fn parses_user_doc_example() {
        let c = UserConfig::parse(DOC_USER, Path::new("/u.toml")).unwrap();
        assert_eq!(c.daemon.budget, 8);
        assert_eq!(c.daemon.ionice_class, IonicClass::BestEffort);
        assert_eq!(
            c.agent_detection.env_any_set,
            ["CLAUDECODE", "SLUICE_AGENT"]
        );
        assert_eq!(
            c.agent_detection.env_any_equals["CLAUDE_CODE_ENTRYPOINT"],
            "cli"
        );
        let r = &c.repos[Path::new("/home/jose/Escritorio/Proyectos/foo")];
        assert_eq!(r.budget_share_max, Some(4));
    }

    #[test]
    fn user_enums_and_validation() {
        let c = UserConfig::parse(
            "version = 1\n[daemon]\nionice_class = \"idle\"\nlast_subscriber_gone = \"finish\"",
            Path::new("/u.toml"),
        )
        .unwrap();
        assert_eq!(c.daemon.ionice_class, IonicClass::Idle);
        assert_eq!(c.daemon.last_subscriber_gone, LastSubscriberGone::Finish);
        assert!(user_err("version = 1\n[daemon]\nnice = 20").contains("daemon.nice"));
        assert!(user_err("version = 1\n[daemon]\nbudget = 0").contains("daemon.budget"));
        assert!(user_err("version = 1\n[daemon]\nbogus = 1").contains("bogus"));
        assert!(user_err("version = 3").contains("version"));
    }

    #[test]
    fn scheduler_table() {
        let c = UserConfig::default();
        assert_eq!(c.scheduler.head_reserve_after, Duration::from_secs(30));
        let c = UserConfig::parse(
            "version = 1\n[scheduler]\nhead_reserve_after = \"2m\"",
            Path::new("/u.toml"),
        )
        .unwrap();
        assert_eq!(c.scheduler.head_reserve_after, Duration::from_mins(2));
        assert!(user_err("version = 1\n[scheduler]\nbogus = 1").contains("bogus"));
        assert!(
            user_err("version = 1\n[scheduler]\nhead_reserve_after = \"soon\"")
                .contains("head_reserve_after")
                || user_err("version = 1\n[scheduler]\nhead_reserve_after = \"soon\"")
                    .contains("duration")
        );
    }

    #[test]
    fn parses_mcp_table() {
        let c = UserConfig::parse("version = 1", Path::new("/u.toml")).unwrap();
        assert_eq!(c.mcp.progress_interval, Duration::from_secs(15));
        let c = UserConfig::parse(
            "version = 1\n[mcp]\nprogress_interval = \"30s\"\n",
            Path::new("/u.toml"),
        )
        .unwrap();
        assert_eq!(c.mcp.progress_interval, Duration::from_secs(30));
        assert!(user_err("version = 1\n[mcp]\nbogus = 1\n").contains("bogus"));
    }

    #[test]
    fn parses_debug_table() {
        let c = UserConfig::parse("version = 1", Path::new("/u.toml")).unwrap();
        assert_eq!(c.debug.level, "info");
        assert_eq!(c.debug.daemon_log_max_mib, 20);
        let c = UserConfig::parse(
            "version = 1\n[debug]\nlevel = \"sluice_gate_daemon=debug,info\"\ndaemon_log_max_mib = 5\n",
            Path::new("/u.toml"),
        )
        .unwrap();
        assert_eq!(c.debug.level, "sluice_gate_daemon=debug,info");
        assert_eq!(c.debug.daemon_log_max_mib, 5);
        assert!(user_err("version = 1\n[debug]\nbogus = 1\n").contains("bogus"));
        let e = user_err("version = 1\n[debug]\nlevel = \"a=b=c\"\n");
        assert!(e.contains("debug.level"), "{e}");
    }

    #[test]
    fn log_level_precedence() {
        let mut env = EnvMap::new();
        assert_eq!(resolve_log_level(&env, None), "info");
        assert_eq!(resolve_log_level(&env, Some("debug")), "debug");
        env.insert("SLUICE_LOG".into(), "trace".into());
        assert_eq!(resolve_log_level(&env, Some("debug")), "trace");
        env.insert("SLUICE_LOG".into(), String::new());
        assert_eq!(resolve_log_level(&env, Some("debug")), "debug");
        env.insert("SLUICE_LOG".into(), "a=b=c".into());
        assert_eq!(resolve_log_level(&env, Some("warn")), "warn");
    }

    #[test]
    fn user_path_honours_sluice_config() {
        let mut env = EnvMap::new();
        env.insert("SLUICE_CONFIG".into(), "/tmp/x/cfg.toml".into());
        assert_eq!(UserConfig::path(&env), PathBuf::from("/tmp/x/cfg.toml"));
        let default = UserConfig::path(&EnvMap::new());
        assert!(default.ends_with("sluice/config.toml"), "{default:?}");
    }

    #[test]
    fn user_load_missing_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = EnvMap::new();
        env.insert(
            "SLUICE_CONFIG".into(),
            dir.path().join("nope.toml").display().to_string(),
        );
        let c = UserConfig::load(&env).unwrap();
        assert_eq!(c.cache.ttl_days, 30);
    }

    #[test]
    fn user_load_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.toml");
        std::fs::write(&file, DOC_USER).unwrap();
        let mut env = EnvMap::new();
        env.insert("SLUICE_CONFIG".into(), file.display().to_string());
        assert_eq!(UserConfig::load(&env).unwrap().daemon.budget, 8);
    }

    #[test]
    fn psi_max_wait_parses_and_validates() {
        let c = UserConfig::parse(
            "version = 1\n[admission]\npsi_max_wait = \"90s\"\n",
            Path::new("/u.toml"),
        )
        .unwrap();
        assert_eq!(c.admission.psi_max_wait, Duration::from_secs(90));
        assert!(
            user_err("version = 1\n[admission]\npsi_max_wait = \"0s\"\n").contains("psi_max_wait")
        );
        assert!(
            user_err("version = 1\n[admission]\npsi_max_wait = \"soon\"\n")
                .contains("psi_max_wait")
                || user_err("version = 1\n[admission]\npsi_max_wait = \"soon\"\n")
                    .contains("invalid")
        );
    }
}

#[cfg(test)]
mod psi_threshold_tests {
    use super::UserConfig;
    use std::path::Path;

    #[test]
    fn psi_thresholds_must_be_percentages() {
        for bad in ["-1.0", "100.5", "nan"] {
            let text = format!("version = 1\n[admission]\ncpu_some_avg10_max = {bad}\n");
            let err = UserConfig::parse(&text, Path::new("u.toml")).unwrap_err();
            assert!(
                err.to_string().contains("cpu_some_avg10_max"),
                "{bad}: {err}"
            );
        }
        let ok = "version = 1\n[admission]\nmemory_some_avg10_max = 0.0\n";
        assert!(UserConfig::parse(ok, Path::new("u.toml")).is_ok());
    }
}
