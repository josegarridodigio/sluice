---
title: "Plan: agent resource containment (v0.5.0)"
tags: [sluice, plan, containment, cgroups]
---

# Plan: agent resource containment (v0.5.0)

Status: S-wave done (2026-10-24); ADR-0017 accepted; design [[sluice/design/containment.md]]; implementation in progress (section 5a). The user granted freedom to change earlier decisions when evidence requires it (the memory policy changed after S2/S5).
Decision record: [[sluice/design/adr/0017-agent-resource-containment.md]]
(proposed until the S-wave closes). Parent: [[sluice/plan/master-plan.md]].

## 1. Why

Interception (guard, shims) puts 99.8 % of real heavy agent commands through
Sluice, but no command analysis reaches 100 %: scripts, `subprocess`, tools
called by path. Sluice is about to be rolled out to a whole team. Goal: no
process of an agent session can saturate the machine, however it is started,
without wrapping the agent's launcher and without configuration by each
developer. Mechanism: cgroup v2 through the systemd user manager (ADR-0017).

## 2. Decisions taken with the user (2026-10-24)

| # | Decision |
|---|---|
| D1 | Two layers: containment (kernel) guarantees stability; the queue and cache stay the efficiency layer |
| D2 | No wrapper around the agent: the plugin's session hook and the per-command environment line do the placement |
| D3 | The agent process and its MCP servers are contained too, in a `ui` leaf with more weight than the `tools` leaf of the same session |
| D4 | CPU by weight, memory by `MemoryHigh` only, `pids.max` per session; never kill the agent |
| D5 | Fail open everywhere; `sluice doctor` explains why a session is not contained |
| D6 | Decide nothing irreversible before the S-wave below has measured it |

## 3. Definition of done

1. On a machine with systemd ≥ 252 and the agent plugin, every process of an
   agent session (agent, MCP servers, shell commands, subagents, background
   commands, scripts, tools by absolute path, git hooks run by the agent) is in
   the session's cgroup; checked by a harness scenario and a real-session e2e.
2. A runaway load in a session (20 busy threads) leaves the desktop and the
   agent's own UI responsive: measured scheduling delay of the agent process
   under load stays within a bound fixed in S2.
3. Gate jobs run in `sluice-gates.scope` with more weight than agents' raw
   processes; the daemon no longer lives in its starter's cgroup.
4. Without systemd, without delegated `cpu`, or inside a sandbox that blocks
   `/sys/fs/cgroup`, everything works as in 0.4.1 and `doctor` says why.
5. `sluice status` shows per-session CPU and memory, and CPU used outside
   Sluice jobs; JSON fields optional (schema 1).
6. Rules of [[sluice/plan/zero-config-plan.md]] section 4 apply (dogfooding,
   no AI attribution, merge queue, KB notes, haiku for real sessions, never
   name client projects); release v0.5.0 waits for the user.

## 5a. Progress

| Task | State | Evidence |
|---|---|---|
| S1 sandbox | done: self-move impossible in the Bash sandbox; hook move works; layout B with nice 19 | [[sluice/research/containment-s1-sandbox.md]] |
| S2 weights | done (CPU only, rerun after the incident) | [[sluice/research/containment-s2-weights.md]], [[sluice/research/containment-s2-incident.md]] |
| S3 agent tree | done: `CLAUDE_PID`, direct write 5–13 ms, inheritance, resume | [[sluice/research/containment-s3-agent-tree.md]] |
| S4 fallbacks | done: matrix and doctor messages | [[sluice/research/containment-s4-fallbacks.md]] |
| S5 memory | done: oomd-based policy, live T0/T3/T5/T6/T7 | [[sluice/research/containment-s5-memory-oomd.md]] |
| C1 core cgroup module + config | in progress | |
| C2 daemon placement + cg-exec | pending (after C1) | |
| C3 session hook + env line, C4 shims | done on branch | [[sluice/changes/containment-session.md]] |
| C5 status, doctor, interventions | pending (after C2, C3) | |
| E1 harness s9, E2 plugin e2e, E3 user checks | pending | |
| R1–R4 | pending | |

## 4. Waves

```
S-wave (spikes, scratch only)      Wave 1 (core)              Wave 2 (integration)        Wave 3 (prove)      Wave 4 (ship)
S1 sandbox and cgroupfs            C1 cgroup module + config  C3 session hook + env line  E1 harness s9       R1 review
S2 ui/tools weights, latency       C2 daemon + gates scope    C4 shims self-move          E2 plugin e2e       R2 fixes
S3 MCP tree, attach in session     (cg-exec helper)           C5 status, doctor           E3 user checks      R3 reconcile
S4 fallback matrix                                                                        (IDE, team)         R4 release 0.5.0
```

Rule for every spike and test: only move processes the spike itself started.
Never move, signal or re-parent an existing process of the system or the
desktop (S4 moved an idle `gnome-session-init-worker` out of its login scope
while testing; moving it back needs root).

Rule after the 2026-10-24 incident
([[sluice/research/containment-s2-incident.md]]): no spike or test creates
memory pressure on a developer's machine (no `MemoryHigh` or `MemoryMax`
experiments, no memory hogs). Memory behaviour is studied from documentation
and in a disposable VM or container with its own systemd.

### S1 — sandbox and cgroupfs (Sonnet, real session with haiku)

Does Claude Code's Bash sandbox (bubblewrap on Linux) leave
`/sys/fs/cgroup/.../cgroup.procs` writable for the self-move? Test with the
sandbox off and on (settings `sandbox.enabled`), from the main session and a
subagent. Output: table and the fallback when it is read-only (for example
moving the shell from the session hook through `AttachProcessesToUnit`, which
runs outside the sandbox, or documenting the limitation).

### S2 — ui/tools split and default values (Sonnet, no AI sessions)

Measure the agent process's scheduling delay (`/proc/<pid>/schedstat` run
delay, or `perf sched` if available) while 20 busy threads run in `tools`,
for: no containment, one leaf for the whole session, `ui`/`tools` with weights
(100/100, 200/50, 400/25). Also the desktop's delay (a probe process outside).
Output: recommended weights for `sluice-agents.scope`, `sluice-gates.scope`,
`ui`, `tools`; whether a CPU ceiling is needed by default; `MemoryHigh` and
`pids.max` defaults from observed session sizes (agent about 0.5 GB, MCP
servers about 0.4 GB here).

### S5 — memory policy with systemd-oomd (documentation and a disposable VM only)

Design the memory side so that, under pressure, the agent session causing it
is the one acted on, never the desktop: `ManagedOOMMemoryPressure=` and its
limit on Sluice's units (below the user manager's 50 %), `ManagedOOMPreference=`
and who may set it, swap interaction, and whether any memory limit should be
on by default. Never run on the developer's machine.

### S3 — moving the agent and its MCP servers (Sonnet, real session with haiku)

From a SessionStart hook: attach `CLAUDE_PID` and its current descendants to
`ui` with `AttachProcessesToUnit`; check that MCP servers started later
inherit it, that a Playwright MCP browser lands in `ui`, that the hook still
finishes within its budget, and what happens on `--resume`, `/clear` and
compaction (hook fires again: must be idempotent). Also confirm which env var
gives the agent's PID to hooks.

### S4 — fallback matrix (Sonnet, no AI sessions)

Behaviour with: no systemd user manager (a container), `cpu` not delegated
(simulate by reading `cgroup.controllers` and refusing), systemd < 252
semantics, cgroup v1 hybrid. Output: detection logic, the `nice` fallback for
CPU, the exact `doctor` messages.

### Lead step after the S-wave

Accept ADR-0017 with the measured values; write the component design
`kb/sluice/design/containment.md`; freeze the user-config section
(`[containment]`: `enabled`, weights, `memory_high`, `pids_max`,
`cpu_ceiling`) and the status JSON fields. Ask the user only if a measurement
contradicts D1–D5.

### Implementation tasks (outline, detailed after the S-wave)

- **C1** `sluice-gate-core::cgroup`: detection (cgroup v2, delegated
  controllers, user manager reachable), creation of the slice and scopes
  through the `systemctl`/`busctl` binaries (shell-out, like git; no D-Bus
  library), session sub-cgroups, leaf creation, limits, pruning of empty
  session cgroups. User config section.
- **C2** daemon: places itself in `sluice-gates.scope`; jobs start through a
  `sluice cg-exec <leaf> -- <cmd>` helper that writes its own PID and `exec`s
  (same no-race technique as the `nice`/`ionice` wrapper of 0.4.1).
- **C3** session hook: attach the agent and its children to `ui`; the
  `CLAUDE_ENV_FILE` line gains the self-move to `tools`; idempotent on resume.
- **C4** shims: self-move to the session's `tools` leaf (or a shared one for
  agents without a session hook) before running a heavy tool.
- **C5** `status` and `doctor`: per-session usage, CPU outside Sluice jobs,
  containment checks with reasons.
- **E1** harness `s9_containment.py`: escapes that interception cannot see
  (script, absolute path, Python `subprocess`) are capped; gates keep priority.
- **E2** plugin e2e (haiku): a real session's agent, MCP and shell processes
  are in the session cgroup.
- **E3** user checks: VS Code and JetBrains integrations, a teammate's
  machine, Ubuntu 22.04 if anyone uses it.
- Team rollout guide: managed settings (`extraKnownMarketplaces`,
  `enabledPlugins`) so every teammate gets the plugin, plus binary install.
