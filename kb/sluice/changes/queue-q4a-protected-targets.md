---
title: "Queue Q4a: warn about protected targets, learn from a refused push (ADR-0025 1-2)"
tags: [sluice, changes, queue, forge, github, doctor, issue-5]
---

# Queue Q4a: protected targets, early warning and learned signal

Wave Q4a of [[sluice/plan/queue-scheduling-plan.md]] (S3). Decision:
[[sluice/design/adr/0025-protected-targets-and-pull-request-landing.md]] items 1 and 2 plus the parts
of 6 and 7 they need; research: [[sluice/research/protected-targets-analysis.md]] sections 1, 2 and
4.2. Status: implemented; verification through Sluice pending (the lead runs `fmt`, `clippy` and
`test`). Pull-request landing (ADR items 3-5) is wave Q4b and is not implemented.

## What changed

1. **The `Forge` abstraction** lives in `sluice-gate-core` (`crates/sluice-gate-core/src/forge/`),
   the only crate both the CLI and the MCP server depend on (the MCP crate does not depend on the
   queue crate). `trait Forge { name, probe_protection }` is the interface the landing wave grows
   with `ensure_pr` and `pr_state`; `GitHub` implements it through `gh`. `Protection` is
   `Open | PullRequestRequired(evidence) | Protected(evidence) | Unknown(reason)`.
   - `exec.rs`: `run` (a program from the caller's `PATH`, shims skipped, exactly the caller's
     environment, own process group, killed on its deadline) and `Budget` (a shared time budget).
   - `remote.rs`: `parse` (https, http, ssh, git, scp-like; userinfo dropped; local paths and
     odd URLs are `None`), `hostname_of`, `real_host` (`ssh -G -- <host>` `hostname`, falling back
     to the URL's host; `ssh.github.com` is `github.com`).
   - `github.rs`: `decide` (pure over a `get(path)` closure), `parse_reply` (`gh api --include`:
     HTTP status line, headers, body), `encode_branch`, `GitHub::api`, `GitHub::logged_in`.
   - `cache.rs`: `forge-probe.json` under `$XDG_CACHE_HOME/sluice` (`dirs::cache_dir`, new),
     5 minutes, keyed `host/owner/repo#target`, definitive answers only, atomic write, no secrets.
   - `mod.rs`: `probe_target` (push URL with `git remote get-url --push`, parse, alias, cache,
     forge), `warnings` (pure texts), `enqueue_warnings` (probe plus learned fact), `PROBE_BUDGET`
     (8 s). `git::drain` became `pub(crate)` for `exec.rs`.
2. **Probe logic and API calls** (all `gh api --include --hostname <host> -H 'Accept: ...'`, `GET`,
   read-only, each call capped at 4 s inside the 8 s budget):
   1. `repos/{o}/{r}/rules/branches/{b}`: a `pull_request` rule is `PullRequestRequired` and ends
      the probe. Other rule types are kept (a failure of this call is ignored: old GHES lacks it).
   2. `repos/{o}/{r}/branches/{b}`: `protected`. 404 `Branch not found` is `Unknown` ("does not
      exist"); any other non-200 is `Unknown` with a reason (401: `gh auth login --hostname`;
      403: forbidden or rate limited; 404: no access). `protected: false` and no rule other than
      `deletion`, `non_fast_forward`, `copilot_code_review` is `Open`.
   3. Only to refine a protected branch: `repos/{o}/{r}/branches/{b}/protection`. 200 with
      `required_pull_request_reviews` is `PullRequestRequired`; 200 without it, or not readable
      (the 404 `Not Found` of a caller without admin, observed on a public repository), is
      `Protected` (status checks, restrictions, rulesets: a direct push may be refused). The 404
      body `Branch not protected` cannot reach this step with `protected: true`.
   The bypass downgrade of the research (fourth call, `permissions.admin`) is not done: both
   outcomes warn, and the docs say the probe describes the `gh` identity.
3. **Forge and remote identification.** GitHub only: `github.com`; any other host needs
   `gh auth status --hostname <host>` to succeed (exit status only); `gitlab.com`, `codeberg.org`
   and `bitbucket.org` are `Unknown` ("cannot be queried yet"). The path must be exactly
   `owner/repo`. For ssh and scp-like URLs the host goes through `ssh -G` first. Every call
   names the host and repository explicitly (no remote guessing by `gh`). The environment is the
   caller's own, plus `GH_PROMPT_DISABLED=1`, `GH_NO_UPDATE_NOTIFIER=1`, `GIT_TERMINAL_PROMPT=0`;
   no token is read, stored or sent.
4. **Warnings.** CLI (`crates/sluice-gate/src/queue.rs`, `enqueue_warnings`): after the daemon
   accepted the job (so the probe knows the resolved remote and target) prints
   `sluice: warning: ...` on stderr. MCP (`crates/sluice-gate-mcp/src/queue.rs`,
   `enqueue_warnings`, `enqueue_value`, `enqueue_text`): `Warning:` lines in the text, a
   `warnings` array in the structured result (`JobSchema.warnings`, optional) and "The job is
   queued anyway: tell the human". Texts: `<remote>/<target> requires a pull request (<evidence>);
   the queue cannot land on this target; see docs/merge-queue.md`, `... is protected (...); a
   direct push may be refused, and then ...`, `could not check <target>: <reason>`. The enqueue
   always proceeds. A local target has no forge and no warning.
5. **Doctor** (`crates/sluice-gate/src/doctor/target.rs`): `target-protection` for the repository's
   `[queue]` remote and target (configured, else the remote's HEAD as already known locally; no
   check without a remote or target). `warn` for `PullRequestRequired` and `Protected`, `ok` for
   `Open`, and `ok` with "could not check ...: <reason>" for `Unknown`: nothing is wrong with the
   installation (no `gh`, no login, offline, another forge), many users never use the queue, and
   the enqueue warns by itself. The clap help of `doctor` no longer says "never uses the network".
6. **Learned signal.**
   - Classifier (`push.rs`): new `PushVerdict::TargetProtected(evidence)`; `classify` upgrades a
     `Rejected` ref line when `protected_evidence(summary, stderr)` finds a recognised text.
     Recognised (**recalled from the forges' usual output; none verified against a live server**,
     the unit tests use these exact texts): GitHub `protected branch hook declined` (ref line),
     `GH006` / `Protected branch update failed`; GitLab `You are not allowed to push code to
     protected branches`; GitHub `GH013` / `Repository rule violations found` only together with
     a `pull request`, `status check` or `merge queue` line (it also covers secret scanning and
     signatures, which are not the target's protection); generic `pre-receive hook declined` only
     with a line saying `protected branch` (Gitea's `Not allowed to push to protected branch`) or
     `TF402455` (Azure DevOps, recalled). The text is searched in the ref line and in stderr
     (`remote:` lines), at most three matching lines are the evidence.
   - Runner (`runner.rs`): the `Rejected`/`HookDeclined` arms of `apply_verdict` moved to
     `fail_pushed`; `TargetProtected` records the fact in the same transaction and fails the
     jobs with reason `target_protected` (detail `output`), dissolving the train like
     `push_rejected`; `Landed` clears the fact for a remote target.
   - Store (`store.rs`): table `queue_protected_targets (endpoint_hash, target, first_seen,
     last_seen, evidence, hits)`, primary key `(endpoint_hash, target)`, created by `init` from
     `PROTECTED_TARGETS_SCHEMA` (`CREATE TABLE IF NOT EXISTS`, the pattern of the 0.6.0 columns:
     idempotent, no `queue_schema` bump). Older binaries name their tables and columns and never
     read it. `Writer::record_protected_target`, `clear_protected_target`, `protected_target`.
   - Enqueue (`extension.rs`): computes the endpoint hash, reads the fact in the insert
     transaction and returns it as the new optional `QueueJobView.learned_protection`
     (`first_seen`, `evidence`; proto `LearnedProtection`). The client prints it next to the
     probe (the learned line only when the probe is `Open` or `Unknown`; a probe that names the
     protection already says it).
   - Rendering: the reason is an additive string. Older clients print it raw and still suggest a
     retry; this version's CLI `queue status` and MCP `queue_status` say the target refuses direct
     pushes and do not suggest a retry.
7. **Agent text.** MCP `INSTRUCTIONS`, `plugin/skills/sluice/SKILL.md` (landing steps),
   `plugin/skills/sluice/reference.md` (reason table), `docs/agents.md`: `target_protected` means
   the forge refuses direct pushes: tell the human, do not retry; a warning in the enqueue result
   is for the human; the job is queued anyway.
8. **Docs.** `docs/merge-queue.md` (section on protected targets and the reasons table),
   `docs/cli.md` (enqueue, doctor check and network note), `docs/compatibility.md`, `CHANGELOG.md`.

## Why

Issue #5: a protected target is found out after verification and approval. The probe runs in the
client because only the client has the user's `gh` login (a keyring login is invisible to the
daemon); the learned signal is forge-agnostic and covers what `gh` cannot see. The user decided
"warn only", so nothing here refuses.

## Verification

Isolated runs during the work (through the Sluice guard's `sluice exec`): `cargo check --workspace
--all-targets`; `sluice-gate-core` `forge` unit tests (18) and `tests/forge_probe.rs` (7, a fake
`gh` and a fake `ssh` on a private `PATH` holding only `git`, no network); `sluice-gate-queue`
lib (82, including the classifier and the in-place table tests) and the two integration tests
`a_protected_target_fails_as_target_protected_and_is_remembered` and
`a_landing_push_forgets_the_learned_protection` (a `pre-receive` hook printing `GH006`); CLI unit
tests, `doctor::target` tests, MCP unit tests and the two end-to-end tests in
`crates/sluice-gate/tests/queue_e2e.rs` (the unknown-target warning with a still-queued job, and
the learned warning on the second enqueue). The full suite, clippy and `fmt --check` are the
lead's through Sluice.

The response shapes were checked with read-only `gh api` calls on a public repository: the
`--include` framing, `rules/branches/{b}` (`[]` or rule objects with `type`), `branches/{b}`
(`protected`, `protection`), the 404 `Branch not found` body, the 401 body with a bad token and
the 404 `Not Found` of `branches/{b}/protection` without admin rights. Not observed: the
`Branch not protected` body in this wave (it is in the research), a ruleset with a `pull_request`
rule, GitHub Enterprise, and any server text of the classifier.

## Deviations and open points

- Doctor uses the network for this one check (bounded, cached); the clap help and `docs/cli.md`
  were reworded. There is no `--offline` switch.
- Doctor does not read the learned fact: it lives in the daemon's store and no client message
  exposes it; the enqueue reply carries it. A `status` field could expose it later.
- The probe runs after the daemon accepted the job (the daemon resolves the remote and target), so
  a refused enqueue prints no probe result.
- `gh` is needed for every non-`github.com` host login check; GitLab, Gitea and Bitbucket are
  `Unknown` (learned signal only), as the ADR's "GitHub first" says.
- The bypass downgrade (admin caller) of the research is not implemented; the docs say the probe
  describes the `gh` identity.
- `GH013` is deliberately narrower than the research's note (see the classifier above).

## Lead review

- `sluice doctor` stays offline (its documented guarantee): the `target-protection` check no
  longer probes; it shows the last verdict an enqueue stored (`forge::last_known`,
  `forge::cache::last`; verdicts are now kept `KEEP_SECS` = 30 days for this, while probes still
  reuse them for `TTL_SECS` = 5 minutes), and is absent when nothing was checked yet. The clap
  help and `docs/cli.md` say "Never uses the network" again; the key computation moved to
  `forge::target_key`, shared by the probe and the lookup.
- Clippy fixes: `unnecessary_wraps` (test helper `reply`), `format_push_string` (`encode_branch`
  uses `write!`), `assert_is_empty` (four tests), `single_match_else` (`doctor/target.rs`,
  `remote_head` extracted). Full fmt, clippy and test through Sluice: pass.
