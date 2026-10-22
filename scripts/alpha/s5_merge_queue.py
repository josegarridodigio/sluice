"""Scenario 5: the merge queue on a real project, with a real bare remote.

Node project (sindresorhus/slugify, gates `lint` = xo and `test` = ava) with a
bare remote and a `[queue]` section. Six simulated agents (scripted MCP
clients, no model) work in six worktrees: two pairs of branches that conflict
with each other (same lines of the same file: A1/A2 on readme.md, B1/B2 on
package.json), one branch that makes `ava` fail (F), one independent branch
(I1). A1 enqueues through the CLI, the others over MCP; a human approves
through the CLI. The queue lands one train per target at a time and a failing
train is split into single-member trains, so the waves are:

  1. A1 verifies, enqueues and holds the target in a ready train (its tree is
     the agent-verified one: gate reuse); A2, B1, B2, I1 queue behind it;
  2. after A1 lands: A2 and B2 conflict (paths and retry hint reported), B1
     and I1 form a ready train; the remote does not move before approval
     (20 s); `queue_approve` over MCP is refused; a human approval lands
     exactly B1 and I1 plus the audit ref;
  3. B2 and A2 rebase, verify and call `queue_retry`; B2 lands on a second
     approval without running a gate; F and A2 queue together, the train
     fails on `ava`, F is split out with the gate name, A2 lands;
  4. SIGKILL of the daemon during the push (a `pre-receive` hook in the
     remote blocks on a FIFO): `needs_reconcile` after the restart, then
     `reconcile` gives `landed` (hook released) or `ready` (hook aborted,
     approval voided, approving again lands);
  5. leak check: nothing of the sandbox is left running.

Check labels start with the plan's step number (1 agents, 2 queue behaviour,
3 crash during push, 4 gate reuse, 5 leak check).

The remote is reached through `setup_remote`, the one place a transport
variant (ssh, smart HTTP: task H2) has to extend. Default: a bare repository
reached by file path.
"""

from __future__ import annotations

import argparse
import errno
import json
import os
import signal
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

from lib import Report, Sandbox, add_worktree, clone_project, descendants, wait_until
from mcp_agent import McpAgent
from pygate import alive, kill_all, sluice_daemon_pids

TB = Path(os.environ["ALPHA_TESTBEDS"])

SRC_NM = TB / "node-slugify" / "node_modules"

LINK_NM = f"""#!/bin/sh
# Per-checkout node_modules linking every package of the test bed (and with
# its own .cache): the merge worktrees of the queue are created by the daemon
# and have no node_modules.
# Both gates of a checkout run at once: whoever wins `mkdir` populates the
# directory, the other waits for the `.ready` marker.
[ -e node_modules/.ready ] && exit 0
if mkdir node_modules 2>/dev/null; then
  for e in {SRC_NM}/* {SRC_NM}/.[!.]*; do
    n=$(basename "$e")
    [ "$n" = .cache ] && continue
    ln -s "$e" "node_modules/$n"
  done
  : > node_modules/.ready
else
  i=0
  while [ ! -e node_modules/.ready ] && [ "$i" -lt 300 ]; do sleep 0.1; i=$((i + 1)); done
fi
"""


def sluice_toml(link: Path) -> str:
    return f"""version = 1

[gates.lint]
cmd = "sh {link} && npx --no-install xo"
weight = 2
timeout = "5m"
key_inputs = ["node --version"]

[gates.test]
cmd = "sh {link} && npx --no-install ava"
weight = 3
timeout = "10m"
key_inputs = ["node --version"]

[queue]
remote = "origin"
target = "main"
gates = ["lint", "test"]
max_train = 4
"""


# -- remote ---------------------------------------------------------------
@dataclass
class Remote:
    """The shared remote the queue lands on. `bare` is the bare repository
    the harness inspects with git; `transport` says how clients reach it."""

    bare: Path
    transport: str = "local"
    env: dict = field(default_factory=dict)  # extra environment of the approver (H2: SSH_AUTH_SOCK, ...)


def setup_remote(sb: Sandbox, transport: str, name: str) -> tuple[Path, Remote]:
    """Clone the test bed with a bare origin and wire `origin` to it. H2
    extends this function: start sshd or the smart-HTTP server for `ssh` and
    `http`, then `git remote set-url origin <url>` (and fill `Remote.env`)."""
    if transport != "local":
        raise SystemExit(f"--transport {transport} is not implemented yet (task H2)")
    repo = clone_project(sb, TB / "node-slugify", name)
    return repo, Remote(bare=sb.root / f"{name}.git")


def bare_git(sb: Sandbox, rem: Remote, *args: str) -> str:
    return sb.run(["git", "--git-dir", str(rem.bare), *args], sb.root).stdout.strip()


def bare_main(sb: Sandbox, rem: Remote) -> str:
    return bare_git(sb, rem, "rev-parse", "refs/heads/main")


def bare_has(sb: Sandbox, rem: Remote, sha: str) -> bool:
    return sb.run(["git", "--git-dir", str(rem.bare), "cat-file", "-e", f"{sha}^{{commit}}"], sb.root).returncode == 0


def bare_ancestor(sb: Sandbox, rem: Remote, sha: str, of: str = "refs/heads/main") -> bool:
    return sb.run(["git", "--git-dir", str(rem.bare), "merge-base", "--is-ancestor", sha, of], sb.root).returncode == 0


# -- queue helpers ----------------------------------------------------------
class Ctx:
    def __init__(self, sb: Sandbox, rep: Report, repo: Path, rem: Remote) -> None:
        self.sb, self.rep, self.repo, self.rem = sb, rep, repo, rem
        self.agents: dict[str, "Agent"] = {}

    def qs(self) -> dict:
        cp = self.sb.sluice(self.repo, "queue", "status", "--json")
        try:
            return json.loads(cp.stdout)
        except ValueError:
            return {"trains": [], "jobs": []}

    def job(self, branch: str) -> dict:
        jobs = [j for j in self.qs().get("jobs", []) if j["branch"] == branch]
        return max(jobs, key=lambda j: j["enqueued_at"]) if jobs else {}

    def train(self, train_id: str) -> dict:
        return next((t for t in self.qs().get("trains", []) if t["train_id"] == train_id), {})

    def text(self) -> str:
        return self.sb.sluice(self.repo, "queue", "status").stdout

    def runs(self) -> int:
        return int(self.sb.totals(self.repo).get("runs", 0))

    def wait(self, pred, timeout: float = 120.0, step: float = 0.4):
        """Poll `pred(status)` until it returns something truthy."""
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            got = pred(self.qs())
            if got:
                return got
            time.sleep(step)
        return None

    def ready_train(self, branches: list[str], timeout: float = 120.0) -> dict | None:
        def pred(st):
            ids = {j["job_id"] for j in st["jobs"] if j["branch"] in branches and j["state"] == "ready"}
            for t in st["trains"]:
                if t["state"] == "ready" and ids and set(t["members"]) == ids:
                    return t
            return None

        return self.wait(pred, timeout)

    def approve(self, train_id: str):
        return self.sb.sluice(self.repo, "queue", "approve", train_id, "--yes", extra=self.rem.env, timeout=300)


@dataclass
class Agent:
    name: str
    branch: str
    wt: Path
    mcp: McpAgent
    head: str = ""

    def call(self, tool: str, **args) -> dict:
        return self.mcp.call(tool, {"working_directory": str(self.wt), **args})


def commit_all(sb: Sandbox, wt: Path, msg: str) -> str:
    sb.git(wt, "add", "-A")
    sb.run(["git", "commit", "-q", "-m", msg], wt, check=True)
    return sb.git(wt, "rev-parse", "HEAD")


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    assert old in text, f"{old!r} not in {path}"
    path.write_text(text.replace(old, new, 1))


def new_agent(c: Ctx, name: str, branch: str, edit, start: str = "origin/main") -> Agent:
    wt = add_worktree(c.sb, c.repo, name, branch, start)
    edit(wt)
    a = Agent(name, branch, wt, McpAgent(c.sb.env(agent=True), wt, name=name))
    a.head = commit_all(c.sb, wt, f"feat: {name}")
    c.agents[name] = a
    return a


# -- crash helpers ------------------------------------------------------------
def daemon_of_sandbox(c: Ctx) -> int | None:
    """Pid of this sandbox's daemon: the pid `status --json` reports, accepted
    only when its socket lives in the sandbox runtime dir and the process
    census (environment pointing at the sandbox) agrees."""
    d = c.sb.status_json(c.repo).get("daemon") or {}
    pid = d.get("pid")
    if pid and str(d.get("socket", "")).startswith(str(c.sb.runtime)) and pid in sluice_daemon_pids(c.sb):
        return pid
    return None


def push_pids(daemon_pid: int) -> list[int]:
    out = []
    for p in descendants(daemon_pid):
        try:
            argv = Path(f"/proc/{p}/cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if Path(argv[0].decode(errors="replace")).name == "git" and b"push" in argv[:8]:
            out.append(p)
    return out


def fifo_write(fifo: Path, text: str, timeout: float = 20.0) -> bool:
    """Write into the FIFO once a reader (the blocked hook) is there."""
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            fd = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
        except OSError as e:
            if e.errno == errno.ENXIO:
                time.sleep(0.1)
                continue
            raise
        os.write(fd, text.encode())
        os.close(fd)
        return True
    return False


class BlockingHook:
    """`pre-receive` hook of the bare remote that blocks on a FIFO while the
    flag file exists: `go` lets the push through, anything else rejects it."""

    def __init__(self, rem: Remote, root: Path) -> None:
        self.dir = root / "hook"
        self.dir.mkdir()
        self.fifo, self.pidfile, self.flag = self.dir / "fifo", self.dir / "pid", self.dir / "block"
        os.mkfifo(self.fifo)
        script = rem.bare / "hooks" / "pre-receive"
        script.write_text(
            "#!/bin/sh\n"
            f'[ -e "{self.flag}" ] || exit 0\n'
            f'echo $$ > "{self.pidfile}"\n'
            f'IFS= read -r cmd < "{self.fifo}"\n'
            '[ "$cmd" = go ] || { echo "alpha: push rejected by the test hook" >&2; exit 1; }\n'
        )
        script.chmod(0o755)

    def arm(self) -> None:
        self.pidfile.unlink(missing_ok=True)
        self.flag.write_text("1")

    def disarm(self) -> None:
        self.flag.unlink(missing_ok=True)

    def running(self) -> int | None:
        try:
            return int(self.pidfile.read_text().strip())
        except (OSError, ValueError):
            return None


def crash_during_push(c: Ctx, hook: BlockingHook, label: str, agent: Agent, release: str, expect: str) -> None:
    """Approve the agent's train with the hook armed, SIGKILL the daemon while
    the push runs, restart it, then release (or abort) the hook and reconcile."""
    sb, rep, rem = c.sb, c.rep, c.rem
    r = agent.call("queue_enqueue")
    rep.check(f"3{label}: {agent.name} enqueues over MCP", not r["is_error"], r["text"][:160])
    train = c.ready_train([agent.branch], 90)
    if not rep.check(f"3{label}: the train reaches ready", train is not None, "" if train else c.text()[-400:]):
        return
    tid = train["train_id"]
    main_before = bare_main(sb, rem)
    hook.arm()
    approver = sb.popen(["sluice", "queue", "approve", tid, "--yes"], c.repo, extra=rem.env)
    started = wait_until(lambda: hook.running() is not None, 60)
    dpid = daemon_of_sandbox(c)
    pushes = push_pids(dpid) if dpid else []
    rep.check(f"3{label}: the push process is alive (blocked in the remote's pre-receive hook)", started and bool(pushes),
              f"hook pid {hook.running()}, push pids {pushes}, daemon {dpid}")
    state = c.train(tid).get("state")
    rep.check(f"3{label}: the train is pushing and the remote's main has not moved", state == "pushing" and bare_main(sb, rem) == main_before,
              f"state {state}")
    if not dpid:
        rep.check(f"3{label}: the daemon of the sandbox is identified", False, "cannot identify it; not killing anything")
        fifo_write(hook.fifo, "abort\n", 5)
        approver.kill()
        approver.communicate()
        return
    os.kill(dpid, signal.SIGKILL)
    wait_until(lambda: not alive(dpid), 10)
    try:
        approver.communicate(timeout=30)
    except Exception:  # noqa: BLE001 - our own child
        approver.kill()
        approver.communicate()
    cp = sb.sluice(c.repo, "queue", "status")  # any CLI call restarts the daemon
    newpid = daemon_of_sandbox(c)
    rep.check(f"3{label}: any CLI call restarts the daemon (new pid)", newpid is not None and newpid != dpid, f"old {dpid}, new {newpid}")
    t = c.train(tid)
    rep.check(f"3{label}: queue status shows needs_reconcile", t.get("state") == "needs_reconcile" and "NEEDS RECONCILE" in cp.stdout
              and "sluice queue reconcile" in cp.stdout, f"state {t.get('state')}", status=cp.stdout[-500:])
    rep.check(f"3{label}: the push process survived the daemon", any(alive(p) for p in pushes), f"{pushes}")
    cp = sb.sluice(c.repo, "queue", "reconcile")
    rep.check(f"3{label}: reconcile while the push runs refuses (push_in_progress) and changes nothing",
              "push_in_progress" in cp.stdout + cp.stderr and c.train(tid).get("state") == "needs_reconcile", (cp.stdout + cp.stderr).strip()[-200:])
    rep.check(f"3{label}: the remote's main is still untouched", bare_main(sb, rem) == main_before)
    ok = fifo_write(hook.fifo, release)
    gone = wait_until(lambda: not any(alive(p) for p in pushes), 60)
    rep.check(f"3{label}: the hook was {'released' if release == 'go' + chr(10) else 'aborted'} and the push process ended", ok and gone,
              f"fifo {ok}, ended {gone}")
    cp = sb.sluice(c.repo, "queue", "reconcile")
    out = cp.stdout + cp.stderr
    rep.check(f"3{label}: reconcile decides {expect}", f"train {tid}: {expect}" in out, out.strip()[-300:])
    t = c.train(tid)
    j = c.job(agent.branch)
    if expect == "landed":
        rep.check(f"3{label}: remote main is the landing commit, train and job landed",
                  bare_main(sb, rem) == train["merge_sha"] and t.get("state") == "landed" and j.get("state") == "landed",
                  f"main {bare_main(sb, rem)[:8]}, S {train['merge_sha'][:8]}, train {t.get('state')}, job {j.get('state')}")
        audit = bare_git(sb, rem, "rev-parse", "--verify", "-q", f"refs/sluice/landed/{train['merge_sha']}")
        rep.check(f"3{label}: the audit ref exists", audit == train["merge_sha"])
    else:
        rep.check(f"3{label}: back to ready, approval voided, nothing landed",
                  t.get("state") == "ready" and not t.get("approved_via") and not t.get("approved_at") and j.get("state") == "ready"
                  and bare_main(sb, rem) == main_before,
                  f"train {t.get('state')}, approved_via {t.get('approved_via')}, job {j.get('state')}")
        hook.disarm()
        cp = c.approve(tid)
        rep.check(f"3{label}: approving again lands it", cp.returncode == 0 and bare_main(sb, rem) == t["merge_sha"],
                  (cp.stdout + cp.stderr).strip()[-200:])
    cp = sb.sluice(c.repo, "queue", "reconcile")
    rep.check(f"3{label}: nothing is left to reconcile", "no train needs reconciling" in cp.stdout, cp.stdout.strip()[-120:])


def census(sb: Sandbox) -> list[int]:
    """Live processes (daemon, reaper, gates, git, MCP servers, hooks) whose
    environment points at this sandbox's runtime directory."""
    needle = f"XDG_RUNTIME_DIR={sb.runtime}".encode()
    out = []
    for e in Path("/proc").iterdir():
        if not e.name.isdigit() or int(e.name) == os.getpid():
            continue
        try:
            if needle in (e / "environ").read_bytes().split(b"\0") and alive(int(e.name)):
                out.append(int(e.name))
        except OSError:
            continue
    return out


def describe(pids: list[int]) -> str:
    out = []
    for p in pids:
        try:
            cmd = Path(f"/proc/{p}/cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")[:70]
            out.append(f"{p}:{cmd}")
        except OSError:
            out.append(str(p))
    return "; ".join(out)


# -- scenario -------------------------------------------------------------
def md_edit(wt: Path, tag: str) -> None:
    replace_once(wt / "readme.md", "# slugify\n", f"# slugify (alpha {tag})\n")


def pkg_edit(wt: Path, tag: str) -> None:
    replace_once(wt / "package.json", '"description": "Slugify a string",', f'"description": "Slugify a string ({tag})",')


def fail_edit(wt: Path) -> None:
    with (wt / "test.js").open("a") as f:
        f.write("\ntest('alpha: deliberate failure', t => {\n\tt.fail('alpha F');\n});\n")


def indep(tag: str):
    def edit(wt: Path) -> None:
        (wt / "alpha-notes").mkdir(exist_ok=True)
        (wt / "alpha-notes" / f"{tag}.md").write_text(f"independent change {tag}\n")

    return edit


def scenario(rep: Report, transport: str) -> None:
    sb = Sandbox("s5")
    c: Ctx | None = None
    try:
        sb.write_user_config('[queue]\ntick = "200ms"\n')
        repo, rem = setup_remote(sb, transport, "slug")
        link = sb.root / "link_node_modules.sh"
        link.write_text(LINK_NM)
        (repo / ".sluice.toml").write_text(sluice_toml(link))
        sb.git(repo, "add", ".sluice.toml")
        sb.run(["git", "commit", "-q", "-m", "chore: sluice gates and queue"], repo, check=True)
        sb.run(["git", "push", "-q", "origin", "main"], repo, check=True)
        c = Ctx(sb, rep, repo, rem)
        base = bare_main(sb, rem)
        print(f"== sandbox {sb.root}, remote {rem.bare} ({rem.transport}), base {base[:8]}", flush=True)

        # -- 1. six agents ---------------------------------------------------
        specs = [
            ("i1", "alpha/i1", indep("i1")),
            ("a1", "alpha/a1", lambda w: md_edit(w, "A1")),
            ("b1", "alpha/b1", lambda w: pkg_edit(w, "B1")),
            ("f", "alpha/f", fail_edit),
            ("a2", "alpha/a2", lambda w: md_edit(w, "A2")),
            ("b2", "alpha/b2", lambda w: pkg_edit(w, "B2")),
        ]
        ags = {n: new_agent(c, n, br, ed) for n, br, ed in specs}
        results: dict[str, dict] = {}

        def run_verify(a: Agent) -> None:
            results[a.name] = a.mcp.verify(a.wt, timeout=300)

        t0 = time.monotonic()
        threads = [threading.Thread(target=run_verify, args=(a,)) for a in ags.values()]
        [t.start() for t in threads]
        [t.join() for t in threads]
        good = [n for n in ags if n != "f" and (results[n]["structured"] or {}).get("all_passed") is True and not results[n]["is_error"]]
        rep.check("1: five agents verify their branch over MCP (lint + test pass)", len(good) == 5,
                  f"{len(good)}/5 passed in {time.monotonic() - t0:.1f}s; failing: {[n for n in ags if n != 'f' and n not in good]}",
                  texts={n: results[n]["text"][:200] for n in ags if n not in good})
        rep.check("1: the agent whose change breaks ava sees its verify fail", (results["f"]["structured"] or {}).get("all_passed") is False,
                  results["f"]["text"][:200])
        runs_after_verify = c.runs()
        # Wave 1: A1 is enqueued through the CLI; its train holds the target while the others queue.
        cp = sb.sluice(ags["a1"].wt, "queue", "enqueue")
        rep.check("1: the CLI agent enqueues its branch", cp.returncode == 0 and "queued alpha/a1" in cp.stdout, (cp.stdout + cp.stderr).strip()[-200:])
        t1 = c.ready_train(["alpha/a1"], 90)
        rep.check("1: the CLI branch's train reaches ready", t1 is not None, "" if t1 else c.text()[-400:])
        if t1 is None:
            return
        gates1 = [(g["gate"], g["status"]) for g in t1["gates"]]
        rep.check("4: a train equal to an agent-verified tree reaches ready without running a gate (A1, fast-forward)",
                  c.runs() == runs_after_verify and len(gates1) == 2 and all(s == "cached" for _, s in gates1),
                  f"runs {runs_after_verify} -> {c.runs()}, gates {gates1}")
        # The agents enqueue over MCP, in a fixed order, while t1 holds the target.
        enq = {n: ags[n].call("queue_enqueue") for n in ("a2", "b1", "b2", "i1")}
        rep.check("1: four agents enqueue over MCP (queue_enqueue)", all(not r["is_error"] for r in enq.values()),
                  "; ".join(f"{n}: {r['text'][:80]}" for n, r in enq.items() if r["is_error"]) or "all queued")
        time.sleep(2)
        st = c.qs()
        live = [t for t in st["trains"] if t["state"] in ("ready", "building", "verifying")]
        rep.check("1: one train per target: the others wait behind the ready train", len(live) == 1 and bare_main(sb, rem) == base,
                  f"trains {[(t['state'], len(t['members'])) for t in st['trains']]}")
        cp = c.approve(t1["train_id"])
        rep.check("2: human `queue approve --yes` lands the first train", cp.returncode == 0 and "landed" in cp.stdout and bare_main(sb, rem) == t1["merge_sha"],
                  (cp.stdout + cp.stderr).strip()[-200:])
        main1 = bare_main(sb, rem)
        rep.check("2: the audit ref refs/sluice/landed/<S> exists for it", bare_git(sb, rem, "rev-parse", "--verify", "-q", f"refs/sluice/landed/{main1}") == main1)

        # -- 2. wave 2: conflicts reported, a train of compatible branches ready ----
        def wave2(st):
            jobs = {j["branch"]: j for j in st["jobs"]}
            # The members of a train include the ones set aside; the survivors are the jobs in state ready.
            ids = {j["job_id"] for j in st["jobs"] if j["state"] == "ready"}
            want = {jobs[b]["job_id"] for b in ("alpha/b1", "alpha/i1") if b in jobs}
            ready = [t for t in st["trains"] if t["state"] == "ready" and len(want) == 2 and want == ids & set(t["members"])]
            both = all(jobs.get(b, {}).get("state") == "conflict" for b in ("alpha/a2", "alpha/b2"))
            return (jobs, ready[-1]) if ready and both else None

        got = c.wait(wave2, 120)
        if not rep.check("2: the queue settles: A2 and B2 in conflict, B1 and I1 in a ready train", got is not None, "" if got else c.text()[-800:]):
            return
        jobs, t2 = got
        text = c.text()
        a2, b2 = jobs["alpha/a2"], jobs["alpha/b2"]
        a2_paths = json.dumps(a2.get("detail"))
        rep.check("2: A2 is reported in conflict with its path (readme.md) and the retry hint",
                  "readme.md" in a2_paths and "CONFLICT readme.md" in text and f"queue retry {a2['job_id']}" in text,
                  f"state {a2['state']}, detail {a2_paths[:100]}")
        b2_paths = json.dumps(b2.get("detail"))
        rep.check("2: B2 is reported in conflict with its path (package.json) and the retry hint",
                  "package.json" in b2_paths and "CONFLICT package.json" in text and f"queue retry {b2['job_id']}" in text,
                  f"state {b2['state']}, detail {b2_paths[:100]}")
        mt = ags["a2"].call("queue_status")
        rep.check("2: the conflicting agent sees the paths and `queue_retry` over MCP",
                  not mt["is_error"] and "readme.md" in mt["text"] and "queue_retry" in mt["text"], mt["text"][:300])
        rep.check("2: the train of compatible branches (B1, I1) is ready with both gates passed",
                  len(t2["gates"]) == 2 and all(g["status"] in ("pass", "cached") for g in t2["gates"]),
                  f"gates {[(g['gate'], g['status']) for g in t2['gates']]}")
        moved = wait_until(lambda: bare_main(sb, rem) != main1, 20, step=0.5)
        rep.check("2: the remote's main does not move before approval (polled 20 s)", not moved and c.train(t2["train_id"]).get("state") == "ready",
                  f"main {bare_main(sb, rem)[:8]}, train {c.train(t2['train_id']).get('state')}")
        r = ags["b1"].call("queue_approve", train_id=t2["train_id"])
        rep.check("2: queue_approve over MCP is refused (not-allowed)", r["is_error"] and "not-allowed" in r["text"]
                  and bare_main(sb, rem) == main1 and c.train(t2["train_id"]).get("state") == "ready", r["text"][:200])
        cp = c.approve(t2["train_id"])
        rep.check("2: human `queue approve <train> --yes` lands the train", cp.returncode == 0 and "landed" in cp.stdout, (cp.stdout + cp.stderr).strip()[-200:])
        main2 = bare_main(sb, rem)
        subjects = bare_git(sb, rem, "log", "--no-merges", "--format=%s", f"{main1}..{main2}").splitlines()
        rep.check("2: the remote's main contains exactly the approved members", sorted(subjects) == ["feat: b1", "feat: i1"]
                  and main2 == t2["merge_sha"] and bare_ancestor(sb, rem, ags["b1"].head) and bare_ancestor(sb, rem, ags["i1"].head)
                  and not any(bare_has(sb, rem, ags[n].head) for n in ("a2", "b2", "f")), f"new commits {subjects}")
        rep.check("2: the audit ref exists for the second train", bare_git(sb, rem, "rev-parse", "--verify", "-q", f"refs/sluice/landed/{main2}") == main2)
        rep.check("2: the members' jobs are landed, the conflicts unchanged", c.job("alpha/b1").get("state") == "landed" and c.job("alpha/i1").get("state") == "landed"
                  and c.job("alpha/a2").get("state") == "conflict" and c.job("alpha/b2").get("state") == "conflict")

        # -- 2. wave 3: the conflicting agents rebase and retry over MCP -----------
        def rebase_verify(a: Agent) -> bool:
            sb.git(a.wt, "fetch", "-q", "origin")
            cp = sb.run(["git", "rebase", "-X", "theirs", "origin/main"], a.wt)
            v = a.mcp.verify(a.wt, timeout=300)
            ok = cp.returncode == 0 and not v["is_error"] and (v["structured"] or {}).get("all_passed") is True
            rep.check(f"2: {a.name} rebases onto the new main and verifies over MCP", ok, (cp.stdout + cp.stderr + v["text"]).strip()[-200:])
            return ok

        b2a, a2a = ags["b2"], ags["a2"]
        rebase_verify(b2a)
        rebase_verify(a2a)
        runs_before_retry = c.runs()
        b2_head = sb.git(b2a.wt, "rev-parse", "HEAD")
        r = b2a.call("queue_retry", job_id=b2["job_id"])
        rep.check("2: queue_retry over MCP requeues B2", not r["is_error"] and "requeued" in r["text"], r["text"][:200])
        t3 = c.ready_train(["alpha/b2"], 90)
        rep.check("2: the retried branch reaches ready in its own train", t3 is not None, "" if t3 else c.text()[-400:])
        if t3 is None:
            return
        gates3 = [(g["gate"], g["status"]) for g in t3["gates"]]
        rep.check("4: the rebased tree was verified by the agent: the train reaches ready without running a gate (totals.runs unchanged)",
                  c.runs() == runs_before_retry and len(gates3) == 2 and all(s == "cached" for _, s in gates3),
                  f"runs {runs_before_retry} -> {c.runs()}, gates {gates3}")
        # F and A2 queue behind B2's ready train, in this order.
        rf = ags["f"].call("queue_enqueue")
        ra = a2a.call("queue_retry", job_id=a2["job_id"])
        rep.check("2: F enqueues and A2 retries over MCP while B2's train waits", not rf["is_error"] and not ra["is_error"] and "requeued" in ra["text"],
                  f"{rf['text'][:100]} | {ra['text'][:100]}")
        cp = c.approve(t3["train_id"])
        rep.check("2: the retried branch lands on the second approval", cp.returncode == 0 and bare_ancestor(sb, rem, b2_head)
                  and c.job("alpha/b2").get("state") == "landed", (cp.stdout + cp.stderr).strip()[-200:])
        desc = bare_git(sb, rem, "show", "refs/heads/main:package.json")
        rep.check("2: main carries B2's resolved change", '"description": "Slugify a string (B2)",' in desc)

        # -- 2. wave 4: a failing member splits out of its train -------------------
        a2_head = sb.git(a2a.wt, "rev-parse", "HEAD")

        def wave4(st):
            jobs = {j["branch"]: j for j in st["jobs"]}
            a, f = jobs.get("alpha/a2", {}), jobs.get("alpha/f", {})
            ready = [t for t in st["trains"] if t["state"] == "ready" and t["members"] == [a.get("job_id")]]
            return (jobs, ready[-1]) if f.get("state") == "failed" and ready else None

        got = c.wait(wave4, 150)
        if not rep.check("2: the queue settles: F failed, A2 alone in a ready train", got is not None, "" if got else c.text()[-800:]):
            return
        jobs, t4 = got
        text = c.text()
        fj = jobs["alpha/f"]
        rep.check("2: the failing branch is split out with the gate name in queue status",
                  fj.get("reason") == "gate_failed" and (fj.get("detail") or {}).get("gate") == "test"
                  and any("alpha/f" in ln and "gate test" in ln for ln in text.splitlines()),
                  f"state {fj['state']}, reason {fj.get('reason')}, detail {json.dumps(fj.get('detail'))[:120]}")
        dissolved = [t for t in c.qs()["trains"] if t["state"] == "dissolved" and len(t["members"]) == 2
                     and {fj["job_id"], a2["job_id"]} == set(t["members"])]
        rep.check("2: the two-member train [F, A2] was dissolved by the failing gate and its members split",
                  bool(dissolved) and any(g["status"] == "fail" for g in dissolved[-1]["gates"]),
                  f"{[(t['state'], t.get('note'), [(g['gate'], g['status']) for g in t['gates']]) for t in dissolved][-1:]}")
        cp = c.approve(t4["train_id"])
        rep.check("2: the surviving member lands on approval and the failing branch never reaches the remote",
                  cp.returncode == 0 and bare_ancestor(sb, rem, a2_head) and not bare_has(sb, rem, ags["f"].head)
                  and c.job("alpha/a2").get("state") == "landed" and c.job("alpha/f").get("state") == "failed",
                  (cp.stdout + cp.stderr).strip()[-200:])
        readme = bare_git(sb, rem, "show", "refs/heads/main:readme.md").splitlines()[0]
        rep.check("2: main carries A2's resolved change", readme == "# slugify (alpha A2)", readme)

        # -- 3. SIGKILL during the push ----------------------------------------
        rep.check("3: sanity: the census finds the sandbox daemon (the leak check is not vacuous)", daemon_of_sandbox(c) is not None,
                  f"daemon {daemon_of_sandbox(c)}, census {describe(census(sb))[:200]}")
        hook = BlockingHook(rem, sb.root)
        for label, tag, release, expect in (("a", "c1", "go\n", "landed"), ("b", "c2", "abort\n", "ready")):
            ag = new_agent(c, tag, f"alpha/{tag}", indep(tag), "origin/main")
            v = ag.mcp.verify(ag.wt, timeout=300)
            rep.check(f"3{label}: {tag} verifies over MCP", not v["is_error"] and (v["structured"] or {}).get("all_passed") is True, v["text"][:160])
            print(f"== crash variant {label}: release={release.strip()} expect={expect}", flush=True)
            crash_during_push(c, hook, label, ag, release, expect)
            hook.disarm()
    finally:
        # -- 5. leak check -------------------------------------------------------
        if c is not None:
            for a in c.agents.values():
                a.mcp.close()
        sb.stop_daemon(sb.root, force=True)
        wait_until(lambda: not census(sb), 20, step=0.3)
        left = census(sb)
        rep.check("5: no daemon, reaper, gate, git or MCP process of the sandbox is left", not left, describe(left))
        kill_all(left)
        sb.close()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--transport", choices=["local", "ssh", "http"], default="local", help="how the remote is reached (ssh and http: task H2)")
    args = ap.parse_args()
    rep = Report("s5_merge_queue", Path(os.environ.get("ALPHA_OUT", ".")))
    try:
        scenario(rep, args.transport)
    finally:
        rep.save()
        print(f"s5_merge_queue: {len(rep.results) - rep.failed}/{len(rep.results)} checks passed")
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
