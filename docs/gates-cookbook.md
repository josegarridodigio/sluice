# Gates cookbook

Ready-to-adapt `.sluice.toml` files per stack. Every example uses only fields
that exist today (see [docs/configuration.md](configuration.md)): `cmd`,
`weight`, `timeout`, `hooks`, `key_inputs`, `key_files`, `env`, `cache`,
`mode`, `paths`, `paths_ignore`, `depends_on` and `[queue]`.

## Rules of thumb

- **Weight is roughly the number of CPU threads the gate uses.** The daemon
  budget defaults to `max(2, CPUs - 1)`; a gate of weight 4 takes 4 units
  while it runs. A single-threaded linter is `weight = 1`; a test runner
  started with `--maxWorkers=4` or `-n 4` is `weight = 4`. Pin the tool's
  thread count in `cmd` so the weight is true.
- **`key_inputs` for versions.** The cache key covers the git tree (tracked
  and untracked, non-ignored files), the gate name, `cmd` and `env`. It does
  not know which `node`, `python3`, `rustc`, `go` or base image runs the
  command. Add one `key_inputs` command per tool whose upgrade should
  invalidate a pass. They run on every cache check, hits included, so keep
  them fast.
- **`key_files` for ignored inputs.** Files that git ignores but the gate
  reads (`.env.test`, a lockfile you do not commit) are invisible to the tree
  hash. List them, relative to the repository root.
- **`cache = false`** when the gate's value is its side effect, not its
  verdict: builds whose output you need, checks against live services,
  anything whose result depends on state that no file or command output can
  capture. The gate is still queued under the budget.
- Keep gates non-mutating. A gate that exits 0 but changes the tree (an
  auto-formatter) is not cached; use the check mode of the tool.

## Node

npm, pnpm and yarn differ only in the command. The example uses pnpm; swap
`pnpm run lint` for `npm run lint` or `yarn lint` as needed.

```toml
version = 1

[gates.lint]
cmd = "pnpm run lint"
weight = 1
timeout = "5m"
hooks = ["pre-commit"]
key_inputs = ["node --version", "pnpm --version"]

[gates.typecheck]
cmd = "pnpm exec tsc --noEmit"
weight = 1
timeout = "10m"
hooks = ["pre-commit"]
key_inputs = ["node --version"]

[gates.test]
cmd = "pnpm test -- --maxWorkers=4"
weight = 4
timeout = "20m"
hooks = ["pre-push"]
key_inputs = ["node --version"]
key_files = [".env.test"]
env = { NODE_ENV = "test" }
```

Notes:

- With npm use `key_inputs = ["node --version", "npm --version"]`; with yarn,
  `yarn --version`.
- If `package-lock.json`, `pnpm-lock.yaml` or `yarn.lock` is committed it is
  already part of the tree; add it to `key_files` only when your repository
  ignores it.

### husky and lefthook

Both managers just need to call Sluice. Print the snippet and paste it into
the manager's file:

```sh
sluice hook print pre-commit    # use in .husky/pre-commit or lefthook.yml
sluice hook print pre-push
```

In the alpha runs a cache hit cost 63 ms per `git commit` with husky and 17 ms
with lefthook, manager included. The gates above need no change for either.

## Python

pytest, mypy and ruff, with the pre-commit framework or plain hooks.

```toml
version = 1

[gates.ruff]
cmd = "ruff check . && ruff format --check ."
weight = 1
timeout = "5m"
hooks = ["pre-commit"]
key_inputs = ["python3 --version", "ruff --version"]

[gates.mypy]
cmd = "mypy src"
weight = 1
timeout = "15m"
hooks = ["pre-commit"]
key_inputs = ["python3 --version", "mypy --version"]

[gates.pytest]
cmd = "pytest -n 4 -q"
weight = 4
timeout = "30m"
hooks = ["pre-push"]
key_inputs = ["python3 --version", "pytest --version"]
key_files = [".env.test"]
```

Notes:

- `pytest -n 4` needs pytest-xdist. Without it, `pytest -q` is `weight = 1`.
- If you use a virtualenv, make sure the gate runs with it
  (`cmd = ".venv/bin/pytest -n 4 -q"`) and that `key_inputs` ask the same
  interpreter: `".venv/bin/python --version"`.
- With the pre-commit framework, register Sluice as a local hook (`sluice hook
  print pre-commit` shows the line). The framework stashes unstaged changes
  while it runs, so the hook sees exactly the content being committed.

## Rust

```toml
version = 1

[gates.fmt]
cmd = "cargo fmt --check"
weight = 1
timeout = "2m"
hooks = ["pre-commit"]
key_inputs = ["rustc --version"]

[gates.clippy]
cmd = "cargo clippy --workspace --all-targets -- -D warnings"
weight = 4
timeout = "20m"
hooks = ["pre-commit"]
key_inputs = ["rustc --version", "cargo clippy --version"]
env = { CARGO_BUILD_JOBS = "4" }

[gates.test]
cmd = "cargo test --workspace -- --test-threads=4"
weight = 4
timeout = "30m"
hooks = ["pre-push"]
key_inputs = ["rustc --version"]
env = { CARGO_BUILD_JOBS = "4", RUST_BACKTRACE = "1" }
```

Cargo compiles with all cores by default, so a cold `cargo clippy` or
`cargo test` can use more than its weight. `CARGO_BUILD_JOBS` in `env` caps
it so the weight stays honest (and `env` is part of the key, so changing it
invalidates passes). `rust-toolchain.toml` is tracked, so toolchain pins are
already in the tree; `rustc --version` also catches a floating `stable`.

## Go

```toml
version = 1

[gates.vet]
cmd = "go vet ./..."
weight = 2
timeout = "10m"
hooks = ["pre-commit"]
key_inputs = ["go version"]

[gates.test]
cmd = "go test -p 4 ./..."
weight = 4
timeout = "20m"
hooks = ["pre-push"]
key_inputs = ["go version"]
env = { CGO_ENABLED = "0" }
```

`go test` caches results itself, but only per `GOCACHE`; Sluice's verdict is
shared by every worktree of the repository.

## Docker builds

A Docker build depends on its base image, which is not in the tree. A tag
such as `FROM node:22-bookworm` can change under you, so fold the image id
into the key:

```toml
version = 1

[gates.docker-test]
cmd = "docker build --target test -t myapp:test ."
weight = 4
timeout = "30m"
hooks = ["pre-push"]
key_inputs = [
  "docker image inspect --format '{{.Id}}' node:22-bookworm",
  "docker --version",
]
key_files = [".env.build"]
```

This gate caches a verdict ("the image builds and its test stage passes").
`docker image inspect` fails when the base is not pulled yet; a failing
`key_inputs` command makes the run uncached (a notice on stderr), never a
failure, so the first run after a pull is simply uncached.

When you need the image itself, for instance to run or push it, do not cache
the build:

```toml
version = 1

[gates.image]
cmd = "docker build -t myapp:dev ."
weight = 4
timeout = "30m"
cache = false
```

A `cache = false` gate always runs but still goes through the queue, so two
agents do not build at the same time beyond the budget. Docker's own layer
cache still applies, which is usually what you want.

From 0.3.0, `mode = "queue-only"` says the same thing more precisely: the
gate always runs under the budget, never writes a marker or a miss record,
and two simultaneous identical requests (same tree, same gate, same extra
arguments) share one run instead of building twice. An agent's pre-commit
hook waits for it instead of failing fast (no `verify` could ever cache it).
Do not combine it with `cache = true` (that is an error); older binaries
refuse a file with `mode`.

```toml
[gates.image]
cmd = "docker build -t myapp:dev ."
weight = 4
timeout = "30m"
mode = "queue-only"
```

Docker commands that are not gates (`docker build` typed in a shell, `docker
compose build`) go through the same budget with `sluice exec` (0.3.0):

```sh
sluice exec --weight 4 -- docker build -t myapp:dev .
sluice exec --label "compose build" -- docker compose build
sluice exec --timeout 20m -- sh -c 'cd services/api && make image'
```

The command runs in the daemon under the budget with your environment and
directory; its stdout and stderr come back byte for byte, its exit code is
yours, and Ctrl-C reaches the whole build. Stdin is `/dev/null` and there is
no terminal (use `SLUICE_DISABLE=1` for an interactive run). The default
weight is half the budget, rounded up. Add `--coalesce-key <k>` to let
simultaneous identical requests (same key, command and directory) share one
run. Without a daemon it runs in-process under the same budget semaphore.

## Monorepos with several packages

By default a gate's key is the whole tree: a change in any package re-runs
every gate. Give each package its own gate and, from Sluice 0.3.0, scope
each gate with `paths` (and `paths_ignore`) so its key only covers the files
it reads: an edit under `services/api/` then leaves `web-test` and
`worker-test` cached, and a README edit leaves all of them cached.

```toml
version = 1

[defaults]
tail_lines = 60

[gates.web-lint]
cmd = "pnpm --filter web lint"
weight = 1
timeout = "5m"
hooks = ["pre-commit"]
key_inputs = ["node --version"]
paths = ["web", "package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"]
paths_ignore = ["web/**/*.md"]

[gates.web-test]
cmd = "pnpm --filter web test -- --maxWorkers=2"
weight = 2
timeout = "15m"
hooks = ["pre-push"]
key_inputs = ["node --version"]
depends_on = ["web-lint"]
paths = ["web", "package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"]
paths_ignore = ["web/**/*.md"]

[gates.api-test]
cmd = "cd services/api && python3 -m pytest -n 2 -q"
weight = 2
timeout = "15m"
hooks = ["pre-push"]
key_inputs = ["python3 --version"]
paths = ["services/api"]

[gates.worker-test]
cmd = "cd services/worker && go test -p 2 ./..."
weight = 2
timeout = "15m"
hooks = ["pre-push"]
key_inputs = ["go version"]
paths = ["services/worker", "go.work", "go.work.sum"]

[gates.docs]
cmd = "mdbook build docs"
paths = ["docs"]

[queue]
gates = ["web-lint", "web-test", "api-test", "worker-test"]
max_train = 4
```

A Rust workspace scoped so that documentation-only commits never re-run the
test suite:

```toml
[gates.test]
cmd = "cargo test --workspace"
weight = 6
hooks = ["pre-push"]
key_inputs = ["rustc --version"]
paths = ["crates/**", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo"]
paths_ignore = ["crates/**/*.md"]
```

`paths_ignore` alone means "everything except": `paths_ignore = ["docs",
"**/*.png"]`.

### Choosing paths

Path scoping is opt-in because a scope that is too small gives **stale
passes**: if the gate reads a file outside its scope, editing that file no
longer re-runs it. Before scoping a gate, list everything it reads:

- the package directory itself;
- root manifests and lockfiles: Rust `Cargo.toml`, `Cargo.lock`,
  `rust-toolchain.toml`, `.cargo/config.toml`; Node `package.json`,
  `package-lock.json` / `pnpm-lock.yaml` / `yarn.lock`,
  `pnpm-workspace.yaml`, `tsconfig*.json`, `.nvmrc`; Python
  `pyproject.toml`, `uv.lock` / `poetry.lock`, `requirements*.txt`,
  `setup.cfg`; Go `go.mod`, `go.sum`, `go.work`; plus `Makefile`,
  `Dockerfile`, `.tool-versions`;
- shared code, fixtures, generated code and `build.rs` inputs that live in
  other directories;
- symlink targets: a symlink in the scope is hashed by its target path,
  not by the target's content.

Rules that catch people out:

- Patterns are root-relative git glob pathspecs. `docs` (no wildcard)
  covers the whole directory; `src/*.rs` matches only files directly in
  `src/`; `**/*.rs` matches at any depth.
- A wildcard pattern does not recurse: `src/1*` does not cover the files in
  `src/12/`; write `src/1*/**`.
- `.sluice.toml` is always in the scope, so editing any gate invalidates
  every scoped gate (as it does unscoped ones).
- Files git ignores are never in the scope; declare them in `key_files`.
- Every Sluice that reads the config must be 0.3.0 or later: older
  binaries reject `paths` as an unknown field.

`sluice cache status` shows how many files each scoped gate covers
(`[scoped: 212 of 5000 files, 3f2a…]`), and `sluice doctor` warns when a
well-known manifest or lockfile next to the scoped files is outside the
scope, when a pattern matches nothing, and when a symlink in the scope
points outside it.

Notes:

- `depends_on` is validated (the gate must exist) but not scheduled yet; it
  documents intent and does not order execution.
- `cmd` runs with `sh -c` from the repository root, so `cd pkg && ...` works.
- `[queue] gates` is what a merge-queue train must pass. Without it, the gates
  with a `pre-push` hook are used, which is already right here; the list makes
  the choice explicit.
- A gate that should run in the merge queue but never in hooks simply has no
  `hooks` and is listed in `[queue] gates`. For a build whose output you need
  every time (never answered from the cache), use `mode = "queue-only"`
  (0.3.0); `sluice hook print` and `sluice doctor` warn when such a gate is
  also listed in `hooks`, since every commit would then run it in full.

## Per-worktree caches

Sluice shares verdicts between worktrees, but tool caches are not verdicts:
each worktree needs its own. Agents work in separate worktrees at the same
time, and a tool cache that holds per-checkout state breaks when two
checkouts share it.

The alpha evidence: with one `node_modules` shared between worktrees, `xo`
fails because it writes a generated `tsconfig` under `node_modules/.cache`,
which then points at one checkout and is overwritten by the next. It is not a
Sluice bug, and it appears the moment parallel runs start.

| Cache | Where | Rule |
|---|---|---|
| Node tools (xo, eslint, babel, vite) | `node_modules/.cache` | Do not symlink or share `node_modules` across worktrees; install per worktree, or share only the package store (pnpm does this by default) |
| Turborepo | `.turbo`, `node_modules/.cache/turbo` | Keep it inside the worktree |
| mypy | `.mypy_cache` | One per worktree; never point `--cache-dir` at a shared path |
| pytest | `.pytest_cache` | One per worktree; do not set `-o cache_dir` to a shared path |
| Rust | `target/` | One per worktree, or a distinct `CARGO_TARGET_DIR` per worktree; a shared target dir serialises builds on its lock and mixes artifacts |

These directories are normally gitignored, so they are not part of the tree
hash and do not affect Sluice's keys. If a gate really reads one of them as an
input, declare it with `key_files`.

## What a cache hit does not give you

A hit means "this exact tree, command, environment and declared inputs passed
before". It is a verdict, not an artifact. Nothing is built, installed or
written on a hit.

- `pnpm run build`, `cargo build`, `docker build` and anything else whose
  output you need afterwards must actually run. Give those gates
  `cache = false`, or run the command directly; a hit would leave you without
  `dist/`, `target/` or the image.
- Do not make a later step depend on files that a cached gate produced. Put
  the producing step in the same `cmd` (`pnpm run build && pnpm test`) or in a
  `cache = false` gate.
- A hit says nothing about the world outside the tree. A test that reaches a
  service, the clock or the network can pass today and fail tomorrow on the
  same key. Declare what it depends on in `key_inputs`, or set `cache = false`.
- A cache hit is a local optimisation. CI stays authoritative (and, with `CI`
  set, Sluice does not use the cache unless `SLUICE_CACHE=1`).

## Choosing hooks

Think in terms of how long the developer, or an agent, waits.

- **Fast gates in `pre-commit`**: format check, linter, type check. Seconds,
  low weight. They run on every commit, usually hit the cache, and their
  failure points at a specific line.
- **Heavy gates in `pre-push`**: full test suites, Docker builds, integration
  tests. They run once per push, and a pass recorded earlier on the same tree
  makes the push instant.
- **Agents verify first.** An agent should call the MCP `verify` tool before
  it commits. `verify` runs the `pre-commit` gates by default, or the gates
  you name, waits safely in the queue, and records the passes. The commit and
  push hooks then hit the cache and finish in milliseconds. Without it, an
  agent's commit on an unverified tree fails fast with exit 3 instead of
  waiting in the queue inside the shell call (see [agents.md](agents.md)).
- A gate may list both hooks (`hooks = ["pre-commit", "pre-push"]`) when it is
  cheap enough to run at commit and you want the push to confirm it; the key
  is the same, so the second run is a hit.
- `git commit` reports any failing hook as exit 1. Sluice's fail-fast exit 3
  is only visible in the message, so read the text, not the code.
