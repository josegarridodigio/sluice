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
use crate::key::{self, CacheKey, KeyMaterial, KeyTree};
use crate::marker::{self, Marker, MarkerStore, MissRecord};
use crate::mode::CacheMode;
use crate::scope::{PathScope, ScopeCount, ScopeHash};
use crate::tree::{self, SnapshotFull, StagedEntries, TreeHash};

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
    build_context(paths, config, env, mode)
}

/// [`resolve_repo`] for a repository that may have no `.sluice.toml` (the hook
/// runner): a missing file means [`RepoConfig::implicit`]; an invalid one is
/// still an error.
///
/// # Errors
///
/// See [`resolve_repo`], except [`crate::Error::ConfigNotFound`].
pub fn resolve_repo_or_implicit(cwd: &Path, env: &EnvMap, mode: &CacheMode) -> Result<RepoContext> {
    let paths = git::discover(cwd, env)?;
    let config = match RepoConfig::load(&paths.toplevel) {
        Err(crate::Error::ConfigNotFound(_)) => RepoConfig::implicit(),
        other => other?,
    };
    build_context(paths, config, env, mode)
}

fn build_context(
    paths: git::RepoPaths,
    config: RepoConfig,
    env: &EnvMap,
    mode: &CacheMode,
) -> Result<RepoContext> {
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

impl RepoContext {
    /// A synthetic gate that is not in `.sluice.toml` (a unit of a repository
    /// hook). Its process environment is `env` without `CLAUDE*` variables
    /// plus `gate.env`; unless `keep_git_env`, the variables git sets for
    /// hooks (`GIT_DIR`, `GIT_INDEX_FILE`, ...) are removed too, like for a
    /// configured gate. A unit that edits or reads the index needs them.
    ///
    /// # Errors
    ///
    /// When git cannot list its local environment variables.
    pub fn select_unit(&self, name: &str, gate: Gate, keep_git_env: bool) -> Result<Resolved> {
        let mut child: EnvMap = if keep_git_env {
            self.env
                .iter()
                .filter(|(k, _)| !k.starts_with("CLAUDE"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        } else {
            git::child_env(&self.env)?
        };
        child.extend(gate.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        Ok(Resolved {
            env: self.env.clone(),
            paths: self.paths.clone(),
            config: self.config.clone(),
            user: self.user.clone(),
            gate_name: name.to_owned(),
            gate,
            probe_env: self.probe_env.clone(),
            scratch: self.scratch.clone(),
            store: self.store.clone(),
            ttl: self.ttl,
            child_env: OnceCell::from(child),
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
    Hit {
        /// The marker that matched.
        marker: Marker,
        /// The current root working tree. For a path-scoped gate it can
        /// differ from `marker.tree` (the tree of the earlier pass), so
        /// anything about "now" must use this one.
        tree: TreeHash,
    },
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
        /// Short machine-friendly code of the reason (`skip-worktree`,
        /// `disabled`, ...), recorded in `client.log`.
        code: &'static str,
    },
}

impl Lookup {
    fn off(reason: String, notice: Option<String>, code: &'static str) -> Self {
        Self::Off {
            reason,
            notice,
            code,
        }
    }
}

/// Looks the current tree up in the marker store. Prints nothing.
///
/// [`CacheMode::Bypass`] is treated like a disabled cache; callers handle
/// bypass before calling this.
pub fn lookup(r: &Resolved, mode: &CacheMode, extra: &[String]) -> Lookup {
    match mode {
        CacheMode::Disabled { reason } => {
            return Lookup::off(
                reason.clone(),
                Some(format!("cache disabled ({reason})")),
                "disabled",
            );
        }
        CacheMode::Bypass => {
            return Lookup::off(
                "SLUICE_DISABLE is set".to_owned(),
                Some("cache disabled (SLUICE_DISABLE is set)".to_owned()),
                "disabled",
            );
        }
        CacheMode::Enabled => {}
    }
    if !r.gate.cache {
        return Lookup::off(
            format!("gate '{}' has cache = false", r.gate_name),
            None,
            "gate-cache-off",
        );
    }
    let (tree, entries) = match tree::snapshot_entries(&r.paths, &r.probe_env, &r.scratch) {
        Ok(SnapshotFull::Known { root, entries }) => (root, entries),
        Ok(SnapshotFull::Unknown { reason }) => {
            return Lookup::off(
                format!("tree unknown: {reason}"),
                Some(format!("cache off for this tree: {reason}")),
                tree::unknown_code(&reason),
            );
        }
        Err(e) => {
            return Lookup::off(
                format!("snapshot failed: {e}"),
                Some(format!("cannot snapshot the tree ({e}); running uncached")),
                "snapshot-failed",
            );
        }
    };
    let (cmd, key) = match derive_key(r, key_tree(&r.gate, &tree, &entries), extra) {
        Ok(v) => v,
        Err(e) => {
            return Lookup::off(
                format!("key derivation failed: {e}"),
                Some(format!(
                    "cannot derive the cache key ({e}); running uncached"
                )),
                "key-failed",
            );
        }
    };
    match r.store.lookup(&key, r.ttl) {
        Ok(marker::Lookup::Hit(marker)) => Lookup::Hit { marker, tree },
        Ok(marker::Lookup::Miss(last_miss)) => Lookup::Miss {
            tree,
            key,
            cmd,
            last_miss,
        },
        Err(e) => Lookup::off(
            format!("cache read failed: {e}"),
            Some(format!("cannot read the cache ({e}); running uncached")),
            "cache-read-failed",
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

/// The command that identifies a call: `normalize_cmd(gate.cmd)` followed,
/// when `extra` is not empty, by one space and the extra arguments joined by
/// single spaces. An argument is wrapped in single quotes (inner `'` written
/// as `'\''`) only when it is empty or contains whitespace or quotes, so the
/// result is deterministic and calls with different arguments never share a
/// key. This string feeds the cache key and is stored in the marker.
pub fn key_command(gate: &Gate, extra: &[String]) -> String {
    crate::key::command_with_args(&gate.normalized_cmd(), extra)
}

/// The tree field of `gate`'s key for a snapshot: the root tree when the
/// gate is unscoped, else the hash of the entries its `paths` and
/// `paths_ignore` select (plus `.sluice.toml`). Used by every front-end and
/// by the merge queue, so they always agree.
pub fn key_tree(gate: &Gate, root: &TreeHash, entries: &StagedEntries) -> KeyTree {
    match PathScope::from_gate(gate) {
        None => KeyTree::Root(root.clone()),
        Some(scope) => KeyTree::Scoped(scope.hash(entries)),
    }
}

/// What a scoped gate selects in a snapshot (`cache status`, MCP `status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSummary {
    /// Selected and total entries.
    pub count: ScopeCount,
    /// The scope hash.
    pub hash: ScopeHash,
}

impl ScopeSummary {
    /// The ` [scoped: <n> of <total> files, <12 hex>]` suffix of a status line.
    #[must_use]
    pub fn status_suffix(&self) -> String {
        format!(
            " [scoped: {} of {} files, {}]",
            self.count.files,
            self.count.total,
            &self.hash.as_str()[..12]
        )
    }
}

/// [`ScopeSummary`] of `gate`, `None` for an unscoped gate.
pub fn scope_summary(gate: &Gate, entries: &StagedEntries) -> Option<ScopeSummary> {
    PathScope::from_gate(gate).map(|scope| ScopeSummary {
        count: scope.count(entries),
        hash: scope.hash(entries),
    })
}

/// The one key derivation shared by the front-ends ([`derive_key`]) and the
/// merge queue: `key_inputs` run and `key_files` read in `root_dir`;
/// `child_env` (the gate process environment, `gate.env` included) is only
/// requested when the gate has key inputs or files. Returns the key command
/// ([`key_command`]) and the key.
///
/// # Errors
///
/// When the environment cannot be built, or a `key_inputs` command or a
/// `key_files` entry cannot be read.
pub fn derive_key_at<'e>(
    gate: &Gate,
    gate_name: &str,
    tree: KeyTree,
    extra: &[String],
    root_dir: &Path,
    child_env: impl FnOnce() -> Result<&'e EnvMap>,
) -> Result<(String, CacheKey)> {
    let (key_inputs, key_files) = if gate.key_inputs.is_empty() && gate.key_files.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        key::collect_inputs(&gate.key_inputs, &gate.key_files, root_dir, child_env()?)?
    };
    let cmd = key_command(gate, extra);
    let material = KeyMaterial {
        tree,
        gate_name: gate_name.to_owned(),
        cmd: cmd.clone(),
        env: gate.env.clone(),
        key_inputs,
        key_files,
    };
    Ok((cmd, key::derive(&material)))
}

/// Derives the cache key of the resolved gate for `tree` (see [`key_tree`])
/// and the extra arguments of the call; returns the key command
/// ([`key_command`]) as well.
///
/// # Errors
///
/// When a `key_inputs` command or a `key_files` entry cannot be read.
pub fn derive_key(r: &Resolved, tree: KeyTree, extra: &[String]) -> Result<(String, CacheKey)> {
    derive_key_at(
        &r.gate,
        &r.gate_name,
        tree,
        extra,
        &r.paths.toplevel,
        || r.child_env(),
    )
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
        // The current tree, never `marker.tree`: on a scoped hit that is the
        // tree of the earlier pass.
        Lookup::Hit { marker, tree } => (Some(marker.key.clone()), Some(tree.clone())),
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
        let Lookup::Miss { tree, key, cmd, .. } = lookup(&r, &CacheMode::Enabled, &[]) else {
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
        assert!(matches!(
            lookup(&r, &CacheMode::Enabled, &[]),
            Lookup::Hit { .. }
        ));

        let disabled = CacheMode::Disabled {
            reason: "CI is set".into(),
        };
        let Lookup::Off { reason, notice, .. } = lookup(&r, &disabled, &[]) else {
            panic!("expected off");
        };
        assert_eq!(reason, "CI is set");
        assert_eq!(notice.as_deref(), Some("cache disabled (CI is set)"));

        let n = resolve(&cwd, &env, "n", &CacheMode::Enabled).unwrap();
        let state = lookup(&n, &CacheMode::Enabled, &[]);
        assert!(matches!(&state, Lookup::Off { notice: None, .. }));
        let m = run_material(&n, &state, &["-x".into()], Some("pre-commit")).unwrap();
        assert!(m.key.is_none() && m.committed_tree.is_none());
        assert_eq!(m.extra, vec!["-x".to_owned()]);

        assert!(resolve(&cwd, &env, "nope", &CacheMode::Enabled).is_err());
    }

    fn pass(r: &Resolved, key: CacheKey, tree: TreeHash, cmd: String) {
        r.store
            .write(&Marker {
                format: crate::CACHE_FORMAT_VERSION,
                key,
                tree,
                gate: r.gate_name.clone(),
                cmd,
                passed_at: crate::dirs::now_rfc3339(),
                duration_ms: 5,
                exit_code: 0,
                job_id: "j".into(),
                sluice_version: crate::SLUICE_VERSION.into(),
                committed_tree: None,
            })
            .unwrap();
    }

    /// Design section 9 case 18: a scoped hit after an out-of-scope edit
    /// reports the current root tree, never the marker's.
    #[test]
    fn scoped_hit_carries_the_current_tree() {
        let (tmp, env) = repo("[gates.s]\ncmd = \"true\"\npaths = [\"src\"]\n");
        let cwd = tmp.path().join("r");
        let r = resolve(&cwd, &env, "s", &CacheMode::Enabled).unwrap();
        let Lookup::Miss { tree, key, cmd, .. } = lookup(&r, &CacheMode::Enabled, &[]) else {
            panic!("expected a miss");
        };
        pass(&r, key.clone(), tree.clone(), cmd);

        std::fs::write(cwd.join("a.txt"), "edited outside the scope\n").unwrap();
        let state = lookup(&r, &CacheMode::Enabled, &[]);
        let Lookup::Hit {
            marker,
            tree: current,
        } = &state
        else {
            panic!("expected a scoped hit, got {state:?}");
        };
        assert_eq!(marker.tree, tree);
        assert_ne!(current, &tree, "the root tree moved");
        let m = run_material(&r, &state, &[], None).unwrap();
        assert_eq!(m.tree.as_ref(), Some(current));
        assert_eq!(m.key.as_ref(), Some(&key));

        // An in-scope edit misses.
        std::fs::create_dir(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("src/x.rs"), "x\n").unwrap();
        assert!(matches!(
            lookup(&r, &CacheMode::Enabled, &[]),
            Lookup::Miss { .. }
        ));
    }

    /// Pins the key of an unscoped gate on a fixed tree, computed with the
    /// 0.2 code before path scoping existed: markers written by 0.2 must keep
    /// hitting (design path-scoped-keys.md, section 6).
    #[test]
    fn unscoped_key_is_byte_identical_to_v0_2() {
        let (tmp, env) = repo(
            "[gates.t]\ncmd = \"cargo  test\"\nkey_inputs = [\"echo hi\"]\n\
             key_files = [\"missing.txt\"]\nenv = { A = \"1\" }\n",
        );
        let r = resolve(&tmp.path().join("r"), &env, "t", &CacheMode::Enabled).unwrap();
        let Lookup::Miss { tree, key, cmd, .. } = lookup(&r, &CacheMode::Enabled, &[]) else {
            panic!("expected a miss");
        };
        assert_eq!(cmd, "cargo test");
        assert_eq!(tree.as_str(), "ac55a1347b7ff3e9a45d482d556ffe9993bd54bf");
        assert_eq!(
            key.as_str(),
            "08351912397b2ba196e749077d406de3f70abc2b4b533d5c63f271c524431d46"
        );
    }

    #[test]
    fn extra_arguments_change_the_key_command() {
        let (tmp, env) = repo("[gates.t]\ncmd = \"cargo  test\"\n");
        let r = resolve(&tmp.path().join("r"), &env, "t", &CacheMode::Enabled).unwrap();
        let k = |a: &[&str]| {
            let a: Vec<String> = a.iter().map(|s| (*s).to_owned()).collect();
            key_command(&r.gate, &a)
        };
        assert_eq!(k(&[]), "cargo test");
        assert_eq!(
            k(&["a", "b c", "", "it's"]),
            "cargo test a 'b c' '' 'it'\\''s'"
        );
        assert_ne!(k(&["a"]), k(&["b"]));
    }
}
