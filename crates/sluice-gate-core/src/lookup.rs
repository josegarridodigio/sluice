//! The cache-hit check shared by `sluice run`, `sluice cache status` and
//! the MCP server: from (working directory, environment, gate name) to
//! either a fresh marker or the material of a daemon `run` request.
//!
//! Nothing here prints; every decision comes back with its reason so each
//! front-end can word it its own way.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Result;
use crate::config::{Gate, RepoConfig, UserConfig};
use crate::git::{self, EnvMap, RepoPaths};
use crate::key::{self, CacheKey, KeyMaterial};
use crate::marker::{self, Marker, MarkerStore, MissRecord};
use crate::mode::CacheMode;
use crate::tree::{self, Snapshot, TreeHash};

/// Environment variable carrying the Claude Code session id.
const SESSION_VAR: &str = "CLAUDE_CODE_SESSION_ID";

/// Repository-level context: everything resolved before a gate is chosen.
#[derive(Debug, Clone)]
pub struct RepoContext {
    /// Environment of the caller.
    pub env: EnvMap,
    /// Repository locations.
    pub paths: RepoPaths,
    /// Parsed `.sluice.toml`.
    pub config: RepoConfig,
    /// User config (defaults under [`CacheMode::Bypass`]).
    pub user: UserConfig,
    /// Environment for git probes.
    pub probe_env: EnvMap,
    /// Scratch root for private indexes.
    pub scratch: PathBuf,
    /// Marker store of the repository.
    pub store: MarkerStore,
    /// Marker time to live.
    pub ttl: Duration,
}

/// A repository context with one gate selected.
#[derive(Debug)]
pub struct Resolved {
    /// Environment of the caller.
    pub env: EnvMap,
    /// Repository locations.
    pub paths: RepoPaths,
    /// Parsed `.sluice.toml`.
    pub config: RepoConfig,
    /// User config (defaults under [`CacheMode::Bypass`]).
    pub user: UserConfig,
    /// Gate name.
    pub gate_name: String,
    /// Gate definition.
    pub gate: Gate,
    /// Environment for git probes.
    pub probe_env: EnvMap,
    /// Scratch root for private indexes.
    pub scratch: PathBuf,
    /// Marker store of the repository.
    pub store: MarkerStore,
    /// Marker time to live.
    pub ttl: Duration,
    /// Gate process environment, computed on first use (it costs a git spawn).
    child_env: OnceCell<EnvMap>,
}

/// Resolves repository, `.sluice.toml`, user config and derived environments.
///
/// Under [`CacheMode::Bypass`] the user config is not read (defaults).
///
/// # Errors
///
/// No repository, missing or invalid `.sluice.toml`, invalid user config,
/// no usable scratch directory.
pub fn resolve_repo(cwd: &Path, env: &EnvMap, mode: &CacheMode) -> Result<RepoContext> {
    let paths = git::discover(cwd, env)?;
    let config = RepoConfig::load(&paths.toplevel)?;
    let user = if *mode == CacheMode::Bypass {
        UserConfig::default()
    } else {
        UserConfig::load(env)?
    };
    let probe_env = git::probe_env(env, Some(&paths));
    let scratch = tree::scratch_root(env)?;
    let store = MarkerStore::new(&paths.common_dir);
    let ttl = Duration::from_hours(u64::from(user.cache.ttl_days) * 24);
    Ok(RepoContext {
        env: env.clone(),
        paths,
        config,
        user,
        probe_env,
        scratch,
        store,
        ttl,
    })
}

impl RepoContext {
    /// Selects a gate of `.sluice.toml`.
    ///
    /// # Errors
    ///
    /// [`crate::Error::UnknownGate`].
    pub fn select(&self, gate_name: &str) -> Result<Resolved> {
        let gate = self.config.gate(gate_name)?.clone();
        Ok(Resolved {
            env: self.env.clone(),
            paths: self.paths.clone(),
            config: self.config.clone(),
            user: self.user.clone(),
            gate_name: gate_name.to_owned(),
            gate,
            probe_env: self.probe_env.clone(),
            scratch: self.scratch.clone(),
            store: self.store.clone(),
            ttl: self.ttl,
            child_env: OnceCell::new(),
        })
    }
}

/// [`resolve_repo`] then [`RepoContext::select`].
///
/// The gate is checked before the user config is read, so an unknown gate
/// is reported even when the user config is broken.
///
/// # Errors
///
/// See [`resolve_repo`] and [`RepoContext::select`].
pub fn resolve(cwd: &Path, env: &EnvMap, gate_name: &str, mode: &CacheMode) -> Result<Resolved> {
    let paths = git::discover(cwd, env)?;
    let config = RepoConfig::load(&paths.toplevel)?;
    config.gate(gate_name)?;
    resolve_repo(cwd, env, mode)?.select(gate_name)
}

/// What the cache says about the current tree for one gate.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // one value per gate; boxing buys nothing
pub enum Lookup {
    /// A fresh marker matched: nothing to run.
    Hit(Marker),
    /// The tree is known and no marker matched.
    Miss {
        /// Snapshot of the working tree.
        tree: TreeHash,
        /// Cache key of this gate on that tree.
        key: CacheKey,
        /// Normalised command (stored in the marker).
        cmd: String,
        /// Last recorded miss for the key, if any.
        last_miss: Option<MissRecord>,
    },
    /// Caching is off for this invocation; the gate runs uncached.
    Off {
        /// Short reason, used as the `not cached` note.
        reason: String,
        /// Line a terminal front-end shows (without the `sluice: ` prefix);
        /// `None` when the condition is silent (`cache = false`).
        notice: Option<String>,
    },
}

impl Lookup {
    fn off(reason: String, notice: Option<String>) -> Self {
        Self::Off { reason, notice }
    }
}

/// Looks the current tree up in the marker store. Prints nothing.
///
/// [`CacheMode::Bypass`] is treated like a disabled cache; callers handle
/// bypass before calling this.
pub fn lookup(r: &Resolved, mode: &CacheMode) -> Lookup {
    match mode {
        CacheMode::Disabled { reason } => {
            return Lookup::off(reason.clone(), Some(format!("cache disabled ({reason})")));
        }
        CacheMode::Bypass => {
            return Lookup::off(
                "SLUICE_DISABLE is set".to_owned(),
                Some("cache disabled (SLUICE_DISABLE is set)".to_owned()),
            );
        }
        CacheMode::Enabled => {}
    }
    if !r.gate.cache {
        return Lookup::off(format!("gate '{}' has cache = false", r.gate_name), None);
    }
    let tree = match tree::snapshot(&r.paths, &r.probe_env, &r.scratch) {
        Ok(Snapshot::Known(tree)) => tree,
        Ok(Snapshot::Unknown { reason }) => {
            return Lookup::off(
                format!("tree unknown: {reason}"),
                Some(format!("tree unknown ({reason}); running uncached")),
            );
        }
        Err(e) => {
            return Lookup::off(
                format!("snapshot failed: {e}"),
                Some(format!("cannot snapshot the tree ({e}); running uncached")),
            );
        }
    };
    let (cmd, key) = match derive_key(r, &tree) {
        Ok(v) => v,
        Err(e) => {
            return Lookup::off(
                format!("key derivation failed: {e}"),
                Some(format!(
                    "cannot derive the cache key ({e}); running uncached"
                )),
            );
        }
    };
    match r.store.lookup(&key, r.ttl) {
        Ok(marker::Lookup::Hit(marker)) => Lookup::Hit(marker),
        Ok(marker::Lookup::Miss(last_miss)) => Lookup::Miss {
            tree,
            key,
            cmd,
            last_miss,
        },
        Err(e) => Lookup::off(
            format!("cache read failed: {e}"),
            Some(format!("cannot read the cache ({e}); running uncached")),
        ),
    }
}

impl Resolved {
    /// The caller's environment minus git hook and `CLAUDE*` variables,
    /// plus the gate's `env`. Computed once.
    ///
    /// # Errors
    ///
    /// When git cannot list its local environment variables.
    pub fn child_env(&self) -> Result<&EnvMap> {
        if let Some(env) = self.child_env.get() {
            return Ok(env);
        }
        let mut env = git::child_env(&self.env)?;
        env.extend(self.gate.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        Ok(self.child_env.get_or_init(|| env))
    }

    /// Lines of log shown on failure (repository override or user config).
    pub fn tail_lines(&self) -> usize {
        self.config
            .defaults
            .tail_lines
            .unwrap_or(self.user.cache.tail_lines)
    }
}

/// Owned copy of [`Resolved::child_env`].
///
/// # Errors
///
/// See [`Resolved::child_env`].
pub fn child_env(r: &Resolved) -> Result<EnvMap> {
    r.child_env().cloned()
}

/// Derives the cache key of the gate for `tree`; returns the normalised
/// command as well.
///
/// # Errors
///
/// When a `key_inputs` command or a `key_files` entry cannot be read.
pub fn derive_key(r: &Resolved, tree: &TreeHash) -> Result<(String, CacheKey)> {
    let (key_inputs, key_files) = if r.gate.key_inputs.is_empty() && r.gate.key_files.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        key::collect_inputs(
            &r.gate.key_inputs,
            &r.gate.key_files,
            &r.paths.toplevel,
            r.child_env()?,
        )?
    };
    let cmd = r.gate.normalized_cmd();
    let material = KeyMaterial {
        tree: tree.clone(),
        gate_name: r.gate_name.clone(),
        cmd: cmd.clone(),
        env: r.gate.env.clone(),
        key_inputs,
        key_files,
    };
    Ok((cmd, key::derive(&material)))
}

/// Tree of the index (the content being committed inside `pre-commit`).
pub fn committed_tree(r: &Resolved) -> Option<TreeHash> {
    tree::index_tree(&r.paths, &r.probe_env, &r.scratch)
        .ok()
        .and_then(|s| s.tree().cloned())
}

/// Everything a daemon `run` request carries except the caller kind and
/// the client pid; the front-ends convert it into the protocol type.
#[derive(Debug, Clone)]
pub struct RunMaterial {
    /// Repository locations.
    pub paths: RepoPaths,
    /// Gate name.
    pub gate_name: String,
    /// Gate definition as configured.
    pub gate: Gate,
    /// Execution timeout in milliseconds (`0` = none).
    pub timeout_ms: u64,
    /// Cache key, `None` when caching is off.
    pub key: Option<CacheKey>,
    /// Working tree, `None` when caching is off or the tree is unknown.
    pub tree: Option<TreeHash>,
    /// Committed tree (only inside `pre-commit` with a known tree).
    pub committed_tree: Option<TreeHash>,
    /// Command line to execute (trimmed, not whitespace-collapsed).
    pub cmd: String,
    /// Extra arguments appended to the command.
    pub extra: Vec<String>,
    /// Working directory of the gate.
    pub cwd: PathBuf,
    /// Gate process environment.
    pub child_env: EnvMap,
    /// Environment for the daemon's git probes.
    pub probe_env: EnvMap,
    /// Agent session id (`CLAUDE_CODE_SESSION_ID`), if any.
    pub session: Option<String>,
    /// Hook that triggered the run, if any.
    pub hook: Option<String>,
}

/// Builds the material of a daemon `run` request for `state`.
///
/// # Errors
///
/// See [`Resolved::child_env`].
pub fn run_material(
    r: &Resolved,
    state: &Lookup,
    extra: &[String],
    hook: Option<&str>,
) -> Result<RunMaterial> {
    let (key, tree) = match state {
        Lookup::Miss { tree, key, .. } => (Some(key.clone()), Some(tree.clone())),
        Lookup::Hit(m) => (Some(m.key.clone()), Some(m.tree.clone())),
        Lookup::Off { .. } => (None, None),
    };
    let committed_tree = if tree.is_some() && hook == Some("pre-commit") {
        committed_tree(r)
    } else {
        None
    };
    Ok(RunMaterial {
        paths: r.paths.clone(),
        gate_name: r.gate_name.clone(),
        gate: r.gate.clone(),
        timeout_ms: u64::try_from(r.gate.timeout.as_millis()).unwrap_or(u64::MAX),
        key,
        tree,
        committed_tree,
        // Trimmed, not whitespace-collapsed: newlines are significant to `sh -c`.
        cmd: r.gate.cmd.trim().to_owned(),
        extra: extra.to_vec(),
        cwd: r.paths.toplevel.clone(),
        child_env: r.child_env()?.clone(),
        probe_env: r.probe_env.clone(),
        session: r.env.get(SESSION_VAR).filter(|s| !s.is_empty()).cloned(),
        hook: hook.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{base_env, git, init_repo};

    fn repo(gates: &str) -> (tempfile::TempDir, EnvMap) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("r");
        std::fs::create_dir(&dir).unwrap();
        init_repo(&dir);
        std::fs::write(dir.join(".sluice.toml"), format!("version = 1\n{gates}")).unwrap();
        git(&dir, &["add", "-A"]);
        let mut env = base_env();
        env.insert(
            "SLUICE_CONFIG".into(),
            tmp.path().join("none.toml").display().to_string(),
        );
        env.insert("XDG_RUNTIME_DIR".into(), tmp.path().display().to_string());
        (tmp, env)
    }

    #[test]
    fn miss_then_hit_and_off_reasons() {
        let (tmp, env) =
            repo("[gates.t]\ncmd = \"true\"\n[gates.n]\ncmd = \"true\"\ncache = false\n");
        let cwd = tmp.path().join("r");
        let r = resolve(&cwd, &env, "t", &CacheMode::Enabled).unwrap();
        let Lookup::Miss { tree, key, cmd, .. } = lookup(&r, &CacheMode::Enabled) else {
            panic!("expected a miss");
        };
        r.store
            .write(&Marker {
                format: crate::CACHE_FORMAT_VERSION,
                key,
                tree,
                gate: "t".into(),
                cmd,
                passed_at: crate::dirs::now_rfc3339(),
                duration_ms: 5,
                exit_code: 0,
                job_id: "j".into(),
                sluice_version: crate::SLUICE_VERSION.into(),
                committed_tree: None,
            })
            .unwrap();
        assert!(matches!(lookup(&r, &CacheMode::Enabled), Lookup::Hit(_)));

        let disabled = CacheMode::Disabled {
            reason: "CI is set".into(),
        };
        let Lookup::Off { reason, notice } = lookup(&r, &disabled) else {
            panic!("expected off");
        };
        assert_eq!(reason, "CI is set");
        assert_eq!(notice.as_deref(), Some("cache disabled (CI is set)"));

        let n = resolve(&cwd, &env, "n", &CacheMode::Enabled).unwrap();
        let state = lookup(&n, &CacheMode::Enabled);
        assert!(matches!(&state, Lookup::Off { notice: None, .. }));
        let m = run_material(&n, &state, &["-x".into()], Some("pre-commit")).unwrap();
        assert!(m.key.is_none() && m.committed_tree.is_none());
        assert_eq!(m.extra, vec!["-x".to_owned()]);

        assert!(resolve(&cwd, &env, "nope", &CacheMode::Enabled).is_err());
    }
}
