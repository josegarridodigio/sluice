---
title: "Containment spike S3: moving the agent and its MCP servers into a cgroup"
tags: [sluice, research, containment, cgroups, claude-code, hooks, spike]
---

# Containment spike S3: moving the agent and its MCP servers into a cgroup

Measured 2026-10-24 with Claude Code **2.1.310**, Ubuntu, systemd 259,
cgroup v2. Task S3 of [[sluice/plan/containment-plan.md]]; feeds ADR-0017
([[sluice/design/adr/0017-agent-resource-containment.md]]) and task C3.

Question: can a `SessionStart` hook move the running agent process and its
already-started children (MCP servers) into a `ui` leaf of a delegated cgroup,
so that MCP servers and anything they spawn later are contained, idempotently
across `--resume`, and fast enough for a session hook?

**Answer: yes.** One `AttachProcessesToUnit` call (about 30 ms) or a loop of
`cgroup.procs` writes (about 2 ms per process) moves the agent and all its
current descendants. Everything started afterwards inherits the leaf.

## Method

- Delegated scope as in the earlier spike: `systemd-run --user --scope -q
  --unit=sluice-s3.scope --slice=sluice-s3.slice -p Delegate=yes sleep 900`.
  Leaves `holder` (the scope's own `sleep`), `sess/ui`, `sess/tools`. `sess`
  holds no processes. Order that works: create `holder`, move the holder PID
  into it, then `+cpu +memory +pids` into the scope's `cgroup.subtree_control`,
  create `sess/ui` and `sess/tools`, then the same `+cpu +memory +pids` into
  `sess/cgroup.subtree_control`. All three controllers were available at
  every level (`cgroup.controllers` of `sess/ui` = `cpu memory pids`). Note:
  the slice nests under the existing `sluice.slice`
  (`.../user@1000.service/sluice.slice/sluice-s3.slice/sluice-s3.scope`)
  because systemd derives the parent from the dashes in the slice name.
- A throw-away plugin (mktemp dir outside the repo, deleted afterwards):
  - `SessionStart` hook, matcher `startup|resume|clear|compact`: dumps its
    stdin, its `CLAUDE*` environment and its parent chain; finds the agent
    PID; lists the agent plus all descendants (recursive `pgrep -P`); attaches
    them to `sess/ui` either with `busctl --user call org.freedesktop.systemd1
    /org/freedesktop/systemd1 org.freedesktop.systemd1.Manager
    AttachProcessesToUnit ssau sluice-s3.scope /sess/ui <n> <pids...>` or by
    writing each PID to `sess/ui/cgroup.procs`; logs timings and the
    `/proc/<pid>/cgroup` before and after.
  - A stdio MCP server (`python3` script, JSON-RPC by hand) that logs its own
    cgroup at start and after 6 s, exposes a `burn` tool, and, from the timer
    and from the tool, spawns `sh -c "timeout N sh -c 'while :; do :; done'"`
    (a child and a grandchild) and logs both cgroups.
- Real sessions only, `claude -p --model haiku --plugin-dir <plugin>
  --output-format stream-json --verbose --permission-mode bypassPermissions`,
  started from a plain shell outside any scope (agent in
  `session.slice/dbus.service`). Three sessions: (1) busctl, startup; (2)
  busctl, `--resume <id>`; (3) direct write, new session. The user's own
  plugins (sluice, Playwright MCP, a Google Docs MCP) were also loaded, which
  gave real extra MCP servers (`npm exec @playwright/mcp@latest`, `sluice
  mcp`, `npm exec ... google-docs-mcp`) to observe.
- Idempotence and scaling were checked with a separate script of plain
  `sleep` processes (repeat attach, 50 PIDs, inner-node and bad-path errors).

## Results

### How the hook learns the agent PID

| Source | Result |
|---|---|
| `CLAUDE_PID` in the hook environment | **Set, and correct**: equal to the `claude` process (`1630342` in session 1, `1633869` on resume, `1636150` in session 3). |
| `$PPID` of the hook script | The `/bin/sh -c <hook command>` wrapper, not the agent. |
| Parent chain | hook script -> `sh -c` -> `claude` (comm `claude`) -> launcher. The agent is the grandparent. Walking up to the nearest ancestor with `comm=claude` gave the same PID. |

Use `CLAUDE_PID`; fall back to walking up to the first `comm` starting with
`claude`; if neither is found, do nothing (fail open). Other useful variables
observed in the hook: `CLAUDE_CODE_SESSION_ID`, `CLAUDE_ENV_FILE`,
`CLAUDE_PROJECT_DIR`, `CLAUDE_PLUGIN_ROOT`, `CLAUDE_PLUGIN_DATA`,
`CLAUDE_CODE_ENTRYPOINT` (`sdk-cli` here), `CLAUDE_CODE_VERSION`. The stdin
JSON has `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `source`;
on resume also `seconds_since_last_response`, `context_tokens`. Not tested in
the interactive TUI or the IDE integrations: the entrypoint differs
(`cli`, `claude-vscode`, ...) and `CLAUDE_PID` must be re-checked there (E3).

### Which processes exist when the hook runs

At the first hook run the agent already had **all** its MCP servers as
children (the test server, `npm exec @playwright/mcp@latest`, `sluice mcp`,
`npm exec ... google-docs-mcp`): the hook found 6 PIDs (agent, the hook's own
`sh`, 4 servers), identical on resume. Servers are started before or
concurrently with the hook, so the hook must move the descendants that exist
and rely on inheritance for the rest. If a server were launched after the
hook (a lazy reconnect, a server added with `/mcp`), it is a child of the
agent and inherits `ui` through fork, so no second pass is required.

### Move methods and cost

| Method | Works | Cost | Notes |
|---|---|---|---|
| `AttachProcessesToUnit` via `busctl --user` | yes, no privilege | **about 22-32 ms** for 6 PIDs (hook runs: 31.8, 32.2 ms; script: 21 ms first call, 8-9 ms when repeated; 50 PIDs: 23 ms) | One call for any number of PIDs; path `/sess/ui` is relative to the unit's cgroup. Dominated by the `busctl` fork plus the manager round trip, not by the PID count. |
| Direct write to `sess/ui/cgroup.procs` | yes (the user owns the delegated subtree and the common ancestor `user@UID.service`) | **about 13 ms for 6 PIDs, about 5.6 ms for 50 PIDs** (loop of `echo > cgroup.procs`, timing includes bash overhead) | No D-Bus, no systemd involvement. Fails if `/sys/fs/cgroup` is read-only (sandbox, see S1). |

Whole hook wall time (process start, env dump, descendant walk, attach,
verification reads): well below the budget, tens of ms; the attach itself is
the dominant part. Recommendation: **direct write first, `busctl` as the
fallback** (the reverse of the hook-side order planned in the plan, which
assumed the bus was the more robust one; here both worked, the write is 2-5
times faster and has no external binary beyond the shell). Keep `busctl` for
the case where the write fails with EACCES/EROFS but the bus is reachable.
Both only target PIDs of the same user.

Error cases of `AttachProcessesToUnit`: target `/sess` (an inner node with
`subtree_control` enabled) -> `Device or resource busy`; non-existent path ->
`No such file or directory`; the hook must create the leaves first (the
daemon's job in C1) and treat any non-zero exit as "not contained".

### Inheritance by later MCP children and their children

Checked from inside the MCP server (it logs `/proc/<pid>/cgroup` of what it
spawns), in the first session and again after resume:

| Process | Cgroup |
|---|---|
| Agent, MCP servers, `npm exec` wrappers, hook's `sh` after the move | `.../sess/ui` |
| MCP server at start (before the move) | `session.slice/dbus.service` (as expected: the hook had not run yet) |
| MCP server 6 s later | `.../sess/ui` |
| Child spawned by the server from its timer, 6 s after the move | `.../sess/ui` |
| That child's grandchild (the busy `sh` loop under `timeout`) | `.../sess/ui` |
| Child and grandchild spawned from a `tools/call` request | `.../sess/ui` |
| The agent's Bash tool shell (`/proc/$PPID` and `/proc/self` in a Bash call) | `.../sess/ui` (inherited from the agent; C3 must self-move it to `tools` through the env-file line) |

So anything an MCP server starts later, including a browser launched by the
Playwright MCP server, is in `ui`. A real Playwright browser launch was not
exercised (the test used a CPU-burner child instead, to avoid the network);
the inheritance mechanism is the kernel's fork rule and does not depend on
the program. One consequence to keep in mind for the design: a browser (and
any MCP-spawned heavy child) shares the `ui` weight with the agent process.
That is the intended trade-off of D3, but it means a runaway MCP-spawned
process is contained at the session level only, not at the `tools` level.

### `--resume`, `/clear`, compaction

- `--resume <session-id>` in `-p` mode starts a **new `claude` process** (new
  `CLAUDE_PID`, new MCP servers, all back in `session.slice/dbus.service`),
  and `SessionStart` fires with `source: "resume"`. The hook found 6 PIDs, the
  attach succeeded (`rc=0`, 32.2 ms) and the MCP children spawned afterwards
  were again in `ui`. No errors, nothing left from the first run (the scope's
  `ui` leaf was empty between runs because the first agent had exited).
- Re-attaching PIDs that are **already** in the target leaf is a no-op, safe
  and cheap: three consecutive `AttachProcessesToUnit` calls and two direct
  writes for the same PIDs all returned 0 (8-9 ms and about 4 ms) and left the
  PID in `ui`. That covers `/clear` and compaction, which re-fire the hook in
  the same `claude` process (not scriptable in `-p`, so not exercised live;
  the idempotence test above is the proxy).
- The same session id used on a different run (resume) gets a leaf with the
  same name: the session cgroup must be named from `session_id` and created
  with `mkdir -p` semantics (an existing leaf is fine).
- Gotcha: the hook's own parent `sh -c` is a descendant of the agent and was
  moved too (harmless, it exits right away); the hook script itself was
  excluded from the list in the test, as it would be moving itself.

### Processes left behind

None. After the three sessions the scope's `ui` and `tools` leaves were
empty (the agent, `npm exec` wrappers and burners all exited; the 20 s
timeout-bounded burners were gone). All units stopped and
`systemctl --user list-units 'sluice-s3*'` is empty. Another spike's
`sluice-s1.slice` was present in `sluice.slice` and was left untouched.

## Recommendation for C3

1. **PID**: `CLAUDE_PID`, fallback nearest ancestor with `comm` starting
   `claude`; skip on failure.
2. **Move**: collect the agent PID and its recursive descendants (excluding
   the hook's own process, or including it: either works), write each PID to
   `<session>/ui/cgroup.procs`; if any write fails, retry the failed PIDs with
   one `busctl --user call ... AttachProcessesToUnit ssau <unit> /<rel>/ui n
   <pids>`; if that also fails, do nothing and let `doctor` explain. Fail
   open with exit 0 in all cases.
3. **Idempotence**: nothing special is needed; re-attaching is a no-op. Do
   not keep a "done" marker keyed by session id (resume starts a new process
   tree and needs the move again); key any marker by `CLAUDE_PID` at most.
4. **Budget**: expect 5-30 ms for the move plus whatever the daemon takes to
   create the session leaves (to be measured in C1; create them from the hook
   with `mkdir` inside the delegated scope when the daemon is not needed on
   that path).
5. **Order of leaf creation**: holder first, then `subtree_control` at the
   scope, then the session node, then its `subtree_control`, then leaves.
   Never put a process in a node with controllers enabled for children.
6. Still to verify in E2/E3 on top of this spike: a real Playwright browser
   in `ui`, the interactive TUI and IDE entrypoints (`CLAUDE_PID` and the
   process tree shape), and a session started *inside* the sandbox (S1).

Behaviour is specific to Claude Code 2.1.310; the spike is cheap to repeat
(three haiku sessions, about two minutes) when the version changes.
