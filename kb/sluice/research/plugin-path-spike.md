---
title: Plugin PATH spike (how an agent plugin puts shims first in PATH)
tags: [sluice, research, claude-code, plugin, path, shims, spike]
---

# Plugin PATH spike (task S0 of the zero-config plan)

Measured 2026-10-23 with Claude Code **2.1.309** on Linux (model haiku, the
only allowed use of real sessions). Question: how can an agent plugin put a
directory of shell shims **first** in the `PATH` of every Bash command of a
session, including subagents and background commands?

## Method

A throw-away plugin (outside the repository, deleted afterwards) loaded with
`claude -p --model haiku --plugin-dir <plugin> --output-format stream-json
--verbose --permission-mode bypassPermissions`:

- `bin/npm` and `bin/probe`: print their own `$0` and `PATH`.
- A stable shim directory `<scratch>/shims/` with an `npm` that prints
  `SHIM npm`.
- A `SessionStart` hook (matcher `startup|resume|clear|compact`) that logs its
  stdin and a timestamp and appends `export PATH="<scratch>/shims:$PATH"` and
  `export SPIKE_ENV=1` to `$CLAUDE_ENV_FILE`.
- A `PreToolUse` hook (matcher `Bash`) that logs its stdin JSON, a timestamp
  and the `PATH`/`SPIKE_ENV` of the hook process itself.

Session 1 asked the agent to run, as separate Bash calls: `echo "$PATH";
echo "SPIKE_ENV=$SPIKE_ENV"; command -v npm; npm`, the same in `( ... )`,
`x=$(npm)`, `sh -c '...'`, a script file `./x.sh`, a `run_in_background`
call (output read from the task output file), and finally a general-purpose
subagent (Agent tool) that ran the foreground, `( )`, `sh -c`, `./x.sh` and
background variants. Session 2 repeated foreground, background and subagent
calls with `--resume <session id>`. Results were taken from the `tool_result`
blocks of the stream-json, not from the model's prose. The user's own sluice
plugin 0.3.0 and a few other plugins were also enabled in these sessions;
they did not interfere (the probed commands are not heavy commands).
`/clear` is not scriptable in `-p` mode and was not tested.

## Results

"Shim first" means `command -v npm` resolved to the shim in `shims/` and
running `npm` printed `SHIM npm` (a real `npm` exists under nvm later in the
PATH, so the shim really shadowed it).

| Context | Plugin `bin/` alone | `CLAUDE_ENV_FILE` prepend |
|---|---|---|
| Main session, foreground Bash | **no** (real npm wins) | **yes** |
| Background Bash (`run_in_background`) | no (same PATH) | **yes** (`SPIKE_ENV=1` too) |
| Subshell `( ... )` | no | **yes** |
| `$(...)` | no | **yes** |
| `sh -c '...'` | no | **yes** |
| Script file `./x.sh` | no | **yes** |
| Subagent, foreground Bash | no | **yes** (`SPIKE_ENV=1` too) |
| Subagent, `( )`, `sh -c`, `./x.sh` | no | **yes** |
| Subagent, background Bash | no | **yes** (`SPIKE_ENV=1` too) |
| After `--resume`: main, background, subagent | no | **yes** (but see duplication below) |

Position of the plugin `bin/` directory: it is **appended at the end** of the
Bash tool `PATH` (after `/snap/bin`), so a `bin/npm` can never shadow a real
`npm` found earlier. The full PATH seen by every Bash command was
`<shims>:<user dirs...>:/snap/bin:/snap/bin:<plugin>/bin`. Plugin `bin/` is
useful to add new command names (e.g. a `sluice-probe`), not to intercept
existing ones.

`CLAUDE_ENV_FILE` content is exported by every Bash invocation, so children
(subshells, `sh -c`, scripts, subprocesses) inherit it with no further work.
Subagent Bash calls and background commands source it exactly like the main
session: nothing special is needed for them.

### Hook facts observed

- **Timing.** `SessionStart` ran at t0, the first `PreToolUse` 4 s later; the
  env file was therefore already written before the first Bash command (and
  the first Bash command already had the shims). Input of the hook:
  `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `source`
  (`startup` here).
- **Resume.** `SessionStart` fires again with `source: "resume"`. The env file
  path is **reused** (`~/.claude/session-env/<session id>/sessionstart-hook-1.sh`)
  and the hook's `>>` appends to the old content, so after a resume the PATH
  became `<shims>:<shims>:...` (duplicated). Harmless, but it grows with every
  resume. The hook must be idempotent: either rewrite the file (`>`), or
  guard the line, e.g.
  `case ":$PATH:" in *":<dir>:"*) ;; *) export PATH="<dir>:$PATH";; esac`.
- **Env file name.** `sessionstart-hook-<n>.sh`: one file per SessionStart
  hook; other plugins' hooks have their own files.
- **PreToolUse hook environment is not the Bash environment.** The hook
  process saw `PATH` without the shims directory, without the plugin `bin/`
  and without `SPIKE_ENV`. A guard must not infer the Bash `PATH` from its own
  environment; it can only know the shim directory from its own configuration
  (state dir) or by checking that the directory exists.
- **Subagent calls.** `PreToolUse` fires for subagent Bash calls and the hook
  input carries `agent_id` and `agent_type` (`general-purpose`); main session
  calls have neither field. The hook input also carries `prompt_id`,
  `permission_mode`, `effort`, `tool_use_id`.
- `CLAUDE_PLUGIN_ROOT` is not exported to the Bash tool (not re-measured; the
  hook command itself is given an absolute path in this spike). The SessionStart
  hook must therefore write an **absolute, stable** directory into the env file
  (the Sluice state dir), not a plugin-cache path that changes on update.

## Recommendation

**Option 2: prepend the stable shim directory through `CLAUDE_ENV_FILE` from
a `SessionStart` hook.** Measured on every context requested, including
subagents and background commands, before and after `--resume`. Option 1
(plugin `bin/` alone) is ruled out: the directory is last in `PATH`, so it
cannot shadow `npm`, `npx`, `ng`, etc. Option 3 (guard fallback for subagents)
is **not needed**: subagents and background commands source the env file.

Caveats to carry into T5:

1. Make the hook idempotent (see resume above): replace the file content or
   guard the `export`.
2. The hook must fail open: if it cannot create the shim dir, it writes
   nothing and exits 0.
3. Not measured here and still limiting for any PATH approach: absolute-path
   invocations (`/usr/bin/npm`), `env -i`, login shells that reset `PATH`
   (`bash -l`, `sh -l`), and `npm run` scripts, which prepend
   `node_modules/.bin` in front of the inherited PATH (a script that calls
   `ng` directly reaches `node_modules/.bin/ng`, not the shim; the shim of
   `npm`/`npx` themselves still catches the entry point).
4. `/clear` and `compact` re-fire `SessionStart` by contract of the matcher but
   were not exercised (not scriptable in `-p`); the idempotent hook covers
   them.
5. Behaviour is specific to Claude Code 2.1.309; re-run the spike (one prompt
   per context) when the version changes. The experiment is cheap: two haiku
   sessions, about a minute.
