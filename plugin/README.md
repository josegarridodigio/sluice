# Sluice plugin for Claude Code

Makes Claude Code verify through [Sluice](../README.md) instead of running
test suites in its own shell: one queue and one cache shared by every agent on
the machine, one run per git tree.

## Install

Prerequisite: the `sluice` binary in the `PATH` that Claude Code sees.

```sh
curl -fsSL https://raw.githubusercontent.com/josegarridodigio/sluice/main/scripts/install.sh | sh
```

Then, inside Claude Code:

```
/plugin marketplace add josegarridodigio/sluice
/plugin install sluice@sluice
```

or from a shell: `claude plugin marketplace add josegarridodigio/sluice` and
`claude plugin install sluice@sluice`. On Claude Code 2.1.275 or later,
`/plugin install sluice --marketplace josegarridodigio/sluice` does both in one
step. Run `/reload-plugins` (or restart) and check `claude mcp list`: the
`plugin:sluice:sluice` server should be connected.

Nothing else has to be added to `CLAUDE.md` or `PATH`: when a session starts,
the plugin installs the shell shims, puts them first in the session's `PATH`
and gives the agent the rules. In a git repository without a `.sluice.toml`
the agent offers to run `sluice init` and asks you first.

## What is inside

| Piece | File | What it does |
|---|---|---|
| MCP server | `.mcp.json` | Registers `sluice mcp`. Tools: `verify`, `status`, `gate_list`, `logs`, `queue_enqueue`, `queue_status`, `queue_approve`, `queue_retry`, `queue_cancel`. From a plugin they are named `mcp__plugin_sluice_sluice__<tool>`; use that form in permission rules. |
| Skill | `skills/sluice/SKILL.md` (+ `reference.md`) | Loaded when the agent is about to run tests, linters or type checks, commit, push or land a branch in a repository with a `.sluice.toml`. Teaches `verify`, reading its result, exit code 3 from hooks, and the merge-queue flow (verify, `queue_enqueue`, stop, never approve). |
| Session hook | `hooks/hooks.json`, `hooks/session-start.sh` | `SessionStart` (`startup`, `resume`, `clear`, `compact`). Runs `sluice agent session-start`: installs the shims in `~/.local/share/sluice/shims`, prepends that directory to the session's `PATH` through `CLAUDE_ENV_FILE` (once) and adds the rules as context. A missing or older `sluice` is reported to the agent, which asks you before installing or updating. Test: `hooks/test-session-start.sh`. |
| Guard hook | `hooks/hooks.json`, `hooks/guard.sh` | `PreToolUse` on `Bash`. Runs `sluice guard`: rewrites gate commands to `sluice run <gate> -v` and heavy commands (builds, test runners) to `sluice exec -- ...` so they share the machine's budget, as `allow` or `ask` depending on your permission settings; denies `sluice queue approve` and raw gate commands it does not rewrite. Falls back to the 0.2.0 deny-only shell logic with an older `sluice`. |

### The guard hook

`hooks/guard.sh` is a thin wrapper. With `sluice` 0.3.0 or later in the
`PATH` it passes the hook JSON to `sluice guard --hook` (no daemon contact, it
reads files only) and prints its decision. With an older or missing binary it
runs the 0.2.0 shell logic described under "Shell fallback", which only denies.

What `sluice guard` does with a Bash command:

1. `sluice queue approve` anywhere in the command is denied: "Approval is
   reserved to humans."
2. It looks for segments to rewrite. A rewrite inserts a fixed prefix in front
   of a simple command of the untouched original; nothing the agent wrote is
   re-quoted, moved or removed (except a gate's own words, see below):
   - a gate command typed as the gate spells it, run from the directory of
     `.sluice.toml`: `cargo test --workspace -- --nocapture` becomes
     `sluice run test -v -- -- --nocapture` (cached per tree, the log streams
     on stderr); another spelling of the same command without arguments
     (`pnpm build` for a gate `npm run build`) becomes `sluice run build -v`;
   - a heavy command (`[agent] heavy_commands` of the user config; default
     `docker build`, `docker buildx build`, `docker compose build`, package
     `build` scripts, `cargo build|test|clippy|nextest`, `go build|test`,
     `make`, `gradle`, `mvn`, `tsc`, `vite build`, `webpack`, `next build`,
     `pytest`, `jest`, `vitest`): `cargo build --release 2>&1 | tail -20`
     becomes `sluice exec --label cargo-build -- cargo build --release 2>&1 | tail -20`
     (same output and exit code, run under the machine's budget).

   Equivalences used for matching only: `npm|pnpm|yarn|bun run X`, `pnpm X`,
   `yarn X` (not their builtins such as `install` or `add`), `npm test`,
   `npx X`, `pnpm exec X`, `pnpm -C dir`, `npm --prefix dir`,
   `yarn --cwd dir`, `python -m pytest`, `./gradlew`, `./mvnw`,
   `docker-compose`. A leading `cd dir &&` moves the directory for gate
   matching; `VAR=x cmd` keeps its assignments in front of the prefix (a gate
   with an env prefix becomes `exec`). Never rewritten: pipelines after the
   first command, `&`, heredocs and every `<` redirection, `$(`, backticks,
   subshells and braces, newlines, backslashes, unbalanced quotes,
   `sh -c`/`bash -c`, wrappers (`sudo`, `env`, `xargs`, `nohup`, `timeout`,
   `exec`, `eval`, ...), an argument `-`, commands starting with `sluice`,
   and `cd` mixed with `||` or `;`.
3. It answers according to the permission settings Claude Code reads
   (`~/.claude/settings.json` or `$CLAUDE_CONFIG_DIR/settings.json`,
   `$CLAUDE_PROJECT_DIR/.claude/settings.json` and `settings.local.json`,
   `/etc/claude-code/managed-settings.json`), because Claude Code matches its
   rules against the rewritten command:

   | Condition (first that applies) | Answer |
   |---|---|
   | a `Bash` deny or ask rule may match the original or the rewrite, or a settings file cannot be parsed | no rewrite |
   | `permission_mode` `plan`, `auto` or `dontAsk` and the original is not covered by allow rules | no rewrite |
   | `bypassPermissions`, or every segment of the original matches an allow rule | `allow` with the rewritten command |
   | `default` or `acceptEdits` | `ask`: the prompt shows the rewritten command |

   Every rewrite adds one line to the agent's context: "Sluice ran this
   through its shared queue as: `<rewritten>`. Output and exit code are the
   command's own. Use the `verify` tool for gates."
4. Without a rewrite, a raw gate command (or a raw runner the repository has a
   gate for) is denied as before, with the `verify` instruction.

It fails open: malformed input, an unreadable or invalid config, anything
unexpected, a panic or a run over 500 ms prints nothing and exits 0. An
invalid user config only disables rewrites. `SLUICE_GUARD=0` disables the
hook; `SLUICE_GUARD=deny` keeps today's deny for gates and denies heavy
commands with the rewritten command as a suggestion ("This machine shares its
CPU through Sluice: run `<rewritten>` instead (same output and exit code).")
for Claude Code versions where `allow` with `updatedInput` misbehaves.
`--allowedTools`/`--disallowedTools` flags are invisible to hooks and are not
taken into account.

#### Shell fallback

For a Bash command inside a repository with a `.sluice.toml`, the shell logic
splits the command on `&&`, `||`, `;`, `|`, `&` and newlines (quotes are not
parsed, so it errs on the side of denying), and compares each piece, after
normalising whitespace and skipping leading `VAR=value` words, with:

- the `cmd = "..."` of every gate (one-line commands): the piece is denied if it
  equals the gate command or starts with it plus a space;
- these raw runners, only when some gate's command starts with the same
  runner: `cargo test`, `cargo clippy`, `npm test`, `npm run test`, `pnpm test`,
  `yarn test`, `pytest`, `go test`.

A match is denied with: "This repository verifies through Sluice: call the
`verify` MCP tool with gates=["<name>"] (gate '<name>' runs this command). Raw
runs bypass the shared queue and cache." Any piece that starts with
`sluice queue approve` is denied with "Approval is reserved to humans."
Everything else is allowed silently.

Behaviour and limits (both paths):

- The fallback needs only `sh`, `sed`, `tr` and `head`. It uses `jq` when
  installed, otherwise a small `sed` fallback to read `tool_input.command` and
  `cwd`. The command text is only pattern-matched, never executed.
- It is a guardrail, not a security boundary: an agent can still build a
  command it does not recognise (`sh -c`, a wrapper script, a `Makefile`
  target). Multi-line `cmd = """..."""` gates are ignored. Pair it with
  `permissions.deny` rules and branch protection on the forge, see
  [docs/agents.md](../docs/agents.md).

## Test the hook

```sh
plugin/hooks/test-guard.sh
```

It feeds JSON fixtures to `guard.sh` and checks the decision: the 0.2.0
fixtures (allowed command, gate command, `cargo test -p x`,
`cd a && cargo test`, `sluice queue approve t-1`, a repository without
`.sluice.toml`, malformed JSON, `SLUICE_GUARD=0`) through the shell fallback
with `sluice` removed from `PATH` (with `jq` and with the `sed` fallback) and
with a fake pre-0.3 `sluice`, then the Rust path with the built binary
(`SLUICE_BIN`, else `target/debug` or `target/release`; skipped with a notice
when none is found): rewrites, `allow`/`ask`, refusals and the echoed
`tool_input`. It exits non-zero on any failure. The full fixture tables (heavy
list, equivalences, permission table, injection suite) are the Rust unit tests
in `crates/sluice-gate/src/guard/`.

## Validate and try locally

```sh
claude plugin validate .          # the marketplace (.claude-plugin/marketplace.json)
claude plugin validate ./plugin   # the plugin
claude --plugin-dir ./plugin      # load it for one session, without installing
scripts/plugin-e2e.sh             # real non-interactive sessions in a sandbox
```

`scripts/plugin-e2e.sh` uses the model in `SLUICE_E2E_MODEL` (default `haiku`)
and a scratch clone with a private HOME; it does not touch your Sluice daemon.
