---
title: Hook managers, part 1 - lefthook v2.2.2 source analysis
tags: [sluice, research, git-hooks, lefthook, hook-runner]
---

# Hook managers, part 1 - lefthook v2.2.2 source analysis

Question: can Sluice become the entry point of a repository's existing git
hooks (through a clone-local `core.hooksPath`) and run the repository's own
lefthook commands one by one as cacheable gates, without modifying the
repository? Alternative to evaluate: instead of re-implementing lefthook's
semantics, ask lefthook itself to run ONE command inside a Sluice job.

## Method and conventions

- Source: `refs/lefthook` at tag v2.2.2 (commit `eb4e504`), Go. Read-only
  analysis of the code; nothing was built or run. Indexed in Pando as
  `ref-lefthook`.
- Every `path:line` below is relative to `refs/lefthook/` and was read from
  the real file. A bare `file:line` such as `run.go:50` means
  `internal/command/run.go:50` when the directory is obvious from the
  section heading.
- Claims not proven by the code are marked **[inference]** (reasoned from the
  code plus general git behaviour) or **[docs]** (taken from `docs/` only).
- Real-world numbers (section 6) come from GitHub code search through `gh`;
  their bias is described there.
- Names: the user-facing CLI flag is `--command` (singular), not
  `--commands`; there is no `--commands` flag (`cmd/run.go:52`).

---

## 1. Install mechanics

### 1.1 What `lefthook install` does

Entry: `cmd/install.go:11-51` -> `internal/command/install.go:44` (`Install`).

1. Reads the config, or **creates a blank `lefthook.yml` in the worktree** if
   none exists (`install.go:104-115`, `createConfig` at `:146`). Sluice must
   never call install on a repo it must not modify.
2. For every configured `remotes:` entry runs `git clone --quiet --origin origin
   --depth 1 [--branch ref]` or `git pull`/fetch into
   `<git-dir>/info/lefthook-remotes/<name>` (`install.go:51-68`,
   `:611-645`; `internal/git/wrapper/clone.go`, `pull.go`;
   folder name `internal/git/remote.go:10`). That is network access at install
   time (and, see 1.3, at run time).
3. `installHooks` (`install.go:96-102`): first `ensureHooksPathUnset` (1.2),
   then `createHooksIfNeeded` (`install.go:289-365`).
4. `createHooksIfNeeded`, for every hook key in the merged config:
   - `cleanHook` (`internal/command/lefthook.go:122-157`): if the target hook
     file already exists and contains the substring `LEFTHOOK`
     (`lefthook.go:31` `hookContentFingerprint`, test at `:96-119`) it is
     simply removed; otherwise it is **renamed to `<hook>.old`**
     (`lefthook.go:150`), and if `<hook>.old` already exists the install fails
     with "can't rename ... file already exists" unless `--force`
     (`lefthook.go:138-147`).
   - Writes the hook script (mode 0755) from `internal/templates/hook.tmpl`
     (`lefthook.go:160-165`, `templates/templates.go:38-61`). Only names in
     `AvailableHooks` (`config/available_hooks.go:11-40`, 28 git hooks) are
     written, plus custom names when `install_non_git_hooks: true`
     (`install.go:324-326`).
   - If the config defines no hooks at all it writes a "ghost"
     `prepare-commit-msg` hook (`install.go:341-351`,
     `available_hooks.go:7`) used to trigger the auto-sync below.
   - Writes `<git-dir>/info/lefthook.checksum` with
     `<md5> <config-mtime-unix> <hook,hook,...>`
     (`install.go:353`, `:476-485`, `templates/templates.go:16,72-74`,
     name const `available_hooks.go:4`).
5. `ai:` section (beta): install also **writes agent settings files into the
   worktree**: `.claude/settings.json`, `.codex/hooks.json`,
   `.cursor/hooks.json`, `.github/hooks/lefthook.json`
   (`internal/config/ai.go:1-36`, `internal/command/install_ai.go:17-29,
   89-127`; `install.go:74-88`). Those hook commands call
   `lefthook run <hook>` for non-git hook names. **[docs]** `docs/configuration/ai.md`.

### 1.2 Where hooks are written; `core.hooksPath`

- The hooks directory is whatever git reports for
  `git rev-parse --path-format=absolute --show-toplevel --git-path hooks
  --git-path info --git-dir --git-common-dir` (`internal/git/wrapper/paths.go:7-47`,
  field `Hooks`). `--git-path hooks` is git's own resolution, so it follows
  `core.hooksPath` **[inference: git behaviour]**; lefthook's own comparison
  against `<root>/.git/hooks` (`install.go:517-520`) is consistent with that.
  In a linked worktree `Info`/`Hooks` come from the git-path (common dir for
  hooks/info) and `worktree` is true when `--git-dir != --git-common-dir`
  (`paths.go:40`); patch/stash names then include the worktree dir name
  (`wrapper/wrapper.go:140-152`, `wrapper/cache.go:28-34`,
  `store_stash.go:15`).
- `ensureHooksPathUnset` (`install.go:511-552`):
  - reads `git config --local core.hooksPath` and `git config --global
    core.hooksPath` (`wrapper/local_hooks_path.go`, `global_hooks_path.go`);
  - a local value equal to `<root>/.git/hooks` is ignored (`:520`), a global
    value always counts (`:521`);
  - if either is set and neither `--force` nor `--reset-hooks-path` is given:
    **prints a multi-line error and fails** (`:527-531`, text at `:555-590`);
  - `--force` / `-f` (`cmd/install.go:21-25`): installs **into the existing
    hooks path** (local wins over global) (`:544-550`); also overwrites
    `.old` files;
  - `--reset-hooks-path` / `-r` (`cmd/install.go:27-31`): runs
    `git config --local --unset-all core.hooksPath` **and**
    `git config --global --unset-all core.hooksPath` when set
    (`install.go:593-609`, `wrapper/unset_local_hooks_path.go`,
    `unset_global_hooks_path.go`). It touches the user's global git config.
  - Lefthook never *sets* `core.hooksPath` anywhere in the code (grep over
    `internal/`, `cmd/`: only get/unset).
  - Integration tests confirm: `core.hooksPath /different-path` makes plain
    `lefthook install` fail, `--reset-hooks-path` succeeds
    (`tests/integration/install_reset_hooks_path_issue_1429.txt`).

### 1.3 Auto-install on run

`lefthook run` calls `syncHooks` before running anything unless
`--no-auto-install` (`cmd/run.go:78-82`) or config `no_auto_install: true`
(`internal/command/run.go:100-111`).

- `syncHooks` (`install.go:159-232`):
  1. (if not the ghost hook) for each configured remote, `shouldRefetch`
     (`install.go:234-266`: `refetch: true`, `refetch_frequency: always|never|<duration>`,
     else based on `FETCH_HEAD` mtime; missing clone -> refetch) and then
     clone/pull: **network access on a normal hook run**; stale remote
     directories are deleted (`install.go:202-218`).
  2. `checkHooksSynchronized` (`install.go:401-460`): reads
     `<git-dir>/info/lefthook.checksum`; if the stored timestamp equals the
     **mtime of the main config file only** -> synchronized
     (`:450-452`); else compares the stored md5 with the md5 of the merged
     config dumped as compact JSON (`config/config.go:66-82`, `:454-459`).
  3. If not synchronized: `installHooks(cfg, hooks, InstallArgs{})` (no
     `--force`); on error only a warning `Skipping hook sync: ...` is logged
     and the run continues (`install.go:227-231`).
- Consequence with a local `core.hooksPath` set by Sluice (and no matching
  checksum file, e.g. lefthook was never installed in this clone, or the
  config changed): every `lefthook run` re-attempts install, prints the
  hooks-path error text (`install.go:529`) plus a warning, writes nothing
  (test: `internal/command/install_test.go:610-660`, "unsynchronized with
  local/global core.hooksPath set": hook files and ghost hook not created),
  and **continues running the hook**. If the checksum already matches, no
  attempt and no noise. **[inference]** on the noise condition, derived from
  the control flow above.
- With no `core.hooksPath`, an auto-sync from a Sluice-launched
  `lefthook run` would write `.git/hooks/*` and the checksum file and
  possibly fetch remotes. `--no-auto-install` avoids all of it, including the
  remote fetch (the whole `syncHooks` call is skipped, `run.go:100`). With
  auto-install off, remotes that were never cloned are silently absent from
  the config (`loader.go:287-329`, missing config file -> `continue`).

### 1.4 Pre-existing hooks

Non-lefthook hook files are renamed to `<hook>.old` on install
(`lefthook.go:122-157`) and are **never executed** by lefthook: the only other
references to `.old` are in uninstall, which renames them back
(`internal/command/uninstall.go:70-81`; grep of `internal/`, `cmd/` shows no
execution). So an existing husky/pre-commit/custom hook that lefthook
replaced is dead unless something else calls it.

`lefthook uninstall` removes lefthook-fingerprinted hooks (all hooks with
`--force`), restores `.old`, deletes the checksum and the remotes folder
(`uninstall.go:18-48`); `--remove-configs` deletes config files
(`cmd/uninstall.go:31`).

### 1.5 Package wrappers

- npm (`packaging/registries/npm/lefthook/postinstall.js:4-14`, same shape
  in `npm-bundled`): on `npm install`, runs `lefthook install` (no flags) in
  `INIT_CWD`, skipped when `CI` is truthy and `LEFTHOOK` is not; the exit
  status is ignored (`spawnSync` without a check). With a Sluice
  `core.hooksPath` this fails harmlessly (1.2).
- `bin/index.js` of the npm package only proxies the binary
  (`npm/lefthook/bin/index.js:1-17`).
- pip (`packaging/registries/pypi/lefthook/main.py:10-23`), gem and composer
  packages only ship/launch the binary; the grep of the pip packaging for
  install/hooks found no install step **[inference: only the listed files were
  inspected]**.
- Go: `go install` or `go get -tool` (`docs/installation/go.md`); the hook
  script probes `go tool lefthook` (`internal/templates/hook.tmpl:71-73`).

---

## 2. Runtime flow

### 2.1 The generated hook script

`internal/templates/hook.tmpl` (113 lines). On Linux it is a `#!/bin/sh`
script whose behaviour is, in order:

1. `LEFTHOOK_VERBOSE` = `1` or `true` -> `set -x` (`hook.tmpl:3-5`).
2. `LEFTHOOK` = `0` -> `exit 0` (`:7-9`). Only the literal `0` here; the
   binary additionally accepts `false` (`run.go:50`).
3. If the config has a top-level `rc:` key, sources that file
   (`:11-14`; `config.go:37`). The sourced environment is inherited by every
   job. A caller that bypasses the script (e.g. calls `lefthook run`
   directly) **loses the rc environment** **[inference]**.
4. `call_lefthook` finds a binary, trying in order (`:16-110`):
   `$LEFTHOOK_BIN`; the `lefthook:` config value (`:21-25`, `config.go:31`);
   `lefthook` on PATH (probed by `lefthook -h`); the absolute path of the
   binary that ran `install` (baked in via `os.Executable`,
   `templates.go:39-41,55`); then under `$(git rev-parse --show-toplevel)`
   the npm layouts `node_modules/lefthook-<os>-<cpu>/bin/lefthook`,
   `node_modules/@evilmartians/lefthook/bin/...`, `.../lefthook-installer/...`,
   `node_modules/lefthook/bin/index.js`, repeated for every `root:` found in
   the config (`install.go:367-397`, `hook.tmpl:44-70`); then `go tool
   lefthook`, `bundle exec lefthook`, `yarn lefthook`, `pnpm lefthook`, `swift
   package lefthook`, `mint run csjones/lefthook-plugin`, `uv run lefthook`,
   `composer exec lefthook`, `mise exec -- lefthook`, `devbox run lefthook`
   (`:74-100`).
5. If none is found: prints `Can't find lefthook in PATH` and **exits 0**
   (the `else` branch ends with `echo`, `:101-109`), unless
   `assert_lefthook_installed: true`, in which case `exit 1` (`:103-108`).
6. Last line: `call_lefthook run "<HookName>" "$@"` (`:113`). Git's hook
   arguments are forwarded verbatim; stdin is not touched by the script and
   passes through to lefthook.

Lefthook does **not** touch `PATH`: no `PATH` or `node_modules/.bin` string
appears anywhere in `internal/`, `cmd/` or `main.go` (grep). `node_modules`
matters only for locating the lefthook binary itself (above).

### 2.2 Environment variables honoured

| Variable | Where | Effect |
|---|---|---|
| `LEFTHOOK` | script `hook.tmpl:7`; `run.go:22,50` | `0` (script) or `0`/`false` (binary): hook skipped, exit 0 |
| `LEFTHOOK_BIN` | `hook.tmpl:18` | binary to call (script only) |
| `LEFTHOOK_VERBOSE` | `hook.tmpl:3`; `lefthook.go:25,50,167-174` | `set -x` / debug logs (any value except empty, `0`, `false`) |
| `LEFTHOOK_CONFIG` | `config/loader.go:131-151`; `install.go:123` | override main config path (relative -> repo root) |
| `LEFTHOOK_EXCLUDE` | `loader.go:494-496` | comma list appended to the hook's `exclude_tags`; matches tags **and** job/group names (`job.go:189-203`) |
| `LEFTHOOK_OUTPUT` | `run.go:23-24,85-92`; `cmd/lefthook.go:19` | overrides the `output:` config (what is printed) |
| `CI` | `run.go:223-254` | defaults for `fail_on_changes: ci|non-ci` and whether `fail_on_changes_diff` prints diffs (any value, even empty, counts: `LookupEnv`) |
| `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE` | `lefthook.go:26-28`; `exec/executor.go:33-50` | colours; lefthook injects `NO_COLOR=true` or `CLICOLOR_FORCE=1` into job environments |
| `GIT_DIR`, `GIT_INDEX_FILE` | `wrapper/pull.go`, `fetch.go`, `clone.go` | removed only for remote-config git calls; all other git calls (including `git diff --cached`) **inherit them** |

`LEFTHOOK=0` is **set** for lefthook's own helper processes
(`internal/system/command.go:36-62`: git calls, `skip: run:` checks, LFS) to
prevent recursion, but **not** for job commands: job processes get
`os.Environ()` plus the job's `env` (`exec/exec_unix.go:82-83`). A job that
itself runs `git commit` re-enters lefthook. **[inference]** for the practical
consequence; the code facts are cited.

### 2.3 How commands are executed

- Each command string runs as `sh -c <string>` (`exec/exec_unix.go:81`;
  `system/sh_unix.go` returns `"sh"`; Windows differs, out of scope).
- Working directory: `<repo root>/<root>` where `root` is the job/group
  `root:` (`job.go:137`, `exec_unix.go:82`). The cwd is **not** the cwd of
  the git invocation; it is `git rev-parse --show-toplevel` (`paths.go:8-9`).
- Environment: process env + `env:` map, values passed through
  `os.ExpandEnv` (`exec_unix.go:52`); group `env` and job `env` merge with the
  job winning (`scope.go:68-79`, `job.go:128-129`).
- Terminal: if lefthook's stdout is a TTY the command runs on a **pty** with
  the terminal's size (`exec_unix.go:94-100,131-141`); with `interactive` or
  `use_stdin` it gets direct stdio (`:86-93`); otherwise (pipe, daemon, CI)
  stdout and stderr are **merged into one buffer**, the child is placed in
  its own process group and a cancel kills the whole group
  (`:101-120`). Output is printed after the job finishes, except with
  `follow` or `interactive` (`run/controller/run.go:24-36`).
- A command with a very long file list is **split into several `sh -c`
  invocations** run one after another, sized by `MaxCmdLen()` = 130000 chars
  on Linux (`system/limits.go:9-23`, `replacer.go:175-225`,
  `exec_unix.go:70-76`).
- stdin: `system.NullReader` unless the job has `use_stdin: true`
  (`run/controller/run.go:19-22`). Real stdin is wrapped in a `CachedReader`
  (`controller.go:57`, `utils/cached_reader.go`) that replays the data to every
  `use_stdin` job, so several jobs can each read git's pre-push ref lines.
  `lefthook run --files-from-stdin` instead consumes stdin as a NUL-separated
  file list (`run.go:180-185,350-353`).
- `interactive: true` without a TTY on stdin opens `/dev/tty` as the input
  (`exec_unix.go:33-46`); ignored when `no_tty` / `--no-tty`
  (`job.go:139`).
- LFS: for `post-checkout`, `post-commit`, `post-merge`, `pre-push`
  (`internal/git/lfs.go:12-17`) lefthook first runs `git lfs <hook> <args>`
  (with cached stdin for pre-push) unless `skip_lfs` / `--skip-lfs`
  (`controller.go:78-82`, `run/controller/lfs.go:16-102`). A manual
  `lefthook run pre-push` with no extra args skips LFS (`lfs.go:22`).

### 2.4 Exit codes

- `main.go:11-18`: any returned error -> exit 1 (message printed unless
  empty). There are no other exit codes in the `run` path.
- A job failure (non-zero exit, timeout, invalid job) -> result `failure` ->
  hook error with an empty message -> exit 1 (`run.go:281-285`,
  `job.go:146-152`; timeout text `job.go:147-149`).
- Skipped jobs (no files, `skip`/`only`, tags) are `skip` results -> exit 0.
- Hook name not in config but a known git hook -> returns nil, exit 0
  (`run.go:162-170`); unknown name -> error exit 1 (`:169`).
- Missing config -> warning, exit 0 (`run.go:61-70`).
- `min_version` newer than the binary -> error exit 1 (`run.go:72-74,355-373`).
- `piped: true` + `parallel: true` on the same hook -> error exit 1
  (`run.go:27,172-174`).
- Hook `fail_text` only changes the printed text (`job.go:151`).
- `lefthook run` does **not** pass through the child's exit code; it is
  always 0 or 1 **[inference from `main.go` + `job.go:151`]**.

---

## 3. Configuration semantics

### 3.0 Config discovery, merging and the final job list

- Main config names: `lefthook`, `.lefthook`, `.config/lefthook` with
  extensions `.yml .yaml .json .jsonc .toml` (`config/loader.go:36-51`,
  first match wins, `loadFirst` `:89-102`; `LEFTHOOK_CONFIG` replaces the
  search, `:131-151`).
- Layers (`loader.go:154-201`, `:378-418`): main -> main `extends` (paths
  relative to the repo root, globs allowed, recursive, cycle = error;
  `:333-376`) -> `remotes` (read from the already-cloned
  `<git-dir>/info/lefthook-remotes/...`, `:287-329`) -> `lefthook-local.*` /
  `.lefthook-local.*` / `.config/lefthook-local.*` (`:36,185`) -> local
  `extends` (`:192-198`). The secondary layers are merged **over** the main
  config (`addHook` loads the override on top: `:484`; `main.Merge(secondary)`
  `:409`). The `lefthook:` key from remotes is ignored (`:181`).
  `lefthook-local` is normally untracked **[docs]** `docs/usage/features/local.md`,
  so it is a per-clone input to the behaviour.
- Job merge: jobs with the same `name` are merged field-wise; `{cmd}` in the
  overriding `run` is replaced by the base `run` (`loader.go:448-470,
  612-700`); `setup` lists are concatenated (`:451`).
- Hook set: all `AvailableHooks` present in config plus any key containing
  `commands|scripts|jobs` (custom hooks) (`loader.go:381-406`, regexp `:35`).
- Final order (`run.go:136-140`): `jobs` (as written) -> `commands` ->
  `scripts`. `commands` and `scripts` come from maps and are sorted:
  by `priority` (ascending, jobs without priority last), else by leading
  number in the name, else alphabetically (`config/command.go:36-95`,
  `config/script.go:29-106`). `jobs` is a list and keeps its order.
  `--command` values are **appended to `--job` values** (`run.go:140`): both
  match job names.

### 3.1 Feature table

"Per-command runner" = Sluice runs one command itself. "Reproducible" means
the behaviour can be reproduced exactly knowing the config; "delegate" means
re-implementing it is risky and it is safer to let lefthook do it.

| Feature | Exact behaviour (code) | Per-command runner |
|---|---|---|
| `parallel` (hook/group) | all jobs in goroutines, results collected in completion order (`controller.go:103-105,134-157`); group-level in `job.go:69-70` | Scheduling only; reproducible. Not expressible when running a single command. |
| `piped` | sequential; after the first **failure** later jobs are skipped "broken pipe" (`controller.go:159-180`). Without `piped`, sequential mode **keeps running after a failure** and only the final exit is 1 (`:171-176`) | Reproducible (stop on first failure); but "run all and report" is the default, not fail-fast. |
| `parallel` + `piped` | config error (`run.go:172-174`) | - |
| `follow` | stream output live instead of buffered (`run/controller/run.go:24-36`) | Cosmetic. |
| `commands` / `scripts` / `jobs` | unified to jobs (see 3.0). A job needs exactly one of `run`, `script`, `group` else failure (`job.go:21-31`) | Reproducible. |
| `scripts` | file looked up at `<source_dir>/<hook>/<script>` in `.lefthook`, `.lefthook-local`, `.config/lefthook`, `.config/lefthook-local` and each remote's `source_dir` (`run.go:197-221`, `build_script.go:49-94`). **It `chmod 0751`s a non-executable script file** (`build_script.go:62-72`), i.e. it can modify file modes in the repo. Runner + args + git args are appended (`:79-93`) | Reproducible but touches the tree; the script file content must be in the cache key. |
| `jobs` / `group` | nested; group has own `root`, `parallel`, `piped`, jobs; glob/tags/root/files/exclude/file_types/env **accumulate down** (`scope.go:51-80`, concat for glob/tags/file_types, first-non-blank for root/files) | Reproducible. |
| `run` templates | `{staged_files}` `{all_files}` `{push_files}` `{files}` (custom `files:` command), `{0}` = all git args joined, `{1}..{n}`, `{lefthook_job_name}` (shell-quoted), plus user `templates:` map (`replacer.go:31-96`, `config/files.go:5-8`, `build_command.go:98`). File lists are shell-quoted per name (`replacer.go:228-243`) and respect surrounding quotes (`:267-294`). `{staged_files}` + `{push_files}` in one `run` is an error (`files.go:12-14`) | Reproducible only if staged/push/all file lists, filters and quoting are reproduced exactly; otherwise delegate. |
| `{staged_files}` | `git diff --name-only --cached --diff-filter=ACMR`, then only paths that exist on disk (`wrapper/staged_files.go:3-9`, `wrapper/utils.go` `selectFiles`); quoted paths are unquoted (`wrapper/utils.go`, `unquoted`) | Depends on the **index and HEAD**, not on a tree alone. |
| `{push_files}` | best effort: `git diff --name-only HEAD @{push}`, else `git diff --name-only <origin HEAD branch>...HEAD`, else `git ls-tree -r --name-only HEAD` (`wrapper/push_files.go:3-42`) | Depends on remote-tracking refs/upstream config. |
| `{all_files}` | `git ls-files --cached` (`wrapper/all_files.go:3`) | Index-dependent. |
| `{files}` / `files:` | arbitrary shell command via `sh -c`, run in the job root, output lines = files, existing ones only (`wrapper/files_by_command.go:8-17`). Hook-level `files` is the default for jobs (`scope.go:25-49`) | Output of arbitrary code (e.g. `git diff-tree ... ORIG_HEAD HEAD` in a real config, section 6): a cache-key input. |
| `glob` | list of patterns; matching is **lower-cased on both sides**, i.e. case-insensitive (`filter.go:101-112`), engine `gobwas` by default or `doublestar` with top-level `glob_matcher: doublestar` (`filter.go:101-135`, `config.go:55`). A group/job glob is concatenated with outer globs and files matching **any** pattern pass (`scope.go:53`, `filter.go:78-99`) | Reproducible only with the same glob library semantics (gobwas `**` differs from doublestar). Delegate. |
| `exclude` | hook-level `exclude`, `--exclude` CLI values and job `exclude` are combined (`scope.go:28-41,57-59`); matching is case-**sensitive** (no lower-casing in `byExclude*`, `filter.go:136-190`) | Same as glob. |
| `root` | files must start with the root prefix (plain `HasPrefix`, **no path-boundary check**: `demo` also matches `demo2/x`) and the prefix is replaced by `.` (`filter.go:191-205`); cwd becomes `<repo>/<root>` (`job.go:137`) | Reproducible but with quirks. |
| `file_types` | `executable`, `not executable`, `symlink`, `not symlink`, `text`, `binary`, or a MIME type containing `/` (`filter.go:210-321`); evaluated on the **worktree files** (lstat, content sniff of first 1024 bytes, `filter.go:323-338`) | Reads file system state: key must include file modes/content. |
| skip when no files | a command whose `run` references any file template and gets an empty list is **skipped** (`build_command.go:29-31`); `files:` set but empty -> skipped (`:34-43`); for **`pre-commit`** a command with *no* template is still skipped if nothing (including deleted files) matches its filters among staged files (`:53-69`, `config/available_hooks.go:42-44`); for **`pre-push`** analogously with push files (`:72-85`, `available_hooks.go:46-48`). Other hooks run unconditionally. `--force` disables all of this (`:29,34,48`) | The "does this command run at all" decision depends on staged/push file sets. |
| `stage_fixed` | **only for `pre-commit`** (`job.go:154`, `controller.go:97`). After the command succeeds, the files it was given (or, if none, `{staged_files}` filtered by glob/exclude/root/file_types; `job.go:154-171`) are queued, and `git add --force -- <files>` runs **after all jobs end**, even if some failed (`guard.go:73-90`, `wrapper/stage_files.go:3-9`). If `git add` fails the hook fails (`guard.go:82-86`) | **Index-modifying**; cannot be cached; must run before the cache-able steps. |
| partially staged files (implicit) | for `pre-commit` (unless `--no-stage-fixed`), the guard hides unstaged changes of files that have both staged and unstaged changes: `git diff --binary ... --output=<git-dir>/info/lefthook-unstaged*.patch`, `git stash create` + `git stash store --message "lefthook auto backup"`, discard those worktree changes, run, `git add` fixed files, re-apply the patch, drop the stash (`guard.go:50-125`, `wrapper/save_unstaged_diff.go`, `store_stash.go`, `repo.go` `SaveUnstagedChanges...`). Applies to **the whole hook run regardless of whether any job uses `stage_fixed`** (`controller.go:93-101`) | Mutates the worktree during the run; with `--no-stage-fixed` it is disabled. |
| `fail_on_changes` | hook-level or `--fail-on-changes`: snapshot of `git status` + `git hash-object` before and after the jobs; any difference fails the hook (`guard.go:157-207`, `repo.go` `Changeset`, `run.go:223-254`) | Detects index/worktree mutation; Sluice could use the same idea but it is lefthook's semantics. |
| `skip` / `only` | values: `true`, `merge`, `rebase`, `merge-commit`, `{ref: <glob>}`, `{run: <shell>}`; lists OR together (`config/skip_checker.go:27-109`). `only` = skip when **not** matching (`:39`). `ref` compares the branch from `.git/HEAD` (`wrapper/state.go:61-86`), exact then gobwas glob (`skip_checker.go:78-92`). `merge` = `MERGE_HEAD` exists, `rebase` = `rebase-merge/` or `rebase-apply/` exists, `merge-commit` = HEAD has more than one parent (`state.go:88-112`). `run:` executes `sh -c` with null stdin, `LEFTHOOK=0`, cwd = process cwd (`command_executor.go:20-50`). Checked at hook, group and job level (`controller.go:73`, `job.go:56,190`) | Depends on git state + arbitrary shell; reproducible only by calling lefthook. |
| `tags` / `exclude_tags` / `--tag` | `--tag` runs jobs whose own or group tags intersect (`job.go:45`); `exclude_tags` (hook-level only, `config/hook.go:13`, + `LEFTHOOK_EXCLUDE`) skips by tag or by job name (`job.go:194-200`) | Reproducible. |
| `env` | see 2.3; values `os.ExpandEnv`-expanded against lefthook's own env | Reproducible; the *expanded* env is a key input. |
| `interactive` | needs a TTY; falls back to `/dev/tty` (`exec_unix.go:33-46`) | A queued non-TTY job cannot satisfy it; delegate/flag. |
| `use_stdin` | passes (cached) stdin to the job (`run/controller/run.go:19-22`) | Input to key: git's ref list for pre-push **[inference]**. |
| `priority` | sort key for `commands`/`scripts` only (`command.go:36-95`); no effect with `parallel` and none on `jobs` | Reproducible. |
| `fail_text` | message on failure only (`job.go:151`) | Cosmetic. |
| `timeout` | `context.WithTimeout` per job; failure text `timeout (<d>)` (`job.go:131-134,147-149`) | Reproducible. |
| `remotes` | git repos cloned into `<git-dir>/info/lefthook-remotes/`, `configs` list (default `lefthook.yml`), `ref`, `refetch`, `refetch_frequency` (`config/remote.go:3-21`, `loader.go:287-329`); fetched at install and at run unless `--no-auto-install` | Their content is not in the repo tree: a hidden input to behaviour and cache keys. |
| `extends` | see 3.0 | Content merges into the effective config. |
| `lefthook-local.yml` | per-clone override layer (3.0) | Hidden input. |
| `source_dir` / `source_dir_local` | script folders (`config.go:33-35`, `run.go:197-221`) | Script content is a key input. |
| `min_version` | exact semver comparison, mismatch = exit 1 (`run.go:355-373`) | Reproducible. |
| `no_tty` | `cfg.NoTTY` or `--no-tty`: disables spinner/interactive (`run.go:145`) | Cosmetic. |
| `setup` | hook-level list of `run` commands executed before the jobs, with templates, **failure is only a warning** (`controller.go:84-86`, `controller/setup.go:13-46`) | Side-effecting; not per-command. |
| `output`, `colors` | print filtering | Cosmetic. |
| `lefthook` (path), `rc`, `assert_lefthook_installed` | only used by the generated script (2.1) | Entry-point concerns. |
| `skip_lfs` | see 2.3 | Side effect (LFS push) if Sluice passes real git args. |

### 3.2 Which features change the set of files a command sees (cache-key critical)

1. The base list: staged (ACMR, existing), push, all (`ls-files --cached`), a
   custom `files:` command, or `--file` / `--all-files` /
   `--files-from-stdin` overrides.
2. Filters applied in this fixed order: `glob` -> `exclude` -> `root` ->
   `file_types` (`filter.go:57-76`), with glob/exclude accumulated from hook,
   group and job levels, plus CLI `--exclude`.
3. The decision to skip (empty list) and, for `pre-commit`/`pre-push`, the
   implicit skip when no staged/push files match even without templates.
4. `stage_fixed` runs on the same filtered list and adds files to the index.
5. `glob_matcher`, `file_types` (reads the worktree), and `root` quirks.
6. Partial-staging hiding (pre-commit): the worktree content a command reads
   differs from the real worktree.

---

## 4. Running a single command

### 4.1 The CLI

```
lefthook run <hook> [git-args...] [options]        (cmd/run.go:11-132)
  --job <name>          repeatable; run only jobs with these names        (:42)
  --command <name>      repeatable; same effect, merged into --job        (:52; run.go:140)
  --tag <tag>           repeatable; run only jobs with these tags         (:47)
  --exclude <glob>      repeatable; exclude files from every template     (:57)
  --file <path>         repeatable; replace ALL file templates with these (:62)
  --all-files           replace file templates with `git ls-files --cached` (:73)
  --files-from-stdin    NUL-separated file list from stdin                 (:99)
  --force, -f           do not skip when there are no files                (:67)
  --no-auto-install     do not sync hooks/remotes                          (:78)
  --no-stage-fixed      ignore stage_fixed AND the partial-staging guard   (:83)
  --no-tty, --skip-lfs, --fail-on-changes[/--no-...], --fail-on-changes-diff,
  --verbose, --colors on|off|auto
```

- There is **no `--commands`**; the flag is `--command` (`cmd/run.go:52`).
  Integration test for `--job`/`--tag`:
  `tests/integration/cli_run_only.txt`.
- Selection is by exact `Name`: `job.Name` for jobs, the map key for
  `commands`, the file name for `scripts` (`job.go:41`, `command.go:38-40`,
  `script.go:33`). **Unnamed jobs cannot be selected.** Naming a *group*
  runs all its jobs (`scope.go:63-65`); nested leaf names are matched at any
  depth (`job.go:41` is evaluated at leaves; groups pass through).
  Nothing prevents a `commands` key and a `jobs` name from being identical:
  both would run.
- `--tag` combined with `--job`: both filters apply (`job.go:41-47`).
- Hook-level `skip`/`only`, group-level `skip`/`only`, `exclude_tags` and
  `LEFTHOOK_EXCLUDE` **still apply** to a selected command: the selection is
  an additional filter, not an override.
- If no job ends up non-skipped, the run **fails with** `no job matching X
  found in hook "<hook>"` (`controller.go:113-129`). The check cannot tell
  "name not found" from "found but skipped" (both are `skip` results:
  `job.go:42,46,91`), so a selected command that is skipped for "no matching
  staged files" **exits 1** **[inference from the code path; the unit test
  only covers the name-not-found case, `controller_test.go:583-596`]**.
- After `<hook>`, every positional becomes a git arg (`cmd/run.go:124`),
  feeding `{0}`, `{1}`... and scripts (`replacer.go:70-81`,
  `build_script.go:88`). Whether urfave/cli v3 accepts flags after the
  positionals was not verified in the library; the tests place flags after the
  hook name with no extra git args.
- `--file` / `--all-files` / `--files-from-stdin` (`run.go:179-195`): with
  files forced, a "mocked" replacer answers **all four file templates** with
  that one list (`replacer.go:83-96`, `build_command.go:89-90`); glob/exclude/
  root/file_types still filter it. The docs say `--all-files` is ignored if
  `--file` is also given; the code **appends** all files after the explicit
  ones (`run.go:186-190`). **[docs vs code mismatch]**
- `lefthook dump [--format yaml|json|toml]` prints the fully merged config
  (main + extends + remotes already cloned + local) (`cmd/dump.go:14-53`,
  `internal/command/dump.go:68-92`). It does not sync remotes
  (`Dump` only calls `LoadConfig`). That is the machine-readable way to
  enumerate hooks, names, `run` strings and options; the raw `commands` map
  is unordered, so ordering must be reconstructed with the rules in 3.0
  **[inference]**.
- `lefthook validate` checks the main and secondary config against the JSON
  schema (`internal/command/validate.go:110-141`); `lefthook check-install`
  exits 0/1 on checksum sync (`internal/command/check_install.go`).

### 4.2 What a single-command run reproduces and what it does not

Reproduces (because it is lefthook's own code path for that job):
`run` templates, quoting and chunking, glob/exclude/root/file_types, `files:`,
`env`, `timeout`, `skip`/`only` at all levels, tags, git args, script runner,
`stage_fixed` queueing, hook- and group-level `root`/`env`/`glob` inheritance,
the "skip if no files" rules, `fail_text`.

Does **not** reproduce, or changes meaning:

- Ordering, `parallel`, `piped` and `priority` across commands: one command is
  run in isolation; the "stop after first failure" of `piped` and the
  all-run-then-fail default are Sluice's job.
- `setup:` instructions still run first on every invocation
  (`controller.go:84`): N single-command runs = N executions of `setup`.
- Partial-staging guard + `git add` of `stage_fixed` files run **per
  invocation** (`controller.go:93-125`): N runs hide/restore the unstaged
  changes N times and stage after each run, unless `--no-stage-fixed` (which
  also disables staging, so Sluice would have to stage itself).
- LFS hook runs per invocation if git args are supplied for LFS hooks (2.3):
  pass `--skip-lfs` and do LFS once.
- `--force` flips skip semantics; omit it unless "always run" is intended.
- Auto-install/remote fetch per invocation: always pass `--no-auto-install`.
- Output is lefthook's own formatting; the exit code is only 0/1 and cannot
  distinguish "ran and passed" from "was skipped" (no job result is exposed;
  only `LEFTHOOK_OUTPUT` sections, `run.go:85-92`, `logger` outputs, say so)
  **[inference]**.

### 4.3 Is it viable for Sluice?

Yes, with these properties (all from the code above):

- Invoke as `lefthook run <hook> <git-args> --command <name> --no-auto-install
  --skip-lfs` (+ `--no-stage-fixed` when Sluice itself orders and stages index
  modifications). It exercises lefthook's real template/filter code for that
  command, so semantic drift is avoided by construction.
- It must run in the **real worktree** with the **inherited git environment**
  (`GIT_INDEX_FILE`, `GIT_DIR`, cwd = repo): lefthook computes staged files
  with plain `git diff --cached` in that environment (`wrapper/cmd.go:109-146`,
  `system/command.go:36-62`). When git runs a pre-commit hook for
  `git commit <paths>` or `-a`, it sets `GIT_INDEX_FILE` to a temporary index;
  a queued job without that variable would compute a different staged list
  **[inference: git behaviour]**.
- Selection only works for **named** jobs. Unnamed jobs and `setup:` can be
  executed only as part of a whole-hook run, i.e. delegation (section 5).
- Enumerating commands needs the merged config (`lefthook dump --format json`)
  or Sluice's own parser (which is exactly the re-implementation to avoid).

### 4.4 What the cache key must include (facts -> inputs)

For `lefthook run <hook> --command X` the result is a pure function of:

1. The lefthook binary version (`lefthook version`) and the effective config:
   `lefthook dump --format json` covers main + extends + local + already
   cloned remotes (it is what lefthook itself hashes for its checksum,
   `config.go:66-82`). It does **not** cover: script files under `source_dir`
   (`build_script.go:49`), remote repositories' refs beyond the merged text,
   `env` expansions, `skip: {run: ...}` outcomes, or `files:` command outputs.
2. The resolved file set for the command: staged list (index vs HEAD), or push
   list (remote-tracking refs, `@{push}`), or all files; after
   glob/exclude/root/file_types. Using Sluice's own list with `--file` makes
   this explicit and cacheable while lefthook still applies its filters
   (`replacer.go:83-96`), at the cost of Sluice owning the base list.
3. The content of those files and, for commands that do not take file
   arguments (a plain `cargo test`), of the whole worktree: a tree hash alone
   is the right granularity only if the worktree equals the tree (partially
   staged files make the worktree differ from the index; lefthook hides the
   difference only in `pre-commit` and only while running, `guard.go:68-125`).
4. Git state used by `skip`/`only`: branch name, merge/rebase markers, HEAD
   parent count (`state.go:61-112`).
5. `git args` (`{0}`, `{1}`..., commit message path for `commit-msg`) and the
   cached stdin for `use_stdin` jobs.
6. Process environment visible to the command: `env:` values (expanded),
   `CI` when `fail_on_changes` is in play, `rc` file effects (script only).
7. Toolchain of whatever `run` calls (outside lefthook).

For commands that read `git diff --cached` themselves (real example in section
6, `tani/athena-prolog`), the input is the **index**, not the worktree.

---

## 5. Delegation and drift

### 5.1 Handing the whole hook back to lefthook untouched

The hook script is only: verbose flag, `LEFTHOOK=0` check, optional `rc`,
binary discovery, `lefthook run <hook> "$@"` (`hook.tmpl:1-113`). Two ways to
delegate, both unmodified from lefthook's viewpoint:

- Execute the lefthook-generated script from the common git dir
  (`<git-common-dir>/hooks/<hook>`) when it exists and carries the `LEFTHOOK`
  fingerprint (`lefthook.go:96-119`). That reproduces binary discovery, `rc`,
  `LEFTHOOK=0` handling and git args/stdin exactly. **[inference]**: the file
  exists only if `lefthook install` ran in this clone before Sluice changed
  `core.hooksPath`, or if the user chains it.
- Call `lefthook run <hook> "$@"` directly with git's stdin/args. It matches
  the script except for `rc`, `LEFTHOOK_BIN`/config `lefthook:` and the
  non-PATH discovery routes (2.1), which Sluice would have to replicate.

The hook is then a black box: auto-install sync and remote fetch still run
unless `--no-auto-install` is added (which differs from "untouched").

### 5.2 When lefthook rewrites hooks or `core.hooksPath`

- It writes hook files only in: `lefthook install` (manual, or npm
  `postinstall` on every `npm install`, `postinstall.js:10`), and `lefthook
  run` auto-sync when the config checksum/mtime differs (`install.go:221-231`).
- It never sets `core.hooksPath`. It can **unset** local and global
  `core.hooksPath` only with explicit `install --reset-hooks-path`
  (`install.go:540-542,593-609`). It can **write into** the configured path
  with `install --force` (`install.go:544-550`), removing fingerprinted files
  and renaming others to `.old` (`lefthook.go:132-155`).
- So a clone-local `core.hooksPath` pointing at a Sluice directory is not
  overridden by default: plain `install`/`postinstall`/auto-sync refuse and
  log (1.2, 1.3). The two ways it could be damaged are an explicit
  `--force` (Sluice's hook scripts: if they do not contain the string
  `LEFTHOOK` they are renamed `.old`; if they do they are deleted) or
  `--reset-hooks-path`.
- Noise, not damage: while out of sync, every `lefthook run` without
  `--no-auto-install` prints the hooks-path error text.

### 5.3 Staying the entry point

- Chaining/delegation to lefthook never needs lefthook's hook files to live in
  `core.hooksPath`; lefthook is invoked as a program.
- Drift to watch: config/remote/local changes alter behaviour with no change
  in Sluice's own config; `lefthook.checksum` going stale is harmless for
  Sluice's path; a repo that later commits `core.hooksPath` is not applicable
  (git config is never committed).
- Version drift: `min_version` (exit 1) and the template (`hook.tmpl`) differ
  between lefthook releases; Sluice should key on the binary version
  **[inference]**.

---

## 6. Real-world usage calibration

### 6.1 Sample and caveats

- Source: `gh search code` (GitHub legacy code search) with
  `--filename lefthook.yml` / `--filename .lefthook.yml` and keyword queries
  `pre-commit` and `run` (lefthook.yml) and `pre-push` and `run`
  (.lefthook.yml), up to 100 hits per query. 266 distinct repos remained after
  de-duplication by repo; 130 repo-root configs (52 `lefthook.yml`, 78
  `.lefthook.yml`) were chosen by a seeded shuffle, downloaded through the
  contents API and **all 130 parsed as YAML mappings** (PyYAML, structural
  analysis, not regex, except where marked "text").
- Bias: keyword-ranked, not random; the filename mix is an artefact of how
  the queries were split (GitHub reports 18,784 `lefthook.yml` and 595
  `.lefthook.yml` files for the empty query, so `.lefthook.yml` is
  over-represented here); forks/small hobby repos are included; configs that
  mention `pre-commit` and `run` are over-represented. Treat the numbers as
  "what a typical public config looks like", not as population rates.
- Total-count probes (same API, term match, approximate): of 18,784
  `lefthook.yml` files, 8,272 contain `stage_fixed` (44%), 4,192 `jobs`
  (22%), 223 `extends` (1.2%), 220 `use_stdin` (1.2%), 179 `git_url`
  (0.95%). The `scripts` count (3,480) is not meaningful: it also matches
  the word in `npm run`/package scripts.

### 6.2 Counts (n = 130 configs)

| Feature | Configs | Share |
|---|---|---|
| defines `pre-commit` | 121 | 93% |
| defines `pre-push` | 62 | 48% |
| defines `commit-msg` | 31 | 24% |
| defines `post-merge` / `post-checkout` | 8 / 2 | 6% / 2% |
| `commands:` map | 97 | 75% |
| `jobs:` list | 38 | 29% |
| `scripts:` (text) | 0 | 0% |
| `parallel: true` on some hook | 88 | 68% |
| `parallel: true` on `pre-commit` | 75 | 58% |
| `piped: true` (hook) / (group) | 5 / 2 | 4% / 2% |
| `glob` | 98 | 75% |
| glob/pattern containing `**` (text) | 39 | 30% |
| `stage_fixed` | 66 | 51% |
| `stage_fixed` on a `pre-commit` that is also `parallel: true` | 45 | 35% |
| `git add` inside a `run` of `pre-commit` | 9 | 7% |
| `pre-commit` has an index-modifying step (`stage_fixed` or `git add`) | 68 | 52% |
| `{staged_files}` | 71 | 55% |
| `{staged_files}` together with a `glob` (text) | 69 | 53% |
| `{1}` (git arg; commit-msg) | 21 | 16% |
| `{files}` | 6 | 5% |
| `{push_files}` | 3 | 2% |
| `{all_files}` | 1 | 1% |
| custom `templates:` use | 4 | 3% |
| some `run` with shell operators (`&&`, `\|`, `;`, `$()`) | 35 | 27% |
| some multi-line `run: \|` | 16 | 12% |
| `group:` (nested jobs) | 15 | 12% |
| `exclude` | 11 | 8% |
| `skip` / `only` anywhere | 16 | 12% |
| of those with `merge`/`rebase` values (text) | 10 | 8% |
| `ref:` skip/only (text) | 5 | 4% |
| `root:` | 7 | 5% |
| `tags` | 6 | 5% |
| `env` / `fail_text` / `priority` | 5 / 5 / 5 | 4% each |
| `files:` (custom file command) | 6 | 5% |
| `output:` | 20 | 15% |
| `min_version` | 5 | 4% |
| `lefthook:` (binary path) | 4 | 3% |
| `glob_matcher` | 3 | 2% |
| `assert_lefthook_installed` | 3 | 2% |
| `extends` | 2 | 2% |
| `no_tty`, `no_auto_install`, `fail_on_changes`, `ai` | 2 each | 2% |
| `skip_lfs`, `rc`, `setup` | 1 each | 1% |
| `remotes`, `scripts`, `file_types`, `use_stdin`, `interactive`, `timeout`, `follow`, `{cmd}` | 0 | 0% |
| JS tooling in `run` (npm/yarn/pnpm/bun/npx) | 69 | 53% |
| Python / Rust / Go / Ruby tooling in `run` (text) | 14 / 14 / 12 / 9 | 11% / 11% / 9% / 7% |

Reading: the dominant shape is
`pre-commit: parallel: true` + `commands`/`jobs` with `glob` +
`{staged_files}` + `stage_fixed: true`. Rare features exist in the wild
(remotes, extends, use_stdin, interactive, scripts) but were absent from this
sample.

### 6.3 Examples (public repos)

- `lancachenet/lancache-dns` (`.lefthook.yml`): `parallel: true` + `jobs`,
  one job runs `docker run ... oxsecurity/megalinter:beta` with
  `stage_fixed: true` and **no file template** (so staging uses
  `{staged_files}` implicitly, `job.go:154-171`).
- `mdn/browser-compat-data`: `parallel: true` + `stage_fixed: true` on
  prettier/eslint/`lint:fix {staged_files}`, one run ends with
  `&& git add lint/common/standard-track-exceptions.txt` (index modification
  inside `run`); `pre-push` with `piped: true` and a parallel `group`;
  `post-merge` with `only: - ref: main`.
- `openneuro-py/openneuro-py`: `skip_lfs: true` with a comment that LFS hooks
  can hold the index lock; nested `group: parallel: true` stages; a
  `post-merge` command with `files: git diff-tree -r --name-only
  --no-commit-id ORIG_HEAD HEAD` and `glob` on `uv.lock` etc.
- `levibuzolic/cinder_ui`: `glob_matcher: doublestar`, group-level `glob`,
  `root: demo`, `stage_fixed` on groups, `mise exec -- mix format`.
- `tani/athena-prolog`: `tags`, `fail_text`, and a `run: |` block that calls
  `git diff --cached --name-only` itself (input = the index).
- `bhovhannes/svg-url-loader`: `extends:` a file under `node_modules/`
  (config content lives outside tracked files).
- `dcdavidev/sidenav`, `bcgov/lear`, `mailersend/mailersend-python`:
  `remotes:` with `git_url`, `ref`, `refetch_frequency`, `configs`;
  `bcgov/lear` has a command that references
  `.git/info/lefthook-remotes/bcregistry-sre-main/...` directly, confirming
  the remote checkout location in the wild.

---

## 7. Implications for the Sluice design

Facts -> constraints, no further recommendations.

1. **Lefthook's config is not self-contained in the repo tree.** Effective
   behaviour = main file + `extends` (possibly under `node_modules/`) +
   `remotes` (clones in `<git-dir>/info/lefthook-remotes`) + untracked
   `lefthook-local.*` + script files + env (`rc`, `LEFTHOOK_*`). A cache key
   of "tree + command + toolchain" is insufficient; it needs the effective
   config (e.g. `lefthook dump --format json`) and the lefthook version, and
   remotes/local are per-clone state (3.0, 4.4).
2. **`{staged_files}` / pre-commit skip rules depend on index and HEAD, and
   `{push_files}` on remote refs**, not only on a tree: 55% of sampled
   configs use `{staged_files}`, 53% also with a `glob`. The decision "does
   this command run at all" is part of the semantics (3.1, 3.2).
3. **Index-modifying work is the common case** (52% of sampled configs have
   `stage_fixed` or an in-`run` `git add` in `pre-commit`; 35% combine it with
   `parallel: true`). Lefthook stages after all jobs end, with `git add
   --force` (3.1). Index-modifying steps cannot be cached and must be ordered
   before cacheable ones, consistent with the stated design; Sluice can only
   know a command is index-modifying from `stage_fixed`, from `git add` text
   in `run`, or from effects (`fail_on_changes` logic).
4. **Lefthook already does heavy git manipulation in `pre-commit` for the
   whole hook** (stash, discard, patch, re-apply) regardless of `stage_fixed`
   usage (`guard.go`). Running N single-command invocations multiplies it;
   `--no-stage-fixed` removes it and `stage_fixed` with it.
5. **Single-command execution through lefthook is supported and exact for
   named jobs** (`--command`/`--job`), reproducing templates, filters,
   `env`, `timeout`, `skip`/`only`. Constraints: unnamed jobs and `setup:`
   are not addressable; a selected-but-skipped command can exit 1; exit code
   is only 0/1 and does not say "skipped"; flags to pass:
   `--no-auto-install`, `--skip-lfs` (and `--no-stage-fixed` when Sluice owns
   staging); there is no `--commands` flag.
6. **Per-command re-implementation would have to match** gobwas vs doublestar
   glob semantics (case-insensitive glob, case-sensitive exclude, `root`
   prefix quirks), shell quoting of file lists, chunking at 130000 chars,
   mime/`file_types` sniffing, `skip: {run:}` shell checks, `{cmd}` merge,
   priority/number sorting. That is the "understand completely" bar of the
   design principle; failing it means delegate.
7. **Entry-point coexistence is safe by default.** Lefthook never sets
   `core.hooksPath`; plain `install`, npm `postinstall` and auto-sync refuse
   to write into a local `core.hooksPath` and only log. Dangerous explicit
   flags: `install --force` (writes into/renames in Sluice's hook dir),
   `install --reset-hooks-path` (unsets local **and global** `core.hooksPath`).
   Pre-existing non-lefthook hooks are renamed `.old` and never executed by
   lefthook, so "the repository's existing hooks" are the lefthook config, not
   `.git/hooks/*.old`.
8. **Never run `lefthook install` from Sluice** (writes `lefthook.yml` if
   absent, hooks, checksum, AI settings files in the worktree, clones
   remotes). Runs must carry `--no-auto-install` to avoid writes under
   `.git/hooks`, `.git/info` and network access.
9. **Environment fidelity matters.** Hook execution needs git's environment
   (`GIT_INDEX_FILE`, cwd, args, stdin for `pre-push`/`use_stdin`) and the
   `rc` file; lefthook's own helper processes set `LEFTHOOK=0` but job
   commands do not, so a job that runs git can re-enter hooks.
10. **Delegation is cheap and complete**: the hook script is a thin launcher
    (`call_lefthook run <hook> "$@"`); executing the generated script or
    `lefthook run <hook> "$@"` hands over the entire hook (parallel, piped,
    unnamed jobs, `setup`, LFS, stash guard) at the price of no per-command
    caching.
11. **Real configs are mostly simple but not uniformly so**: dominant shape is
    `parallel` + `glob` + `{staged_files}` + `stage_fixed`; 27% have shell
    operators in `run`, 12% nested groups, 12% `skip`/`only`; rare features
    (`remotes`, `extends`, `use_stdin`, `interactive`, `scripts`) appear in
    about 1% of all public files by term count. A detector for "fully
    understood" would have to treat any rare feature as a trigger to delegate.
