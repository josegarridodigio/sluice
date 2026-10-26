//! The only place that spawns the `git` binary (ADR-0002).
//!
//! Every function takes the environment explicitly as an [`EnvMap`] so that
//! the caller's environment is never mutated and hook variables
//! (`GIT_DIR`, `GIT_INDEX_FILE`, ...) are handled on purpose.

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::tree::TreeHash;

/// A snapshot of an environment: variable name to value.
pub type EnvMap = BTreeMap<String, String>;

/// Oldest git release Sluice works with (`rev-parse --path-format` appeared in 2.31).
pub const MIN_GIT_VERSION: &str = "2.31";

/// Absolute locations of one repository, as seen from one worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPaths {
    /// Absolute, canonical worktree root.
    pub toplevel: PathBuf,
    /// Absolute per-worktree git dir.
    pub git_dir: PathBuf,
    /// Absolute git dir shared by all worktrees.
    pub common_dir: PathBuf,
    /// Absolute index path; honours an inherited `GIT_INDEX_FILE`.
    pub index_file: PathBuf,
    /// Absolute object directory; honours an inherited `GIT_OBJECT_DIRECTORY`.
    pub objects_dir: PathBuf,
}

/// Returns the value of `key` in `env` when it is set and non-empty.
fn non_empty<'a>(env: &'a EnvMap, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

/// Global options every git process Sluice spawns gets: no automatic
/// maintenance and no `gc --auto`. `commit`, `merge` and `fetch` (the merge
/// queue runs the last two against the user's object store) otherwise start
/// `git maintenance run --auto`, which may repack or prune the user's
/// objects and, since git 2.55, holds `objects/maintenance.lock` from a
/// detached child after git itself has exited. `-c` also reaches git's own
/// subprocesses (through `GIT_CONFIG_PARAMETERS`).
pub const NO_AUTO_MAINTENANCE: [&str; 4] = ["-c", "maintenance.auto=false", "-c", "gc.auto=0"];

/// A bare `git` command carrying [`NO_AUTO_MAINTENANCE`].
fn git_binary() -> Command {
    let mut cmd = Command::new("git");
    cmd.args(NO_AUTO_MAINTENANCE);
    cmd
}

/// Spawns git and collects its output. Spawn failure maps to [`Error::GitMissing`].
pub(crate) fn output(args: &[&str], cwd: &Path, env: &EnvMap, read_only: bool) -> Result<Output> {
    let mut cmd = git_binary();
    if read_only {
        cmd.arg("--no-optional-locks");
    }
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().map_err(Error::GitMissing)
}

/// Builds (without spawning) a `git` command with `args`, `cwd` and exactly
/// `env`, stdin from `/dev/null`. For callers that need to own the process
/// (the merge queue's push: own process group, timeout, pid tracking);
/// everything else uses [`run`] or [`run_output`].
#[must_use]
pub fn command(args: &[&str], cwd: &Path, env: &EnvMap) -> Command {
    let mut cmd = git_binary();
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null());
    cmd
}

/// Runs git and returns its raw [`Output`] whatever the exit status, for
/// callers that interpret the status themselves (`merge-base
/// --is-ancestor`, `merge`, `diff --quiet`).
///
/// # Errors
///
/// [`Error::GitMissing`] when git cannot be spawned.
pub fn run_output(args: &[&str], cwd: &Path, env: &EnvMap) -> Result<Output> {
    output(args, cwd, env, false)
}

/// Grace period between SIGTERM and SIGKILL when a deadline expires.
pub(crate) const DEADLINE_GRACE: Duration = Duration::from_secs(2);
/// Polling period while waiting for a process with a deadline.
pub(crate) const DEADLINE_POLL: Duration = Duration::from_millis(20);

/// Reads a pipe to the end on a helper thread; the result arrives on the
/// returned channel (never, when a stray grandchild keeps the pipe open).
fn drain(mut pipe: impl std::io::Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Sends `signal` to the process group `pgid` (best effort).
pub(crate) fn signal_group(pgid: u32, signal: nix::sys::signal::Signal) {
    if let Ok(raw) = i32::try_from(pgid)
        && raw > 1
    {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), signal);
    }
}

/// Like [`run_output`], for git calls that may hang on the network (the
/// merge queue's `fetch` and `ls-remote`): git runs in its own process group
/// and, when it has not exited after `timeout`, the whole group (git, its
/// transport helper, ssh, ...) gets SIGTERM, then SIGKILL after 2 s, and is
/// reaped.
///
/// # Errors
///
/// [`Error::GitMissing`] when git cannot be spawned, [`Error::GitTimeout`]
/// when the deadline expired.
pub fn run_output_deadline(
    args: &[&str],
    cwd: &Path,
    env: &EnvMap,
    timeout: Duration,
) -> Result<Output> {
    let mut cmd = command(args, cwd, env);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().map_err(Error::GitMissing)?;
    let out_rx = child.stdout.take().map(drain);
    let err_rx = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(DEADLINE_POLL),
            Ok(None) | Err(_) => break None,
        }
    };
    let Some(status) = status else {
        let pgid = child.id();
        signal_group(pgid, nix::sys::signal::Signal::SIGTERM);
        let grace = Instant::now() + DEADLINE_GRACE;
        while Instant::now() < grace && matches!(child.try_wait(), Ok(None)) {
            std::thread::sleep(DEADLINE_POLL);
        }
        // Whatever is left of the group (helpers that ignored SIGTERM).
        signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
        let _ = child.wait();
        return Err(Error::GitTimeout {
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            timeout,
        });
    };
    // A helper that outlived git may hold the pipes: collect what is there.
    let collect = |rx: Option<mpsc::Receiver<Vec<u8>>>| {
        rx.and_then(|rx| rx.recv_timeout(DEADLINE_GRACE).ok())
            .unwrap_or_default()
    };
    Ok(Output {
        status,
        stdout: collect(out_rx),
        stderr: collect(err_rx),
    })
}

/// Converts a failed [`Output`] into [`Error::GitCommand`].
#[must_use]
pub fn command_error(args: &[&str], out: &Output) -> Error {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // Prefer git's own complaint over hints and warnings.
    let mut picked: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.starts_with("fatal:") || l.starts_with("error:"))
        .collect();
    if picked.is_empty() {
        picked = lines;
    }
    picked.truncate(5);
    Error::GitCommand {
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        status: out.status.to_string(),
        stderr: picked.join("\n"),
    }
}

/// Runs git and returns raw stdout bytes; non-zero exit maps to [`Error::GitCommand`].
pub(crate) fn run_raw(args: &[&str], cwd: &Path, env: &EnvMap, read_only: bool) -> Result<Vec<u8>> {
    let out = output(args, cwd, env, read_only)?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(command_error(args, &out))
    }
}

/// Runs git with `args` in `cwd` with exactly `env`.
///
/// `read_only = true` adds `--no-optional-locks` before the subcommand so a
/// probe never takes `index.lock`.
///
/// # Errors
///
/// [`Error::GitMissing`] when git cannot be spawned, [`Error::GitCommand`]
/// when it exits with a non-zero status.
pub fn run(args: &[&str], cwd: &Path, env: &EnvMap, read_only: bool) -> Result<String> {
    let stdout = run_raw(args, cwd, env, read_only)?;
    Ok(String::from_utf8_lossy(&stdout).trim().to_owned())
}

/// Returns the `git --version` string.
///
/// # Errors
///
/// [`Error::GitMissing`] when git cannot be spawned, [`Error::GitCommand`]
/// when it fails.
pub fn version() -> Result<String> {
    let out = Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(Error::GitMissing)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(command_error(&["--version"], &out))
    }
}

/// Resolves `path` against `base` when it is relative.
fn absolute(base: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Derives the worktree a hook-style `GIT_DIR` (without `GIT_WORK_TREE`)
/// belongs to, from the filesystem only: the parent of a `.git` directory,
/// or the parent of the path named by a linked worktree's `gitdir` file.
fn worktree_of_git_dir(git_dir: &Path) -> Option<PathBuf> {
    let gitdir_file = git_dir.join("gitdir");
    if gitdir_file.is_file() {
        let target = std::fs::read_to_string(&gitdir_file).ok()?;
        let target = absolute(git_dir, target.trim());
        let parent = target.parent()?;
        return Some(
            parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf()),
        );
    }
    let canonical = git_dir.canonicalize().ok()?;
    if canonical.file_name()? == ".git" {
        return canonical.parent().map(Path::to_path_buf);
    }
    None
}

/// Returns the work tree to inject when `env` has `GIT_DIR` without
/// `GIT_WORK_TREE` (the plain-hook case), or `None` when git's own
/// discovery is already right (no `GIT_DIR`, or `core.worktree` configured).
fn hook_work_tree(cwd: &Path, env: &EnvMap) -> Result<Option<PathBuf>> {
    let Some(git_dir) = non_empty(env, "GIT_DIR") else {
        return Ok(None);
    };
    if non_empty(env, "GIT_WORK_TREE").is_some() {
        return Ok(None);
    }
    let configured = output(&["config", "--get", "core.worktree"], cwd, env, true)?;
    if configured.status.success() && !configured.stdout.trim_ascii().is_empty() {
        return Ok(None);
    }
    Ok(worktree_of_git_dir(&absolute(cwd, git_dir)))
}

/// Discovers the repository from `cwd` using `env` (a snapshot of the
/// caller's environment).
///
/// Works outside hooks, inside hooks where git exported `GIT_DIR` without
/// `GIT_WORK_TREE`, and with `GIT_INDEX_FILE` pointing at a temporary
/// index. Costs one `git rev-parse` spawn (two in the hook case).
///
/// # Errors
///
/// [`Error::NotARepository`] when git cannot find a repository with a work
/// tree from `cwd`, [`Error::GitMissing`] when git cannot be spawned.
pub fn discover(cwd: &Path, env: &EnvMap) -> Result<RepoPaths> {
    let mut probe = env.clone();
    if let Some(work_tree) = hook_work_tree(cwd, env)? {
        if let Some(git_dir) = non_empty(env, "GIT_DIR") {
            let git_dir = absolute(cwd, git_dir);
            probe.insert("GIT_DIR".into(), git_dir.to_string_lossy().into_owned());
        }
        probe.insert(
            "GIT_WORK_TREE".into(),
            work_tree.to_string_lossy().into_owned(),
        );
    }
    let args = [
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-dir",
        "--git-common-dir",
        "--git-path",
        "index",
        "--git-path",
        "objects",
    ];
    let stdout = match run(&args, cwd, &probe, true) {
        Ok(s) => s,
        Err(Error::GitCommand { stderr, .. }) => {
            if stderr.contains("path-format") {
                let found = version().unwrap_or_else(|_| "unknown".to_owned());
                return Err(Error::GitTooOld {
                    found,
                    required: MIN_GIT_VERSION,
                });
            }
            return Err(Error::NotARepository(cwd.to_path_buf()));
        }
        Err(e) => return Err(e),
    };
    let lines: Vec<&str> = stdout.lines().collect();
    let [toplevel, git_dir, common_dir, index_file, objects_dir] = lines.as_slice() else {
        return Err(Error::GitCommand {
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            status: "unexpected output".into(),
            stderr: stdout.clone(),
        });
    };
    Ok(RepoPaths {
        toplevel: PathBuf::from(toplevel),
        git_dir: PathBuf::from(git_dir),
        common_dir: PathBuf::from(common_dir),
        index_file: PathBuf::from(index_file),
        objects_dir: PathBuf::from(objects_dir),
    })
}

/// Builds the environment for Sluice's own git probes.
///
/// Starts from `env` and keeps `GIT_INDEX_FILE` and `GIT_DIR` (made
/// absolute from `paths`, since probes run from the toplevel). When
/// `GIT_DIR` is set without `GIT_WORK_TREE` (the hook case) it injects
/// `GIT_WORK_TREE`. The caller's environment is never changed.
///
/// Without `paths`, the work tree is derived from the filesystem only and a
/// relative `GIT_DIR` is resolved against `PWD` from `env`.
#[must_use]
pub fn probe_env(env: &EnvMap, paths: Option<&RepoPaths>) -> EnvMap {
    let mut probe = env.clone();
    let has_git_dir = non_empty(env, "GIT_DIR").is_some();
    let has_work_tree = non_empty(env, "GIT_WORK_TREE").is_some();
    let set = |probe: &mut EnvMap, key: &str, value: &Path| {
        probe.insert(key.to_owned(), value.to_string_lossy().into_owned());
    };
    match paths {
        Some(p) => {
            if has_git_dir {
                set(&mut probe, "GIT_DIR", &p.git_dir);
            }
            if has_git_dir || has_work_tree {
                set(&mut probe, "GIT_WORK_TREE", &p.toplevel);
            }
            if non_empty(env, "GIT_INDEX_FILE").is_some() {
                set(&mut probe, "GIT_INDEX_FILE", &p.index_file);
            }
            if non_empty(env, "GIT_OBJECT_DIRECTORY").is_some() {
                set(&mut probe, "GIT_OBJECT_DIRECTORY", &p.objects_dir);
            }
            if non_empty(env, "GIT_COMMON_DIR").is_some() {
                set(&mut probe, "GIT_COMMON_DIR", &p.common_dir);
            }
        }
        None => {
            if let (Some(git_dir), false) = (non_empty(env, "GIT_DIR"), has_work_tree) {
                let base = non_empty(env, "PWD").map_or_else(
                    || std::env::current_dir().unwrap_or_default(),
                    PathBuf::from,
                );
                if let Some(work_tree) = worktree_of_git_dir(&absolute(&base, git_dir)) {
                    set(&mut probe, "GIT_WORK_TREE", &work_tree);
                }
            }
        }
    }
    probe
}

/// The variables listed by `git rev-parse --local-env-vars`, cached per process.
static LOCAL_ENV_VARS: OnceLock<Vec<String>> = OnceLock::new();

/// Returns (and caches) the repository-local variables git knows about.
fn local_env_vars(env: &EnvMap) -> Result<&'static [String]> {
    if let Some(vars) = LOCAL_ENV_VARS.get() {
        return Ok(vars);
    }
    // Query with a neutral environment so a broken inherited GIT_DIR cannot fail it.
    let neutral: EnvMap = env
        .iter()
        .filter(|(k, _)| !k.starts_with("GIT_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let out = run(
        &["rev-parse", "--local-env-vars"],
        Path::new("/"),
        &neutral,
        true,
    )?;
    let vars = out.split_whitespace().map(str::to_owned).collect();
    Ok(LOCAL_ENV_VARS.get_or_init(|| vars))
}

/// Builds the environment for gate processes: `env` minus every variable
/// listed by `git rev-parse --local-env-vars` and minus every variable whose
/// name starts with `CLAUDE`.
///
/// # Errors
///
/// [`Error::GitMissing`] or [`Error::GitCommand`] when the variable list
/// cannot be obtained from git (only on the first call of the process).
pub fn child_env(env: &EnvMap) -> Result<EnvMap> {
    let local = local_env_vars(env)?;
    Ok(env
        .iter()
        .filter(|(k, _)| !k.starts_with("CLAUDE") && !local.iter().any(|l| l == *k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect())
}

/// Returns the tree id of `HEAD`, or `None` on an unborn branch.
///
/// `env` should be the probe environment from [`probe_env`].
///
/// # Errors
///
/// [`Error::GitMissing`] when git cannot be spawned, [`Error::GitCommand`]
/// when git fails for another reason or prints an invalid id.
pub fn head_tree(paths: &RepoPaths, env: &EnvMap) -> Result<Option<TreeHash>> {
    let args = ["rev-parse", "--verify", "--quiet", "HEAD^{tree}"];
    let out = output(&args, &paths.toplevel, env, true)?;
    if out.status.success() {
        let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        return TreeHash::try_from(id)
            .map(Some)
            .map_err(|e| Error::GitCommand {
                args: args.iter().map(|a| (*a).to_owned()).collect(),
                status: "invalid output".into(),
                stderr: e.to_string(),
            });
    }
    // `--verify --quiet` exits 1 silently when HEAD does not resolve (unborn branch).
    if out.status.code() == Some(1) && out.stderr.trim_ascii().is_empty() {
        return Ok(None);
    }
    Err(command_error(&args, &out))
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Throwaway repositories driven by the real git binary.

    use super::EnvMap;
    use std::path::Path;
    use std::process::Command;

    /// The test process environment without inherited `GIT_*` variables,
    /// isolated from user and system git config, with a fixed identity.
    pub fn base_env() -> EnvMap {
        let mut env: EnvMap = std::env::vars()
            .filter(|(k, _)| !k.starts_with("GIT_"))
            .collect();
        for (k, v) in [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_AUTHOR_NAME", "Test"),
            ("GIT_AUTHOR_EMAIL", "test@example.com"),
            ("GIT_COMMITTER_NAME", "Test"),
            ("GIT_COMMITTER_EMAIL", "test@example.com"),
        ] {
            env.insert(k.into(), v.into());
        }
        env
    }

    /// Runs git in `dir` with `env`, panicking on failure; returns trimmed stdout.
    ///
    /// Setup commits never start git's (detached) auto-maintenance, so tests
    /// that compare `.git/objects` before and after see only Sluice's effect.
    pub fn git_with(dir: &Path, env: &EnvMap, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(super::NO_AUTO_MAINTENANCE)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(env)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Runs git in `dir` with [`base_env`].
    pub fn git(dir: &Path, args: &[&str]) -> String {
        git_with(dir, &base_env(), args)
    }

    /// Creates a repository in `dir` with one committed file `a.txt`.
    pub fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-q", "-m", "initial"]);
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{base_env, git, init_repo};
    use super::*;

    #[test]
    fn discover_plain_repo_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        init_repo(&root);
        std::fs::create_dir(root.join("sub")).unwrap();
        let paths = discover(&root.join("sub"), &base_env()).unwrap();
        assert_eq!(paths.toplevel, root);
        assert_eq!(paths.git_dir, root.join(".git"));
        assert_eq!(paths.common_dir, root.join(".git"));
        assert_eq!(paths.index_file, root.join(".git/index"));
        assert_eq!(paths.objects_dir, root.join(".git/objects"));
    }

    #[test]
    fn discover_outside_repo_is_not_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = base_env();
        env.insert(
            "GIT_CEILING_DIRECTORIES".into(),
            tmp.path().display().to_string(),
        );
        let err = discover(tmp.path(), &env).unwrap_err();
        assert!(matches!(err, Error::NotARepository(_)), "{err:?}");
    }

    #[test]
    fn discover_honours_inherited_index_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        init_repo(&root);
        let mut env = base_env();
        env.insert("GIT_INDEX_FILE".into(), ".git/alt-index".into());
        let paths = discover(&root, &env).unwrap();
        assert_eq!(paths.index_file, root.join(".git/alt-index"));
        let probe = probe_env(&env, Some(&paths));
        assert_eq!(
            probe["GIT_INDEX_FILE"],
            root.join(".git/alt-index").display().to_string()
        );
    }

    #[test]
    fn head_tree_matches_and_is_none_when_unborn() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        let env = base_env();
        let paths = discover(&root, &env).unwrap();
        assert_eq!(head_tree(&paths, &env).unwrap(), None);
        std::fs::write(root.join("a"), "x").unwrap();
        git(&root, &["add", "a"]);
        git(&root, &["commit", "-q", "-m", "c"]);
        let expected = git(&root, &["rev-parse", "HEAD^{tree}"]);
        assert_eq!(head_tree(&paths, &env).unwrap().unwrap().as_str(), expected);
    }

    /// Every git Sluice spawns (here a `commit`, `merge` and `fetch` like
    /// the merge queue's) never starts git's automatic maintenance or gc in
    /// the user's repository.
    #[test]
    fn spawned_git_never_starts_auto_maintenance() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        init_repo(&root);
        let trace = root.join("trace.txt");
        let mut env = base_env();
        env.insert("GIT_TRACE".into(), trace.display().to_string());
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        run(&["add", "b.txt"], &root, &env, false).unwrap();
        run(&["commit", "-q", "-m", "second"], &root, &env, false).unwrap();
        run(
            &["checkout", "-q", "-b", "side", "HEAD~1"],
            &root,
            &env,
            false,
        )
        .unwrap();
        run(&["merge", "-q", "--no-edit", "main"], &root, &env, false).unwrap();
        let origin = root.display().to_string();
        run(&["fetch", "-q", &origin, "main"], &root, &env, false).unwrap();
        let mut cmd = command(&["commit", "-q", "--allow-empty", "-m", "e"], &root, &env);
        assert!(cmd.status().unwrap().success());
        let trace = std::fs::read_to_string(&trace).unwrap();
        assert!(trace.contains("built-in: git commit"), "{trace}");
        assert!(
            !trace.contains("maintenance") && !trace.contains(" gc "),
            "auto-maintenance was started:\n{trace}"
        );
    }

    #[test]
    fn run_maps_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(&["no-such-subcommand"], tmp.path(), &base_env(), true).unwrap_err();
        assert!(matches!(err, Error::GitCommand { .. }), "{err:?}");
        assert!(version().unwrap().starts_with("git version"));
    }

    #[test]
    fn deadline_kills_a_stalled_transport_group() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        init_repo(&root);
        // The transport helper (`ssh` replaced by a shell that sleeps) runs in
        // git's process group; the deadline must kill both.
        let pid_file = root.join("helper.pid");
        let mut env = base_env();
        env.insert(
            "GIT_SSH_COMMAND".into(),
            format!("echo $$ > '{}'; sleep 600 #", pid_file.display()),
        );
        let started = Instant::now();
        let err = run_output_deadline(
            &["ls-remote", "--", "ssh://stall.invalid/r.git"],
            &root,
            &env,
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert!(matches!(err, Error::GitTimeout { .. }), "{err:?}");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = (0..100).any(|_| {
            let dead = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err();
            if !dead {
                std::thread::sleep(Duration::from_millis(20));
            }
            dead
        });
        assert!(gone, "the helper {pid} survived the deadline");
        // A quick command finishes normally.
        let out = run_output_deadline(&["rev-parse", "HEAD"], &root, &env, Duration::from_secs(30))
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 41);
    }

    #[test]
    fn child_env_strips_repository_and_claude_variables() {
        let mut env = EnvMap::new();
        for key in [
            "GIT_INDEX_FILE",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_PREFIX",
            "GIT_OBJECT_DIRECTORY",
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
        ] {
            env.insert(key.into(), "x".into());
        }
        env.insert("PATH".into(), std::env::var("PATH").unwrap_or_default());
        env.insert("HOME".into(), "/home/test".into());
        let child = child_env(&env).unwrap();
        let keys: Vec<&str> = child.keys().map(String::as_str).collect();
        assert_eq!(keys, ["HOME", "PATH"]);
    }

    #[test]
    fn probe_env_injects_work_tree_without_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        init_repo(&root);
        let mut env = base_env();
        env.insert("GIT_DIR".into(), root.join(".git").display().to_string());
        let probe = probe_env(&env, None);
        assert_eq!(probe["GIT_WORK_TREE"], root.display().to_string());
        assert_eq!(probe["GIT_DIR"], env["GIT_DIR"]);
        assert!(!env.contains_key("GIT_WORK_TREE"));
    }
}
