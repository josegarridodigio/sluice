# gate-cache (natelandau/gate-cache) — research note

Source studied: `<repo>/refs/gate-cache` at version 0.6.0
(`src/gate_cache/__init__.py:3`, `CHANGELOG.md:3`). All `file:line` cites below are relative
to that directory. Nothing in the repo was modified.

## 1. What it is

gate-cache is a small Python 3.11+ CLI (one dependency, `platformdirs`, `pyproject.toml:10-12`)
that wraps one shell command ("gate") and skips it when the same gate name already passed on the
same git working-tree hash (`README.md:3-17`). It records a marker file only on success, keyed on
`sha256(tree + "\0" + name)`, under the git common dir so all worktrees share markers. It ships as
a prek / pre-commit hook and is meant to be shared by hooks, task runners and agents
(`README.md:52-74`). It does **not** queue, coalesce, or bound concurrency — each invocation is
independent; there is no daemon, server, or MCP surface.

License: MIT, copyright 2026 Nate Landau (`LICENSE:1-3`).

## 2. Tree hash computation

Entry point: `take_snapshot()` (`src/gate_cache/snapshot.py:311-338`). It returns a frozen
`Snapshot(tree, reason)`; `tree is None` means "unknown, run uncached" (`snapshot.py:14-29`).

### 2.1 Pre-checks (`_inspect`, `snapshot.py:184-195`)

1. `git rev-parse --git-dir` fails -> `not a git repository` (`:186-187`).
2. `git rev-parse --show-toplevel` to find the work-tree root; all later probes run from the
   root because `ls-files` only lists paths under its cwd and `:/` pathspecs break under
   `GIT_LITERAL_PATHSPECS` (`:188-192`).
3. `git ls-files --unmerged` non-empty -> `unmerged index` (`:193-194`).
4. `git ls-files -v -z`: any lowercase tag (assume-unchanged) or `S` (skip-worktree, which
   sparse checkout sets) -> unknown (`_hidden_entry_reason`, `:78-87`).

### 2.2 The private index (`_private_snapshot`, `snapshot.py:267-308`)

- Resolve the real index and object dir with `git rev-parse --git-path index` / `objects`
  (`_git_path`, `:198-207`, called at `:274-275`). Because `--git-path` honours
  `GIT_INDEX_FILE`, an index that git handed to a hook (e.g. the temporary index of a partial
  `git commit <paths>`) is the one that gets copied. Test: `tests/test_snapshot.py:528-546`
  (relative `GIT_INDEX_FILE=.git/alt-index`, real and alt index left byte-identical) and
  `:733-751` (relative `GIT_INDEX_FILE` resolved from the directory git runs in).
- Create a scratch dir (see 2.5), `shutil.copy2(index, private/index)` (`:282-283`).
  `copy2` is deliberate: it preserves the index mtime so git's "racily clean" logic still
  re-hashes a file edited in the same second as the index write (`:280-281`, test
  `tests/test_snapshot.py:595-607`).
- Build a private env (`:285-291`):
  `GIT_INDEX_FILE=<scratch>/index`, `GIT_OBJECT_DIRECTORY=<scratch>/objects` (fresh, empty),
  `GIT_ALTERNATE_OBJECT_DIRECTORIES=<real objects>[:inherited alternates]`. New blobs go to
  the throwaway object dir; existing ones resolve through the alternate. Test that nothing is
  written to `.git/objects`: `tests/test_snapshot.py:508`.
- `_require_private` (`:210-219`, called `:292`) refuses to run if either variable points
  outside the scratch dir — a guard against future regressions that would stage into the
  real index.
- `git -c core.splitIndex=false add -A` from the root (`:294-295`). Split-index mode is
  forced off because it would write a shared-index file into the real git dir (`:293`, test
  `tests/test_snapshot.py:517`). If `add -A` fails (e.g. an embedded repo with no commit), the
  first `fatal:`/`error:` stderr line becomes the reason (`:296-303`).
- `_staged_reason` (`:228-244`): post-staging sanity checks (submodules, case) — see 2.4.
- `git -c core.splitIndex=false write-tree` -> the tree hash (`:307`).
- Scratch dir is deleted on block exit (`scratch.py:57-67`).

So the tree is exactly "what `git add -A && git write-tree` would commit": tracked +
untracked non-ignored files, their bytes and modes; ignored files excluded
(`docs/how-it-works.md:7,12`). The exec bit is part of the key (`tests/test_gate.py:77`).
`-unormal`/`--untracked-files=normal` is only used in the submodule status probe
(`snapshot.py:118-127`); untracked files in the main tree are picked up by `add -A`
regardless of `status.showUntrackedFiles` (`tests/test_snapshot.py:655`).

### 2.3 Being invoked inside a git hook

- `_hook_work_tree` (`snapshot.py:32-52`): if `GIT_DIR` is set and `GIT_WORK_TREE` is not,
  git treats cwd as the top of the work tree, so a hook that `cd`s into a subdirectory would
  see every other file as changed. gate-cache derives the real work tree: if the git dir holds
  a `gitdir` file (linked worktree) it uses that file's parent; if the dir is named `.git` it
  uses its parent; else gives up. It does **nothing** if `core.worktree` is configured
  (`:41-42`, test `tests/test_snapshot.py:332-341`).
- `_git_env` (`:55-63`) then injects `GIT_WORK_TREE` **only into gate-cache's own git probes**.
  The wrapped command is spawned with the inherited environment untouched
  (`gate.py:169` -> `runner.py:112` `subprocess.Popen(list(argv))` with no `env`; test
  `tests/test_gate.py:734-746`).
- `GIT_INDEX_FILE` from the hook is honoured via `--git-path index` (2.2).
- Tests for the hook case from a subdirectory of both the main checkout and a linked worktree:
  `tests/test_snapshot.py:282-318`, `tests/test_gate.py:717-731`.
- Documented in `docs/how-it-works.md:32`.

### 2.4 Things that make the tree "unknown" after staging (`_staged_reason`, `:228-244`)

- If the private index has any gitlink (`160000`, `_gitlinks` `:145-153`): run
  `git --no-optional-locks status --porcelain=v2 -z --untracked-files=normal
  --ignore-submodules=none` (`_status`, `:112-142`) and fail on any submodule record with
  `C`/`M`/`U` flags (`:107-109`, `:233-235`). Then probe every submodule directly and
  recursively with the superproject env vars stripped via `git rev-parse --local-env-vars`
  (`_submodule_env`, `:222-225`; `_gitlink_reason`, `:156-181`): uninitialised, not a repo,
  hidden entries, dirty status, nested gitlinks. Rationale: the superproject's status obeys the
  submodule's own config, so `diff.ignoreSubmodules` etc. could hide changes
  (`docs/how-it-works.md:17-19`).
- `--no-optional-locks` is used so the status probe never takes `index.lock` and does not
  block on a concurrent git command (`:114-116`, test `tests/test_snapshot.py:632-642`).
- On `core.ignorecase=true`, every indexed path is checked component-by-component against
  real directory listings; a case-only mismatch -> unknown (`_case_mismatch_reason`,
  `:247-264`).

### 2.5 Scratch directory (`src/gate_cache/scratch.py`)

`platformdirs.user_runtime_path("gate-cache")` (honours `XDG_RUNTIME_DIR`, falls back to
`/run/user/<uid>` or the system tmp dir), created mode 0700 (`scratch.py:23-37`). Each snapshot
gets a `snap-*` `TemporaryDirectory` (`:57-67`); on every call, sibling `snap-*` dirs older than
3600 s are swept as leftovers of killed runs (`:20`, `:40-54`). Documented at
`docs/how-it-works.md:30`.

### 2.6 Pre-push vs pre-commit; fast path

- There is **no** code that distinguishes hook stages. The hook manifest declares
  `stages: [pre-commit, pre-push, manual]` (`.pre-commit-hooks.yaml:9`). The documented
  behaviour is a consequence of the runner: at commit time prek/pre-commit stash unstaged
  edits, so the tree equals the staged content; at push time with a clean checkout the tree
  equals `HEAD^{tree}` (`docs/how-it-works.md:28`, tests `tests/test_snapshot.py:20`,
  `tests/test_gate.py:942-953` for the partial-commit case).
- There is **no fast path for a clean tree**: every call copies the index, runs `add -A` and
  `write-tree` (`snapshot.py:282-307`). The docs note that when the working copy matches the
  index the result equals the index tree (`docs/how-it-works.md:12`), but the code does not
  shortcut. The only skipped work is the full `git status` scan, which runs only when the
  index has gitlinks (`snapshot.py:230-233`).
- The snapshot is taken **twice** per real run: once before the command and again after a
  passing run to detect files changed by the command (`gate.py:133`).

## 3. Cache key

- `key = sha256(f"{tree}\0{name}".encode()).hexdigest()` (`src/gate_cache/markers.py:23-36`).
  Pinned as a public contract by `tests/test_markers.py:29-38` and documented with a bash
  equivalent at `docs/how-it-works.md:104-121`.
- `name` is either the `--name` value or `shlex.join(command)` (`src/gate_cache/cli.py:235`),
  so `pytest 'a b'` and `pytest a b` are different gates (`docs/how-it-works.md:40`).
- **Not** in the key: environment variables, tool versions, ignored files, cwd, config
  version, exec bit when `core.fileMode=false`, anything a clean filter strips
  (`docs/how-it-works.md:80-90`). The documented workaround is to bake such inputs into
  `--name`, e.g. `--name "test-uv$(uv --version ...)"` or `--name "integration-$(git
  hash-object .env)"` (`README.md:105-121`). The project itself does this in `duties.py:45-55`.
- There is no key versioning / schema version field in the key or the marker.

## 4. Markers

- Location: `<git rev-parse --git-common-dir>/gate-cache/` (`markers.py:18`, `:39-48`;
  `git_common_dir()` `snapshot.py:341-364`, relative output resolved against cwd). Using the
  common dir is what makes linked worktrees share markers (`tests/test_gate.py:139`,
  `tests/test_snapshot.py:143`). Note markers for a *dirty* tree are therefore also visible from
  another worktree that reaches the same dirty content.
- File naming: `<key>` for a pass; `<key>.miss` for a recorded non-pass (`markers.py:20`,
  `:65-78`); in-flight temp files are `.tmp-*` (`:19`).
- Content: JSON `{"tree","name","command":[...],"timestamp":"...Z"}` for a pass (`:81-93`,
  `:124-128`); miss records add `"exit_code"` and `"reason"` (`:96-121`). Timestamp is UTC
  ISO-8601 seconds. The **existence** of the pass file is the signal (`gate.py:165`); the JSON
  is only read by `status` to list names, and a content-less marker from another tool still
  causes skips but cannot be listed (`markers.py:144-159`, `docs/how-it-works.md:125`).
- Atomic write: `NamedTemporaryFile` in the same dir, then `Path.replace`
  (`markers.py:124-141`); concurrent writers to one path leave one valid file
  (`tests/test_markers.py:72`).
- Listing validation: a record counts only if its `tree` matches and `sha256(tree, name)`
  reproduces its own filename (`markers.py:219-239`), defending against renamed/foreign files.
- TTL: `GATE_CACHE_MAX_AGE_DAYS` (default 30, `gate.py:34-35`, `:69-95`). Pruning deletes any
  regular, non-symlink file whose `st_mtime` is older than the cutoff, swallowing all OS errors
  (`markers.py:242-257`). It runs after every marker or miss write (`gate.py:114`, `:125`),
  never on a skip, so a hit does not refresh the age (`docs/how-it-works.md:96`).
- Clearing: `gate-cache clear` unlinks every file (markers, misses, temps), tolerates vanishing
  files and subdirectories, removes the dir, counts only real markers (`markers.py:260-297`,
  `gate.py:333-349`).
- Failed run: no marker; a `.miss` record with `exit_code` and `reason="exit code N"`
  (`gate.py:172-180`). A pass later deletes the miss (`gate.py:112-113`). A run interrupted by
  SIGINT/SIGTERM/SIGHUP writes neither (`gate.py:170-171`).
- Auto-fixers / files modified by the gate: yes, detected. After exit 0 the snapshot is retaken
  and compared to the pre-run tree; mismatch -> message `files changed during the run, not
  recording pass` and a miss record with `exit_code=0` (`gate.py:39`, `:130-139`; tests
  `tests/test_gate.py:302`, `:313`, `:774`). Nothing is cached for the *new* tree either.
- Error policy: any failure to write a marker/miss only warns and never changes the command's
  exit code (`gate.py:104-127`, `:182-186`; `docs/how-it-works.md:55`).
- No in-progress / "running" marker exists: two concurrent invocations on the same tree both run
  the command. No coalescing, no locking beyond the atomic rename.

## 5. CLI surface and config

Usage (`cli.py:25-28`):

```
gate-cache [--name NAME] -- COMMAND [ARGS...]
gate-cache status [--name NAME] [--json]
gate-cache clear
```

- Run form is hand-parsed (`_split_run`, `cli.py:183-212`): only `--name X` / `--name=X` may
  precede `--`; anything else, a missing `--`, an empty name, a repeated `--name` or no command
  is a usage error exit 2 (`:17`, `:108-109`, `:230-234`). Everything after the first `--` is
  passed verbatim (`tests/test_cli.py:331`). `-h/--help/--version` only when first token
  (`:19`, `:221-222`).
- `status`: text or `--json`; exit 0 on hit (or at least one pass without `--name`), else 1;
  reports `tree`, `reason`, `passed`, `missed[]` (with `exit_code`, `reason`, `timestamp`), or
  `name`/`marker`/`result`/`last_run` (`gate.py:190-330`, `README.md:169-212`).
- `clear`: exit 0/1 (`gate.py:333-349`).
- Exit codes of the run form: child's code; 0 on skip; 127 not found; 126 cannot exec;
  128+signal (`runner.py:18-21`, `:112-127`); 130 on Ctrl-C outside the child (`cli.py:18`,
  `:245-246`). SIGTERM/SIGHUP are forwarded to the child and gate-cache keeps waiting; SIGINT
  is not forwarded because the child shares the process group; signals arriving during spawn are
  queued and forwarded after `Popen` (`runner.py:38-61`, `:69-93`). Ignored dispositions are
  preserved so `nohup` behaves (`:81-85`).
- Output: one `gate-cache: ...` line on stderr for skip/uncached/warnings (`console.py:12-18`);
  skip line shows the first 12 chars of the tree (`gate.py:166`). `status` text on stdout with
  `NO_COLOR`/`TERM=dumb` awareness (`console.py:21-25`).
- Env vars (`gate.py:32-66`): `GATE_CACHE_DISABLE` (non-empty wins; `0`/`false` force-enable
  even in CI), `CI` (truthy -> uncached, no marker written), `GATE_CACHE_MAX_AGE_DAYS`
  (invalid -> warn and use 30). Truthiness: `value.strip().lower() not in {"", "0", "false"}`
  (`:42-44`). When disabled, no git call is made at all (`tests/test_gate.py:620`).
- Config file: **none**. No project config, no gate definitions; the gate is whatever the caller
  passes. Hook config is the standard `.pre-commit-config.yaml` (`README.md:56-68`).
- Hook manifest: `.pre-commit-hooks.yaml:1-9` (`language: python`, `pass_filenames: false`,
  `minimum_pre_commit_version: 3.2.0`, stages pre-commit/pre-push/manual).

## 6. Measured latency / performance claims

None. Neither `README.md`, `docs/how-it-works.md`, `CHANGELOG.md` nor the source states any
timing, benchmark or overhead figure (verified by grep for ms/latency/overhead/benchmark). The
only quantitative figures are the illustrative `412 passed in 94.20s` in `README.md:11`.
What can be inferred from the code: one cache lookup costs a Python interpreter start plus
roughly 10 git subprocesses (`rev-parse --git-dir`, `--show-toplevel`, `ls-files --unmerged`,
`ls-files -v`, `--git-path index`, `--git-path objects`, `add -A`, `ls-files --stage`,
`config core.ignorecase`, `write-tree`, `rev-parse --git-common-dir`, plus `config
core.worktree` in hook mode), an index copy and, for a real run, a second full snapshot.

## 7. Ideas for Sluice

### Take

- Private index copy via `GIT_INDEX_FILE` + `GIT_OBJECT_DIRECTORY` + alternates
  (`snapshot.py:285-291`): zero side effects on the user's index, lock or object store, and the
  tree equals the eventual commit tree so a dirty pass is reused at commit time.
- `copy2` to preserve index mtime, defeating the racily-clean edge case (`snapshot.py:280-283`).
- Honour `GIT_INDEX_FILE` through `git rev-parse --git-path index` so partial commits key on
  what is actually being committed (`snapshot.py:274`).
- Derive `GIT_WORK_TREE` from `GIT_DIR` in hooks but only for our own probes; never mutate the
  child's env (`snapshot.py:32-63`, `tests/test_gate.py:734`).
- `--no-optional-locks` on every read-only probe so a cache check never blocks on `index.lock`
  (`snapshot.py:114-119`) — essential with many agents hitting git at once.
- "Every uncertain case runs the command" (`docs/how-it-works.md:26`): unknown tree -> run
  uncached with a printed reason, never fail the gate.
- Re-snapshot after a pass and refuse to cache if the tree moved (`gate.py:130-139`): catches
  formatters and gates that leave files behind.
- Marker existence as the signal, JSON body only for reporting; `key = sha256(tree\0name)`
  documented as a public contract so shell/hook code can read markers without the tool
  (`docs/how-it-works.md:100-125`).
- Miss records (`<key>.miss`) with exit code, reason and timestamp, superseded by a later pass
  (`markers.py:65-78`, `gate.py:112-113`) — cheap, and exactly what an agent wants to query.
- Cache error policy: never alter the child's exit code (`gate.py:104-127`).
- `CI`/`DISABLE` truthiness rule and the "DISABLE=0 overrides CI" precedence (`gate.py:42-66`).
- Age pruning on write only, mtime-based, errors swallowed (`markers.py:242-257`).
- Submodule/gitlink and skip-worktree/assume-unchanged checks as "unknown" reasons
  (`snapshot.py:78-87`, `:228-244`).

### Change or reject

- No concurrency control at all: two agents on the same tree both run the gate. Sluice's
  queue/coalescing is precisely the missing piece; add an in-flight record (lease with PID +
  expiry) next to the marker.
- Key excludes env, tool versions and cwd and relies on users hand-rolling `--name`
  (`README.md:105-121`): Sluice should accept declared extra inputs (env var names, file globs
  outside git, tool version commands) and fold their hashes into the key itself.
- No key/schema version: bake a format version into the directory name or key prefix so a
  future change does not silently reuse stale markers.
- No clean-tree fast path (`snapshot.py:282-307`): Sluice can compare `HEAD^{tree}` after a
  `git status --porcelain -z` emptiness check, or use `git write-tree` on the real index when
  status is clean, before falling back to the private copy.
- Two full snapshots per real run (`gate.py:133`): fine for minute-long test suites, costly for
  sub-second linters; consider a cheaper post-run check (status emptiness vs. pre-run status).
- Python + ~10 git subprocesses per lookup: Sluice in Rust should aim at a single `git` child
  (or libgit2/gitoxide) and measure it, since gate-cache publishes no numbers.
- Marker directory inside `.git/gate-cache` with no index: `status` lists by scanning every
  file (`markers.py:219-239`). With many trees and gates this is O(files); Sluice should keep a
  small index or per-tree subdirectories.
- The skip line prints only 12 hex chars of the tree (`gate.py:166`); for agents prefer a
  machine-readable, full-hash output mode on the run path, not just on `status`.
- Markers are shared across worktrees even for dirty trees (common dir): correct, but Sluice
  should document that a pass recorded by one agent's worktree is trusted by another's.
- No notion of multiple gates per invocation or dependencies between gates: each process wraps
  one command. Sluice's named-gate config supersedes this.

## 8. Pitfalls they document

- A marker does not cover ignored files, env vars, unpinned tool versions, external services,
  the working directory, exec-bit changes under `core.fileMode=false`, or anything a clean
  filter (autocrlf, LFS) strips — stale skips are possible after such changes
  (`docs/how-it-works.md:80-90`, `README.md:216`).
- Never put secrets in a gate name: names are stored in plain text in markers and shown by
  `status`; a command line with a secret argument needs `--name` (`README.md:119`,
  `cli.py:57-64`).
- Without `--name`, each argument set is its own gate; with `--name`, variants such as
  `-k unit` share the full-suite marker (`README.md:92-103`, `docs/how-it-works.md:40-42`).
- Shell `$(...)` in names only works in a recipe, not in a hook `entry`/`args` list
  (`README.md:121`).
- Hook venv: `pre-commit`/`prek` run gate-cache in its own virtualenv; a bare tool name resolves
  there; `uv run` warns about `VIRTUAL_ENV` mismatch (`README.md:72`,
  `docs/how-it-works.md:57-76`).
- Commit-time semantics depend on the hook runner stashing unstaged work; a pass on the full
  working copy does not match a partial commit (`docs/how-it-works.md:28`,
  `tests/test_gate.py:942-953`).
- `CI` turns the cache off by default because persistent runners keep `.git` between jobs;
  Jenkins does not set `CI` (`README.md:151-167`).
- Clean filters such as Git LFS still run during the private `git add -A` and may write to their
  own stores (`docs/how-it-works.md:12`).
- A killed run can leave a scratch dir; it is swept after one hour (`docs/how-it-works.md:30`).
- Uninitialised submodules: `git submodule update --init` fills the directory without changing
  the tree, so an earlier pass would wrongly match; hence "unknown" (`snapshot.py:163-166`).
- Case-insensitive filesystems: a case-only rename keeps the old index entry
  (`snapshot.py:247-252`).
- Never wrap checks that depend on the commit message or branch name — the tree does not include
  them (`cli.py:63-64`, `docs/how-it-works.md:92`).
- A marker written by another tool without JSON content still skips but is invisible to
  `status` (`docs/how-it-works.md:125`).
