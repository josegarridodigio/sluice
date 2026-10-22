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
4. Default exec weight is `ceil(budget / 2)`; coalescing only with `--coalesce-key`.
5. Queue-only gates use a new optional field `mode = "queue-only"`; `cache` stays a bool.
6. The guard's logic moves into `sluice guard` (Rust); `guard.sh` is a thin wrapper that keeps
   today's shell deny logic as fallback when the binary lacks `guard`.
7. Rewrites insert a fixed prefix at a segment start of the untouched original string and are
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
sluice exec [--weight N] [--label L] [--coalesce-key K] [--timeout DUR] [--quiet] -- <cmd> [args...]
```

Module `crates/sluice-gate/src/exec.rs`, `pub fn exec(args: &ExecArgs) -> ExitCode`. `<cmd>` is an
argv (no shell); `sluice exec -- sh -c '...'` is the way to run a script.

### 2.1 Client flow

1. If `SLUICE_JOB` is set (we are inside a Sluice job): `CommandExt::exec` the command directly
   (inherited stdio, no queue). The parent job's weight already covers it.
2. Resolve `argv[0]` with `sluice_gate_core::which::resolve(name, PATH, cwd, skip_shims=true)`
   (skips the shim dir and any file starting with the shim marker, section 5). Not found: `sluice:
   exec: cannot run '<cmd>': command not found`, exit 127; not executable: 126.
3. Label: `--label` or the first two words of argv (`cargo build`), keeping only letters, digits,
   space and `._:/-`, max 48 chars. Weight: `--weight` or `[agent] exec_weight` or `ceil(effective_budget / 2)`;
   clamped by the daemon as for gates.
4. Repo identity: best-effort `git rev-parse --show-toplevel --git-common-dir` in cwd (one call,
   failure ignored) so `repos.<path>.budget_share_max` applies; else `null`.
5. `client::ensure_daemon`, handshake. Daemon without feature `"exec"`, refusal, or any error before
   `queued` → in-process fallback (2.6).
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

`server.rs`: `handle_exec` registers an `Entry` like `register` with `key = None`, `request =
JobRequest::Exec(Arc<ExecRequest>)`. `start_job` builds the `ExecSpec` with two new fields: `launch:
Launch` (`Shell { cmd, extra }` for gates, `Argv { program, args }` for exec) and `sink: Sink`
(`Log` or `Spool`). Everything else is reused: new process group, `renice`/`ionice`, `KILL_GRACE`,
orphan reconciliation, `NO_TIMEOUT` unless `--timeout`. Every job's env gets `SLUICE_JOB=<job id>`
(gates too). Exec ignores `last_subscriber_gone = finish` (nothing to cache): the last subscriber
leaving kills it.

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
  only detaches this subscriber; then after `KILL_GRACE` + 5 s with the group alive,
  `terminate_group`. A second signal at the client, or the client's SIGKILL (Bash tool timeout),
  closes the connection: the existing cancel path kills the group.

### 2.5 Exit codes and messages

| Situation | Exit | stderr (omitted with `--quiet`, except errors) |
|---|---|---|
| child exited `n` | `n` | nothing |
| child killed by signal `s` | 128+s | nothing |
| `--timeout` expired | 124 | `sluice: exec '<label>' TIMEOUT after <d>` |
| killed by `daemon stop`/lost lease | 143 | `sluice: exec '<label>' KILLED: the daemon stopped (job <id>)` |
| not found / not executable | 127 / 126 | `sluice: exec: cannot run '<cmd>': <why>` |
| Sluice error after `queued` | 125 | `sluice: exec: <message>` |
| waited ≥ 1 s in the queue | — | `sluice: exec '<label>' queued (position P, U of B units busy)` then `sluice: exec '<label>' started after 12.4s (job <id>)` |
| coalesced | — | `sluice: exec '<label>' sharing job <id>` |

### 2.6 In-process fallback

`sluice: daemon unavailable (<why>); running '<label>' in-process` (one stderr line), then take
`weight` slots of the `flock` semaphore (`acquire_budget`, moved from `run.rs` to
`crates/sluice-gate/src/budget.rs` and shared), spawn with inherited stdin/stdout/stderr in the
client's process group, set `SLUICE_JOB=inproc-<pid>`, wait, map the code. Signals are blocked only
after the spawn (the child keeps a clean mask); a received signal is forwarded with `kill(child)`.
Not visible in `status`, no nice/ionice (as gates today).

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
"queue-only" never caches; remove cache = true`; `cache = false` is accepted (redundant). Docs:
"needs sluice ≥ 0.3.0".

Semantics (`run.rs`, `lookup.rs`, `server.rs`):
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
  merge queue may list it in `queue.gates`.

## 4. Guard rewrite (agent plugin)

### 4.1 Shape

`plugin/hooks/guard.sh` becomes: exit 0 if `SLUICE_GUARD=0`; run `sluice guard --hook` with the hook
JSON on stdin (no daemon contact, reads files only); if it exits 0, print its stdout when it starts
with `{"hookSpecificOutput"`; otherwise (binary missing, a pre-0.3 binary without `guard`, crash)
run the current shell logic unchanged (`legacy_main`: deny gates, deny `sluice queue approve`).
Always exit 0. Reason: rewriting needs exact JSON echo of `tool_input`, the user TOML config, the
Claude settings JSON and a quote-aware tokenizer; doing that in `sh` + optional `jq` is fragile, and
the binary is already required by the plugin's MCP server. A Rust start costs ~2 ms, less than
today's dozen `sed` forks.

Code: `crates/sluice-gate/src/guard/{mod.rs,lex.rs,matcher.rs,rules.rs}`;
`sluice_gate_core::heavy::{DEFAULT_HEAVY_COMMANDS, HeavyMatcher, canonical_words}` shared with
shims. `guard::run_hook() -> ExitCode` never returns non-zero and never panics outward
(`catch_unwind`, output only on success).

### 4.2 Tokenizer (`lex::split(command) -> Option<Vec<Segment>>`)

A segment is a simple command with byte offsets into the original string: optional leading
`VAR=value` words, words, redirections, and the operator that ends it (`&&`, `||`, `;`, `|`). Single
and double quoted words are supported when balanced and on one line; inside double quotes `$`, `` `
`` and `\` make the whole command unparseable. `None` (never rewritten, never denied by the rewrite
logic; the legacy deny checks still run on the naive split) for: newlines or `\` continuations,
heredocs `<<`, input redirection `<`, process substitution, `$(`, backticks, `(`, `)`, `{`, `}`, a
single `&` (background), `|&`, unbalanced quotes, non-UTF-8, more than 4096 bytes.

### 4.3 Matching

Per segment, after skipping env assignments and an optional leading `time`:
- **Skip** when the first word is `sluice` (`sluice queue approve` stays a deny), or one of `sudo`,
  `env`, `nohup`, `nice`, `timeout`, `xargs`, `exec`, `eval`, `sh`, `bash`, `zsh`, `command`,
  `builtin` (wrappers and `sh -c` are never looked into).
- **Directory**: a previous segment `cd <word>` joined with `&&` changes the effective directory
  (relative to `cwd`, no `~` or variables, must exist), else no rewrite at all.
- **Gate**: config found by walking up from the effective directory. A gate matches when the
  effective directory is the config's directory, the segment has no env assignments, and the
  canonical words of the segment start with the canonical words of the gate `cmd` (one-line gates
  only). Rewrite: `sluice run <gate> -v -- <rest>` when the typed words equal the gate words as
  spelled (rest = the remaining original substring, verbatim), or `sluice run <gate> -v` when only
  the canonical forms are equal and there is no rest (a different package manager would parse extra
  args differently). Runner-only matches (`cargo test -p x` for a gate `cargo test --workspace`) are
  not gate matches; they fall through to heavy.
- **Heavy**: canonical words start with an entry of the heavy list (word boundary). Rewrite: insert
  `sluice exec --label <L> -- ` before the command word (after env assignments, so they reach exec
  and then the child). `L` = first two canonical words joined with `-`, filtered to `[a-z0-9._-]`,
  max 32 chars. Not rewritten when an argument is `-` (reads stdin) or the segment is not the first
  of a pipeline (it would read stdin). A heavy segment first in a pipeline is fine: exec's stdout is
  the child's.

Canonical words (`canonical_words`), used only for matching, never for output: `npm run X`, `npm
run-script X`, `pnpm run X`, `yarn run X`, `bun run X` → `run X`; `npm test|start|stop|restart` →
`run <that>`; `pnpm X` / `yarn X` → `run X` unless X is a package-manager builtin (`add audit bin
cache check config create dedupe dlx exec import info init install i link list ls outdated pack
prune publish rebuild remove rm root store unlink up update upgrade why workspace workspaces`); `npx
[-y|--yes] X`, `pnpm exec X`, `yarn exec X`, `bunx X` → `X`; `pnpm -C d|--dir d`, `npm --prefix d`,
`yarn --cwd d` are kept as words (they are part of what runs) but skipped when finding the command
word; `python -m pytest`, `python3 -m pytest` → `pytest`; `./gradlew` → `gradle`, `./mvnw` → `mvn`;
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
clippy`, `cargo nextest`, `go build`, `go test`, `make`, `gradle`, `mvn`, `tsc`, `vite build`,
`webpack`, `next build`, `pytest`, `jest`, `vitest`. The guard reads the config through the normal
strict parser (re-read per call, as the client does); a parse error means "no rewrites" and a stderr
note that only the debug log shows. Old binaries reading a user config with `[agent]` fail loudly;
the user config is per machine and upgrades with the binary, so this is accepted.

### 4.5 Fail-open rules

Any error (unreadable JSON, missing `tool_input.command`, unparseable command, unreadable config,
settings that cannot be parsed, a panic) → no output, exit 0, and the original command goes through
Claude Code's normal flow. The guard never blocks on I/O beyond local files, never contacts the
daemon, and caps its own run at 500 ms (a watchdog thread exits 0 with no output). `SLUICE_GUARD=0`
disables everything, `SLUICE_GUARD=deny` selects suggestion mode (4.8).

### 4.6 Permission safety

`guard::rules` reads `permissions.allow|ask|deny` from `$CLAUDE_CONFIG_DIR` or
`~/.claude/settings.json`, `$CLAUDE_PROJECT_DIR/.claude/settings.json` and `settings.local.json`,
and `/etc/claude-code/managed-settings.json`. Only `Bash` rules matter (`Bash`, `Bash(prefix:*)`,
`Bash(glob *)`, exact).

| Condition (first that applies) | Output |
|---|---|
| any deny or ask rule matches any segment of the original, or a bare `Bash` ask/deny rule, or a settings file fails to parse | nothing (normal flow on the original) |
| `permission_mode` is `plan`, `auto` or `dontAsk` and the original is not covered by allow rules | nothing |
| `permission_mode = bypassPermissions`, or every segment of the original matches an allow rule | `allow` + `updatedInput` + `additionalContext` |
| `default` / `acceptEdits` (absent counts as `default`) | `ask` + `updatedInput` + reason (the user would have been prompted for the original anyway; now the prompt shows the rewritten command) |

So a rewrite never runs something the user would not have run, and never hides a deny or ask rule.
Command-line `--disallowedTools` flags are invisible to hooks; documented.

### 4.7 Injection argument, output and examples

The rewrite is `original[..o] + PREFIX + original[o..]` for each insertion offset `o` (segment
command-word starts found by the tokenizer). `PREFIX` is a constant plus the gate name
(`[a-z0-9_-]`, validated by the config parser) or a label from `[a-z0-9._-]`. Nothing from the
original is re-quoted, unescaped or moved, and every character of the original keeps its position
relative to its neighbours, so the shell parses the agent's words exactly as before; only words are
added in front. The JSON output is built with `serde_json` from the received `tool_input` with
`command` replaced.

| Original (`cwd` = repo root, gate `test` = `cargo test --workspace`) | Rewritten |
|---|---|
| `cargo test --workspace -- --nocapture` | `sluice run test -v -- -- --nocapture` |
| `cargo build --release 2>&1 \| tail -20` | `sluice exec --label cargo-build -- cargo build --release 2>&1 \| tail -20` |
| `cd web && pnpm build` | `cd web && sluice exec --label run-build -- pnpm build` |
| `RUSTFLAGS=-Dwarnings cargo clippy` | `RUSTFLAGS=-Dwarnings sluice exec --label cargo-clippy -- cargo clippy` |
| `npx vitest run` | `sluice exec --label vitest-run -- npx vitest run` |
| `make && make install` | `sluice exec --label make -- make && sluice exec --label make -- make install` |
| `docker build -t "my app" .` | `sluice exec --label docker-build -- docker build -t "my app" .` |
| `sh -c "make"`, `make &`, `cat x \| make`, `docker build - < Dockerfile`, `echo "cargo build"`, `sluice exec -- make`, `bash -c "$(cat run.sh)"` | unchanged |

What the agent sees: the rewritten command's own output and exit code (`sluice run -v` streams the
gate log on stderr and prints the gate summary; `sluice exec` prints only the command's output plus
at most two queue lines on stderr), and the `additionalContext`: ``Sluice ran this through its
shared queue as: `<rewritten>`. Output and exit code are the command's own. Use the `verify` tool
for gates.`` (gate case: ``... as gate '<gate>' (cached per tree)``).

### 4.8 Suggestion mode and missing support

`SLUICE_GUARD=deny` (for Claude Code versions where allow + `updatedInput` misbehaves, found in A2's
haiku check or reported by users): gate matches keep today's deny text; heavy matches deny with
``This machine shares its CPU through Sluice: run `<rewritten>` instead (same output and exit
code).`` The legacy shell path never rewrites.

## 5. Shell shims (H5)

`sluice shim install [--dir DIR] [--force]` writes one file per command word of the heavy list
(`docker cargo go make gradle gradlew mvn mvnw tsc vite webpack next pytest jest vitest npm pnpm
yarn bun npx`) into `~/.local/share/sluice/shims` (mode 0755, dir 0700, atomic rename; refuses to
overwrite a file without the marker unless `--force`):

```sh
#!/bin/sh
# sluice-shim v1
SLUICE_SHIM_DIR='/home/u/.local/share/sluice/shims'; export SLUICE_SHIM_DIR
if [ -x '/home/u/.local/bin/sluice' ]; then
    exec '/home/u/.local/bin/sluice' shim exec make -- "$@"
fi
PATH=$(printf '%s' "$PATH" | tr ':' '\n' | grep -vxF "$SLUICE_SHIM_DIR" | paste -sd: -)
exec make "$@"
```

`sluice shim exec <name> -- args` (`crates/sluice-gate/src/shim.rs`):
1. `real = which::resolve(name, PATH, cwd, skip_shims=true)`: walks `PATH`, skips every entry whose
   canonical path equals the canonical `SLUICE_SHIM_DIR`, and every candidate whose first 64 bytes
   contain `# sluice-shim v1` or that canonicalises to the running `sluice` binary. Not found:
   `sluice: shim: no '<name>' in PATH after <dir>`, exit 127.
2. Pass-through (`CommandExt::exec(real, args)`, argv[0] kept) when `SLUICE_JOB` is set (inside
   `sluice exec`, a gate, or a rewritten command: no double wrapping), when `SLUICE_SHIMS` is falsy,
   when neither `SLUICE_SHIMS` is truthy nor `agent::detect` says agent, when the canonical words of
   `name args` do not match the heavy list (`docker ps`, `npm install`), or when an argument is `-`.
3. Otherwise run the exec client in-process (no extra `sluice` spawn) with argv `[real, args...]`,
   label = canonical words.

`sluice shim uninstall` removes only files with the marker; `sluice shim status` lists shims,
whether the dir is in `PATH` and before the real binaries, and which real binary each resolves to.
`doctor` reports a shim dir that is in `PATH` while the shims are stale (sluice path changed). Docs
(`docs/agents.md`): put the dir first in `PATH` only for agent shells
(`PATH="$HOME/.local/share/sluice/shims:$PATH"` in the agent's environment or launcher), set
`SLUICE_SHIMS=1` for agents that set no detectable variable, plus a ready-to-paste `AGENTS.md`
block. Claude Code users do not need shims (the guard covers them); both together are safe because
of `SLUICE_JOB` and the resolver skip.

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

## 8. Open questions (with recommendation)

1. Forward the caller's stdin to the daemon child by `SCM_RIGHTS`? Recommended: not in alpha;
   `/dev/null` plus the guard/shim rules that leave stdin readers alone.
2. Offer a TTY for humans (`--tty`, in-process with inherited stdio)? Recommended: no;
   `SLUICE_DISABLE=1` already gives a direct run.
3. Cap spool size? Recommended: no cap in alpha; measure in E3 and add `[daemon] exec_spool_max_mib`
   later if builds produce gigabytes.
4. Allow-rule matching in 4.6 is an approximation of Claude Code's matcher. Recommended: keep it
   conservative (any doubt → `ask` or no rewrite) and compare with the haiku run.
5. `sluice run` invoked with `SLUICE_JOB` set (a gate inside an exec job) can wait for budget the
   parent holds. Recommended: same pass-through rule as exec (run in-process, unqueued) in A1,
   logged as decision `nested`.
6. Default exec weight `ceil(B/2)` vs B0's learned weights. Recommended: ship `ceil(B/2)`; B1 may
   replace it with the learned weight per label once available.
