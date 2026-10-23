# How it works

Sluice sits between whatever asks for a verification (a git hook, a terminal,
a coding agent) and the commands that do the verifying (your gates). It does
three things: it remembers which trees already passed, it bounds how many gates
run at once, and it makes identical simultaneous requests share one run.

## Architecture

```
 git hook ──▶ sluice run ──┐                        ┌──▶ gate (process group)
 terminal ──▶ sluice run ──┼──(Unix socket)──▶ daemon ─┼──▶ gate (process group)
 agent    ──▶ sluice mcp ──┘                        └──▶ …  bounded by budget
                │
                └── cache hit: marker found under .git/, exit 0 in milliseconds,
                    no daemon involved
```

- `sluice run <gate>` serves git hooks, humans and scripts. `sluice mcp` serves
  coding agents as an MCP server over stdio. Both talk to the same daemon and
  share cache, queue and logs.
- The daemon is per user. It starts itself the first time a miss needs it and
  keeps running until `sluice daemon stop`. If it cannot be reached or started,
  `sluice run` runs the gate in-process under a file-lock semaphore instead, so
  a broken daemon never blocks a commit.
- Only declared gates (and commands wrapped in `sluice exec`) go through the
  queue. `sluice exec -- <cmd>` runs any other command under the same budget,
  without cache.

## What happens on `sluice run <gate>`

1. The client computes the tree hash from a **private copy of the index**
   (`git add -A` plus `git write-tree` against a scratch index and a scratch
   object directory). Your real index, `index.lock` and object store are never
   touched. Every tracked, staged, unstaged and untracked non-ignored file
   counts.
2. It derives the cache key from the tree, the gate name, the normalised
   command, the gate's environment and any extra inputs you declare
   (`key_inputs` such as `node --version`, `key_files` such as `.env.test`). A
   gate with `paths` hashes only the matching part of the tree (see
   [configuration.md](configuration.md#path-scoping)).
3. If a marker for that key exists under
   `$(git rev-parse --git-common-dir)/sluice/markers/<key>.json`, that is a
   hit. Markers are shared by every worktree of the repository. The key layout
   and the marker format are a documented contract
   ([design note](../kb/sluice/design/cache-key.md)).
4. Otherwise the request goes to the daemon, which attaches it to an identical
   in-flight job or queues it under the budget, runs it in its own process
   group with output captured to a log, and streams `queued`, `started` and
   `done` back.
5. Only a run that exits 0 **and leaves the tree unchanged** writes a marker.
   Auto-fixers that modify files always get re-verified.

## Scheduling

- **Budget.** The daemon runs gates under a budget of units (default
  `max(2, cpus)`, `[daemon] budget` in the user config; `cpus` honours CPU
  affinity and cgroup limits). Each gate declares a `weight`, so a heavy suite
  takes more of the budget than a linter. With `[scheduler] learned_weights`
  (default on) a gate's weight follows its measured CPU demand once it has
  run 3 times, never above its configured `weight`.
- **Queue.** Jobs a person waits for (`sluice run` from a terminal, human
  hooks) go before agent jobs (`human_first`); at equal priority, worktrees
  take turns (`fairness = "lane"`) and arrival order decides (`order =
  "fifo"`, or `sjf`). A job that does not fit is backfilled around, up to a
  head-of-line reservation (`head_reserve_after`, 30 s) that also bounds how
  long anything can be overtaken, so heavy jobs never starve. While the
  machine is calm and CPUs are idle, one job at a time may start beyond the
  budget, up to `overcommit` (default 1.5) times it, at most once per
  `overcommit_interval` (5 s). `sluice logs --timeline` shows why each job
  waited.
- **PSI admission.** Admission pauses while the machine is under real pressure
  (Linux PSI `some avg10` above 60 % CPU or 20 % memory by default, the
  `[admission]` table of the user config). While pressure blocks, the head of
  the queue is still admitted once it has waited `psi_max_wait`.
- **Coalescing.** Two requests for the same tree and gate share one run; the
  second waits for the first one's result. If a marker appears while a request
  is queued, it completes without running.
- **Queue-only gates and `sluice exec`.** A gate with `mode = "queue-only"`
  always runs under the budget, is never cached, and identical simultaneous
  requests share one run. `sluice exec` does the same for an arbitrary command.

## Agents never hang

A git hook invoked by an agent runs inside the agent's `git commit` call, and
the agent's shell tool has a timeout. On a cache miss in such a hook, Sluice
fails fast with an actionable message (run the gate through Sluice first, then
retry) instead of waiting; `agent_hook_policy` selects the behaviour. Humans
wait, because humans are not subject to tool timeouts. The MCP `verify` tool
blocks safely, with progress notifications and cancellation. Full output goes
to a log file; callers get one line on success and the tail of the log on
failure (40 lines by default, `defaults.tail_lines` or `cache.tail_lines`).

## Things to know before trusting a cache hit

- A hit is a **local optimisation**. CI stays authoritative. Caching is off
  when `CI` is set unless you export `SLUICE_CACHE=1`.
- Inputs git cannot see (tool versions, environment variables, ignored files,
  running services) must be declared with `key_inputs` and `key_files`, or a
  stale pass is possible.
- A path-scoped gate (`paths`) ignores every change outside its scope: if it
  reads a file there (a root lockfile, shared fixtures), add that file to
  `paths`. `sluice doctor` flags the usual suspects. Without `paths`, a
  documentation-only change re-runs every gate, because the key is the whole
  tree.
- A cache hit is a verdict, not an artifact: it does not restore build outputs.
- Agents bypass the queue whenever they can run the test command directly
  through their shell tool. Permission rules such as `Bash(npm test:*)`, or
  running with permissions bypassed, make that easy. The Claude Code plugin
  ([plugin/README.md](../plugin/README.md)) ships a hook that denies the raw
  commands your gates run; for other agents, tell them to verify through Sluice
  ([agents.md](agents.md) has a ready-to-paste section).
- Tools that keep state inside the checkout (such as `node_modules/.cache`)
  break when worktrees share one `node_modules`. Give each worktree its own
  caches; Sluice cannot fix this.

Recipes for common stacks: [gates-cookbook.md](gates-cookbook.md).

## Prior art

Sluice reimplements ideas from three projects and tries to combine what each
one got right:

- [gate-cache](https://github.com/natelandau/gate-cache): skip a command that
  already passed on the same tree. Sluice takes its tree hashing from a private
  index copy and adds a queue, coalescing and declared key inputs.
- [agent-task-queue](https://github.com/block/agent-task-queue): an MCP task
  queue so agents do not hit shell timeouts. Sluice takes the
  MCP-blocks-instead-of-the-shell insight and the output-to-file design, and
  adds a cache, weights and load awareness.
- [mergetrain](https://github.com/yongjip/mergetrain): a local merge queue for
  agent worktrees. Sluice borrows its lease-fenced single-runner discipline for
  the daemon.

See [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md). Design history (the
plan, the architecture, one ADR per decision) lives in
[`kb/`](../kb/sluice/plan/master-plan.md).
