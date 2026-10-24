---
title: "Change: containment proved with real agent sessions (task E2)"
tags: [sluice, change, containment, cgroups, e2e, plugin]
---

# Change: containment e2e with real sessions (2026-10-24)

Task E2 of [[sluice/plan/containment-plan.md]]. Builds on
[[sluice/changes/containment-session.md]] (C3, C4) and
[[sluice/changes/containment-daemon.md]] (C2); method from the spikes
[[sluice/research/containment-s1-sandbox.md]] and
[[sluice/research/containment-s3-agent-tree.md]].

## What changed

`scripts/plugin-e2e.sh` gains four cases, selectable with
`SLUICE_E2E_CASES` (default now `a,c,b,d,e,f,g,h,i,j,k,l,m,n`). Every session
is a real `claude -p --model haiku` with the plugin loaded by `--plugin-dir`;
commands are tiny (`cat`, `echo`, `ps`, one `sleep` of 6 to 11 s so that the
poller can see the process); no CPU or memory load.

| Case | Asserted |
|---|---|
| k | Agent (`claude`), the plugin's `sluice mcp` in `.../s-<session>/ui`; the Bash shell in `.../s-<session>/tools`. Inside view (the command prints `/proc/$$/cgroup` and `/proc/$CLAUDE_PID/cgroup`) and outside view (poller). |
| l | A Task subagent's Bash command is in `tools` of the same session (subagent's own `/proc/$$/cgroup` plus the poller seeing its `sleep 9` in `tools`). |
| m | Bash sandbox on (`--settings` file: `sandbox.enabled`, `autoAllowBashIfSandboxed`, `network.allowAllUnixSockets` for this Ubuntu, S1 finding 0): pid 1 of the command is `bwrap` (proves the sandbox was active), shell in `ui`, `ps -o ni= -p $$` is 19, the poller sees the command's `sleep 11` in `ui` at nice 19 and never in `tools`. |
| n | `--resume <id>`: two different `CLAUDE_PID`s, both agents in `ui`, both shells in `tools`, two `claude` processes seen by the poller in a `ui` leaf, and the env file of the session holds `SLUICE_SESSION_LEAF=` once (no duplicate line). |

All four also assert `contained=full` in the hook's `client.log` and that every
process the poller saw is inside the test units.

### Isolation

- Units: `SLUICE_TEST_CONTAINMENT_PREFIX=sluice-test-e2e-<pid>-<case>`; the
  slice, daemon scope and agents scope never touch `sluice.slice`,
  `sluice-daemon.scope` or `sluice-agents.scope`. Cleanup after each case:
  stop the poller, `sluice daemon stop --force` for the case's private runtime
  dir, then stop and reset every unit matching `sluice-test-e2e-<pid>*` and
  the implicit parent `sluice-test-e2e.slice` (only when no other run uses
  it); `sluice-test.slice` is shared with other tests and left alone.
- The script's HOME and `XDG_RUNTIME_DIR` are private, so the hook could not
  find the user manager. Each containment case gets its own runtime dir whose
  `systemd` and `bus` entries are symlinks to the real manager's sockets
  (`detect` and `systemctl --user` follow them), plus its own `XDG_STATE_HOME`.
  The daemon, its lock and socket stay private, and the old cases still run
  without containment (no prefix, unreachable manager), as before.
- The poller (`python3 -I`, background) lists `/proc/*/cgroup` every 0.3 s and
  records pid, comm, cmdline, cgroup and nice of every process whose cgroup
  contains the run's prefix. It only reads `/proc`.
- Real sessions use the user's login (`~/.claude` symlink) as the other cases.
  Their `session-env/<id>` directories stay there like for every case.

## Verification

Claude Code **2.1.310**, haiku, Ubuntu with systemd 259, cgroup v2, bubblewrap
0.11.1, release build of `containment-integration` (`3d1ac07`):

`SLUICE_E2E_CASES=a,k,l,m,n SLUICE_BIN=<release> scripts/plugin-e2e.sh`

| Case | Result | Evidence (outside view, pid, leaf, nice) |
|---|---|---|
| a (smoke, old case) | PASS | `verify` tool called |
| k | PASS | `claude 2052088` and `sluice mcp 2052130` in `s-650a1ddb-.../ui`; `zsh 2052341` and `sleep 7` in `.../tools`; all nice 0; `contained=full`, hook 84 ms |
| l | PASS | `claude` and `sluice mcp` in `ui`; subagent shell printed `.../s-<id>/tools`; `zsh` and `sleep 9` in `tools`; hook 78 ms |
| m | PASS | pid 1 `bwrap`; `zsh` and `sleep 11` in `.../s-d7a0bb2e-.../ui` at nice 19; none in `tools`; hook 167 ms |
| n | PASS | session 1 agent 2055756, resumed agent 2056374 (new process), both in `s-b5f14b69-.../ui` with their `sluice mcp`; shells and `sleep 6` in `tools`; session id unchanged on resume; env file `sessionstart-hook-1.sh` holds the line once (`total=1 max-per-file=1`) |

Before this final run, k, l and m passed on the first attempt and n passed on
its own and in a `m,n` run. No prompt needed a retry (haiku followed all of
them the first time).

After every run `systemctl --user list-units --all 'sluice-test-e2e*'` is
empty and no process has `sluice-test-e2e` in its cgroup.

## Failure seen once (not reproduced)

In the first complete run (`l,m,n,k,a`, all four cases sharing the unit names
`sluice-test-e2e-<pid>*`), case n failed: the hook of the first session
logged `contained=partial` (157 ms), so that session's agent and shell stayed
in the user's terminal scope (`app-gnome-dev.warp.Warp-*.scope`); the resumed
session (second hook) was `full`. Afterwards the agents scope was `failed`
and its `holder/` leaf of a dead scope still held the hook's stopgap
`sleep 86400` (a process outside any live unit until killed by hand; the
test killed it).

What is known: the hook ran seconds after case m's cleanup had stopped units
with the same names; no daemon was running in that case (no daemon log), so
the hook alone created the scope; another agent was running its own
`sluice-test-e1-*` tests on the same user manager at the time (a `daemon-reload`
warning appeared). Repeats: the same `m,n` pair once, `n` alone once, the
full `a,k,l,m,n` run once, and 48 hook invocations in a loop that stop the
units and start the hook again at once (script not kept; same names) all gave
`contained=full`. Not reproduced, root cause not established. Likely
suspects: a race between a transient scope restarted under the name of one
that was just stopped, or the concurrent `daemon-reload`. Risk in real use:
low (the scope name is reused only when the last session of a day ends and a
new one starts immediately), but the symptom matters: a hook that creates the
scope must not leave its `sleep 86400` holder in a scope that then dies. Worth
a look in review (R1): after `ensure_agents_scope` succeeds, re-check that
the scope is still active, and kill the holder when it is not.

The script now gives every case its own unit names
(`sluice-test-e2e-<pid>-<case>`), which removes the restart-under-the-same-name
pattern from the harness; the final run above used it.

## Notes for later

- The default sandbox (seccomp filter on) still cannot be exercised here:
  case m needs `allowAllUnixSockets` (S1). When the AppArmor profile is fixed
  on a host, drop that key from the settings in case m.
- Interactive TUI and IDE integrations (E3) are not covered: `CLAUDE_PID` and
  the process tree were checked only for `-p` sessions.
