---
title: "ADR-0017: Contain agent sessions in cgroups without wrapping the agent"
tags: [sluice, adr, containment, cgroups, systemd]
---

# ADR-0017: Contain agent sessions in cgroups without wrapping the agent

Status: proposed (2026-10-24), accepted in principle by the user; the values
and the open points below are settled by the S-wave of
[[sluice/plan/containment-plan.md]]. Builds on
[[sluice/design/adr/0014-zero-config-agent-integration.md]].

## Context

Sluice keeps heavy work in its queue by intercepting commands: the guard
rewrites simple commands, the shims catch tools resolved through `PATH`.
Measured on 1 901 real heavy commands from local agent transcripts
(2026-10-24): 99.8 % are queued or sent to `verify`; the rest call a tool by
path. Interception can never reach 100 %: an agent can write a script, call
`subprocess` from Python or a tool by absolute path. The user's requirement
for an enterprise rollout is that no process an agent starts can saturate the
machine, however it is started, and without wrapping the agent's launcher.

A spike on the user's machine (Ubuntu, systemd 259, cgroup v2) showed:

- the user manager delegates `cpu memory pids` (upstream default since systemd
  252, commit b8df7f8629, 2022-07-05);
- an unprivileged user can create a transient scope with `Delegate=yes` in its
  own slice, and a running shell outside it (in `session.slice/dbus.service`)
  can move itself into a leaf of that scope by writing its PID to
  `cgroup.procs` (the nearest common ancestor, `user@UID.service`, is owned by
  the user); cost about 0.17 ms per move;
- the kernel then enforces the limits: four busy loops for 3 s in a scope
  capped at one CPU used 3 016 ms of CPU;
- `AttachProcessesToUnit` on the user manager also moves a running process
  into a leaf of the scope.

## Decision

1. **Two layers.** Containment (kernel, every process of an agent session)
   guarantees the machine stays usable; coordination (queue, cache,
   coalescing) avoids repeated work and stays as it is. A command that escapes
   interception costs efficiency, never stability.
2. **Tree.** The daemon creates, through the user manager, a `sluice.slice`
   with two delegated scopes: `sluice-gates.scope` (the daemon's jobs; higher
   weight) and `sluice-agents.scope` (low weight against the desktop, like the
   user manager's `background.slice`). Inside the agents scope, one sub-cgroup
   per session with two leaves: `ui` (the agent process and its MCP servers,
   higher weight within the session) and `tools` (everything the agent's shell
   runs, lower weight). The daemon itself leaves the cgroup of whoever started
   it.
3. **No wrapper.** The agent plugin's session hook moves the agent process and
   its existing children into `ui` once per session through
   `AttachProcessesToUnit`; later MCP servers inherit it. The line the hook
   already writes to `CLAUDE_ENV_FILE` gains a self-move of the shell into
   `tools`, so every Bash command, subagent and background command is
   contained. Shims do the same self-move before running a heavy tool, which
   covers agents other than Claude Code.
4. **Limits.** CPU by weight (work-conserving: no slowdown on an idle
   machine), with an optional ceiling; memory by `MemoryHigh` (throttling),
   never `MemoryMax`; `pids.max` per session. Defaults come from the S-wave.
5. **Fail open.** If the cgroup cannot be created or joined (no systemd user
   manager, no delegated `cpu`, a sandbox that makes `/sys/fs/cgroup`
   read-only), nothing breaks: the session runs as today and `sluice doctor`
   says why.
6. **Observability.** Per-session CPU and memory from `cpu.stat` and
   `memory.current`, and CPU used outside Sluice jobs, appear in
   `sluice status` and its JSON (optional fields, schema 1).

## Consequences

- Containment is not a security boundary: a process of the same user can
  move itself out. It protects against accidental overload, the actual risk.
- Distributions with systemd older than 252 (Ubuntu 22.04) get memory and
  pids containment only, unless an administrator adds a `user@.service`
  drop-in delegating `cpu`; the fallback for CPU there is `nice` on the
  shell, decided in the S-wave.
- The agent process shares the session's weight; the `ui`/`tools` split keeps
  it responsive while its own build runs.
- Linux only, like the rest of Sluice.
