---
title: Command control beyond gates (exec, queue-only gates, guard rewrite, shims)
tags: [sluice, design, alpha, exec, guard, shims]
---

# Command control beyond gates (design A0)

Status: design for alpha tasks A1, A2 and H5 ([[sluice/plan/alpha-plan.md]]), 2026-10-22. Builds on
[[sluice/design/protocol.md]], [[sluice/design/executor.md]], [[sluice/design/scheduler.md]],
[[sluice/design/config-format.md]]. Protocol stays 1, `.sluice.toml` stays `version = 1`, every
addition is optional.

## 0. Decisions in one line each

1. `sluice exec` is a new `exec` client message (not a `run` variant), advertised by the welcome
   feature `"exec"`; without it the client runs in-process.
2. Output goes through a framed spool file (`<state>/logs/<job>.exec`), stdout and stderr kept as
   separate streams in arrival order; the client replays it to its own fds. Stdin is `/dev/null`, no
   TTY, exit code propagated, signals forwarded.
3. The caller's environment is passed verbatim (no sanitising); cwd is the caller's.
4. Default exec weight is `[agent] exec_weight`, else `ceil(budget / 2)`; exec jobs keep that
   declared weight (never learned, ADR-0011); coalescing only with `--coalesce-key`.
5. Queue-only gates use a new optional field `mode = "queue-only"`; `cache` stays a bool.
6. The guard's logic moves into `sluice guard` (Rust); `guard.sh` is a thin wrapper that keeps
   today's shell deny logic as fallback when the binary lacks `guard`.
7. Rewrites only add fixed words to the untouched original string (`sluice exec --label L -- `
   before a heavy command word; `sluice run <gate> -v` in place of the typed gate words) and are
   emitted as `allow` only where that cannot widen permissions; `ask` or no rewrite elsewhere (table
   in section 4.6).
8. Shims are POSIX `sh` wrappers that call `sluice shim exec <name>`; every process Sluice runs
   carries `SLUICE_JOB=<job id>`, which turns nested exec and shims into pass-through (no double
   wrapping, no nested-queue deadlock).

## 1. Claude Code hook contract (verified)

Source: https://code.claude.com/docs/en/hooks (markdown `hooks.md`, HTTP `last-modified: Thu, 22 Oct
2026 21:25:15 GMT`, read 2026-10-22), section "PreToolUse decision control":

> `updatedInput` | Modifies the tool's input parameters before execution. Replaces the
> entire input object, so include unchanged fields alongside modified ones. Claude Code
> evaluates permission rules and a Bash command's auto-background eligibility against
> the input your hook returns, not the input Claude sent. Combine with `"allow"` to
> auto-approve, or `"ask"` to show the modified input to the user. For `"defer"`, ignored

> `permissionDecisionReason` | For `"ask"`, shown to the user in the permission prompt.
> [...] For `"deny"`, shown to Claude. For `"allow"` and `"defer"`, written to the debug log only

> When multiple PreToolUse hooks return different decisions, precedence is `deny` > `defer` > `ask` > `allow`.

> `additionalContext` | String added to Claude's context alongside the tool result.

Permissions page (https://code.claude.com/docs/en/permissions, "Precedence among rules, hooks, and
mods"): "Claude Code evaluates deny and ask rules regardless of what a PreToolUse hook returns".
"Hook approvals in plan mode": `"allow"` has no effect on a non read-only call, "and Claude Code
still applies any `updatedInput` the hook returns". The hook input carries `permission_mode`
(`default`, `plan`, `acceptEdits`, `auto`, `dontAsk`, `bypassPermissions`; "Not all events receive
this field").

Consequences: (a) allow + `updatedInput` is supported and documented; (b) the whole `tool_input`
must be echoed (`description`, `timeout`, `run_in_background`); (c) the doc does not say whether
Claude sees the rewritten command, so the guard always adds an `additionalContext` line; (d)
deny/ask rules are matched against the *rewritten* command, so a user rule `Bash(make deploy:*)`
would no longer match `sluice exec -- make deploy`: the guard must check those rules itself (4.6);
(e) `allow` skips prompts the original would have raised: same section. A2 confirms (a) and (c) with
one `claude -p --model haiku` session and records the transcript excerpt in its change document.

## 2. `sluice exec`

```
sluice exec [--weight N] [--label L] [--coalesce-key K] [--timeout DUR] [-q|--quiet] -- <cmd> [args...]
```

Module `crates/sluice-gate/src/exec.rs`, `pub fn exec(args: &ExecArgs) -> ExitCode`. `<cmd>` is an
argv (no shell); `sluice exec -- sh -c '...'` is the way to run a script.

### 2.1 Client flow

1. Resolve `argv[0]` with `sluice_gate_core::which::resolve(name, env, cwd, skip_shims=true)`
   (skips the shim dir and any file starting with the shim marker, section 5). Not found: `sluice:
   exec: cannot run '<cmd>': command not found`, exit 127; not executable: `... permission denied`,
   126.
2. If `SLUICE_JOB` is set (we are inside a Sluice job): `CommandExt::exec` the resolved command
   directly (argv[0] kept, inherited stdio, no queue). The parent job's weight already covers it.
3. Label: `--label` or the first two words of argv (the program as its base name, `cargo build`),
   keeping only letters, digits, space and `._:/-`, max 48 chars (`exec` when nothing is left).
   Weight: `--weight` or `[agent] exec_weight` or `ceil(effective_budget / 2)` (`[daemon] budget`,
   at least 1); clamped by the daemon as for gates. A user config that cannot be parsed prints
   `sluice: warning: <error>; using the default settings` and exec goes on with the defaults.
4. Repo identity: best-effort repository discovery in cwd (`git::discover`, one call, failure
   ignored): the main worktree (the parent of a common dir named `.git`) or the common dir, so
   `repos.<path>.budget_share_max` applies; else `null` (the daemon then uses the cwd as the
   repository for the cap).
5. `SLUICE_DISABLE=1`, or a non-UTF-8 environment (2.6): straight to the in-process run.
   Otherwise `client::ensure_daemon`, handshake. Daemon without feature `"exec"`, refusal, or any
   error before `queued` → in-process fallback (2.6).
6. Block SIGINT, SIGTERM, SIGHUP with `SigSet::thread_block` before any thread starts; a thread
   reads them from `nix::sys::signalfd::SignalFd` (safe API, `unsafe_code` stays forbidden) and
   forwards them (2.4).
7. Send `exec`; on `started` open the spool and replay it (2.3); on `done` drain to EOF, print
   nothing on success, exit with the mapped code (2.5).

### 2.2 Protocol additions (protocol stays 1)

- `welcome.features` gains `"exec"` (`FEATURE_EXEC`) and `"queue-only"` (`FEATURE_QUEUE_ONLY`).
  Clients send `exec` or `no_marker` only to a daemon listing them.
- New client message (`ClientMessage::Exec { id, request: ExecRequest }`):

```json
{"type":"exec","id":1,"argv":["/usr/bin/cargo","build","--release"],
 "cwd":"/abs/worktree","env":{"PATH":"...","HOME":"...","CLAUDECODE":"1"},
 "label":"cargo build","weight":4,"timeout_ms":0,"coalesce":null,
 "repo":"/abs/main-worktree|null","caller":"agent","session":"<id|null>","client_pid":4242}
```

  An old daemon answers `error{bad-request}` (unknown type) before `queued`: the client falls back
  in-process, so even a skipped feature check is safe.
- Server events on that connection: `queued`, `started`, `progress`, `done`, as for `run`. `started`
  gains optional `spool` (absolute path of the framed output). `Outcome` gains optional `signal`
  (number that killed the leader; omitted when absent).
- New client message `signal {"type":"signal","id":1,"signal":"INT|TERM|HUP"}`, valid on an `exec`
  connection only (`bad-request` elsewhere, as `cancel`).
- `status`: `RunningJob`/`QueuedJob` gain optional `kind` (`"exec"`; omitted means gate) and
  `label`; `gate` is `"exec:<label>"` so v0.2 clients print something meaningful. `status` schema
  stays 1. Database: no schema change (`jobs.gate = "exec:<label>"`, `key` NULL, `cmd` = argv
  shell-quoted for display).
- `RunRequest` gains optional `no_marker: bool` (`skip_serializing_if` false), section 3.

### 2.3 Daemon execution and output

`server.rs`: `handle_exec` calls `register_exec`, which goes through the same `register_job` as
gates with `key = None`, `via = "exec"`, `request = JobRequest::Exec(Arc<ExecRequest>)`; the job
keeps its declared weight (it has no `stats_key`, so no weight is learned, ADR-0011 M2) and its
lane is the cwd. `start_exec` builds the `ExecSpec` with two new fields: `launch: Launch` (`Shell {
cmd, extra }` for gates, `Argv { program, args }` for exec) and `sink: Sink` (`Log` or `Spool`). Everything else is reused: new process group, `renice`/`ionice`, `KILL_GRACE`,
orphan reconciliation, `NO_TIMEOUT` unless `--timeout`. Every job's env gets `SLUICE_JOB=<job id>`
(gates too). Exec ignores `last_subscriber_gone = finish` (nothing to cache): the last subscriber
leaving kills it. Unlike a gate log, an unwritable spool is a start error (the client replays the
file).

Output choice: **separate streams, ordered at chunk granularity, through a spool file.** Agents need
the real stdout/stderr (tools parse stdout; Claude's Bash tool returns both) and the real exit code.
Socket streaming over the existing broadcast channel drops data when a subscriber lags; unbounded
per-subscriber buffers make memory depend on the slowest reader. A file gives byte-exact,
binary-safe, lossless output, the child never blocks on a slow reader, and a late coalesced
subscriber replays from offset 0.

Spool format (`executor::spool`): magic `SLUICEX1\n`, then records `[stream: u8 (1|2)][len: u32
BE][bytes]`, written by the two pumps under one mutex, 8 KiB chunks. The client
(`exec::SpoolReader`) reads whole records, writes stream 1 to fd 1 and 2 to fd 2, polls every 20 ms
(backing off to 100 ms when idle), and after `done` (sent only after the pumps are joined) reads to
EOF. `EPIPE` on the client's stdout → client exits 141 and closes the connection, which kills the
job (same as a pipeline). `sluice logs <job>` decodes `.exec` files (streams merged); `tail` in
`done` is decoded the same way; retention prunes them with the job logs. Interleaving between stdout
and stderr is exact at chunk level, like any pipe-capturing runner; within a stream it is exact.

Stdin is `/dev/null` (the daemon cannot share the caller's stdin without fd passing; see open
question 1). TTY: the child never has one, so tools fall back to their non-TTY output, which is what
agents get anyway. A human who wants a TTY runs the command directly or uses `SLUICE_DISABLE=1`
(in-process path, inherited stdio).

### 2.4 Signals

Client receives SIGINT/SIGTERM/SIGHUP:
- request still queued → close the connection (job dropped if it has no other subscriber), print
  `sluice: exec '<label>' interrupted while queued`, exit 128+sig;
- running → send `signal{sig}`; the daemon does `killpg(sig)` when this is the only subscriber, else
  only detaches this subscriber; once the leader has exited, the rest of its group is SIGKILLed
  at once (background children that ignore SIGINT would hold the output pipes); with the leader
  still alive after `KILL_GRACE` + 5 s the cancel sequence (`terminate_group`) runs. A second
  signal at the client, or the client's SIGKILL (Bash tool timeout), closes the connection: the
  existing cancel path kills the group. The exit code is 128+N of the signal that killed the
  job, or of the signal the client received when the job ended as killed (not 143).

The `interrupted while queued` line is informational and omitted with `--quiet`.

### 2.5 Exit codes and messages

| Situation | Exit | stderr (omitted with `--quiet`, except errors) |
|---|---|---|
| child exited `n` | `n` | nothing |
| child killed by signal `s` | 128+s | nothing |
| `--timeout` expired | 124 | `sluice: exec '<label>' TIMEOUT after <d>` |
| killed by `daemon stop`/lost lease | 143 | `sluice: exec '<label>' KILLED: the daemon stopped (job <id>)` |
| not found / not executable | 127 / 126 | `sluice: exec: cannot run '<cmd>': <why>` |
| Sluice error after `queued` | 125 | `sluice: exec: <message>` |
| connection lost after `queued` | 125 | `sluice: exec: the daemon died while running exec '<label>'; it was killed` (after `started`) or `sluice: exec: lost the connection to the daemon: <why>` |
| the client's stdout is a closed pipe | 141 | nothing (the connection is closed, which kills the job) |
| client received SIGINT/SIGTERM/SIGHUP | 128+N | nothing (`interrupted while queued` when it was still queued) |
| waited ≥ 1 s in the queue | — | `sluice: exec '<label>' queued (position P, U of B units busy)` then `sluice: exec '<label>' started after 12.4s (job <id>)` |
| coalesced | — | `sluice: exec '<label>' sharing job <id>` |

### 2.6 In-process fallback

`sluice: daemon unavailable (<why>); running '<label>' in-process` (one stderr line, omitted with
`--quiet`; `SLUICE_DISABLE=1` goes in-process without it), then take
`weight` slots of the `flock` semaphore (`budget::acquire`, moved from `run.rs` to
`crates/sluice-gate/src/budget.rs` and shared; it prints `sluice: waiting for N of B budget units
for '<label>'` once unless `--quiet`, and when the semaphore directory cannot be used the command
runs unthrottled after `sluice: warning: cannot take budget slots (<why>); running unthrottled`), spawn with inherited stdin/stdout/stderr in the
command's own process group (`process_group(0)`), set `SLUICE_JOB=inproc-<pid>`, wait, map the
code. When the client's group is the foreground group of the terminal on stdin, the terminal is
handed to the command's group (`tcsetpgrp`, SIGTTOU blocked, then SIGCONT in case a read already
stopped it) and taken back when it ends, as a shell does for a foreground job. Signals are blocked
only after the spawn (the child keeps a clean mask); a received signal is forwarded to the whole
group with `killpg`, and a `--timeout` sends SIGTERM to the group, then SIGKILL after 5 s, so
background children of the command never outlive it (alpha review finding 9). Not visible in
`status`, no nice/ionice (as gates today).

The environment travels to the daemon as UTF-8 strings. The client reads it with `vars_os`
(`sluice_gate_core::config::process_env`, which drops non-UTF-8 entries instead of panicking);
when any entry is not valid UTF-8 (`process_env_is_utf8`), `sluice exec` skips the daemon (one line:
`the environment is not valid UTF-8; running '<label>' in-process`) and runs in-process, where the command inherits the raw environment unchanged. Shims and the guard use the
same reader, so a non-UTF-8 variable never makes them panic (alpha review finding 3).

### 2.7 Coalescing

Only with `--coalesce-key K`. Identity = SHA-256 of `"sluice-exec-v1\0" K "\0" cwd "\0" argv joined
by NUL`, kept in a separate in-flight map (`State::exec_inflight`), so the same K with another
command or directory never attaches. Env differences are ignored (documented). No marker, no miss
record, ever.

## 3. Queue-only gates

```toml
[gates.build]
cmd = "pnpm build"
weight = 6
mode = "queue-only"      # new, optional: "gate" (default) | "queue-only"
```

Why a new field and not `cache = "queue-only"`: both make a v0.2 binary refuse the file (strict
parser), which is the right outcome, since silently treating a build as a cacheable gate would skip
it. `mode` keeps `cache` a bool forever (no type change of a frozen field, see
[[sluice/design/compatibility.md]]), gives a clear old-binary error (``unknown field `mode` ``
instead of ``invalid type: string, expected a boolean``), and leaves room for later modes.
Validation: `mode = "queue-only"` with an explicit `cache = true` → `gates.<name>: mode =
"queue-only" never caches; remove cache = true`; `cache = false` is accepted (redundant); the parser
sets `cache = false` on every queue-only gate (`RepoConfig::check_queue_only`). Docs:
"needs sluice ≥ 0.3.0".

Semantics (`run.rs`, `config.rs`, `server.rs`):
- Always runs: no marker lookup, no hit path, no `hit` event. The tree is still snapshot to derive
  the normal key K; the request carries `key = sha256("queue-only\0" K)` (own namespace, never
  matches a marker) and `no_marker = true`.
- Queued under the budget with its weight; simultaneous requests for the same tree, gate definition
  and extra args coalesce (scheduler rule 2 unchanged).
- The daemon skips the pre-start marker double-check and never writes a marker or a miss record when
  `no_marker`. A daemon without `"queue-only"` gets `key = null` instead (runs, never coalesces,
  cannot write a marker).
- Hooks: the agent `fail-fast` policy never applies to a queue-only gate (no `verify` could satisfy
  it, the commit would loop); the hook waits. `sluice hook print` and `doctor` warn when a
  queue-only gate has `hooks`. `verify` runs it every time and reports `pass` (never `cached`); the
  merge queue may list it in `queue.gates`. `verify` and the merge-queue runner send it with `key =
  null` (it always runs and never writes a marker, but is not coalesced there; `sluice run`
  coalesces).

## 4. Guard rewrite (agent plugin)

### 4.1 Shape

`plugin/hooks/guard.sh` becomes: exit 0 if `SLUICE_GUARD=0` (the Rust guard also treats `false`, `no`
and `off` as off); run `sluice guard --hook` with the hook
JSON on stdin (no daemon contact, reads files only); if it exits 0, print its stdout when it starts
with `{"hookSpecificOutput"`; otherwise (binary missing, a pre-0.3 binary without `guard`, crash)
run the current shell logic unchanged (`legacy_main`: deny gates, deny `sluice queue approve`).
Always exit 0.

The approval deny (`matcher::runs_approve`) looks at tokenised words, not a text prefix: quotes are
removed, the program is taken by its base name (`"sluice"`, `/usr/bin/sluice`), and it looks
through `env` (options and assignments), `command`, `builtin`, `exec`, `nohup`, `time`, `setsid`
and `sluice exec ... --`. Wrappers whose arguments it does not parse (`sh -c`, `eval`, `xargs`,
`sudo`, `timeout`, ...) and commands the tokenizer refuses are denied when their text mentions both
`queue` and `approve` (deny when unsure). The daemon enforces the same rule for every surface (see
`merge-queue.md`), so the guard is a first line, not the only one (alpha review finding 2). Reason: rewriting needs exact JSON echo of `tool_input`, the user TOML config, the
Claude settings JSON and a quote-aware tokenizer; doing that in `sh` + optional `jq` is fragile, and
the binary is already required by the plugin's MCP server. A Rust start costs ~2 ms, less than
today's dozen `sed` forks.

Code: `crates/sluice-gate/src/guard/{mod.rs,lex.rs,matcher.rs,rules.rs}`;
`sluice_gate_core::heavy::{DEFAULT_HEAVY_COMMANDS, HeavyMatcher, canonical_words}` shared with
shims. `guard::run_hook() -> ExitCode` never returns non-zero and never panics outward
(`catch_unwind`, output only on success).

### 4.2 Tokenizer (`lex::split(command) -> Option<Vec<Segment>>`)

A segment is a simple command with byte offsets into the original string: optional leading
`VAR=value` words, words, a count of output redirections (`>`, `>>`, `2>&1`, `&>`), and the operator
that ends it (`&&`, `||`, `;`, `|`). Single and double quoted words are supported when balanced and
on one line; inside double quotes `$`, `` ` `` and `\` make the whole command unparseable. `None`
(never rewritten, never denied by the rewrite logic; the legacy deny checks still run on the naive
split) for: newlines and other control characters (tab is fine), any unquoted `\`, heredocs `<<`,
input redirection `<`, process substitution (`<(`, `>(`), `$(`, `${`, `$[`, `$'...'`, `$"..."`,
backticks, `(`, `)`, `{`, `}`, a single `&` (background), `|&`, `;;`, `;&`, comments, unbalanced
quotes, a segment starting with a compound-command keyword (`if`, `for`, `while`, `case`, `!`,
`[[`, ...), an operator with no command before it, more than 4096 bytes. A trailing `;` is
accepted. Input that is not valid UTF-8 never reaches the tokenizer (the hook gives no answer).

### 4.3 Matching

Per segment, after skipping env assignments and an optional leading `time`:
- **Skip** when the first word starts with `sluice` (`sluice queue approve` stays a deny), or is one
  of `sudo`, `doas`, `env`, `nohup`, `nice`, `ionice`, `timeout`, `xargs`, `exec`, `eval`, `sh`,
  `bash`, `zsh`, `dash`, `ksh`, `fish`, `command`, `builtin`, `source`, `.`, `watch`, `parallel`,
  `flock`, `setsid`, `stdbuf`, `chrt`, `taskset`, `strace` (wrappers and `sh -c` are never looked
  into), and when a leading assignment sets `PATH` (`PATH=... cmd` would also pick which `sluice`
  runs). A bare leading `time` is passed over; `time` with options is skipped. A segment that is
  downstream in a pipeline, or that has an argument `-`, is never rewritten (it reads stdin).
- **Directory**: a previous segment `cd <word>` (exactly one plain argument, no assignments or
  redirections) joined with `&&` changes the effective directory (no `~` or variables, must exist),
  else no rewrite at all; `pushd`/`popd`, and any `||` or `;` in a command that contains a `cd`,
  also mean no rewrite at all (the directory of later segments would depend on exit codes). The target is resolved lexically on
  the logical directory (the hook's `cwd`, the shell's `$PWD`): `..` drops the previous component
  as typed, like the shell's default `cd -L`; only the result is canonicalised to find the
  `.sluice.toml`. No rewrite at all when `CDPATH` is set (a relative `cd` may go elsewhere) or when
  a `cd` argument has a `..` component while the logical and canonical current directories differ
  (alpha review finding 1).
- **Gate**: config found by walking up from the effective directory (a `.sluice.toml` that cannot
  be read or parsed: no output at all). A gate matches when the effective directory is the config's
  directory, the segment has no env assignments and no redirections and is not in a pipeline (`cargo
  test --workspace 2>&1 | tail` becomes an exec rewrite: `sluice run -v` prints different bytes), and
  the segment starts with the gate `cmd` (only gates whose `cmd` is one simple command of unquoted
  literal words take part; no `$ ~ * ? [ ]`). Rewrite: the typed gate words are replaced by `sluice
  run <gate> -v -- ` followed by the rest of the original (` --` only when arguments follow; the
  longest gate wins) when the typed words equal the gate words as spelled, or by `sluice run <gate>
  -v` when only the canonical forms are equal, there is no rest, every word is literal and no
  package-manager directory option is present (a different package manager would parse extra args
  differently). Runner-only matches (`cargo test -p x` for a gate `cargo test --workspace`) are
  not gate matches; they fall through to heavy.
- **Heavy**: canonical words start with an entry of the heavy list (word boundary; the longest
  entry wins). Rewrite: insert `sluice exec --label <L> -- ` before the command word (after env
  assignments, so they reach exec and then the child). `L` = up to the first two canonical words,
  stopping at the first word that starts with `-`, joined with `-`, lowercased, filtered to
  `[a-z0-9._-]`, max 32 chars (`exec` when nothing is left). A heavy segment first in a pipeline is
  fine: exec's stdout is the child's.

Canonical words (`canonical_words`), used only for matching, never for output: `npm run X`, `npm
run-script X`, `pnpm run X`, `yarn run X`, `bun run X` → `run X`; `npm test|start|stop|restart` →
`run <that>`; `pnpm X` / `yarn X` → `run X` unless X is a package-manager builtin (`add audit bin
cache check config create dedupe dlx exec import info init install i link list ls outdated pack
prune publish rebuild remove rm root store unlink up update upgrade why workspace workspaces`); `npx
[-y|--yes] X`, `pnpm exec X`, `yarn exec X`, `bunx X` → `X`; `pnpm -C d|--dir d` (also `--dir=d`),
`npm --prefix d`, `yarn --cwd d` are skipped when finding the command word and left out of the
canonical words (which are used only for matching, never to build the command; launchers are
unwrapped at most four times); `python -m pytest`, `python3 -m pytest` → `pytest`; `./gradlew` → `gradle`, `./mvnw` → `mvn`;
`docker-compose` → `docker compose`; whitespace collapsed.

### 4.4 Heavy list and configuration

User config, new optional table (`AgentConfig` in `sluice_gate_core::config`):

```toml
[agent]
heavy_commands = ["make", "cargo build"]   # replaces the default list when present
heavy_commands_extra = ["bazel build"]     # appended to the (default or replaced) list
exec_weight = 0                            # 0 = ceil(budget / 2)
```

Default (`DEFAULT_HEAVY_COMMANDS`, canonical words): `docker build`, `docker buildx build`, `docker
compose build`, `run build` (npm/pnpm/yarn/bun build scripts), `cargo build`, `cargo test`, `cargo
clippy`, `cargo check`, `cargo doc` (W6 addendum: a workspace `cargo check` is about twenty parallel
`rustc` and starved timing-sensitive gates), `cargo nextest`, `go build`, `go test`, `make`, `gradle`, `mvn`, `tsc`, `vite build`,
`webpack`, `next build`, `pytest`, `jest`, `vitest`, plus (zero-config, v0.4.0) `ng build|test|lint|e2e`,
`nx run|run-many|affected|build|test|lint|e2e`, `turbo run|build|test|lint`, `playwright test`,
`cypress run`, `react-scripts build|test`, `astro build`, `nuxt build`, `dotnet build|test`,
`bazel build|test`, `ninja`, `cmake --build`. Entries are validated by the config parser
(not empty, no control characters, at most 8 words; at most 256 entries across both lists) and
canonicalised before matching, so `pnpm build` and `run build` are the same entry. The guard reads
the config through the normal strict parser (re-read per call, as the client does); a parse error means "no rewrites" and a stderr
note that only the debug log shows. Old binaries reading a user config with `[agent]` fail loudly;
the user config is per machine and upgrades with the binary, so this is accepted.

#### 4.4.1 Never heavy, and package scripts (zero-config, T1)

`heavy::is_never_heavy(canonical)` is checked before the list, so a dev server or a watcher stays
direct (it never ends and would hold a queue slot forever): `ng serve|s`, `nx serve`, `next dev`,
`astro|nuxt dev|preview`, `react-scripts start`, `vite` alone or with options only (and `vite
dev|serve|preview`), and any word `--watch`, `--watch=true`, `--watchAll`, `--watchAll=true`
(`--watch=false` is not a watch); `-w` counts only for `tsc jest vitest webpack vite`, where it
means `--watch`. `HeavyMatcher::matches` (words only) applies this rule too.

`HeavyMatcher::classify(canonical, dir, scripts: &dyn ScriptSource) -> Option<Match>` adds package
scripts (`crates/sluice-gate-core/src/pkgscripts.rs`). Words are matched first (no I/O). Otherwise,
for canonical `run <name> ...` (so `npm test` is `run test`), the nearest `package.json` from `dir`
upwards (stopping after the git root, or at `/`; the nearest file wins even without that script;
at most 1 MiB; unreadable or invalid means not heavy) supplies `scripts.<name>`. The body is split
on `&&`, `||`, `;`, `|`, `&` and newlines outside quotes, leading `NAME=value`, `env` and
`cross-env` words are dropped, and each piece is canonicalised and classified with the same rules,
recursively for `npm run x` / `pnpm x` pieces in the same package (depth at most 4, a script
already on the stack ends the walk). Heavy when any piece is. `pre`/`post` scripts are ignored.
`Match { entry, script }` says why. The rewrite label stays the canonical words the agent typed
(`run-axe`). `-C/--dir/--prefix/--cwd` options are not followed: the script is read from `dir`.
The guard gets scripts through `World::scripts()` (the real `Fs` returns `FsScripts`; fake worlds
default to none); the shims use `FsScripts` directly.

### 4.5 Fail-open rules

Any error (unreadable JSON, a tool other than `Bash`, missing `tool_input.command`, a `.sluice.toml`
that cannot be read, a panic) → no output, exit 0, and the original command goes through Claude
Code's normal flow. An unparseable command, a user config or settings that cannot be parsed, or
an unknown settings location mean no rewrite, but the legacy deny of a raw gate command (4.3)
still applies; `sluice queue approve` is denied before any of this. The guard reads at most 1 MiB
of input, never blocks on I/O beyond local files, never contacts the daemon, and caps its own run
at 500 ms (a watchdog thread exits 0 with no output). `SLUICE_GUARD=0` (or `false`, `no`, `off`)
disables everything, `SLUICE_GUARD=deny` selects suggestion mode (4.8).

### 4.6 Permission safety

`guard::rules` reads `permissions.allow|ask|deny` from `$CLAUDE_CONFIG_DIR` or
`~/.claude/settings.json`, `$CLAUDE_PROJECT_DIR/.claude/settings.json` and `settings.local.json`,
and `/etc/claude-code/managed-settings.json`. Only `Bash` rules matter (`Bash`, `Bash(prefix:*)`,
`Bash(glob *)`, exact).

| Condition (first that applies) | Output |
|---|---|
| any deny or ask rule matches (liberally: any doubt counts) the whole command or any segment of the original or of the rewrite, or a bare `Bash` or unrecognised `Bash...` ask/deny rule, or a settings file fails to parse, or the settings location is unknown (`CLAUDE_PROJECT_DIR` unset, or neither `CLAUDE_CONFIG_DIR` nor `HOME`) | nothing (normal flow on the original) |
| `permission_mode` is `plan`, `auto` or `dontAsk` and the original is not covered by allow rules, or the mode is unknown | nothing |
| `permission_mode = bypassPermissions`, or every segment of the original matches an allow rule (strictly; a segment with a redirection never does) | `allow` + `updatedInput` + `additionalContext` + reason |
| `default` / `acceptEdits` (absent counts as `default`) | `ask` + `updatedInput` + `additionalContext` + reason (the user would have been prompted for the original anyway; now the prompt shows the rewritten command) |

When the table says "nothing", the legacy deny of a raw gate command still applies, as in 0.2.0.

So a rewrite never runs something the user would not have run, and never hides a deny or ask rule.
Command-line `--disallowedTools` flags are invisible to hooks; documented.

### 4.7 Injection argument, output and examples

A heavy rewrite is `original[..o] + PREFIX + original[o..]` for each insertion offset `o` (segment
command-word starts found by the tokenizer); a gate rewrite replaces the typed gate words
`original[s..e]` with `sluice run <gate> -v` (plus ` --` when arguments follow). The inserted text
is a constant plus the gate name (`[a-z0-9_-]`, validated by the config parser) or a label from
`[a-z0-9._-]`. Nothing else from the original is re-quoted, unescaped or moved, and every character
of the original keeps its position relative to its neighbours, so the shell parses the agent's
words exactly as before; only words are added in front (or, for a gate, swapped for the gate
runner). The JSON output is built with `serde_json` from the received `tool_input` with
`command` replaced.

| Original (`cwd` = repo root, gate `test` = `cargo test --workspace`) | Rewritten |
|---|---|
| `cargo test --workspace -- --nocapture` | `sluice run test -v -- -- --nocapture` |
| `cargo build --release 2>&1 \| tail -20` | `sluice exec --label cargo-build -- cargo build --release 2>&1 \| tail -20` |
| `cd web && pnpm build` | `cd web && sluice exec --label run-build -- pnpm build` |
| `RUSTFLAGS=-Dwarnings cargo clippy` | `RUSTFLAGS=-Dwarnings sluice exec --label cargo-clippy -- cargo clippy` |
| `npx vitest run` | `sluice exec --label vitest-run -- npx vitest run` |
| `make && make install` | `sluice exec --label make -- make && sluice exec --label make-install -- make install` |
| `docker build -t "my app" .` | `sluice exec --label docker-build -- docker build -t "my app" .` |
| `sh -c "make"`, `make &`, `cat x \| make`, `docker build - < Dockerfile`, `echo "cargo build"`, `sluice exec -- make`, `bash -c "$(cat run.sh)"` | unchanged |

What the agent sees: the rewritten command's own output and exit code (`sluice run -v` streams the
gate log on stderr and prints the gate summary; `sluice exec` prints only the command's output plus
at most two queue lines on stderr), and the `additionalContext`: ``Sluice ran this through its
shared queue as: `<rewritten>`. Output and exit code are the command's own. Use the `verify` tool
for gates.`` (gate case: ``Sluice ran this through its shared queue as gate '<gate>' (cached per tree): `<rewritten>`. The exit code is the gate's; a cached pass skips the run. Use the `verify` tool for gates.``).

### 4.8 Suggestion mode and missing support

`SLUICE_GUARD=deny` (for Claude Code versions where allow + `updatedInput` misbehaves, found in A2's
haiku check or reported by users): gate matches keep today's deny text; heavy matches deny with
``This machine shares its CPU through Sluice: run `<rewritten>` instead (same output and exit
code).`` The legacy shell path never rewrites.

## 5. Shell shims (H5)

`sluice shim install [--dir DIR] [--force]` writes one file per command word of the heavy list
(`docker cargo go make gradle gradlew mvn mvnw tsc vite webpack next pytest jest vitest npm pnpm
yarn bun npx`) into `$XDG_DATA_HOME/sluice/shims` or `~/.local/share/sluice/shims` (mode 0755, dir 0700, atomic
rename; refuses to overwrite a file without the marker unless `--force`):

```sh
#!/bin/sh
# sluice-shim v1
# Generated by Sluice (`sluice shim install`); `sluice shim uninstall` removes it.
# Routes 'make' through the Sluice queue in agent sessions; runs it unchanged otherwise.
SLUICE_SHIM_DIR='/home/u/.local/share/sluice/shims'; export SLUICE_SHIM_DIR
if [ -x '/home/u/.local/bin/sluice' ]; then
    exec '/home/u/.local/bin/sluice' shim exec make -- "$@"
fi
# Sluice is gone: run the real 'make', skipping every Sluice shim in PATH.
sluice_path=
set -f
IFS=:
for sluice_d in $PATH; do
    if [ -f "${sluice_d:-.}/make" ] && grep -q '^# sluice-shim v1$' "${sluice_d:-.}/make" 2>/dev/null; then
        continue
    fi
    sluice_path=${sluice_path:+$sluice_path:}$sluice_d
done
unset IFS sluice_d
set +f
PATH=$sluice_path; export PATH
unset sluice_path
exec make "$@"
```

`sluice shim exec <name> -- args` (`crates/sluice-gate/src/shim.rs`):
1. `real = which::resolve(name, env, cwd, skip_shims=true)`: walks `PATH`, skips every entry whose
   canonical path equals the canonical `SLUICE_SHIM_DIR` (else the default shim dir), and every
   candidate whose first 64 bytes contain `# sluice-shim v1` or that canonicalises to the running
   `sluice` binary. Not found: `<name>: command not found`, exit 127 (not executable:
   `<name>: Permission denied`, 126).
2. Pass-through (`CommandExt::exec(real, args)`, argv[0] kept) when `SLUICE_JOB` is set (inside
   `sluice exec`, a gate, or a rewritten command: no double wrapping), when `SLUICE_SHIMS` is falsy,
   when neither `SLUICE_SHIMS` is truthy nor `agent::detect` says agent, when an argument is `-`,
   or when `HeavyMatcher::classify` (4.4.1: list plus package scripts, in the cwd) says the
   canonical words of `name args` are not heavy (`docker ps`, `npm install`, `npm run lint`).
   Before the heavy check: when the cwd is the directory of a `.sluice.toml` and the canonical
   words of `name args` equal a gate's canonical command (same rule as `gate_edit`: exact, no extra
   arguments, no directory option; `matcher::exact_gate`), the route is `Route::Gate`: `sluice run
   <gate> -v` in this process (cache, coalescing, queue). Config errors there fail open.
3. Otherwise run the exec client in-process (no extra `sluice` spawn) with argv `[real, args...]`,
   label = the first two canonical words joined with a space.

`sluice shim uninstall` removes only files with the marker (and the directory when it is then
empty); `sluice shim status` lists shims, whether the dir is in `PATH` and before the real
binaries, and which real binary each resolves to (a `doctor` check for stale shims does not exist
yet, see 5.1). Docs
(`docs/agents.md`): put the dir first in `PATH` only for agent shells
(`PATH="$HOME/.local/share/sluice/shims:$PATH"` in the agent's environment or launcher), set
`SLUICE_SHIMS=1` for agents that set no detectable variable, plus a ready-to-paste `AGENTS.md`
block. Claude Code users do not need shims (the guard covers them); both together are safe because
of `SLUICE_JOB` and the resolver skip.

### 5.1 As implemented (H5, 2026-10-23): deviations

- Not found: `shim exec` prints `<name>: command not found` and exits 127, the way a shell
  reports it (not `sluice: shim: no '<name>' in PATH after <dir>`). A non-executable match exits
  126 with `<name>: Permission denied`.
- Label: the first two canonical words joined with a space (`make all`, `run build`, `cargo build`,
  `gradle build`), the `sluice exec` label convention, instead of all canonical words.
- Shim names come from the heavy list (`HeavyMatcher::shim_names`): the first word of each entry,
  `run` expanded to `npm pnpm yarn bun`, `gradle`/`mvn` also as `gradlew`/`mvnw`, plus `npx`;
  user `[agent] heavy_commands(_extra)` therefore add or remove shims. `gradlew`/`mvnw` are matched
  as `./gradlew`/`./mvnw` (`shim_canonical_words`).
- Wrapper fallback (recorded `sluice` gone): drops from `PATH` every directory whose `<name>` is a
  Sluice shim (line `# sluice-shim v1`), not only the literal `SLUICE_SHIM_DIR`, so a symlinked or
  duplicated shim dir or a second shim copy cannot loop.
- `install` also removes Sluice shims of commands no longer in the heavy list. The directory is
  created 0700; an existing directory keeps its mode. `install`/`uninstall` treat as theirs only
  regular files (not symlinks) that start exactly with `#!/bin/sh\n# sluice-shim v1\n`; the
  resolver keeps the looser 64-byte marker check.
- `status` and `uninstall` accept `--dir` like `install`; without it `SLUICE_SHIM_DIR`, then the
  default directory, is used. `status --json` (`schema: 1`): `dir`, `exists`, `in_path`,
  `path_index`, `sluice`, `this_shell {queue, reason}`, `shims [{name, state, real, active}]`
  (`current | stale | missing | foreign`), `unlisted`.
- Not done: the `doctor` check for stale shims (`doctor.rs` belonged to task X1 in the same
  wave); `sluice shim status` reports `stale` instead. Follow-up.
- `install` fails (exit 1) when the user config cannot be parsed; `status` warns and `shim exec`
  silently uses the defaults. `--dir` is made absolute against the cwd.
- Through the queue stdin is `/dev/null` (as `sluice exec`); only a `-` argument passes through.
  `shim exec` uses `sluice exec` without `--quiet`, so a command that waited at least 1 s gets the
  two queue lines on stderr.

## 6. Security and failure modes

- **Rewrite injection**: prefix-only insertion (4.7); the tokenizer refuses anything it cannot
  model; labels and gate names come from fixed character sets. R1 fuzzes `lex` with random shell
  strings and checks that the rewrite, with the inserted words removed, is byte-identical to the
  original.
- **Permission widening**: table 4.6. A rewrite can only turn a prompt into an `ask` showing the
  rewritten command, or into `allow` where the original was already allowed.
- **Exec runs as the user** with the user's environment; only the user can reach the socket (`0600`
  in a `0700` dir), so `exec` adds no privilege. Spool files live in the `0700` logs dir and may
  hold secrets printed by builds (as gate logs do).
- **Daemon down or old**: fallback in-process; exec never fails because Sluice is broken (only exit
  125 after the job was accepted). Lost connection after `queued`: exit 125, the job is killed by
  the cancel path.
- **Deadlock**: nested exec, shims and nested `sluice exec` inside gates pass through via
  `SLUICE_JOB`, so a job never waits for budget it is itself holding.
- **Shim recursion/shadowing**: resolver skips by dir, marker and binary identity; the wrapper's
  no-sluice fallback strips its own dir; shims are inert for humans unless `SLUICE_SHIMS=1`.
- **Disk**: spool files grow with output; pruned by `log_retention_days` (open question 3).

## 7. Test plans

Plans as written at design time. As built (see `kb/sluice/changes/command-control-exec.md`,
`guard-rewrite.md`, `shell-shims.md`): the large-output test uses 12 MiB instead of 200 MB, and
`plugin/hooks/test-guard.sh` has 92 checks.

A1 (`crates/sluice-gate/tests/exec.rs`, daemon tests in `sluice-gate-daemon`):
1. exit codes 0, 1, 42, 255 propagate; child killed by SIGKILL → 137.
2. stdout and stderr land on the right fds; `printf` 1 MiB of random binary to stdout →
   byte-identical (sha256); 200 MB through `yes | head -c` completes, memory flat.
3. alternating stdout/stderr writes with flushes keep their order in each stream.
4. stdin is `/dev/null` (`cat` exits at once); no TTY (`test -t 1` false).
5. env: a custom variable and `CLAUDECODE` reach the child; `SLUICE_JOB` is set; cwd is the
   caller's.
6. SIGINT and SIGTERM to the client while running → group gone within 6 s (grandchild included,
   `/proc` scan), exit 130/143; while queued → removed from `status`.
7. client SIGKILL → group killed by the cancel path even under `finish`.
8. 20 concurrent `exec --weight 2` with budget 8 → peak concurrency ≤ 4 (counter file and `/proc`);
   `status` shows `kind: exec` and labels, queued `reason: budget`.
9. `--coalesce-key k` ×3 simultaneous → one execution, three identical outputs and codes; same key
   with different argv → two executions; no key → never coalesced.
10. no daemon (`SLUICE_SOCKET` unusable) → in-process, inherited stdio, semaphore held; daemon
    without `"exec"` (fake server) → in-process; `exec` sent to an old server answering
    `bad-request` → in-process.
11. `--timeout 1s` on `sleep 10` → 124; command not found → 127; `daemon stop --force` → 143 with
    the message.
12. nested `sluice exec` inside an exec job runs directly (no second job in `status`).
13. queue-only: two passes in a row both run (counter = 2), no marker or miss record; simultaneous
    identical requests → one run; different trees → two runs; agent hook with `fail-fast` waits
    instead of exit 3; v1 config without `mode` parses unchanged; `mode = "queue-only"` + `cache =
    true` → error text; proto round-trip and golden fixtures unchanged for `run`, `status`,
    `welcome`.

A2 (`plugin/hooks/test-guard.sh` end to end + unit tests in `guard/`; ≥ 60 fixtures): each heavy
entry rewritten; each canonical equivalence (`npm run build`, `npm run-script build`, `pnpm build`,
`pnpm run build`, `yarn build`, `bun run build`, `npx tsc`, `pnpm exec tsc`, `python -m pytest`,
`./gradlew build`, `docker-compose build`); builtins not mapped (`pnpm install`, `yarn add x`); gate
verbatim, gate with extra, gate in another spelling with/without extra, runner-only match → exec,
gate under `cd sub &&` → exec, env prefix on a gate → exec; `&&`, `||`, `;`, first-in-pipeline
rewrite; downstream of a pipe, `&`, heredoc, `<`, `$(`, backticks, subshell, braces, newline,
unbalanced quotes, quoted heavy word (`echo "make"`), `sh -c`, `sudo make`, `xargs make`, `sluice
...` → unchanged; `docker build -` unchanged; `sluice queue approve` still denied; permission matrix
rows (deny rule, ask rule, allow rule, bypass, default, auto, plan, dontAsk, unparseable settings);
`updatedInput` keeps `description`, `timeout`, `run_in_background`; JSON with escapes and unicode
round-trips; malformed JSON, empty input, missing cwd; `SLUICE_GUARD=0`, `SLUICE_GUARD=deny` texts;
`sluice` missing from PATH and a fake pre-0.3 `sluice` → legacy deny results identical to today's 21
fixtures; user config `heavy_commands`, `heavy_commands_extra`, invalid config → no rewrite;
property test: removing inserted prefixes restores the original byte for byte.

H5 (`crates/sluice-gate/tests/shim.rs`): resolution with nested PATHs (shim dir first, middle,
twice, via symlink); a second shim copy in another dir is skipped; shim dir only → 127, no recursion
(process count bounded); exit codes, stdout/stderr bytes and a TTY (via `script`) preserved in
pass-through; human env → pass-through with zero Sluice side effects; `SLUICE_SHIMS=1` and agent env
→ exec job visible in `status`; `SLUICE_JOB` set → pass-through; non-heavy subcommand pass-through;
wrapper fallback when the recorded `sluice` path is gone; 10 shells calling a shimmed `make` with
budget 4 and weight 2 → peak 2; install/uninstall never touch files without the marker.

## 8. Open questions (decided)

1. Forward the caller's stdin to the daemon child by `SCM_RIGHTS`? Decided: no; stdin is
   `/dev/null`, plus the guard/shim rules that leave stdin readers alone.
2. Offer a TTY for humans (`--tty`, in-process with inherited stdio)? Decided: no;
   `SLUICE_DISABLE=1` already gives a direct run.
3. Cap spool size? Decided: no cap in 0.3.0; `[daemon] exec_spool_max_mib` can be added later if
   builds produce gigabytes (spools are pruned with the job logs).
4. Allow-rule matching in 4.6 is an approximation of Claude Code's matcher. Decided: conservative
   (deny and ask rules match liberally against the original and the rewrite, allow rules strictly;
   any doubt → `ask` or no rewrite); the haiku run confirmed the hook behaviour (guard-rewrite.md).
5. `sluice run` invoked with `SLUICE_JOB` set (a gate inside an exec job) can wait for budget the
   parent holds. Decided: same pass-through rule as exec (run in-process, unqueued), logged as
   decision `nested`.
6. Default exec weight `ceil(B/2)` vs B0's learned weights. Decided: `[agent] exec_weight` or
   `ceil(B/2)`; exec jobs never learn a weight (their CPU may escape the process group, ADR-0011
   M2).
