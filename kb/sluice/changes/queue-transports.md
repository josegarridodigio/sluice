---
title: "Change: merge queue against authenticated remotes (SSH, smart HTTP)"
tags: [sluice, change, merge-queue, alpha, h2, security]
---

# Change: merge queue against authenticated remotes (2026-10-23)

Alpha plan task H2 ([[sluice/plan/alpha-plan.md]]). Until now every queue
test pushed to a local bare repository; this change drives the queue through
the real `sluice` binary against an SSH server and a smart HTTP server with
real authentication, and fixes the four defects the tests found. Design:
[[sluice/design/merge-queue.md]] sections 6, 7 and 12 (security notes
confirmed); user docs: `docs/merge-queue.md` "Authenticated remotes".

## Test servers (`crates/sluice-gate/tests/queue_transports/`)

`main.rs` (harness and cases), `servers.rs` (servers and guards),
`git_http_server.py` (HTTP server, embedded with `include_str!`). Each test
starts its own servers inside its sandbox temp dir; every process is a child
of the test owned by a guard that kills exactly that pid on drop (also on
panic). After the runs no `sshd -D -e`, `ssh-agent -a <sandbox>`,
`git_http_server.py` or `sleep 600` of the tests was left (checked with `ps`).

- **SSH.** `ssh-keygen` makes a host key and three client keys (`good`,
  `wrong`, `stall`). `sshd -D -e -f <config>` runs unprivileged on a free port
  of 127.0.0.1 (`free_port`, three attempts) with `HostKey`,
  `AuthorizedKeysFile` (the `good` key plain; the `stall` key with
  `command="stall.sh",no-pty`), `AllowUsers <user>`, `StrictModes no`,
  `UsePAM no`, no password or keyboard-interactive auth, `PidFile none`.
  `stall.sh` runs `git-upload-pack` normally (so `ls-remote` and fetches
  work) and turns `git-receive-pack` into `exec sleep 600`, recording its pid
  for the drop guard. Two `ssh-agent -D -a <sock>` hold the keys (`good` and
  `stall` in one, `wrong` in the other); the private key files are deleted
  after `ssh-add`, so the agent is the only way in. Remote
  `ssh://<user>@127.0.0.1:<port>/<sandbox>/srv/remote.git`; each credentials
  set is `SSH_AUTH_SOCK` plus `GIT_SSH_COMMAND="ssh -F /dev/null -o
  BatchMode=yes -o IdentitiesOnly=yes -i <key>.pub -o UserKnownHostsFile=<file>
  -o GlobalKnownHostsFile=/dev/null -o StrictHostKeyChecking=accept-new -o
  ConnectTimeout=5"` (both allowlisted). Without OpenSSH (`sshd` looked up in
  `/usr/sbin`, `/usr/bin`, then `PATH`) or when `sshd` cannot start
  unprivileged, the SSH tests print `skipped: …` and pass.
- **Smart HTTP.** `python3 -I git_http_server.py <srv> <config.json>`:
  `ThreadingHTTPServer` on port 0 (printed on stdout), each request run
  through `git http-backend` as a CGI program by hand (`GIT_PROJECT_ROOT`,
  `PATH_INFO`, `QUERY_STRING`, `REMOTE_USER`, chunked bodies decoded), basic
  auth checked by the server against users `alice` (ok), `bob` (stall: any
  `git-receive-pack` request sleeps 600 s) and `carol` (URL-embedded
  password); `anonymous_read` lets fetches through without credentials. The
  config is re-read per request, so a test flips it on the fly. A watchdog
  thread exits when the parent test process goes away. Credentials sets are
  `GIT_ASKPASS=<script>` answering `Username…` / `Password…`.

The sandbox repository has `[queue] push_timeout = "12s"`,
`network_timeout = "5s"`; the daemon is always started by a credential-free
CLI call (`queue status`), so credentials only reach it through the client
allowlist of `enqueue` and `approve`.

## Cases (each for SSH and HTTP)

| Test | What it proves | Result |
|---|---|---|
| `*_push_lands_with_the_approver_agent` / `_askpass` | enqueue (good) → ready → `queue approve --yes` (good) → landing commit and audit ref on the remote, job `landed` | pass, approve ~1.0 s |
| `*_wrong_{key,password}_fails_definitively_without_hanging` | approve with the wrong key / password: exit 1 at once, `train … failed: feat/a: push_rejected the remote refused the approver's credentials: …`, job `failed`, train dissolved, remote untouched. HTTP also: `retry`, anonymous reads on, approve again: the push itself gets the 401 → `push_rejected` (detail `Authentication failed`) | pass, ~1.0 s each (was 48 s and a train left `approved` forever) |
| `*_stalled_push_is_bounded_and_other_repositories_keep_landing` | approve with the stall credentials: the connection is accepted, the push never answered; meanwhile a second repository (local remote) enqueues, gets ready, is approved and lands while the first train is still `pushing`; after `push_timeout` the first train is `needs_reconcile` (note `push_timeout`) and `approve` says so; `reconcile` over the transport (stored approver env) decides `ready`; re-approval with good credentials lands | pass, approve returns after ~15 s (12 s push timeout + polling) |
| `*_reconcile_after_a_killed_push` | FIFO `pre-receive` on the served bare repository; SIGKILL of the daemon while the push blocks; the credential-free restart parks the train; `reconcile` refuses `push_in_progress`; hook released → `reconcile` reads the remote over the transport → `landed` | pass |
| `http_password_in_the_remote_url_never_leaks` | `origin = http://carol:<wrong>@…`: three base fetch failures → job `failed` (`base_unavailable`) with the error in its detail; then the right password in the URL, `retry`, approve with no env → lands | pass after the fix (failed before: the wrong password was in `sluice.db-wal`) |

Every case ends with `Q::assert_no_leaks`: one `sluice run test` with the
credentials in its env (so `client.log` has a line), then every file of the
state directory (`sluice.db`, `-wal`, `-shm`, `daemon.log`, `logs/client.log`,
job logs, merge worktrees) and of the extracted `sluice doctor --bundle`, plus
every CLI output of the test, is searched byte for byte for the private key
material (base64 body lines of the three client keys) or the four passwords.
The check asserts it reads the real files (`daemon.log` contains `train
claimed`, database and `client.log` exist). Nothing leaked once the URL fix was
in.

## Defects found and fixed (`sluice-gate-queue`)

1. **Refused credentials hung the approval.** The pre-push check
   (`runner::pre_push_checks`) runs `ls-remote` with the approver's env; any
   failure was "remote unreachable, retry next tick", so a wrong key or
   password left the train `approved` forever (retried every tick) and
   `sluice queue approve` waited its whole deadline (48 s here, 11+ min with
   the default `push_timeout`) before saying "still APPROVED". Fix: new
   `push::credentials_refused` (SSH `Permission denied`, forge `Permission to
   … denied`, `Authentication failed`, HTTP 401/403, `could not read
   Username/Password`, `Host key verification failed`); on such an error the
   check returns `PreCheck::FailMembers("push_rejected", "the remote refused
   the approver's credentials: …")`. Regression: both
   `*_wrong_*_fails_definitively_without_hanging` (`took < 20 s`).
2. **The SSH reason was dropped from network errors.** `core::git::command_error`
   keeps only `fatal:`/`error:` lines, so `user@host: Permission denied
   (publickey).` never reached the classification. `Git::net` now appends
   the transport's own last stderr lines (at most three, not already in the
   message). Regression: `ssh_wrong_key_fails_definitively_without_hanging`.
3. **A push refused with `Authentication failed` was `needs_reconcile`.**
   `push::classify` only knew "permission denied" and 403; an HTTP 401 on the
   push (anonymous reads, authenticated pushes: the common forge setup) was
   ambiguous although nothing was sent. `classify` now uses
   `credentials_refused` → `Rejected` (`push_rejected`). Regression: unit test
   `push::tests::refused_credentials_are_definitive` and the second half of
   `http_wrong_password_fails_definitively_without_hanging`.
4. **A password in the remote URL was stored and logged.** Network errors
   carried the whole git command line and the `git fetch <url>` /
   `git ls-remote <url>` contexts, URL included; they are written to job
   details (`base_unavailable`), train notes and `daemon.log`, contrary to
   design section 12 ("push URLs are never stored"). Fix: `git::hide_userinfo`
   replaces the URL by `target::strip_userinfo(url)` in every `Git::net`
   error (`fetch`, `ls_remote`, `remote_head`) and both contexts use the
   stripped URL. Git itself already anonymises URLs in its own messages.
   Regression: `http_password_in_the_remote_url_never_leaks`, unit tests
   `git::tests::userinfo_is_hidden_from_messages` and
   `git::tests::a_failed_network_call_does_not_reveal_the_url_password`.

Files: `crates/sluice-gate-queue/src/git.rs` (`Git::net` signature with the
URL, `hide_userinfo`, stderr tail), `push.rs` (`credentials_refused`,
`classify`), `runner.rs` (`pre_push_checks`); tests above;
`docs/merge-queue.md`; [[sluice/design/merge-queue.md]] sections 6, 7, 12.

## Verified

`sluice run fmt`, `sluice run clippy`, `sluice run test` (see the task
report for the three lines). The transport tests take about 20 s with four
threads (the two stall cases dominate: one `push_timeout` each).

## Open points

- A stall during the pre-push `ls-remote` (not the push) is still retried
  every tick with the train `approved` (design: unreachable remote); each try
  is bounded by `network_timeout`, the runner keeps serving others
  (`a_stalled_remote_times_out_without_blocking_other_repositories`), and
  `approve` stops waiting after its deadline.
- Credential helpers that need a desktop session (libsecret via D-Bus) are
  not reachable from the daemon: `DBUS_SESSION_BUS_ADDRESS` is not in the
  allowlist. Documented; not tested.
- `hide_userinfo` strips the user name too (`ssh://git@host` →
  `ssh://host`) in messages, as `strip_userinfo` does for the endpoint hash.
