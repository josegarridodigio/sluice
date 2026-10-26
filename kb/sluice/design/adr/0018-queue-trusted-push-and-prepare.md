---
title: "ADR-0018: The queue trusts its own verification at push time, prepares its worktrees, and lets a human bootstrap its configuration"
tags: [sluice, adr, merge-queue, cache-key]
---

# ADR-0018: The queue trusts its own verification at push time, prepares its worktrees, and lets a human bootstrap its configuration

Status: accepted (2026-10-26, user decisions in section "Decisions"). Plan:
[[sluice/plan/queue-trust-plan.md]]. Amends [[sluice/design/adr/0009-merge-queue.md]]
and [[sluice/design/merge-queue.md]] (sections 5, 6, 7, 11, 12). Issues: GitHub #3, #4.

## Context

Two reports from an external user on a pnpm monorepo (Sluice 0.5.0):

- **#3**: a train's gates run in `<state>/merge/<train>`, a fresh `git worktree add`
  with no ignored files, so every gate that needs `node_modules` fails in 0.5 s
  (`spawn tsx ENOENT`). The failure is reported as `gate_failed` and a
  multi-member train is split into solo trains that fail the same way. Gates are
  read from the target, so the repository cannot fix them through the queue: the
  only way out is a direct push.
- **#4**: the daemon's `git push` of a verified train runs the repository's
  `pre-push` hook (decision 9 of the design); the hook's `sluice run --hook
  pre-push test` missed the cache, re-ran a 9.5-minute suite, `push_timeout`
  (which covers the hook) expired and the train ended `needs_reconcile`. The
  design promised that a Sluice hook "answers from the markers the train just
  produced".

A full read of the code (2026-10-26) found that these are symptoms of three
structural faults, not two bugs:

1. **The daemon's environment is whatever client autostarted it.**
   `sluice-gate-proto` `client.rs` spawns the daemon with the client's env minus
   `GIT_*`; `server::sanitised_env` removes only `CLAUDE*` and git's local vars.
   `SLUICE_JOB`, `SLUICE_AGENT`, `SLUICE_CACHE`, `SLUICE_DISABLE`,
   `SLUICE_SESSION_LEAF`, `SLUICE_SHIM_DIR`, the shim directory in `PATH` and the
   user's agent-detection variables survive for the daemon's life. The push adds
   `LC_ALL=C` and the approver's `PATH`/`HOME`.
2. **The push re-derives keys instead of trusting the train's record.** A key
   contains the stdout of `key_inputs` (`node --version`, `pnpm --version`, written
   by `sluice init` for every JS repository), which depends on `PATH`; and
   `key::collect_inputs` does not clear the environment, so inputs also see
   `CLAUDE*`, `GIT_DIR`, etc. The train derives keys in the daemon's env, the hook
   in the push env: any difference (`PATH`, `CI`/`SLUICE_CACHE` turning the cache
   off, an agent variable making the hook fail fast) breaks the promise. Which one
   hit #4 cannot be read from here; the fix must not depend on it.
3. **A train's worktree has no preparation phase.** Every stack whose
   dependencies are git-ignored (`node_modules`, `.venv`) cannot verify in it.

Latent bugs found on the way (all verified in code unless marked):

- The runner treats a gate `pass` as verified without looking at
  `outcome.marker_written`: a gate that leaves untracked files gets no marker and
  the train is still `ready`. A gate that edits tracked files makes
  `pre_push_checks` dissolve the train, the rebuild repeats it: an approval loop
  that never lands.
- `config_changed` uses a two-dot diff (flags members that never touched
  `.sluice.toml`) and is rendered nowhere; a gate failure overwrites it.
- `target::defaults_at` turns an unparsable head config into defaults silently
  (an old binary can enqueue for the wrong target).
- `store::from_json` falls back to `TrainSettings::default()` (derived: zeros) on
  any parse error, which means a 1 ms push timeout.
- On `push_timeout` the hook's output is discarded; `client.log` is written only
  at exit and carries no key; `--explain` ignores the cache mode.
- A failing `key_input` in the merge worktree fails every member with `internal`.
- Under `last_subscriber_gone = "finish"` a train's aborted gates keep writing
  into a worktree being removed.
- No test runs a real Sluice `pre-push` hook during a train push.

## Decision

### 1. A neutral daemon environment

The daemon's process environment is the starting client's env minus a fixed
list: `GIT_*`, `CLAUDE*`, `SLUICE_JOB`, `SLUICE_AGENT`, `SLUICE_CACHE`,
`SLUICE_DISABLE`, `SLUICE_SESSION_LEAF`, `SLUICE_SHIM_DIR`, `SLUICE_SHIMS`, `SLUICE_GUARD`, the variables named by
the user config's `[agent_detection]`, and the shim directory removed from `PATH`.
One shared function (`sluice_gate_core::env::daemon_env`) is applied both when a
client spawns the daemon and in `sanitised_env` (a daemon started by hand).
`CI`, `LC_*`, `PATH` (minus shims) and everything else stay: they are the user's.

### 2. Key inputs run in exactly the environment they are given

`key::collect_inputs` clears the environment before applying the given map. Every
caller already passes a complete environment. Key format unchanged (cache key v1):
only processes whose inputs printed environment-dependent output see new keys.

### 3. The daemon vouches for its own push (fixes #4 whatever the cause)

A new optional `Extension` method lets an extension answer a `run` request before
the scheduler sees it. The queue answers when **all** hold:

- a train is `pushing` and the request's `toplevel` is that train's worktree and
  its `common_dir` the train's repository;
- the peer process (`SO_PEERCRED`) descends from the train's recorded `push_pid`
  with matching start ticks (walk of `/proc/<pid>/stat`, bounded);
- the daemon's own check of the worktree gives `HEAD == S` and no change to tracked
  files (the pre-push checks' rule: untracked gate output does not change what is pushed).

Then: a gate the train verified (`pass` or `cached`) gets `Done{Cached}` naming the
train and its job; any other gate gets `Done{Error}` with an actionable message
(**fail fast**, user decision 2): the hook exits non-zero at once, the push is a
definitive `pre_push_hook` failure instead of a timeout and `needs_reconcile`.
When a condition does not hold the request goes the normal way. It is a fact the
daemon states about its own locked worktree holding exactly the verified `S`;
it lets nobody skip a gate anywhere else. Decision 9 (the hook runs) stands: other
hooks keep their say. It works with older hook clients (daemon-side only).

Implementation note (review correction): a merged tree whose `.sluice.toml` asks the
hook for gates the train does not verify is detected at build time, before preparing or
running gates (`runner::reject_hook_gates`): the members that changed the configuration
fail with `hook_gate_unverified` (a hint: land the configuration change first, or a human
retries with `--trust-member-config`), the others are requeued; a merged config that does
not parse fails them with `member_config_invalid`. It is the same fail-fast decision, taken
before the human is asked to approve.

To make "not verified by the train" rare, the train's gate set becomes
`[queue] gates ∪ {gates whose hooks contain pre-push}` (else every gate, as now).

### 4. Train results must be real

- A gate `pass` whose run left tracked files modified fails the train with reason
  `gate_modified_tree` (detail: gate, job, paths), attributed like a gate failure.
- A `pass` without a marker for any other reason keeps the train going and is
  shown as `pass (not cached: <reason>)` (new optional `TrainGate.note`).
- A train's gate and prepare jobs are killed when the train drops them, whatever
  `last_subscriber_gone` says.

### 5. `[queue] prepare` (fixes #3)

```toml
[queue]
prepare = "pnpm install --frozen-lockfile --prefer-offline"
prepare_timeout = "10m"   # default 10m
prepare_weight = "auto"   # same syntax as a gate weight; default "auto"
```

Read from the base like every `[queue]` key (older binaries reject the file:
accepted, user decision 1; the docs state the minimum version). Lifecycle, after
the merges and before step 5 (snapshot):

1. Fast path: derive keys and look them up; if every gate hits, the train is
   `ready` without preparing (an already verified fast-forward costs nothing).
2. Otherwise submit the prepare command through the scheduler (`key: None`,
   `no_marker`, job name `queue:prepare`, which no gate can use, weight and timeout
   from the config, killed when dropped), in the merge worktree with the daemon's
   environment. The train stays `verifying`; no new state, no schema version bump
   (one nullable column for its view).
3. After a pass the worktree must still snapshot to `T`; otherwise reason
   `prepare_dirty` (detail: the paths), handled as a prepare failure.
4. Keys are derived after preparing. A `key_input` that fails is reason
   `key_input_failed`, handled as an environment failure (below), never `internal`.

**Attribution of a prepare failure** (user decision 3, on the analysis' advice):
a prepare can fail because a member broke it (lockfile out of sync) or because of
the environment (registry down, missing tool, broken base). The runner decides by
a fact: it prepares the **base alone** in a probe worktree
(`<state>/merge/<train>-base`, removed afterwards). Implementation note (review
correction): no probe result is remembered. A remembered pass for `(repo, base_sha,
command)` would blame the members for a later environment failure (registry down,
timeout, memory kill) on the same base, so every prepare failure probes the base afresh.

- Base also fails → nobody's fault: every member back to `queued` (not `solo`),
  reason `prepare_failed`, detail with the error and `base: true`, paced and
  counted like `base_unavailable`, `failed` after 3 consecutive attempts.
- Base prepares → a member's fault: a multi-member train splits (solo trains, as
  for gates); a single member is `failed` with `prepare_failed`.

### 6. Human bootstrap of the configuration (the "dead end")

`sluice queue enqueue --trust-member-config` and `sluice queue retry <job>
--trust-member-config` build the job's train from the **member's** `.sluice.toml`
(gates and `[queue]`, `remote`/`target` excepted: they were resolved at enqueue):

- Human only, stricter than approval: refused for `via: mcp` and for a CLI whose
  environment is detected as an agent's, even with `allow_agent_approval = true`.
  A train built this way can only be approved the same way.
- Such a job is always alone in its train. The train records `config_source =
  member` and the hash of the config blob, which enters the train hash (appended
  only for these trains, so ordinary approvals survive the upgrade).
- `queue status` and the approval plan show the `.sluice.toml` diff against the
  base and a warning; the TTY prompt is shown after it.
- Optional proto fields, `welcome.features` gains `queue-trust-config`; a client
  that does not see it refuses the flag instead of being silently ignored.
- A plain `retry` clears the flag.

### 7. Corrections and observability

Three-dot `config_changed` shown in CLI/MCP/plan and kept when a gate fails;
`defaults_at` reports an invalid head config (`bad-request` when the default
target is needed, a warning otherwise); `TrainSettings` with real defaults and
`#[serde(default)]`, a parse failure logged; the hook's partial output kept in the
`push_timeout` note; `key=` in `client.log`; the daemon logs every hook `run`
request (gate, tree, key, hook, decision) at `info`; `--explain` reports the cache
mode.

## Decisions (user, 2026-10-26)

| # | Question | Answer |
|---|---|---|
| 1 | Where `prepare` lives | `.sluice.toml` `[queue]`; breaking older binaries is acceptable |
| 2 | A pre-push gate the train did not verify | Fail fast with a clear message |
| 3 | Prepare failure in a multi-member train | Attribute with a base probe (analysis recommendation) |
| 4 | Human configuration bootstrap | Included now |
| — | Sub-agents | Run on Sonnet 5.5 |

## Consequences

- #4 is fixed independently of its exact trigger, and every remaining push-time
  miss is a fast, explained failure instead of a 10-minute ambiguous end.
- Train keys and agents' keys still differ when their `PATH`s resolve different
  toolchains; that is correct (different toolchain), and no longer matters at push.
- `.sluice.toml` with `prepare` needs the new version on every machine that reads
  it (hooks included).
- The extension API grows one method with a default; the daemon crate still does
  not depend on the queue.
- The bootstrap deliberately lets a branch choose its judges, with a human reading
  the diff as the control; the guard, the agent detection and the forge's
  protection stay the outer layers.

## Alternatives considered

- **`--no-verify` on the train push**: reverses decision 9; non-Sluice hooks lose
  their say.
- **Run `pre-push` separately before the push**: emulating git's hook invocation
  (`core.hooksPath`, arguments, stdin; no `git hook run` in git 2.31) is fragile
  with husky, lefthook and pre-commit.
- **Make the hook derive keys in the runner's environment**: the push needs the
  approver's `PATH` for credential helpers and git rewrites the hook env; it keeps
  the fragile re-derivation.
- **An env variable the hook trusts**: an agent can set it; the vouch rests on facts
  the daemon checks itself.
- **`install && test` in a gate's `cmd`**: train gates run concurrently in one
  worktree (concurrent installs into `node_modules`) and every `verify` pays it.
- **Per-gate `setup`**: parsed and run in every developer worktree, same race.
- **Split, or requeue everything, on any prepare failure**: each is wrong in one of
  the two failure causes; the base probe decides by a fact.
- **`prepare` in the user config**: no compatibility break, but not shared with the
  team; rejected by decision 1.
