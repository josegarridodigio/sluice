---
title: "Hook managers: husky v9.1.7 source analysis (install, runtime, grammar, delegation, drift)"
tags: [sluice, research, hook-managers, husky, git-hooks, core-hooksPath, hook-runner, survey]
---

# Hook managers: husky v9.1.7

Source analysis of husky **v9.1.7** (`<repo>/refs/husky`, shallow clone, tag
`v9.1.7`, commit `799e84b`). The whole package is 98 lines of code: `index.js`
(25), `bin.js` (26), `husky` (22 lines of `sh`), `package.json` (25). There is
no CHANGELOG or release-notes file in the repository; the only history in it is
the docs (`docs/migrate-from-v4.md`) and the deprecation message in
`index.js:6`. All `file:line` references are relative to the husky checkout
unless prefixed with `sluice:`.

Purpose: feed the design in which Sluice becomes the entry point of a
repository's existing git hooks (clone-local `core.hooksPath`) and runs the
hook's commands as cacheable gates, delegating untouched anything it does not
fully understand.

Evidence classes used below:

- **Code**: read directly, cited with `file:line`.
- **Measured**: reproduced locally on 2026-10-28 with git 2.53.0 by copying the
  real `husky` script to `_/h` and writing the stubs exactly as `index.js:22`
  does (no npm run, nothing installed). Scratch repos, deleted afterwards.
- **Shell/git semantics**: documented behaviour of POSIX `sh` or git, not husky
  code.
- **Inference**: marked as such.

## 1. Install mechanics

### 1.1 Entry points

`package.json:18-20` exposes the `husky` bin (`bin.js`) and `exports`
`./index.js`. Husky itself never runs by itself: something must call it. The
convention is the `prepare` npm script (`docs/how-to.md:237`, `:244`);
yarn classic uses `postinstall` because it has no `prepare`
(`docs/how-to.md:260-261`). That is the only trigger; there is no git hook,
daemon or postinstall inside husky.

`bin.js` dispatch (`bin.js:7-26`):

| invocation | effect |
|---|---|
| `husky init` | `bin.js:10-20`, see 1.4 |
| `husky add` / `set` / `uninstall` | print "command is DEPRECATED" to stderr, `exit 1` (`bin.js:22-23`) |
| `husky install` | deprecation message, then behaves like plain `husky` (`bin.js:24,26`) |
| `husky` | `i(undefined)` -> default dir `.husky` (`index.js:8`, `bin.js:26`) |
| `husky <dir>` | `i(dir)`, custom hooks dir, for example `husky sub/.husky` (`bin.js:26`, test `test/2_in-sub-dir.sh:8`) |

`bin.js:26` writes the string returned by `i()` to stdout and **never sets a
non-zero exit code**. Every failure path of `index.js` is a returned message, so
`prepare: husky` cannot fail because of husky's own guards (test
`test/4_not-git-dir.sh:8,11` expects exit 0 with no `.git`). Only `init` (can
throw on a missing/invalid `package.json`, `bin.js:12-13`) and the deprecated
verbs exit non-zero. The docs' `"prepare": "husky || true"` and
`install.mjs` (`docs/how-to.md:75`, `:84-93`) are for the case where the husky
binary itself is not installed (production install with only `dependencies`,
`docs/how-to.md:67`).

### 1.2 What `index.js` does (the whole install)

`index.js:8-25`, in order:

1. `process.env.HUSKY === '0'` -> return `'HUSKY=0 skip install'`, nothing
   written (`index.js:9`; test `test/9_husky_0.sh:7-8` expects
   `core.hooksPath` to stay empty).
2. `d.includes('..')` -> `'.. not allowed'` (`index.js:10`). Docs: "Husky
   doesn't install in parent directories (`../`) for security reasons"
   (`docs/how-to.md:116`). Absolute paths are not rejected by this check
   (inference: no other check in the file).
3. `!fs.existsSync('.git')` -> `".git can't be found"` (`index.js:11`). The
   check is relative to the **process cwd**, so husky only installs when run
   from a directory that directly contains `.git` (a directory, or the gitlink
   *file* of a worktree/submodule: `existsSync` is true for both).
4. **The only git config written**: `git config core.hooksPath <d>/_`
   (`index.js:14`), via `spawnSync('git', [...])` with the default cwd.
   - No `--global`, `--local` or `--worktree` flag: it is a plain `git config
     <key> <value>`, so it writes the repository's local config
     (`.git/config`, or `<common-dir>/config` in a linked worktree)
     (git semantics; measured: `core.hooksPath = .husky/_` appears in
     `.git/config`, section `[core]`).
   - The value is **relative** (`.husky/_` by default; `sub/.husky/_` with a
     custom dir; slash-joined with a template string, not `path.join`,
     `index.js:14`). Tests assert exactly this (`test/1_default.sh:9`,
     `test/2_in-sub-dir.sh:12`, `test/3_from-sub-dir.sh:31`).
   - `spawnSync` status `null` (git not found) -> `'git command not found'`
     (`index.js:15`, test `test/5_git_command_not_found.sh`); non-zero status
     -> returns git's stderr (`index.js:16`). Neither fails the process.
5. Files (`index.js:18-23`), all under `<d>/_/`:
   - `rmSync(_/husky.sh, {force})` then `mkdirSync(_, {recursive})`
     (`:18-19`).
   - `_/.gitignore` containing exactly `*` (`:20`): the whole generated
     directory, including its own `.gitignore`, is untracked by construction.
   - `_/h` = byte copy of the package file `husky` (`:21`), no mode set
     (so not necessarily executable; it is only ever sourced).
   - One stub per hook in the list `l` (`index.js:5`), mode `0o755` (`:22`),
     content (no trailing newline):
     ```
     #!/usr/bin/env sh
     . "$(dirname "$0")/h"
     ```
   - `_/husky.sh` = an `echo "husky - DEPRECATED ..."` one-liner (`:6`, `:23`),
     kept so that v8-style hook files that still source `_/husky.sh` keep
     working with a warning (test `test/12_deprecated.sh:8-14` expects exit
     0).
6. Return `''`.

Hook list `l` (`index.js:5`), 14 names: `pre-commit`, `pre-merge-commit`,
`prepare-commit-msg`, `commit-msg`, `post-commit`, `applypatch-msg`,
`pre-applypatch`, `post-applypatch`, `pre-rebase`, `post-rewrite`,
`post-checkout`, `post-merge`, `pre-push`, `pre-auto-gc`. Not generated (so
silently dead under husky, because `core.hooksPath` replaces the default hooks
directory): `post-index-change`, `reference-transaction`, `push-to-checkout`,
`fsmonitor-watchman`, `sendemail-validate`, `update`, `pre-receive`,
`post-receive`, `post-update`, `proc-receive` (git hook names from git
semantics; absence from `l` is code).

Not touched: `.git/hooks/*` (bypassed, not deleted; the troubleshooting doc
confirms they stop working while `core.hooksPath` is set and need
`git config --unset core.hooksPath` after uninstalling,
`docs/troubleshoot.md:15`), the user's hook files in `<d>/`, `package.json`
(except `init`), global config, any file outside `<d>/_`.

### 1.3 When is it rewritten

Every time the `husky` command runs; there is no "already installed" shortcut
and no version/content comparison (`index.js:14-23` unconditional after the
three guards). In practice: each `npm install` / `npm ci` / `pnpm install` /
`yarn install` that triggers `prepare` (or `postinstall` for yarn,
`docs/how-to.md:260-261`), and `npm run prepare` by hand
(`docs/how-to.md:279-292`). Conditions that **skip** it:

| condition | code | note |
|---|---|---|
| `HUSKY=0` in the environment | `index.js:9` | the documented CI/Docker switch (`docs/how-to.md:59-65`) |
| dir contains `..` | `index.js:10` | |
| no `.git` entry in cwd | `index.js:11` | monorepo with the package in a subdirectory: must `cd ..` first (`docs/how-to.md:116-130`) |
| git binary missing / `git config` fails | `index.js:15-16` | partial: nothing else is written |

Not checked anywhere: `CI`, `NODE_ENV`, the git version (the docs say "above
2.9", `docs/troubleshoot.md:11`, but no code tests it), whether
`core.hooksPath` is already set. `CI=true` is only used inside the *user-written*
example `install.mjs` (`docs/how-to.md:84-93`).

Stale generated files are not cleaned: only `husky.sh` is removed before
rewriting (`index.js:18`); stubs of older husky versions stay (inference from
absence of other `rmSync`).

### 1.4 `husky init`

`bin.js:10-20`: reads `package.json`, sets `scripts.prepare = 'husky'`
**overwriting any existing `prepare`** (`:14`), rewrites the file keeping tab or
2-space indent (`:15`), calls `i()` (`:16`), creates `.husky` (`:17`), and
writes `.husky/pre-commit` containing `<pm> test\n` where `<pm>` is the first
segment of `npm_config_user_agent` (default `npm`) (`:18`). The write is
unconditional, so an existing `.husky/pre-commit` is overwritten. `init` is the
only code path that creates a user hook file; the husky repo's own hook is one
line, `./test.sh` (`.husky/pre-commit:1`).

### 1.5 Existing `core.hooksPath` pointing elsewhere

Overwritten, silently. husky never reads the key (`index.js:14` only sets it) and
prints nothing. Measured: running the equivalent `git config core.hooksPath
.husky/_` on a repo whose local value was something else replaces it with no
output. The only way a previous value survives is if it lives in a *different
config file* than the local one (see 5.3).

### 1.6 Worktrees and submodules

- **Linked worktree** (`.git` is a file): `existsSync('.git')` is true
  (`index.js:11`), `git config` writes the **common** config, so the value
  `.husky/_` is shared by the main checkout and every linked worktree
  (measured: `git config core.hooksPath` in the worktree prints `.husky/_`).
  Because the value is relative and git resolves it against the worktree's
  root, each worktree needs its **own** `.husky/_` directory. `_` is untracked
  (`index.js:20`), so a fresh `git worktree add` has `.husky/<hook>` (tracked)
  but **no** `.husky/_`. Measured: `git commit` in such a worktree runs **no
  hook at all and prints nothing** (git ignores a missing hooks dir).
  Husky protection only appears after `husky` runs inside that worktree.
- **Submodule** (`.git` is a gitlink file): `existsSync` true; `git config`
  from the submodule's cwd writes the submodule's own config
  (`<super>/.git/modules/<name>/config`) (git semantics, inference: not
  exercised by husky tests). Running husky at the superproject root never
  configures a submodule's hooks.
- Windows: `index.js:13` uses `path.join` (backslashes) for file operations but
  a forward-slash template for the config value (`:14`). Noted only.

## 2. Runtime flow when git runs a hook

1. Git looks up `core.hooksPath` (relative to the worktree root for hooks that
   run in a worktree; git semantics), finds `.husky/_/<hook>`, executes it with
   the hook's arguments, stdin and cwd = worktree top-level (git semantics;
   measured `pwd` = repo root). The stub has an `env sh` shebang and
   `$0` is the path git used.
2. Stub: `. "$(dirname "$0")/h"` (`index.js:22`): sources `h` in the stub's
   shell, so `$0` and `$@` in `h` are the stub's (`husky:3-17`).
3. `h` (the `husky` script):
   - `[ "$HUSKY" = "2" ] && set -x` (`husky:2`): `HUSKY=2` is the debug trace
     (stderr).
   - `n=$(basename "$0")` = the **hook name**; `s=$(dirname "$(dirname
     "$0")")/$n` = the **user hook file** one level above `_`, e.g.
     `.husky/pre-commit` (`husky:3-4`). Consequence (measured): executing
     `_/h` itself directly makes `n=h`, `s=.husky/h`, which does not exist ->
     silent `exit 0`.
   - `[ ! -f "$s" ] && exit 0` (`husky:6`): **a missing user hook file is a
     silent success**, before init.sh and before the `HUSKY=0` check.
   - `~/.huskyrc` present -> prints a deprecation line (`husky:8-10`).
   - `i="${XDG_CONFIG_HOME:-$HOME/.config}/husky/init.sh"`; if it is a file it
     is **sourced** (`husky:11-12`), in the same shell, with `n`, `s`, `i`
     (shell variables, not exported), `$@`, `$0` visible. Anything it
     `export`s reaches the user hook; anything it `exit`s ends the hook;
     `export HUSKY=0` in it disables the hook (test `test/9_husky_0.sh:21-22`);
     `set -u` in it must not break the script (test `test/8_set_u.sh`, which is
     why the later line uses `${HUSKY-}`). init.sh is arbitrary shell
     (docs: nvm/zsh sourcing, `docs/how-to.md:216-231`), so its effects are not
     statically knowable.
   - `[ "${HUSKY-}" = "0" ] && exit 0` (`husky:14`): runtime kill switch,
     evaluated **after** init.sh (so init.sh can set it, test `:21-22`).
   - `export PATH="node_modules/.bin:$PATH"` (`husky:16`): a **relative**
     entry, prepended. This is what puts project binaries first and what
     bypasses PATH-based shims (see Sluice context in section 7). Test
     `test/7_node_modules_path.sh:11` asserts it. Measured: the stub invoked
     with cwd `/` still shows `node_modules/.bin` as the first PATH entry, so
     it resolves against whatever cwd the commands run in.
   - `sh -e "$s" "$@"` (`husky:17`): the user's file is run by **`sh -e`**
     regardless of its shebang (the shebang line is a comment to `sh`; a
     `#!/bin/bash` file is therefore interpreted by `sh`: inference from
     `husky:17`, plus shell semantics), as a child process with the hook's
     **arguments** (`$1`, `$2`, ... replace v4's `HUSKY_GIT_PARAMS`,
     `docs/migrate-from-v4.md:43-56`), **stdin inherited** (measured:
     `pre-push` receives its ref lines on stdin through stub, `h` and the user
     file: `cat` in the hook printed them) and cwd unchanged.
   - `c=$?`; if non-zero: `echo "husky - $n script failed (code $c)"`
     (`husky:20`); if 127 additionally `echo "husky - command not found in
     PATH=$PATH"` (`husky:21`). The messages go to **stdout**. Then `exit $c`
     (`husky:22`): the user script's exit code is the hook's exit code
     (test `test/1_default.sh:14`; `test/6_command_not_found.sh:13-14`: user
     `exit 127` -> `git commit` fails).
4. Env visible to the user script: everything git sets, plus `PATH` with
   `node_modules/.bin` first, plus whatever init.sh exported. `n`, `s`, `i`
   are not exported (they are shell variables of the sourcing shell).

Because `sh -e` runs the file, **errexit applies**, with the usual exceptions
(shell semantics): a failing command that is a non-final member of an AND-OR
list, inside a condition, or negated does not abort. Measured: a file
`false && true` followed by another `echo` line runs the second line and exits
0. This matters for `npm test && npm run foo` style lines (the v4 migration
doc even recommends splitting them into separate lines,
`docs/migrate-from-v4.md:11-22`).

Git semantics, not husky: for `pre-*`, `commit-msg`, `prepare-commit-msg`,
`applypatch-msg`, `pre-applypatch` a non-zero exit aborts the operation; for
`post-*` hooks it is ignored (git documentation, not exercised by husky
tests). `git commit --no-verify` skips `pre-commit` and `commit-msg` (docs:
`-n/--no-verify`, `docs/how-to.md:24`).

Husky does not wrap or buffer stdout/stderr, does not run hooks in the
background, and does not parse the hook file at all.

## 3. Hook file semantics and a safe grammar

### 3.1 What a `.husky/<hook>` file can contain

Anything `sh -e` can run (`husky:17`): the docs show one-line `npm test`
(`docs/how-to.md:3-8`), several commands on separate lines
(`docs/migrate-from-v4.md:11-22`), command substitution, pipes and `sed`
(`docs/get-started.md:64-74`: `prettier $(git diff --cached --name-only
--diff-filter=ACMR | sed ...) --write ...` followed by `git update-index
--again`), a `cd frontend` for a monorepo (`docs/how-to.md:137`), `node
.husky/pre-commit.js` for non-shell logic (`docs/how-to.md:141-158`), a here-doc
that hands a script to `bash` (`docs/how-to.md:160-173`), `exit 1` to test
(`docs/how-to.md:96-106`), `. .husky/common.sh` to source a helper
(`docs/troubleshoot.md:21-38`), `exec < /dev/tty` (same place). No format is
imposed, no schema.

The v9 migration also leaves legacy first lines in many repos
(`#!/usr/bin/env sh` and `. "$(dirname -- "$0")/_/husky.sh"`); both are
harmless in v9 (the latter only echoes a deprecation text, `index.js:6`,
`:23`; "They WILL FAIL in v10.0.0").

### 3.2 Constructs and what a line-by-line runner can do with them

| construct | classification | why |
|---|---|---|
| blank line, `# comment`, shebang `#!/bin/sh` or `#!/usr/bin/env sh` | skip | no effect under `sh -e` |
| v8 bootstrap `. "$(dirname [--] "$0")/_/husky.sh"` | skip (exact match) | only prints a deprecation notice in v9 (`index.js:6,23`) |
| `set -e` | skip (exact) | already in force (`husky:17`) |
| `npm run x`, `npx lint-staged`, `pnpm test`, `bun run x`, `node s.js`, `git add f` with literal args | **simple command: understood** | one process, no shell features |
| positional args `$1`..`$9`, `"$@"` as whole words | understood, with hook arguments in the key | hook-specific input (for `commit-msg` the message file path; its *content* is not in the tree key) |
| `FOO=1 cmd` | extension (literal assignments only) | changes the environment of one command |
| trailing `exit 0` | extension (last line only) | no effect on result |
| `cd <literal dir>` | extension (static cwd state) | changes cwd of all following lines (monorepo pattern) |
| `a && b`, `a \|\| b` | delegate (or extension, see 3.4) | errexit does not apply to non-final members (measured), authors usually assume it does |
| `a \| b`, redirections `> < >> 2>&1`, `&`, `;` | delegate | pipelines/redirects/background, multiple commands on a line |
| `$(...)`, backticks, `$VAR`, `${...}`, globs `* ? [`, `~`, `!` | delegate | values depend on runtime state (e.g. `git diff --cached --name-only`) |
| `if/then/else/fi`, `for/while/until/do/done`, `case/esac`, `{ }`, `( )`, functions | delegate | control flow; the set of executed commands is data-dependent |
| `while read local_ref local_sha ...` (pre-push) | delegate | consumes **stdin**, the only channel of pre-push's input |
| variable assignment `x=...`, `export`, `unset`, `set -u/-o pipefail`, `eval`, `exec`, `trap`, `read`, `shift`, `.`/`source` | delegate | changes shell state or executes unknown text |
| here-doc `<<`, line continuation `\` at end of line, multi-line quotes | delegate | one logical command spans lines |
| shebang other than `sh` (bash, node, python) | delegate | author intent is not `sh -e`; husky ignores it, but a runner should not guess |

### 3.3 Proposed conservative grammar (everything else -> delegate)

A hook file is **understood** if and only if it has at least one command and
every physical line is one of:

1. empty or whitespace only;
2. a comment (`#` first non-blank character), including a first-line shebang
   restricted to `#!/bin/sh` or `#!/usr/bin/env sh` (any other shebang
   delegates the file);
3. the exact v8 bootstrap line (both `dirname "$0"` and `dirname -- "$0"`
   spellings);
4. the exact line `set -e`;
5. a **simple command**: one or more words separated by blanks, where a word is
   - bare: characters in `[A-Za-z0-9_@%+=:,./-]`, and the first word does not
     match `NAME=`;
   - single-quoted: `'...'` with no embedded quote;
   - double-quoted: `"..."` containing none of `$ \` \ !`;
   - or exactly `$1`..`$9`, `"$1"`..`"$9"`, `"$@"`;
   and the first word is not a shell reserved word or a builtin other than
   `echo`, `printf`, `true`, `:` (treated as trivial, run live, never a gate).

There is **no** operator, no multi-line construct, no substitution, no
variable. Lines are independent: a simple command that fails stops the hook
(errexit; a simple command is not in an exempt context, shell semantics), so
the runner reproduces `sh -e` exactly by running the lines in order and
stopping at the first non-zero exit. The runner must then also reproduce what
`h` adds around it (section 4).

Optional extensions, each independently decidable and each needing its own
cache-key rule: E1 trailing `exit 0`; E2 literal `NAME=value` prefixes; E3
`cd <literal relative dir>`; E4 `&&`-only lists, modelled as "run left; run
right only if left succeeded; the list's status counts only if it is the final
command of the file".

### 3.4 Survey of real files (calibration)

Method. `gh search code --filename pre-commit --match path ".husky"` and the
same for `pre-push`, 100 results each (the maximum of one query), then fetched
`https://raw.githubusercontent.com/<repo>/HEAD/<path>` on 2026-10-28:
**199 files** (99 `pre-commit`, 100 `pre-push`; one `pre-commit` result was a
different filename and was dropped). The first 25 lines of each were read in a
condensed dump, and the grammar of 3.3 (without extensions) was applied with a
throw-away script (not committed). Bias to keep in mind: GitHub ranks results
by its own relevance, the sample is one query per hook, almost all repos are
JS/TS projects, and no repository was deduplicated by organisation. Counts are
heuristic (for example `echo` lines count as commands, a trailing `exit 0`
makes a file delegate).

| | pre-commit (99) | pre-push (100) | total (199) |
|---|---|---|---|
| fully understood by the 3.3 grammar | 54 | 51 | **105 (53%)** |
| delegate | 44 | 47 | **91 (46%)** |
| no command (comment-only) | 1 | 2 | 3 |

Shape of the 105 understood files: 67 have one command, 27 two, 6 three, 4
four, 1 six. 28 of them run a staged-files tool (`lint-staged`,
`pretty-quick`, `nano-staged`) and 4 run `git add` after other commands, i.e.
index-modifying steps do occur inside otherwise plain files. Most common
commands: `npm run <script>` (40), `npx lint-staged` (13), `pnpm run` (7),
`bun run` (5), `node <script>` (5), `echo` (5), `git <cmd>` (4).

Examples, understood (repository: file content):
- `AlexGarrixen/Cool-Contrast` pre-commit: `npm run lint:fix`, `git add .`
- `EndBug/add-and-commit` pre-commit: `npm run test`, `npm run build`,
  `git add lib`
- `yousufkalim/html-to-json` pre-commit: `npx lint-staged`, `npm run test`,
  `npm run build`, `git add dist/`
- `SAP/guided-answers-extension` pre-commit: `pnpm pretty-quick --staged
  --ignore-path .prettierignore`, `pnpm audit`
- `gravity-ui/aikit` pre-commit: `npx nano-staged`, `node
  scripts/generate-exports.js --check`
- `Tardo/OdooTerminal` pre-push: `pnpm run prod:build`, `pnpm run dev:web-ext`
- `duplojs/zod-to-typescript` pre-push: `npm run build`, `npm run test:lint`,
  `npm run test:types`, `npm run test:tu`
- `aliou/pi-processes` pre-commit: `pnpm install --frozen-lockfile --silent`,
  `pnpm typecheck`, `pnpm lint`, `pnpm check:changesets`

Delegate reasons (a file can have several; counts are files):

| reason | files | examples |
|---|---|---|
| control flow or functions (`if/fi/else/while/case/done`, `{ }`) | ~46 as the primary cause (`fi` appears in 44 files) | `S4tvara/Sietch` pre-push (`if ! command -v go ...`), `dundring/trainer` pre-push (function `exit_with_warning_message() {`), `ckt1031/obsidian-wordwise-plugin` pre-push (`while read local_ref ...`) |
| `&&` / `\|\|` chains | 26 any, 17 as the only reason | `Azure/k8s-deploy` pre-commit (`npm run typecheck \|\| {`), `Coly010/ng-rspack-build` pre-push (`pnpm check-format &&`) |
| command substitution `$(...)` | 25 | `0xPolygon/static` pre-push (`branch=$(git rev-parse --abbrev-ref HEAD)`), `Kastalien-Research/thoughtbox` pre-commit |
| variables / `$VAR` in quotes | 24 | `EC-CUBE/ec-cube` pre-commit (`[ "$HUSKY" = "0" ] && exit 0`), `DiegoFleitas/letterboxd-movie-justwatch` pre-push (`BUN_INSTALL="${BUN_INSTALL:-$HOME/.bun}"`) |
| `NAME=value` assignment | 17 | `aikirun/aiki` pre-push (`zero_sha="000..."`), `fiscozen/design_system` pre-push (`SKIP_CHANGESET=0`) |
| `;` on a line (`if ...; then`) | 23 | `alchemy-fr/phrasea` pre-commit |
| pipes | 6 | `inferablehq/inferable` pre-commit (`git diff --cached --name-only --diff-filter=ACMR \| grep -E ... \| xargs ...`), `govuk-one-login/txma-audit` |
| non-`sh` shebang (`bash`) | 8 | `dxos/dxos` pre-push (`set -euo pipefail`) |
| `set -eu`, `unset GIT_DIR`, `.`/`source` | ~8 | `nextlyhq/nextly` pre-commit (`unset GIT_DIR GIT_WORK_TREE`), `Vonage/vonage-video-react-app` |
| globs / line continuation | 5 | `archway-network/arch3.js` pre-push (`git diff --quiet -- **/*.ts`), `yjl9903/AnimeGarden` pre-commit |
| pre-push reading stdin (`while read`) | 7 | `ckt1031/obsidian-wordwise-plugin` |

What each extension would add (same sample): E1+E2 flip 4 files (trailing
`exit 0`, literal `NAME=` prefixes); modelling `&&` lists (E4) flips 17 more.
So 105/199 (53%) with the base grammar, about 122/199 (61%) with E1, E2 and
E4. The remaining ~39% are real shell programs (conditions on branch name,
tools probing, here-doc, functions, stdin readers) that only delegation can
serve faithfully. Cross-cutting: 52 of the 199 files mention `lint-staged`,
`pretty-quick` or `nano-staged`; 16 contain `git add`.

Also note (not measured by the script): wrappers such as `npm run <x>` and
`yarn <x>` hide arbitrary compound commands inside `package.json` scripts, so
"understood" at the hook level does not mean "one process"; Sluice already has
`sluice_gate_core::pkgscripts` for that (`sluice: crates/sluice-gate/src/init/hooks.rs:11`).

## 4. Delegation path

Two ways to hand a hook back to husky, with the facts from section 2.

**A. Call the generated stub**: `<repo>/.husky/_/<hook> "$@"` with stdin and
cwd inherited from git (cwd = worktree top-level). Everything husky does
happens: init.sh (nvm etc.), `HUSKY=0`/`=2`, relative PATH prepend,
`sh -e <user file>` with the same arguments, the failure and 127 messages,
the exit code. Measured: the stub works from any location of itself (it uses
`dirname "$0"`, `husky:3-4`), forwards stdin (the pre-push ref lines arrived
intact) and arguments. Requirements and caveats:
- The directory `.husky/_` must **exist** in that checkout. It is untracked
  (`index.js:20`) and absent in a fresh clone before `prepare` ran and in
  every new linked worktree (measured, 1.6). If missing, A is impossible
  without generating it (which would write into the user's working tree).
- The stub must be called as `<hook>` by name; the shared `h` cannot be called
  (measured: silent exit 0, `husky:3-6`).
- cwd must be the checkout root for the relative `node_modules/.bin`
  (`husky:16`) to mean the right thing; with `cd frontend` inside the hook
  (`docs/how-to.md:137`) the relative PATH entry then resolves under
  `frontend/` (inference: relative PATH entries are resolved against the
  current directory at lookup time).
- A hook named in `.husky/` but missing from list `l` (`index.js:5`) has no
  stub: nothing to call, and under husky it never ran. If Sluice installs
  stubs for more hook names than husky does, it would start running files
  husky never ran (parity constraint).
- Exit code semantics are husky's: Sluice must propagate the exit code
  unchanged.

**B. Run the user file itself**: `sh -e <repo>/.husky/<hook> "$@"`. Loses
everything `h` adds: init.sh sourcing, `HUSKY=0` (both env and from init.sh),
`HUSKY=2` tracing, the PATH prepend, the "script failed (code N)" and "command
not found in PATH" messages. To be equivalent Sluice would have to re-implement
`husky:2-22`, including sourcing an arbitrary user init.sh in a shell
(possible only by running `sh` with the same prologue, which is delegating A
in all but name). B is only defensible as "what husky would run" for caching
decisions, never as the executor of a hook Sluice does not fully understand.

Conclusion from the code: A is the only full-fidelity path; B is a partial
re-implementation.

What a runner that *does* understand a file must replicate to stay faithful
(from `husky:2-22`): honour `HUSKY=0` (and cannot know init.sh's effect without
sourcing it), set PATH `node_modules/.bin:$PATH` (relative), run with `-e`
semantics (stop on first failing simple command), pass the hook arguments,
keep stdin available (pre-push), print husky-compatible failure messages if
parity of output is wanted (they are on stdout).

## 5. Re-activation and drift

### 5.1 Exact code path where husky overwrites `core.hooksPath`

Only `index.js:14` (`spawnSync('git', ['config', 'core.hooksPath', `${d}/_`])`),
reached on every husky run that passes the guards `index.js:9-11` (HUSKY=0,
`..`, no `.git` in cwd). Triggers: `npm install`, `npm ci`, `pnpm install`,
`yarn install` (through `prepare`/`postinstall`, `docs/how-to.md:244-261`),
`npm run prepare`, `npx husky`, `husky init` (`bin.js:16`). husky never reads
or compares the previous value, so a Sluice-owned local `core.hooksPath` is
replaced on the next install, with no message. (A husky repository used with
Sluice already showed `npm install` resetting `hooksPath`; consistent with
this code.)

### 5.2 What husky checks at install time

Only: `HUSKY` env, `..` in the dir argument, `.git` presence in cwd, git
availability. It does not check the current `core.hooksPath`, the git
version, `CI`, ownership, or whether the dir is inside the repository.

### 5.3 Strategies derived from the code and from measurements

1. **Detect and re-assert** (inference): since the husky value is always the
   literal `<d>/_` (default `.husky/_`), a Sluice command that runs often
   (`doctor`, any `sluice run`, a daemon tick, or a Sluice stub invoked
   through a different path) can compare `git config --local core.hooksPath`
   to the expected value and re-write it. Cost: a window between `npm install`
   and the next Sluice invocation in which hooks bypass Sluice (but still run
   husky normally, so no functional loss, only cache loss).
2. **Chaining instead of replacing**: Sluice's hooks directory contains stubs
   that, for hooks Sluice delegates, execute `<repo>/.husky/_/<hook> "$@"`
   (path A in section 4). This does not stop husky from replacing
   `core.hooksPath`, it only avoids Sluice *breaking* husky while it is the
   entry.
3. **A config file that wins over the local `[core]` entry** (measured with git
   2.53.0): with `[include] path = sluice.cfg` appended at the end of
   `.git/config` and `core.hooksPath = <sluice dir>` inside `sluice.cfg`,
   `git config core.hooksPath` (read) returns the include's value, and after
   running husky's exact write (`git config core.hooksPath .husky/_`) the
   local file gets `.husky/_` but the effective value, and
   `git rev-parse --git-path hooks`, remained the included file's value
   (last value wins, includes are expanded in place). Husky's write therefore
   does not dislodge it. Caveats: relies on git's last-wins ordering and on
   `.git/config` (or the common config) keeping the `[include]` at the end;
   anything that rewrites `.git/config` wholesale (some GUIs, `git config
   --edit`) could drop it; `git config --unset core.hooksPath` run by husky
   uninstall docs (`docs/troubleshoot.md:15`) would only unset the local entry.
   `git worktree` shares the common config (1.6), so one include covers all
   worktrees. This touches only the clone's `.git/config`, not the index or the
   working tree.
4. **Per-worktree config** (`extensions.worktreeConfig` +
   `config.worktree`) is not touched by `git config core.hooksPath`'s default
   scope; **not tested**, noted as inference only.
5. **Global / system `core.hooksPath` is useless against husky**: the local
   value husky writes (`index.js:14`) takes precedence over global/system
   (git config precedence, git semantics).
6. **Hooking inside husky itself**: the generated stubs and `_/h` are rewritten
   by every run (`index.js:18-23`), so editing them is futile. The sanctioned
   extension point is the user-level `init.sh` (`husky:11-12`): it is sourced in
   the hook's own shell before anything runs and could `exec` another program
   with `$n` (hook name) and `$@`. It lives outside the repository but inside
   the user's `$XDG_CONFIG_HOME`/`~/.config`, is shared by every husky repo of
   that user, and runs only if `.husky/<hook>` exists (`husky:6` precedes it).
   Mentioned as a fact of the mechanism; whether Sluice should touch user-level
   config is a separate policy question.

## 6. Risks and edge cases

- **`HUSKY=0`**: at install (`index.js:9`) leaves `core.hooksPath` untouched
  (so Sluice's value survives a CI/Docker `npm ci` under `HUSKY=0`), and at
  run time (`husky:14`) makes every hook an `exit 0`. A Sluice runner that
  bypasses the stub must apply the same rule itself, and it can be set by
  init.sh (`test/9_husky_0.sh:21-22`), which Sluice cannot see statically.
- **CI**: husky does not look at `CI`; the docs rely on `HUSKY=0` in the CI
  environment (`docs/how-to.md:59-65`). Hooks of a repo may also self-guard
  with `[ -n "$CI" ] && exit 0` (2 files in the sample) or
  `[ "$HUSKY" = "0" ] && exit 0` (2 files).
- **`prepare` failing**: husky itself never fails the script (1.1); a failing
  `prepare` for other reasons (husky not installed in a prod install,
  `docs/how-to.md:67`) leaves whatever `core.hooksPath` was there.
- **`git commit --no-verify` / `-n`**: git skips `pre-commit` and `commit-msg`
  entirely, so the stub, husky and Sluice are never entered; `pre-push
  --no-verify` the same. Commands with no such flag are bypassed with `HUSKY=0
  git ...` (`docs/how-to.md:30-33`), which only affects husky's stub.
- **GUI clients**: GUIs start without the user's shell PATH; husky documents
  `init.sh` as the fix (`docs/how-to.md:175-231`). A Sluice entry point
  replacing the stub needs the same PATH/nvm bootstrap or must delegate to the
  stub (path A) so init.sh still runs.
- **Windows / git bash**: the stubs are `sh` scripts and `init.sh` is under
  `C:\Users\<user>\.config\husky\` (`docs/how-to.md:18`); yarn on git bash
  needs a tty workaround (`docs/troubleshoot.md:17-38`). Noted only.
- **Monorepo with husky in a subdirectory**: husky only installs when cwd has
  `.git` (`index.js:11`), so the documented recipe is
  `"prepare": "cd .. && husky frontend/.husky"` (`docs/how-to.md:127-130`) and
  the hook begins with `cd frontend` (`docs/how-to.md:133-138`). The resulting
  value is `frontend/.husky/_` (`index.js:14`); the hook then changes cwd,
  which a line-by-line runner must model (E3) or delegate. Hooks files and
  `_` live in a subdirectory; `.husky` at the root is not guaranteed
  (Sluice's current code only looks at `<root>/.husky`, `sluice:
  crates/sluice-gate/src/init/hooks.rs:306,320`).
- **Worktrees**: shared config, per-worktree `_` that does not exist until
  husky runs there (1.6, measured): a linked worktree has no hooks at all
  (husky or Sluice-through-stub) unless Sluice is the entry via a path that
  does not depend on `_`.
- **Custom hooks dir**: `core.hooksPath` may be `<anything>/_`; detection must
  not assume `.husky/_` (`bin.js:26`, `test/2_in-sub-dir.sh`).
- **v8 repos**: a repository still on husky 8 (hooks that source
  `_/husky.sh`, no `h`, hooksPath `.husky`) has a different layout; this
  analysis covers v9 only. The v9 file `_/husky.sh` exists only as a
  deprecation shim (`index.js:6,23`).
- **`echo`/messages on stdout**: failure messages are written to stdout
  (`husky:20-21`), which matters if Sluice captures hook output.
- **`sh` is not bash**: husky runs the file with `sh -e` (`husky:17`); on
  Debian/Ubuntu `sh` is dash. Hook files with bashisms that "work" because
  their shebang says bash (8 of 199 in the sample) are interpreted by `sh`
  under husky (inference).
- **Missing hook file**: silent success (`husky:6`); under a Sluice entry
  point, "no `.husky/<hook>`" must also mean "run nothing".

## 7. Sluice context (read, not changed)

- `sluice: crates/sluice-gate/src/init/hooks.rs:306-312` already reads
  `<root>/.husky/<hook>` as `Format::Script` with `HookManager::Husky`, and
  `:319-320` skips directories under `.husky` when scanning the effective
  hooks dir ("Husky 9 points core.hooksPath at its own generated `.husky/_`").
  `:403-409` skips v8 `. ` / `source ` bootstrap lines, in line with grammar
  item 3.
- `sluice: crates/sluice-gate/src/doctor.rs:1121` warns "no hook in <dir> calls
  sluice ... ignore this if a hook manager owns the hooks", which is the
  situation produced by `index.js:14`.
- PATH shims are bypassed because `husky:16` prepends a relative
  `node_modules/.bin`, ahead of any shim directory inherited from the agent's
  environment (and `init.sh` can rewrite PATH again before it, `husky:12`).

## 8. Implications for the Sluice design

Facts to design constraints, no further recommendation.

1. Husky's `core.hooksPath` is written unconditionally by every `husky` run
   (`index.js:14`), never read, never warned about. Any Sluice-owned
   `hooksPath` equal to the local `[core]` value is volatile: Sluice needs a
   drift check (compare to expected, re-assert) or a mechanism that survives
   the write (the `[include]` precedence measured in 5.3).
2. The generated stubs are location independent and idempotent to call
   (`husky:3-4`): `.husky/_/<hook> "$@"` is a complete, faithful delegation
   (init.sh, HUSKY switch, PATH, `sh -e`, messages, exit code), provided
   `.husky/_` exists and cwd is the checkout root. `_` is untracked, so Sluice
   must expect it to be missing (fresh clones, new worktrees, before first
   `npm install`) and decide what delegation means then (run user file with
   `sh -e` as a degraded path, or do nothing, as husky itself would).
3. A hook file missing -> husky exits 0 silently (`husky:6`); hook names outside
   the 14 in `index.js:5` never ran under husky. Sluice must not make more
   hooks run, or run them differently, than husky did.
4. Hook files are arbitrary `sh -e` scripts run by `sh`, not by their shebang.
   In a 199-file sample about half are one or two plain commands and half need
   real shell semantics. The base grammar (3.3) accepts 53%; the rest must be
   delegated whole. Partial understanding of a file is not safe because of
   errexit exceptions (`a && b` mid-file does not stop the script, measured),
   control flow, `cd`, and stdin consumption in pre-push.
5. A runner that executes lines itself takes on husky's responsibilities
   (`husky:2-22`): HUSKY=0 from env **and from an init.sh it cannot analyse**,
   the relative PATH prepend, `-e` semantics, arguments, stdin for pre-push,
   stdout failure messages. Each is either replicated explicitly or the whole
   hook is delegated.
6. Index-modifying steps (`lint-staged`, `pretty-quick --staged`,
   `nano-staged`, `git add ...`) are common even in the "simple" files (28 of
   105 run a staged-files tool, 4 run `git add`); a runner must classify them
   and order them first/uncached as the design says, and must treat any file
   that mixes them with unparsed constructs as delegate.
7. Worktrees share the config (so a clone-local Sluice `hooksPath` covers them)
   but not `.husky/_`; monorepo installs put the hooks dir in a subdirectory
   and `cd` inside the hook. Neither can be assumed away: dir discovery must
   follow the effective `core.hooksPath`, not `.husky` at the root.
8. `--no-verify` and `HUSKY=0 git ...` are bypasses that no hook-side design
   can intercept; GUI clients run the stub without the user's shell PATH,
   which is why husky has `init.sh`: a Sluice entry point that does not go
   through the stub must supply an equivalent.
