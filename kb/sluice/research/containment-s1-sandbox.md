---
title: "Containment S1: cgroup self-placement inside the agent sandbox"
tags: [sluice, research, containment, cgroups, sandbox, bubblewrap, spike]
---

# Containment S1: cgroup self-placement inside the agent sandbox

Spike S1 of [[sluice/plan/containment-plan.md]] (ADR
[[sluice/design/adr/0017-agent-resource-containment.md]]). Measured
2026-10-24 with Claude Code **2.1.310**, model haiku, Ubuntu with systemd 259,
cgroup v2, bubblewrap 0.11.1, kernel 7.0.0, AppArmor with
`kernel.apparmor_restrict_unprivileged_userns = 1`.

Question: when the agent's Bash sandbox is on, can a command still move its own
shell into a delegated cgroup by writing `$$` to `<leaf>/cgroup.procs`, from the
main session and from a subagent? If not, what works?

## Method

- Delegated scope as in the earlier spike: `systemd-run --user --scope
  --unit=sluice-s1 --slice=sluice-s1.slice -p Delegate=yes sleep 900`, leaves
  `holder` (holds the scope's own process) and `tools`.
- Throw-away plugin (outside the repository, deleted afterwards) with a
  `SessionStart` hook that appends to `$CLAUDE_ENV_FILE`
  `echo $$ > <cg>/tools/cgroup.procs 2>/dev/null || echo "sluice-s1: move failed $?" >&2`,
  and logs its own cgroup, `CLAUDE_PID` and `/sys/fs/cgroup` mount options.
- Real sessions only: `claude -p --model haiku --plugin-dir <plugin>
  --output-format stream-json --verbose --permission-mode bypassPermissions
  [--settings '<json>']`. The prompt makes the agent run, as literal Bash
  commands, `echo SELF=$$; grep NSpid /proc/self/status; cat /proc/1/comm; cat
  /proc/self/cgroup; ls -l <cg>/tools/cgroup.procs; mount | grep cgroup; ls -l
  /run/user/1000/bus; sh -c 'cut -d: -f3 /proc/self/cgroup'` in the foreground,
  with `run_in_background` (output file read back), and through a synchronous
  Agent-tool subagent (foreground and background there too). Outputs were taken
  from the `tool_result` blocks of the stream-json.
- Settings keys (settings reference and sandboxing page of the docs):
  `sandbox.enabled`, `sandbox.failIfUnavailable`,
  `sandbox.autoAllowBashIfSandboxed`, `sandbox.network.allowAllUnixSockets`,
  `sandbox.filesystem.allowWrite`, `sandbox.allowNestedUserNamespaces`,
  `sandbox.enableWeakerNestedSandbox`.
- Direct `bwrap` emulations (read-only root, pid namespace) were used only for
  mechanics that a session cannot show (cgroup namespace, pre-sandbox move).

## Finding 0: the sandbox does not start with default settings on this host

With `{"sandbox":{"enabled":true,...}}` every Bash command (main session and
subagent, foreground and background) fails before running, even under
`bypassPermissions`:

```
apply-seccomp: write /proc/self/setgroups (nested userns is capability-restricted; caller must provide CAP_SYS_ADMIN): Permission denied
```

Cause: Ubuntu 24.04+ restricts unprivileged user namespaces; its stock
`/etc/apparmor.d/bwrap-userns-restrict` profile lets `bwrap` create the
namespace but strips capabilities from its children, and the agent's
`apply-seccomp` step (which blocks Unix sockets) needs a nested user namespace.
The docs' fix is a root-owned `/etc/apparmor.d/bwrap` profile granting `userns`
(not applied: out of scope for this spike). `allowNestedUserNamespaces` and
`enableWeakerNestedSandbox` did not help. Setting
`sandbox.network.allowAllUnixSockets: true` (which drops the seccomp filter)
makes the sandbox start; all "sandbox on" measurements below use that setting,
unless stated otherwise. Consequence for rollout: on such a host a developer who
enables the sandbox either gets a broken Bash tool (and then fixes AppArmor or
sets the flag) or runs without the sandbox, so containment must not assume the
sandbox is in either state.

## Results

Self-move = the `CLAUDE_ENV_FILE` line `echo $$ > <leaf>/cgroup.procs`.
"In leaf" = `/proc/self/cgroup` of the command ends in `/tools`.

| Context | Sandbox off | Sandbox on, default | Sandbox on + `allowWrite` of the leaf | Sandbox on + hook moves the agent pid |
|---|---|---|---|---|
| Main session, foreground | self-move ok, in leaf | **write fails: read-only file system**, shell stays in the user's cgroup | self-move ok, in leaf | self-move fails, but **in leaf by inheritance** |
| Main session, background command | ok, in leaf | fails, not in leaf | ok, in leaf | in leaf (inherited) |
| Subagent, foreground | ok, in leaf | fails, not in leaf | ok, in leaf | in leaf (inherited) |
| Subagent, background command | ok, in leaf | fails, not in leaf | ok, in leaf | in leaf (inherited) |
| `sh -c` child of the shell | in leaf | not in leaf | in leaf | in leaf |

Sources: sandbox off run `off-run`; default sandbox run `on2` (self-move
failed: `(eval):1: sistema de archivos de solo lectura: .../tools/cgroup.procs`
then `sluice-s1: move failed 1`); `allow3`; `on4`. Not measured: `--resume`,
`/clear`, compaction (belongs to S3).

### Why the default fails

Inside the sandbox `/sys/fs/cgroup` is the same cgroup2 filesystem, bind-mounted
read-only: `cgroup2 on /sys/fs/cgroup type cgroup2 (ro,nosuid,nodev,noexec,...)`
(outside: `rw`). Shell uid and file ownership are unchanged (`-rw-r--r-- jose`),
so permissions are not the obstacle, the read-only mount is. The sandbox also
runs the command in its own PID namespace: `echo $$` prints `2`, `NSpid` shows
`7`, `/proc/1/comm` is `bwrap`. A direct `bwrap --unshare-cgroup` run showed the
cgroup namespace view becomes `/`, which would not matter once writable.

### Hook runs outside the sandbox

The `SessionStart` hook process is not sandboxed: `/sys/fs/cgroup` is `rw` in
its mountinfo, no `bwrap` parent (its parent is a plain `sh`), `NoNewPrivs 0`,
`Seccomp 0`, it lives in the user's own cgroup (`session.slice/dbus.service`
here), and it receives **`CLAUDE_PID`** (the agent process, a child of whoever
launched it; the hook's `PPID` is an intermediate `sh`, not the agent). It
could write `CLAUDE_PID` into the leaf: `hook moved CLAUDE_PID ok`.

### D-Bus from inside the sandbox

The user bus socket is visible and reachable (`/run/user/1000/bus`, with
`allowAllUnixSockets`; with the seccomp filter on, Unix sockets are blocked by
design). But `AttachProcessesToUnit` needs a **host** pid, and inside the pid
namespace `$$` is namespace-local: `busctl ... AttachProcessesToUnit ssau
sluice-s1.scope /tools 1 $$` answered `Process 2 is a kernel thread, refusing`
(host pid 2, `kthreadd`); an ns-local pid can name a wrong, unrelated process
(here refused only by luck). Pid `0` is accepted by systemd but means the caller,
which is the short-lived `busctl` itself, so the shell stays where it was. There
is no way for the shell to learn its host pid from inside. Dead end.

### Inheritance across the sandbox start

A process moved into the leaf **before** the sandbox starts transmits it:
the sandbox wrapper (`bwrap`) and every command are descendants of the agent
process, so once the hook has moved `CLAUDE_PID`, all Bash commands of the main
session, background commands and subagents are in the leaf, with or without a
working self-move (`on4`; also reproduced with a plain `bwrap` after moving the
parent shell). Only processes forked after the move inherit it: anything the
agent started before the hook ran (early MCP servers) stays where it was, which
S3 measures.

### `allowWrite` of the leaf works but needs a static path

`sandbox.filesystem.allowWrite: ["<cg>/tools"]` re-mounts that leaf directory
read-write inside the sandbox (an extra `cgroup2 on <cg>/tools type cgroup2
(rw,...)` mount), and the self-move then works in every context (`allow3`).
But the setting is static JSON, while the leaf is per session and created at
runtime; a stable per-user path would be needed (for example
`.../sluice-agents.scope/tools`), and it would have to be written into the
developer's own settings (project settings cannot widen the sandbox under
admin-required sandbox policies). Not zero-config.

## Recommendation for sandboxed sessions

1. **Primary mechanism: the `SessionStart` hook moves the agent process
   (`CLAUDE_PID`) into the session's `tools`-parent leaf** (or `ui`, then the
   hook's child shells need `tools` below); because the hook is outside the
   sandbox and every command is a descendant of the agent, this contains main
   session, background commands and subagents with no per-command write, and
   it works identically with the sandbox off. This is the same hook that
   already writes the env line, so no extra moving part.
   Conflict with ADR-0017 D3 (agent in `ui`, shells in `tools`): inheritance
   alone places the shells wherever the agent is. To keep the `ui`/`tools` split
   in a sandboxed session, the sandboxed shell cannot move itself. Options for
   S2/S3: (a) accept one leaf per session when the sandbox is on and doctor
   says so, (b) allowWrite the leaf for developers who enable the sandbox and
   want the split (documented, opt-in).
2. Keep the env-file self-move for unsandboxed sessions (that is where the
   `ui`/`tools` split works), but write it as
   `[ -w <leaf>/cgroup.procs ] && echo $$ > <leaf>/cgroup.procs`:
   when the mount is read-only the test is false and the command stays silent.
   The plain `echo ... 2>/dev/null` form still prints, under zsh, a
   `read-only file system` line because the shell reports the redirection
   failure itself, and the `|| echo ... >&2` form adds a `move failed` line to
   every command (measured in `on2`), which is noise the agent will read.
3. Do not use `AttachProcessesToUnit` from the shell (no host pid, wrong
   process risk). Use it only from the hook, where the pid (`CLAUDE_PID`) is a
   host pid and the process is outside any sandbox.
4. `sluice doctor` detects the sandbox situation: configured `sandbox.enabled`
   in the effective settings, `/sys/fs/cgroup` mounted `ro` for a probe
   process, and the failure of the self-move; it reports "contained through
   the agent process" or "not contained" accordingly.

## Caveats

- Containment through the agent pid depends on the hook firing before the
  agent starts the first commands (it did in every run) and on the agent's
  descendants staying below it; a process that double-forks away still inherits
  the cgroup (cgroups follow fork), only an explicit move leaves it.
- The measured sandbox is the Linux bubblewrap one with
  `allowAllUnixSockets: true` because the default configuration cannot start on
  an Ubuntu 24.04+ host without the root-owned AppArmor profile (Finding 0).
  With the profile fixed, the default (seccomp filter on) was not measured:
  expected same cgroupfs behaviour (read-only mount), and D-Bus blocked by the
  filter; unverified.
- Other sandbox implementations (macOS Seatbelt, containers, the IDE
  integrations) were not tested; the cgroup mechanism is Linux only anyway.
- Prompts that contain a `busctl AttachProcessesToUnit` command were refused by
  the subagent as an attempt to bypass resource controls; the D-Bus probe was
  therefore run from the main session only.
- Measured with Claude Code 2.1.310 only; re-run (a handful of haiku sessions)
  when the version changes. Scope, slice and scratch files were removed
  afterwards; `systemctl --user list-units 'sluice-s1*'` is empty.
