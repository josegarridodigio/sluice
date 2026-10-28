---
title: Protected targets - early detection and pull-request landing (analysis for issue #5 / plan S3)
tags: [sluice, research, merge-queue, forge, github, gitlab, issue-5]
---

# Protected targets: early detection and pull-request landing

Analysis for plan item S3 ([[sluice/plan/queue-scheduling-plan.md]]) and GitHub issue #5,
input to one ADR. Collected 2026-10-28 on release 0.7.0 (`main`). Analysis only: no
product code was changed. Sources are marked: **[code]** read in this repository,
**[docs]** fetched from the cited page on 2026-10-28 (a page that did not state something
is said so), **[observed]** a read-only call made on this machine, **[recalled]** from
general knowledge, not re-verified: confirm before it becomes a requirement.

## 0. Summary and recommendation

1. Today the queue lands by one `git push --atomic` of `S:refs/heads/<target>` from the
   merge worktree, with the **approver's** allowlisted env. A target that only accepts pull
   requests refuses it after verification and approval (`push_rejected`, no dedicated reason).
2. **Detection.** Do it in the **client process** (CLI `enqueue`/`approve`, the MCP server's
   `queue_enqueue`, `sluice doctor`), not in the daemon, through the forge CLI the user already
   authenticated (`gh`, `glab`) with read-only calls and a three-valued result
   (`open` / `protected` / `unknown`). `unknown` warns and never blocks. Add a forge-agnostic
   **learned** signal: when a push ends with a recognised protected-branch rejection, give it
   the reason `target_protected` and remember `(endpoint_hash, target)`; the next enqueue
   warns with the evidence. Refuse an enqueue only on a *definitive* "pull request required
   and the caller cannot bypass" verdict, as a config-controlled policy.
3. **PR landing** (`[queue] land = "push" | "pull-request"`): GitHub first, behind a small
   `Forge` trait (GitLab second; other forges only when a CLI or push option exists). After
   human approval, push the verified `S` to `sluice/train/<id>` and find-or-create **one PR
   per train** (head pinned to `S`). Sluice never merges (the forge or a human does;
   optionally `--auto` armed by the approver). New train and job state `awaiting_merge`; the
   train is `landed` when polling sees the PR merged, recording whether the merged tree
   equals the verified tree. A PR closed unmerged fails its members (`pr_closed`), never
   silently requeues.
4. Never store tokens: no `GH_TOKEN`/`GITLAB_TOKEN` in the env allowlist (the DB stores
   `push_env` values, `store.rs:53`); rely on the CLIs' own credential stores via `HOME`.

Evidence that shaped the design ([observed], read-only `gh` calls on a repository where the
caller is admin; names omitted):

- `GET repos/<o>/<r>/branches/main` returns `protected` and a `protection` summary with plain
  read access; `GET .../rules/branches/main` returned `[]`; `GET .../branches/main/protection`
  returned `404 {"message":"Branch not protected"}` (a plain "Not Found" would be
  indistinguishable from "no access" by status alone); `GET repos/<o>/<r>` returns
  `permissions.{admin,push,...}` for the caller.
- The `origin` remote was an **ssh config alias** (`git@<alias>:<o>/<r>.git`), not
  `github.com`: forge detection from the URL's host fails; it needs `ssh -G <alias>`.
- `gh` was logged in through the **system keyring**, which a daemon (no D-Bus variables in
  the allowlist) cannot reach: another reason to probe from the client.
- `gh` 2.102 and `glab` are installed on the dev machine; `tea` is not.

## 1. Current code map

### 1.1 How a train lands today

| Step | Where |
|---|---|
| Train states `building, verifying, ready, approved, pushing, landed, dissolved, needs_reconcile`; job states add `queued, merging, conflict, failed, cancelled`; transition table | `crates/sluice-gate-queue/src/model.rs:70-135` (`ALL_*_STATES`, `JOB_TRANSITIONS`); unknown wire states map to `T::Unknown` (`model.rs:60`), so a new train state degrades gracefully for old readers |
| Enqueue: validates, resolves the default target from the head's config, `target::resolve`, inserts the job with the **allowlisted** client env | `extension.rs:375-470` (`enqueue`); `target.rs:160` (`resolve`), `target.rs:75` (`defaults_at`) |
| `is_policy()` refusals (`own_branch`, `target_checked_out`) map to `ErrorCode::NotAllowed`: the natural shape for a `target_protected` refusal | `target.rs:62`; `extension.rs` enqueue error mapping |
| Approve: refuses agents unless allowed; stores the approver's allowlisted env as `push_env`; plan hash must match | `extension.rs:595-660`; `store.rs:53,267,1200` |
| Env allowlist `SSH_AUTH_SOCK, SSH_AGENT_PID, GIT_SSH_COMMAND, GIT_ASKPASS, SSH_ASKPASS, HOME, PATH, TERM`; `Git::with_client_env`; `Core::git_for` | `git.rs:29-49`, `git.rs:81`, `runner.rs:241` |
| Push pre-checks: HEAD == `S`, tracked files clean, endpoint unchanged, local target not checked out, target still at `base_sha` (`ls-remote`); refused credentials become `push_rejected` before any push | `runner.rs:2158-2203` (`pre_push_checks`; `target_at` `runner.rs:466`) |
| `push_train`: pin ref `refs/sluice/pending/<id>`, one transaction writing the marker and moving jobs/train to `pushing`, then the push (remote) or `land_local` | `runner.rs:2205-2360`; marker `store.rs:503-514` |
| Push args: `git push --atomic --porcelain --no-follow-tags --recurse-submodules=no --force-with-lease=refs/heads/<t>:<base> [--force-with-lease=refs/sluice/landed/<S>:] -- <remote> S:refs/heads/<t> [S:refs/sluice/landed/<S>]`; hooks enabled | `push.rs:186-204`; spawn in its own process group `push.rs:249` |
| Verdicts `Landed`, `Moved` (`[rejected]`, requeue), `Rejected` (`[remote rejected]`, `permission denied`, refspec), `HookDeclined` (`pre_push_hook`), `Ambiguous` (-> `needs_reconcile`) | `push.rs:14-35`, `classify` `push.rs:98-182`; applied in `apply_verdict` `runner.rs:2361-2430` |
| A protected target is a `[remote rejected]` ref line: `Rejected` -> jobs `failed`, reason `push_rejected`, `detail.output` holds the server text, train dissolved | `push.rs:157-160`; `runner.rs:2384-2410` |
| Reconcile: pure 10-row decision table over `ls-remote`/fetch facts (target == `S` -> landed; audit ref -> landed; `S` ancestor -> landed; target == base -> `ready` or cancelled; else queued / `ancestry_unknown`), using the train's `push_env` | `reconcile.rs:58-123` (`decide`), `:125` (`gather`); `extension.rs:794-860` |
| Daemon start parks `pushing` trains as `needs_reconcile` | `extension.rs:319-350` |

Constraints for the design: (a) the daemon has **no credentials of its own**; the approver's
env is kept until the train is terminal (`store.rs:1208`); (b) `[queue]` settings are read
from the target commit, never from the branch (ADR-0018 section 6); (c) agents can neither
approve (unless the user enabled it) nor use `--trust-member-config`.

### 1.2 What `sluice doctor` checks for the queue

Almost nothing queue-specific and nothing about the remote: `train-tools` (`doctor.rs`, the
`key_inputs` tools resolvable in the trains' PATH, needs a running daemon) and `queue-only-hooks`
(`doctor.rs:537-577`). The doctor runs in the client (`socket_to_contact`, `client::connect`),
has the repo config and the user's env, and can run `git remote get-url --push`: the right place
for a `target-protection` check with no daemon change.

### 1.3 Where an enqueue could check the target

- **Client side, before the request** (CLI `enqueue`, MCP server `queue_enqueue`): has the
  user's `HOME`, keyring, `gh`/`glab` config; can run a bounded subprocess. Advisory only: a
  hostile client could skip it, and the forge's protection stays the last word.
- **Daemon side, in `extension::enqueue`** (next to `target::resolve`): authoritative for a
  refusal, but the env is the allowlist (no D-Bus, no token variables), so a keyring-backed
  `gh` fails there. It can use only what is learned (2.6).
- **At approval** (`pre_push_checks`, `runner.rs:2158`, or the approving client before it sends
  the approval): the approver's env and a human at the terminal. Too late to save the
  verification but the right place to say "this will be refused" or to offer PR landing.

## 2. Early detection

### 2.1 Result type

`Protection::{Open, PullRequestRequired{evidence}, Protected{evidence}, Unknown{reason}}`.
`Protected` = some protection exists but is not shown to block direct pushes (status checks
only, or the caller may bypass). **Only `PullRequestRequired` justifies a refusal; `Unknown`
and bypass-capable results warn.** A push can always still be refused later; detection moves
the failure earlier, it does not replace it.

### 2.2 GitHub (GitHub first)

| Call | Needs | What it says |
|---|---|---|
| `GET /repos/{o}/{r}/rules/branches/{b}` | read access [observed: `[]` on an unprotected branch]. [docs]: "Returns all active rules that apply to the specified branch"; rulesets with enforcement `evaluate` or `disabled` are excluded; the branch need not exist; only `200` is documented | rule objects: `pull_request` (with `required_approving_review_count`), `required_status_checks` (with `strict_required_status_checks_policy`), ... A `pull_request` rule = PR required. Does **not** report classic branch protection |
| `GET /repos/{o}/{r}/branches/{b}` | read access [observed] | `protected` bool and a `protection` summary (`required_status_checks`, `enforcement_level`). `protected: true` with no `pull_request` rule = classic protection of unknown kind -> `Protected`, warn |
| `GET /repos/{o}/{r}/branches/{b}/protection` | [docs]: the page lists only `404` and does not state the permission for GET (setting protection needs admin/owner); treat as **admin-only** [recalled]. Body `Branch not protected` = none [observed] | exact classic settings: `required_pull_request_reviews`, `enforce_admins`, `restrictions`, `required_status_checks.strict` [docs] |
| `GET /repos/{o}/{r}` -> `permissions` | read | `admin`/`push` of the caller [observed]. Admin with `enforce_admins` off, or a ruleset bypass list, can push to a PR-required branch: `admin: true` downgrades `PullRequestRequired` to a warning |

Status codes: [docs] give only `200`/`404` for these GETs. In practice [recalled]: `404` =
repo missing, token without access (private repos are hidden), or not a GitHub host; `403` =
rate limit, SAML SSO not authorised for the token, or missing permission; `401` = bad
credentials. **Every non-200 (other than the `Branch not protected` message) is `Unknown`**,
with `gh`'s message (`gh: <message> (HTTP <code>)`) as the reason.

Procedure: rules call -> `pull_request` rule present -> `PullRequestRequired`; else branches call
-> `protected: false` -> `Open`; `protected: true` -> try the admin-only protection call
(`required_pull_request_reviews` present -> `PullRequestRequired`; `Branch not protected` ->
`Open`; other -> `Protected`/`Unknown`). Then apply the bypass downgrade. At most four small
GETs, bounded by `network_timeout`, cached per `(endpoint_hash, target)` for a few minutes in
the client (a file under the state dir, no secrets).

Auth: `gh api` uses `GH_TOKEN`/`GITHUB_TOKEN`, else `~/.config/gh/hosts.yml`, else the system
keyring [observed: keyring]. Pass `--hostname <host>` for GitHub Enterprise Server (it needs
its own `gh auth login --hostname`) [recalled].

### 2.3 GitLab

[docs] `GET /projects/:id/protected_branches/:name` returns `push_access_levels` and
`merge_access_levels` (entries with `access_level`: `0` No access, `30` Developer, `40`
Maintainer, `60` Administrator on self-managed), `allow_force_push`, `code_owner_approval_required`
(Premium and Ultimate); the page names no minimum role for GET. `push_access_levels` with only
`access_level: 0` (no user, group or deploy-key entry) = nobody can push = `PullRequestRequired`
(merge request required). Level 30/40 = depends on the caller's role (`GET /projects/:id` ->
`permissions` [recalled]) -> compare. Call: `glab api projects/<url-encoded path>/protected_branches/<branch>`.
`404` for an unprotected branch [recalled]; `401/403` -> `Unknown`. Push rules and approval
rules of paid tiers are not visible through this call.

### 2.4 Gitea / Forgejo and Bitbucket

- Gitea/Forgejo: [docs] the API index lists get/list/create/edit/delete **branch protections**,
  "get a branch (with its effective branch protection)" and "merge a pull request"; field details
  were not on the page. Recalled fields: `enable_push`, `push_whitelist_usernames`,
  `required_approvals`, `block_on_outdated_branch`, `enable_status_check`. Reading protections
  probably needs repo admin [recalled]. CLI `tea` is not installed here: no CLI -> `Unknown`.
- Bitbucket Cloud: branch restrictions API (`kind` `push`, `restrict_merges`,
  `require_approvals_to_merge`, `require_passing_builds_to_merge`) [recalled; the docs page
  could not be read]. No first-party CLI.
- Recommendation: out of scope for v1 beyond the generic mechanisms (learned signal, plain git).

### 2.5 Plain git servers (no API)

Only the push result tells. `git push --dry-run` does not run server hooks (issue #5,
`docs/merge-queue.md`). A no-op push of the current target SHA is skipped client-side as up to
date, and a push of a throwaway ref does not test the target's rule. There is no safe probe:
detection for unknown forges is the learned signal only.

### 2.6 Learned signal (forge-agnostic)

Recognise protected-branch rejections in `PushVerdict::Rejected` and give them their own reason
`target_protected` (still `failed`, still not requeued, same dissolve): GitHub's `GH006:
Protected branch update failed` and `GH013: Repository rule violations found`, GitLab's `You are
not allowed to push code to protected branches on this project` [recalled]; the issue quotes
`protected branch hook declined`; keep a generic fallback. Persist `(endpoint_hash, target,
first_seen, evidence_tail)` in the queue DB (`endpoint_hash` exists, `target.rs:342`) and make
the next enqueue say "this target refused a direct push on <date>: <evidence>". Staleness:
protection can be removed, so the learned fact warns (cleared when a push lands) and never
refuses by itself. It is also the only diagnosis available for forges Sluice cannot query.

### 2.7 Identifying the forge from the remote

Input: `git remote get-url --push <remote>` (what the push uses, `insteadOf`/`pushurl` applied:
`Git::push_url`, `git.rs:245`; `strip_userinfo`, `target.rs:306`). Forms: `https://[user[:tok]@]host[:port]/<path>[.git]`,
`ssh://[user@]host[:port]/<path>`, scp-like `[user@]host:<path>`, `git://`, local paths (no forge).

1. Strip userinfo; take lowercase `host` (port dropped) and `path` minus `.git` and the leading
   `/`. GitHub: exactly `owner/repo`; GitLab: `group[/sub...]/project` (nested groups); Gitea may
   sit under a URL prefix.
2. **Resolve ssh aliases** [observed: the origin here is an ssh config alias]: `ssh -G <host>`
   and read `hostname`/`port`; `ssh.github.com:443` is `github.com` [recalled]. Without this,
   alias setups are `Unknown`.
3. Map host -> kind: built-ins `github.com`, `gitlab.com`, `codeberg.org` (Forgejo),
   `bitbucket.org`; then hosts the user's CLIs are logged into (`gh auth status`, `glab auth
   status`); then user config `[forge."<host>"] kind = "github|gitlab|gitea"` (user config only,
   like `allow_agent_approval`: a repo file must not choose which binary runs against a host);
   else `Unknown`. A network probe (`/api/v3/meta`, `/api/v4/version`, `/api/v1/version`) only
   behind an explicit key: it is a request to a host the user did not name for that purpose.
4. Always call the CLIs with an **explicit repo and host** (`gh api --hostname h repos/o/r/...`,
   `gh pr ... -R h/o/r`, `glab api` with the path); without it `gh` picks a remote by its own
   rules (several remotes, forks).
5. Fork topologies (the push URL is a fork of the repo that owns the target) are out of scope:
   `Unknown` for detection, refused for PR mode.

### 2.8 Credentials for detection

The enqueuer's/approver's own environment, never stored: the client runs `gh`/`glab` itself and
only the verdict (a short string) crosses to the daemon. ADR-0018 keeps token variables out of
the stored env; ADR-0020 forbids the enqueuer's `PATH` for the **judges** of a branch, which
does not apply to a read-only advisory probe, but the probe must never be able to *pass*
something: it produces a warning or a refusal, never a verdict that lets a train land. No CLI or
no login -> `Unknown("gh not found")` / `Unknown("not logged in")`, and `sluice doctor` says how
to log in.

### 2.9 Options for detection

| | A. Client probe, advisory | B. Learned only | C. Daemon probe at enqueue/approve |
|---|---|---|---|
| Forge-agnostic | no (GitHub, then GitLab) | yes | no |
| Credentials | the user's `gh`/`glab` login, in the client | none | the daemon cannot reach keyrings or tokens |
| When | before any work | after the first failure | before any work |
| Risks | a client may skip it (harmless), rate limits (cache), alias parsing, false `Open` for bypass | the first failure still wastes a run; staleness | breaks on keyring logins; tempts allowlisting tokens |

**Recommendation: A + B**, with the same probe run by the approving CLI before it sends the
approval (`unknown` never blocks) and `sluice doctor` printing `target-protection: <verdict>`.
Policy key `[queue] protected_target = "refuse" | "warn" | "ignore"` read from the target commit
(default: refuse on `PullRequestRequired`, warn otherwise). With `land = "pull-request"` a
protected target is simply fine.

## 3. Pull-request landing

### 3.1 What others do

- **GitHub merge queue** [docs]: a temporary branch per queued PR holds the latest target plus the
  PRs ahead (its SHA differs from the PR's); workflows **must** trigger on `merge_group` or the
  required check is never reported; third-party CI must run on pushes to branches prefixed
  `gh-readonly-queue/{base_branch}`; a "Merge method" setting (merge, rebase, squash); a status
  check timeout removes the PR; `gh pr merge` enqueues when checks passed and enables auto-merge
  otherwise; `--admin` bypasses. The forge owns the queue; no local runner.
- **Mergify** [docs, partial]: updates each queued PR against the latest target and re-runs CI;
  speculative parallel checks, batching; merge strategies are on a separate page, not read.
- **bors-ng** [docs, partial]: `bors r+` runs the tests then **pushes to the base branch**;
  `status` = statuses that must pass on the merge commit; `use_squash_merge`; the public
  instance is being phased out for GitHub merge queues. It needs push rights on the target (an
  exempt app), so it is itself a direct-push tool.
- **Kodiak, Aviator**: not fetched; recalled as bots driving the forge's own merge (auto-merge,
  queue), never a direct push to a protected branch.
- **`ref-mergetrain`** [code, `refs/mergetrain/docs/pr-workflows.md`]: direct push only; a
  rejected protected ref is parked `blocked`. It documents hybrid pattern 2 ("one review branch,
  one PR": push the validated train to a branch where pushes are allowed, open one PR to the
  protected target; "the operator must ensure the PR head is the exact validated train") and
  pattern 4 ("local validation is not proof that the forge assembled the same commit"). It does
  not implement PR landing.

The design below is pattern 2, automated, one PR per train. Honest limit: it cannot give the
forge queue's "test the merge group" semantics; the verified `S` is the exact merged tree only
when the target did not move (3.5).

### 3.2 Flow

After human approval (unchanged: plan hash, agent refusal, `--trust-member-config` rules), the
runner replaces step 3 of `push_train` (`runner.rs:2323`):

1. keep pin ref and marker (the marker gains `land`, `pr_branch`, later `pr_number`, `pr_url`,
   `forge`); train -> `pushing` as today;
2. `git push --porcelain --force-with-lease=refs/heads/sluice/train/<id>: -- <remote>
   S:refs/heads/sluice/train/<id>` with the same approver env and classifier (a rejection here is a
   real `push_rejected`; no audit ref: the branch is the durable record);
3. find-or-create the PR: `gh pr list -R h/o/r --head sluice/train/<id> --state all --json
   number,state,headRefOid,mergedAt,mergeCommit`; if none, `gh pr create -R h/o/r --base <target>
   --head sluice/train/<id> --title ... --body <members, gates, train id>`. REST: `POST
   /repos/{o}/{r}/pulls` [docs: `head`, `base`, `title`, `draft`; `201/403/422`]. GitHub refuses a
   second open PR for the same head and base with `422` [recalled], so the step is idempotent;
4. optional `gh pr merge --auto --match-head-commit S` (+ `--merge|--squash|--rebase`) when the
   approver asked for it (`--auto-merge`) or the user config says so. [docs] `--auto` merges only
   after the requirements are met, `--match-head-commit` merges only if the head is `S`; with a
   merge queue on the target `gh pr merge` enqueues it. `--admin` is **never** used;
5. record the PR; jobs and train -> `awaiting_merge`.

Agents cannot merge: the PR exists only after a human approved (the approver's credentials,
`allow_agent_approval` unchanged); Sluice never calls the merge endpoint, only arms `--auto`,
which the forge executes subject to its own required reviews and checks. A repository where an
agent's own `gh` can merge its PR is the forge's configuration, as direct push is today
(`docs/merge-queue.md`, closing paragraph).

### 3.3 What "landed" means

[docs] `GET /repos/{o}/{r}/pulls/{n}` gives `merged`, `merged_at` and `merge_commit_sha`: the
merge commit for a merge, the **squashed commit** for squash, "the commit the base branch was
updated to" for rebase. [docs] `PUT .../merge` takes `merge_method` (`merge|squash|rebase`) and
`sha` (the head must match, `409` otherwise); `405` = cannot merge; it "doesn't support ... merge
queues".

| Method | Result on the target if it did not move since `B` | Check |
|---|---|---|
| merge commit | merge commit `M`, parents (`B`, `S`); tree(`M`) == tree(`S`) == `T` | `S` ancestor of the target and tree(`merge_commit_sha`) == `T` |
| squash | one new commit, tree == `T`; `S` is **not** an ancestor; members' commits collapsed | tree(`merge_commit_sha`) == `T` |
| rebase | commits replayed onto the target; merge commits of `S` linearised, SHAs change; tree == `T` when unmoved | tree(`merge_commit_sha`) == `T` |

Rule: **`landed` = PR merged and its merge commit reachable from the target** (fetch it into a
Sluice ref, `merge-base --is-ancestor`), recording `verified_tree = (tree(merge_commit_sha) == T)`
in the train note. Equal: `landed`. Different (the target moved, or the forge or a human changed
content): still `landed` (a human's decision) with note `tree_differs_from_verified`, shown in
status; never call a different tree "verified". The audit-ref and `S`-ancestor rows of the
existing reconcile table do not apply to squash and rebase; PR state replaces them.

### 3.4 Required status checks

- A repository with forge CI: its CI runs on the PR head `S` (and on `merge_group` if a queue is
  on); Sluice's local verification is a preflight, not the authority. Nothing to post.
- If the protection requires a context only Sluice can produce, post a **commit status** on `S`:
  `POST /repos/{o}/{r}/statuses/{sha}` with `state`, `context`, `target_url`, `description`;
  [docs] "Users with push access ... can create commit statuses for a given SHA" (1000 per SHA and
  context). Check runs are, recalled, creatable only by GitHub Apps (the statuses page did not say):
  not available to a user token. So statuses, opt-in only (`status_context = "sluice/verify"`),
  posted with the approver's credentials at PR creation, `success` only for the tree actually
  verified.
- Trust note: anyone with push access can post any status, so a required `sluice/verify` is only
  as strong as the push credentials (it is not a defence against an agent holding the user's
  token; ADR-0018 already names the forge's protection and the guard as outer layers). A target
  with "Require merge queue" is supported only as "enqueue through `gh pr merge`"; statuses
  cannot satisfy `merge_group` checks.

### 3.5 The target moves before the forge merges

`S` was built on `B`; the target advances to `B'`:

- Strict protection ("require branches up to date" = `required_status_checks.strict` [docs]) blocks
  the merge until the branch is updated; the forge's update creates a new head (not `S`): the
  verified artifact is gone. Rebuilding is a full verification (minutes).
- Non-strict: the forge merges `S` onto `B'`, a result nobody verified (note of 3.3).

Options: **(a) wait** (default): the PR stays; status says `target moved N commits since the train
was built`; forge CI on the PR remains authoritative; a human decides. **(b) requeue**
(`on_target_moved = "requeue"`, opt-in): on a poll, if `target != base`, the PR is unmerged and no
auto-merge is armed, close the PR with a comment, delete the branch (lease on `S`), dissolve the
train so members go `queued`, rebuild, re-verify, ask approval again. It can loop on a busy target:
cap it like `base_unavailable` (3 attempts, `store.rs:1051`). **(c) rely on the forge**: (a) without
the status line. Recommend (a) default, (b) opt-in.

### 3.6 Detecting merge or close: polling

Sluice is local, so no inbound webhooks: poll `gh pr view <n> -R h/o/r --json
state,mergedAt,mergeCommit,headRefOid,baseRefName,mergeStateStatus,autoMergeRequest` from the
runner every `pr_poll_interval` (default 60 s, backing off to 10 min as the PR ages). GitHub's
REST limit is 5000 requests per hour per user [recalled]: a handful of PRs is negligible; rate
limit or auth errors are `forge_unreachable`, never state. Polling needs credentials possibly days
after approval: the daemon uses the stored `push_env` (`HOME` -> `~/.config/gh`), fine for a
file-stored login, failing for a keyring login or an expired agent socket. Then the train stays
`awaiting_merge` with `forge_unreachable: <reason>` (like `remote_unreachable`) and
`sluice queue reconcile`, a client command with the user's real env, also polls PRs, closing the
keyring gap. A foreground `sluice queue watch` is a possible later addition.

Outcomes: merged -> `landed`. Closed unmerged -> jobs `failed`, reason `pr_closed` (a human
decided; requeueing would fight them); `sluice queue retry` is the way back. Head changed
(`headRefOid != S`, someone pushed to the PR): block auto-merge arming, show `pr_head_changed`;
if it is merged anyway it counts as landed with the tree note (3.3). Branch deleted by a human:
same as closed. `cancel` of a job in `awaiting_merge`: `gh pr close` when the approver's env
allows, else report and leave the PR.

### 3.7 One PR per train or one per member

One PR per train: the train is the verified unit and `S` combines all members. Cost: the forge's
squash collapses members into one commit and rebase drops merge commits. Per-member PRs would each
need their own verification (losing the combined check, the point of the queue). Recommend one PR
per train, a body listing members (branch, head SHA, job id) and gates, the `merge` method (or
squash for solo trains), and a `doctor`/plan warning when the target only allows squash or rebase
and the train has several members. A split after a gate failure works as today (before any PR).

### 3.8 Partial failures and reconcile

| Failure after | State | Recovery |
|---|---|---|
| marker, branch push unfinished | `needs_reconcile` | `ls-remote` the branch: absent -> nothing happened -> `ready` (today's row 8); at `S` -> continue to the PR |
| branch at `S`, PR not created | `needs_reconcile` | find-or-create (idempotent, 3.2 step 3) |
| PR created, DB write lost | `needs_reconcile` | `gh pr list --head` finds it, adopt |
| PR created, auto-merge arming failed | `awaiting_merge`, note `auto_merge_failed` | shown in status; a human merges; never blocks |
| daemon restart in `awaiting_merge` | unchanged (nothing is running) | next poll |

Reconcile gets a second pure decision table for `land = pull-request` (inputs: remote branch SHA,
PR state, merged, merge commit reachable, auth ok), tested like `decide` (`reconcile.rs:58`).
Branch and PR are deleted only when Sluice created them, the train is dissolved and the PR closed,
with lease on the recorded SHA. Schema: marker columns (`store.rs:58`) and `queue_status` proto
(optional fields; `welcome.features` gains e.g. `queue-pr-landing`, so a client that does not see
it refuses the flag, as for `queue-trust-config`); new states read as `Unknown` in old clients.

### 3.9 Credentials for PR landing

- `gh`/`glab` run by the daemon from `push_env` (allowlist + `HOME`): works for a file-stored
  login; with a keyring login PR creation would fail after approval. So the approving client runs
  a pre-flight (`gh auth status`, a read-only API call) from the **daemon's view** (an env
  restricted to the allowlist) and the daemon refuses to arm PR landing, with an instruction, when
  the CLI is not usable there (`gh auth login --insecure-storage` or `GH_TOKEN` in a file-based
  setup; flag recalled, verify). Do not add token variables to the allowlist.
- No-CLI alternative: GitLab **push options** (`git push -o merge_request.create -o
  merge_request.target=<t> -o merge_request.merge_when_pipeline_succeeds`) and Gitea/Forgejo AGit
  (`refs/for/<target>`, `-o topic=...`) [both recalled, verify]: the push itself opens the PR with
  the git credentials already allowlisted. GitHub has no equivalent. A good second phase through
  the same trait.
- Never log PR bodies beyond train metadata.

## 4. Options and recommendation

### 4.1 PR landing

| | P1. CLI-driven (`gh`, later `glab`) | P2. Push options only (GitLab, Forgejo AGit) | P3. Documentation only (done in 0.7) |
|---|---|---|---|
| Coverage | GitHub, then GitLab | GitLab, Forgejo | everything, by doing nothing |
| Credentials | the approver's CLI login via `HOME`; keyring logins do not reach the daemon | the existing git credentials | none |
| Merge detection | CLI polling | still needs a CLI or API | n/a |
| Required checks | opt-in commit status | forge CI | n/a |
| Risk | CLI output drift (use `--json`), rate limits, keyring | small coverage | issue #5 stays open |

**Recommendation: P1 for GitHub first behind a `Forge` trait (`detect`, `probe_protection`,
`ensure_pr`, `pr_state`, `arm_auto_merge`), with P2 and GitLab added through the same trait.** Two
increments, each its own change note: (1) detection (A + B, `target_protected` reason, doctor,
docs, one small table for the learned fact); (2) PR landing (`land` key, `awaiting_merge`, marker
columns, reconcile table, poll). Defaults: `land = "push"`; `land = "pull-request"` only through the
target commit's `.sluice.toml` or the user config, never a branch; the approval plan states "will
open a pull request to <target>".

### 4.2 Risks to carry into the ADR

- A keyring-only login is invisible to the daemon (observed): the ADR must state what Sluice does
  (refuse PR mode at approval with an instruction), not guess.
- Bypass-capable callers and identity mismatch: the push may use an SSH identity different from the
  `gh` account [observed: SSH alias and `gh` login are separate], so the probe describes the `gh`
  identity only; refuse early only when `PullRequestRequired` and the caller is not admin.
- Agents merging: nothing here gives an agent a merge path; PR creation follows human approval;
  `--admin` is never used; `--auto` only by the approver or user config.
- API limits: at most four GETs per enqueue with a short cache; polls with backoff.
- Verified-tree drift (3.3, 3.5): record and show, never hide.
- Orphans: `sluice/train/<id>` branches and PRs after cancel or dissolve: delete only what Sluice
  created, with lease; list leftovers in `doctor`.

### 4.3 Forge-agnostic versus GitHub-first

Forge-agnostic: the `Protection` tri-state, the learned signal and the `target_protected` reason,
the `land` key, `awaiting_merge`, marker columns, the PR reconcile table, the `landed` rule (merged
+ reachable + tree note), polling and backoff, one PR per train, approval semantics, ssh alias and
URL parsing. GitHub-first (behind the trait): the rules/branches/protection calls, `gh pr
create|list|view|merge --auto`, commit statuses. GitLab second (`glab`, protected branches, push
options). Gitea/Forgejo and Bitbucket: unknown or learned only.

## 5. Open questions for the ADR (user decisions)

1. Refuse on `PullRequestRequired` by default, or only warn (recommended: refuse, with a key to
   downgrade).
2. `awaiting_merge` as a new job and train state (recommended) or a sub-state of `pushing`.
3. When the target moves: wait (recommended) or requeue (opt-in).
4. The opt-in `sluice/verify` commit status in the first PR increment or later (recommended later).
5. Whether to require a file-based `gh` login for PR mode, and the `doctor` message.
6. Whether `sluice queue reconcile` also polls PRs (recommended yes).

## Sources

- GitHub REST, rules for a branch: https://docs.github.com/en/rest/repos/rules
- GitHub REST, branch protection: https://docs.github.com/en/rest/branches/branch-protection
- GitHub REST, pull requests: https://docs.github.com/en/rest/pulls/pulls
- GitHub REST, commit statuses: https://docs.github.com/en/rest/commits/statuses
- GitHub merge queue: https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/incorporating-changes-from-a-pull-request/merging-a-pull-request-with-a-merge-queue
  and https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue
- `gh pr merge`: https://cli.github.com/manual/gh_pr_merge
- GitLab protected branches API: https://docs.gitlab.com/api/protected_branches/
- Gitea API index: https://docs.gitea.com/api/1.22/
- Mergify merge queue: https://docs.mergify.com/merge-queue/
- bors-ng: https://bors.tech/documentation/
- Reference: `refs/mergetrain/docs/pr-workflows.md` (see [[sluice/research/references/mergetrain.md]])
- Design: [[sluice/design/merge-queue.md]], [[sluice/design/adr/0018-queue-trusted-push-and-prepare.md]],
  [[sluice/design/adr/0020-tool-environment-of-trains.md]], `docs/merge-queue.md`
