"""Scenario 3: correctness invariants under concurrency (Python project).

Test bed: pallets/itsdangerous with a controllable gate (see pygate.py).
Agents are scripts: 16 `sluice mcp` servers speaking MCP over stdio, and CLI
calls; no language model.

  1. 16 concurrent `verify` calls on one tree -> exactly one run, one job id,
     one verdict.
  2. 16 concurrent requests on 16 distinct trees -> 16 runs, never more at
     once than the budget allows (sampled from /proc).
  3. A tree whose gate fails is never reported as pass, including after a
     concurrent pass on a sibling tree that differs by one byte.
  4. Untracked non-ignored file, mode change, rename and symlink target
     change -> miss; ignored file -> hit (and every revert hits again).
  5. `key_inputs` output change (PATH shim for `python3 --version`), `key_files`
     content change and TTL expiry -> miss.
  6. The user's index, `index.lock` and `.git/objects` are byte-identical
     before and after 200 cache checks.
  7. `CI=1` disables the cache; `SLUICE_DISABLE=1` bypasses everything.

Knobs: ALPHA_TESTBEDS, ALPHA_PYLIB, ALPHA_OUT, TMPDIR (see README.md).
"""

from __future__ import annotations

import hashlib
import json
import os
import stat
import sys
import threading
import time
from pathlib import Path

from lib import Report, Sandbox, add_worktree, wait_until
from mcp_agent import McpAgent
from pygate import (Cleanup, counter_lines, gate_pids, gate_toml, require_testbeds, setup_repo, sluice_daemon_pids,
                    which_real)

TOKEN = f"alpha-s3-{os.getpid()}"
FANOUT = 16


def lookup(sb: Sandbox, repo: Path, gate: str, extra: dict | None = None) -> str:
    """HIT / MISS / other word of `sluice cache status` for one gate."""
    out = sb.run(["sluice", "cache", "status"], repo, extra=extra).stdout
    for line in out.splitlines():
        parts = line.split()
        if parts and parts[0] == gate:
            return parts[1]
    return f"?({out.strip()[-120:]})"


def verify_many(sb: Sandbox, jobs: list[tuple[Path, list[str]]], hold_barrier: bool = True, timeout: float = 300) -> list[dict]:
    """One MCP agent per job, all initialised first, then all calling `verify` at once."""
    agents = [McpAgent(sb.env(agent=True), repo, name=f"v{i}") for i, (repo, _) in enumerate(jobs)]
    results: list[dict | None] = [None] * len(jobs)
    barrier = threading.Barrier(len(jobs))

    def go(i: int) -> None:
        repo, gates = jobs[i]
        if hold_barrier:
            barrier.wait()
        results[i] = agents[i].verify(repo, gates, timeout=timeout)

    threads = [threading.Thread(target=go, args=(i,)) for i in range(len(jobs))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    for a in agents:
        a.close()
    return results  # type: ignore[return-value]


def gate_result(res: dict, gate: str) -> dict:
    for r in (res.get("structured") or {}).get("results", []):
        if r.get("gate") == gate:
            return r
    return {}


class PidSampler(threading.Thread):
    """Peak number of simultaneously running gate processes (from /proc)."""

    def __init__(self, token: str) -> None:
        super().__init__(daemon=True)
        self.token = token
        self.halt = threading.Event()
        self.peak = 0
        self.seen: set[int] = set()

    def run(self) -> None:
        while not self.halt.is_set():
            pids = gate_pids(self.token)
            self.seen.update(pids)
            self.peak = max(self.peak, len(pids))
            time.sleep(0.02)


def tree_fingerprint(repo: Path) -> dict:
    """Byte-level fingerprint of the index, index.lock and .git/objects."""
    git = repo / ".git"
    index = git / "index"
    objs = hashlib.sha256()
    count = 0
    for p in sorted((git / "objects").rglob("*")):
        rel = p.relative_to(git / "objects").as_posix()
        if p.is_file() and not p.is_symlink():
            objs.update(rel.encode() + b"\0" + hashlib.sha256(p.read_bytes()).digest())
            count += 1
        else:
            objs.update(rel.encode() + b"\0dir")
    st = index.stat()
    return {
        "index_sha256": hashlib.sha256(index.read_bytes()).hexdigest(),
        "index_mtime_ns": st.st_mtime_ns,
        "index_lock": (git / "index.lock").exists(),
        "objects_sha256": objs.hexdigest(),
        "object_files": count,
    }


def main() -> int:
    require_testbeds()
    out = Path(os.environ.get("ALPHA_OUT", "."))
    rep = Report("s3_correctness", out)
    clean = Cleanup()
    t_start = time.monotonic()
    real_python = which_real("python3")

    # A PATH shim: `python3 --version` is answered from a file when the file exists.
    sb = Sandbox("s3", base=Path(os.environ.get("TMPDIR") or None))
    clean.sandboxes.append(sb)
    shim = sb.root / "shim"
    shim.mkdir()
    shim_state = sb.root / "python-version-override"
    (shim / "python3").write_text(
        "#!/bin/sh\n"
        f'if [ "$1" = "--version" ] && [ -f "{shim_state}" ]; then cat "{shim_state}"; exit 0; fi\n'
        f'exec {real_python} "$@"\n'
    )
    (shim / "python3").chmod(0o755)
    path_extra = {"PATH": f"{shim}:{sb.bin}:{os.environ.get('PATH', '/usr/bin:/bin')}"}
    clean.tokens.append(TOKEN)

    try:
        # The budget is small and explicit so that scenario 2 has something to bound.
        sb.write_user_config("[daemon]\nbudget = 8\n")
        gates = {
            "slow": {"hold": 3.0, "weight": 1, "hooks": ["pre-commit"]},
            "wide": {"hold": 1.5, "weight": 3, "hooks": ["pre-commit"]},
            "real": {"hold": 0.2, "weight": 1, "pytest": True, "hooks": ["pre-commit"]},
        }
        repo, counter = setup_repo(sb, "py", TOKEN, gates)

        # ---- 1. 16 concurrent verify calls on one tree ------------------------
        print("== 1. 16 concurrent verify calls on one tree", flush=True)
        (repo / "one.txt").write_text("tree one\n")
        sb.git(repo, "add", "-A")
        before = int(sb.totals(repo).get("runs", 0))
        res = verify_many(sb, [(repo, ["slow"])] * FANOUT)
        runs_after = int(sb.totals(repo).get("runs", 0))
        rs = [gate_result(r, "slow") for r in res]
        job_ids = {r.get("job_id") for r in rs}
        verdicts = {r.get("status") for r in rs}
        executions = counter_lines(counter, "slow")
        rep.check("1: 16 concurrent verify calls on one tree run the gate exactly once",
                  executions == 1 and runs_after - before == 1,
                  f"gate executions {executions}, daemon runs +{runs_after - before}",
                  coalesced=sum(1 for r in rs if r.get("coalesced")), cached=sum(1 for r in rs if r.get("cached")))
        rep.check("1: all 16 callers got the same job id and the same verdict",
                  len(job_ids) == 1 and None not in job_ids and verdicts == {"pass"} and all(not r["is_error"] for r in res),
                  f"job ids {sorted(map(str, job_ids))}, verdicts {sorted(map(str, verdicts))}")

        rep.check("1: sanity: the process census finds the daemon (the final leak check is not vacuous)",
                  sb.daemon_pid(repo) in sluice_daemon_pids(sb), f"daemon {sb.daemon_pid(repo)}, census {sluice_daemon_pids(sb)}")

        # ---- 2. 16 requests on 16 distinct trees ------------------------------
        print("== 2. 16 concurrent requests on 16 distinct trees", flush=True)
        wts = []
        for i in range(FANOUT):
            wt = add_worktree(sb, repo, f"d{i}", f"alpha/d{i}")
            (wt / f"distinct-{i}.txt").write_text(f"tree {i}\n")
            sb.git(wt, "add", "-A")
            wts.append(wt)
        weight = 3
        budget = 8
        limit = budget // weight  # 2 gates of weight 3 fit in a budget of 8
        before = int(sb.totals(repo).get("runs", 0))
        sampler = PidSampler(TOKEN + "-wide")
        sampler.start()
        t0 = time.monotonic()
        res = verify_many(sb, [(wt, ["wide"]) for wt in wts])
        wall = time.monotonic() - t0
        sampler.halt.set()
        sampler.join()
        runs_after = int(sb.totals(repo).get("runs", 0))
        executions = counter_lines(counter, "wide")
        rs = [gate_result(r, "wide") for r in res]
        rep.check("2: 16 distinct trees produce 16 runs, all passing",
                  executions == FANOUT and runs_after - before == FANOUT and all(r.get("status") == "pass" for r in rs),
                  f"gate executions {executions}, daemon runs +{runs_after - before}, distinct job ids {len({r.get('job_id') for r in rs})}")
        rep.check("2: concurrent gates never exceed the budget (sampled from /proc)",
                  1 <= sampler.peak <= limit,
                  f"peak {sampler.peak} concurrent gates of weight {weight} (budget {budget} => at most {limit}), "
                  f"{len(sampler.seen)} gate processes seen, wall {wall:.1f}s",
                  peak=sampler.peak)

        # ---- 3. failing tree is never reported as pass ------------------------
        print("== 3. a failing tree is never reported as pass", flush=True)
        good = add_worktree(sb, repo, "good", "alpha/good")
        bad = add_worktree(sb, repo, "bad", "alpha/bad")
        (good / "flag.txt").write_text("ok\n")
        (good / "sibling.txt").write_text("the sibling\n")
        (bad / "flag.txt").write_text("ox\n")  # one byte differs from "ok"
        (bad / "sibling.txt").write_text("the sibling\n")
        for w in (good, bad):
            sb.git(w, "add", "-A")
        jobs = [(good, ["real"])] * 4 + [(bad, ["real"])] * 4
        res = verify_many(sb, jobs)
        good_ok = [gate_result(r, "real").get("status") == "pass" for r in res[:4]]
        bad_st = [gate_result(r, "real").get("status") for r in res[4:]]
        bad_all = [(r.get("structured") or {}).get("all_passed") for r in res[4:]]
        rep.check("3: a concurrent pass on the sibling tree does not turn the failing tree into a pass",
                  all(good_ok) and all(s != "pass" for s in bad_st) and not any(bad_all),
                  f"good {good_ok}, bad statuses {bad_st}, all_passed {bad_all}")
        again = []
        for _ in range(3):
            r = verify_many(sb, [(bad, ["real"])])[0]
            again.append((gate_result(r, "real").get("status"), (r.get("structured") or {}).get("all_passed")))
        cp_bad = sb.run(["sluice", "run", "real"], bad)
        cp_good = sb.run(["sluice", "run", "real"], good)
        rep.check("3: the failing tree stays failing on retries, CLI and MCP, while the sibling hits",
                  all(s != "pass" and a is not True for s, a in again) and cp_bad.returncode != 0 and "PASS" not in cp_bad.stderr + cp_bad.stdout
                  and cp_good.returncode == 0 and lookup(sb, bad, "real") == "MISS" and lookup(sb, good, "real") == "HIT",
                  f"retries {again}, CLI bad exit {cp_bad.returncode}, CLI good exit {cp_good.returncode}, "
                  f"bad lookup {lookup(sb, bad, 'real')}, good lookup {lookup(sb, good, 'real')}")

        # ---- 4. what does and does not invalidate the tree --------------------
        print("== 4. tree-content invalidation", flush=True)
        t4 = add_worktree(sb, repo, "t4", "alpha/t4")
        (t4 / ".gitignore").write_text("__pycache__/\nignored.log\nlocal.env\n")
        (t4 / "target-a.txt").write_text("a\n")
        (t4 / "target-b.txt").write_text("b\n")
        os.symlink("target-a.txt", t4 / "link")
        sb.git(t4, "add", "-A")
        sb.run(["git", "commit", "-q", "-m", "chore: fixture"], t4, check=True)
        cp = sb.run(["sluice", "run", "real"], t4)
        base_runs = counter_lines(counter, "real")
        rep.check("4: baseline pass on the fixture tree is cached", cp.returncode == 0 and lookup(sb, t4, "real") == "HIT", f"exit {cp.returncode}")

        def case(label: str, mutate, revert, expect_hit: bool, reverts_hit: bool = True) -> None:
            mutate()
            state = lookup(sb, t4, "real")
            ran_before = counter_lines(counter, "real")
            cp = sb.run(["sluice", "run", "real"], t4)
            ran = counter_lines(counter, "real") - ran_before
            ok = (state == "HIT") == expect_hit and (ran == 0) == expect_hit and cp.returncode == 0
            rep.check(f"4: {label} -> {'hit' if expect_hit else 'miss'}", ok,
                      f"lookup {state}, run executed the gate {ran} time(s), exit {cp.returncode}")
            revert()
            back = lookup(sb, t4, "real")
            if reverts_hit:
                rep.check(f"4: reverting '{label}' hits again (content addressed)", back == "HIT", f"lookup {back}")

        def write_untracked():
            (t4 / "untracked.txt").write_text("new\n")

        def remove_untracked():
            (t4 / "untracked.txt").unlink()

        case("untracked non-ignored file", write_untracked, remove_untracked, expect_hit=False)
        case("ignored file", lambda: (t4 / "ignored.log").write_text("noise\n"), lambda: (t4 / "ignored.log").unlink(), expect_hit=True)
        target = t4 / "CHANGES.rst"
        mode = target.stat().st_mode
        case("file mode change (chmod +x)", lambda: target.chmod(mode | stat.S_IXUSR), lambda: target.chmod(mode), expect_hit=False)

        def rename():
            os.rename(t4 / "README.md", t4 / "README-renamed.md")

        def unrename():
            os.rename(t4 / "README-renamed.md", t4 / "README.md")

        case("rename of a tracked file", rename, unrename, expect_hit=False)

        def retarget():
            (t4 / "link").unlink()
            os.symlink("target-b.txt", t4 / "link")

        def untarget():
            (t4 / "link").unlink()
            os.symlink("target-a.txt", t4 / "link")

        case("symlink target change", retarget, untarget, expect_hit=False)

        # ---- 5. key inputs, key files, TTL ------------------------------------
        print("== 5. key_inputs, key_files, TTL", flush=True)
        keyed = {"keyed": {"hold": 0.2, "key_inputs": ["python3 --version"], "key_files": ["local.env"], "hooks": ["pre-commit"]}}
        k5 = add_worktree(sb, repo, "k5", "alpha/k5")
        (k5 / ".gitignore").write_text("__pycache__/\nlocal.env\n")
        (k5 / ".sluice.toml").write_text(gate_toml(TOKEN, counter, {**gates, **keyed}))
        (k5 / "local.env").write_text("A=1\n")
        sb.git(k5, "add", "-A")
        sb.run(["git", "commit", "-q", "-m", "chore: keyed gate"], k5, check=True)

        def run_keyed() -> tuple[int, int]:
            n0 = counter_lines(counter, "keyed")
            cp = sb.run(["sluice", "run", "keyed"], k5, extra=path_extra)
            return cp.returncode, counter_lines(counter, "keyed") - n0

        rc, ran = run_keyed()
        rc2, ran2 = run_keyed()
        rep.check("5: keyed gate: first run executes, the second hits", rc == 0 and ran == 1 and rc2 == 0 and ran2 == 0, f"runs {ran}, {ran2}")
        shim_state.write_text("Python 9.9.9\n")
        state = lookup(sb, k5, "keyed", path_extra)
        rc, ran = run_keyed()
        rep.check("5: key_inputs output change (python3 --version via PATH shim) -> miss", state == "MISS" and rc == 0 and ran == 1,
                  f"lookup {state}, executed {ran} time(s)")
        shim_state.unlink()
        rep.check("5: restoring the key_inputs output hits again", lookup(sb, k5, "keyed", path_extra) == "HIT")
        (k5 / "local.env").write_text("A=2\n")
        state = lookup(sb, k5, "keyed", path_extra)
        rc, ran = run_keyed()
        rep.check("5: key_files content change (ignored file local.env) -> miss", state == "MISS" and rc == 0 and ran == 1,
                  f"lookup {state}, executed {ran} time(s)")
        rc, ran = run_keyed()
        rep.check("5: the new key_files content is cached too", rc == 0 and ran == 0, f"executed {ran} time(s)")
        # TTL: the smallest configurable TTL is 1 day (`cache.ttl_days`, integer), so the
        # marker is aged by setting its mtime two days back; the daemon and clients apply
        # max(mtime age, passed_at age) > ttl.
        sb.write_user_config("[daemon]\nbudget = 8\n[cache]\nttl_days = 1\n")
        common5 = Path(sb.git(k5, "rev-parse", "--path-format=absolute", "--git-common-dir"))
        markers = list((common5 / "sluice" / "markers").glob("*.json"))
        keyed_markers = [m for m in markers if not m.name.endswith(".miss.json") and '"gate": "keyed"' in m.read_text()]
        old = time.time() - 2 * 86400
        for m in keyed_markers:
            os.utime(m, (old, old))
        state = lookup(sb, k5, "keyed", path_extra)
        rc, ran = run_keyed()
        rep.check("5: TTL expiry (ttl_days = 1, markers aged by mtime to 2 days) -> miss", bool(keyed_markers) and state == "MISS" and rc == 0 and ran == 1,
                  f"{len(keyed_markers)} keyed marker(s) aged, lookup {state}, executed {ran} time(s); "
                  "limitation: ttl_days is an integer number of days, so the age is simulated through mtime")
        sb.write_user_config("[daemon]\nbudget = 8\n")

        # ---- 6. the user's git state is untouched by cache checks --------------
        print("== 6. index / index.lock / objects untouched by 200 cache checks", flush=True)
        t6 = add_worktree(sb, repo, "t6", "alpha/t6")
        (t6 / "staged.txt").write_text("staged\n")
        sb.git(t6, "add", "staged.txt")
        (t6 / "untracked-6.txt").write_text("not in the index\n")
        (t6 / "README.md").write_text((t6 / "README.md").read_text() + "\nmodified, unstaged\n")
        sb.run(["sluice", "run", "real"], t6)  # populate the cache for the hit path
        gitdir = Path(sb.git(t6, "rev-parse", "--absolute-git-dir"))
        # worktree git dirs hold their own index; objects live in the common dir
        common = Path(sb.git(t6, "rev-parse", "--path-format=absolute", "--git-common-dir"))
        idx = gitdir / "index"

        def fp() -> dict:
            h = hashlib.sha256()
            n = 0
            for p in sorted((common / "objects").rglob("*")):
                rel = p.relative_to(common / "objects").as_posix()
                if p.is_file():
                    h.update(rel.encode() + b"\0" + hashlib.sha256(p.read_bytes()).digest())
                    n += 1
                else:
                    h.update(rel.encode() + b"\0dir")
            status = sb.git(t6, "--no-optional-locks", "status", "--porcelain=v2", "--untracked-files=all")
            st = idx.stat()
            return {"index": hashlib.sha256(idx.read_bytes()).hexdigest(), "index_mtime_ns": st.st_mtime_ns,
                    "index_lock": (gitdir / "index.lock").exists() or (common / "index.lock").exists(),
                    "objects": h.hexdigest(), "object_files": n,
                    "status": status}

        # The repository's own index for the main worktree is checked as well.
        main_fp0 = tree_fingerprint(repo)
        a = fp()
        failures = 0
        for i in range(200):
            if i % 2:
                cp = sb.run(["sluice", "cache", "status"], t6)
            else:
                cp = sb.run(["sluice", "run", "real"], t6)
            failures += cp.returncode != 0
        b = fp()
        main_fp1 = tree_fingerprint(repo)
        rep.check("6: worktree index is byte-identical (content and mtime) after 200 cache checks",
                  a["index"] == b["index"] and a["index_mtime_ns"] == b["index_mtime_ns"], f"sha256 {a['index'][:16]} -> {b['index'][:16]}")
        rep.check("6: no index.lock was left behind", not a["index_lock"] and not b["index_lock"])
        rep.check("6: .git/objects is byte-identical (no new loose objects or packs)", a["objects"] == b["objects"] and a["object_files"] == b["object_files"],
                  f"{a['object_files']} files -> {b['object_files']} files")
        rep.check("6: `git status` output is unchanged and all 200 checks succeeded", a["status"] == b["status"] and failures == 0, f"{failures} non-zero exits")
        rep.check("6: the main worktree's index and objects are untouched as well", main_fp0 == main_fp1)
        leftovers = [p.name for p in (sb.runtime / "sluice").glob("snap-*")]
        rep.check("6: no snapshot scratch directories left in the runtime dir", not leftovers, f"{leftovers}")

        # ---- 7. CI=1 and SLUICE_DISABLE=1 ---------------------------------------
        print("== 7. CI=1 and SLUICE_DISABLE=1", flush=True)
        t7 = add_worktree(sb, repo, "t7", "alpha/t7")
        (t7 / "seven.txt").write_text("seven\n")
        sb.git(t7, "add", "-A")
        sb.run(["sluice", "run", "real"], t7)
        n0 = counter_lines(counter, "real")
        rep.check("7: baseline: the second run hits", sb.run(["sluice", "run", "real"], t7).returncode == 0 and counter_lines(counter, "real") == n0)
        cp = sb.run(["sluice", "run", "real"], t7, extra={"CI": "1"})
        ci_ran = counter_lines(counter, "real") - n0
        rep.check("7: CI=1 ignores the cache (the gate runs although a marker exists)", cp.returncode == 0 and ci_ran == 1 and "PASS (cached" not in cp.stderr + cp.stdout and "cache disabled (CI is set)" in cp.stderr,
                  f"exit {cp.returncode}, executed {ci_ran} time(s), output {(cp.stdout + cp.stderr)!r}")
        t7b = add_worktree(sb, repo, "t7b", "alpha/t7b")
        (t7b / "seven-b.txt").write_text("seven b\n")
        sb.git(t7b, "add", "-A")
        sb.run(["sluice", "run", "real"], t7b, extra={"CI": "1"})
        rep.check("7: CI=1 does not write markers either", lookup(sb, t7b, "real") == "MISS", f"lookup after a CI=1 pass: {lookup(sb, t7b, 'real')}")
        n0 = counter_lines(counter, "real")
        cp = sb.run(["sluice", "run", "real"], t7, extra={"CI": "1", "SLUICE_CACHE": "1"})
        rep.check("7: SLUICE_CACHE=1 overrides CI=1 (documented)", cp.returncode == 0 and counter_lines(counter, "real") == n0, f"executed {counter_lines(counter, 'real') - n0} time(s)")
        cp = sb.run(["sluice", "run", "real"], t7, extra={"CI": "0"})
        rep.check("7: a falsy CI (CI=0) does not count as set", cp.returncode == 0 and counter_lines(counter, "real") == n0)

        # SLUICE_DISABLE=1: no cache, no queue, no daemon, no user config, no log, no marker.
        sb.stop_daemon(t7)
        wait_until(lambda: not sluice_daemon_pids(sb), 10)
        sb.write_user_config("this is [[ not valid toml\n")
        t7c = add_worktree(sb, repo, "t7c", "alpha/t7c")
        (t7c / "seven-c.txt").write_text("seven c\n")
        sb.git(t7c, "add", "-A")
        logs_before = len(list((sb.state / "sluice" / "logs").glob("*.log"))) if (sb.state / "sluice" / "logs").exists() else 0
        n0 = counter_lines(counter, "real")
        cp1 = sb.run(["sluice", "run", "real"], t7c, extra={"SLUICE_DISABLE": "1"})
        cp2 = sb.run(["sluice", "run", "real"], t7c, extra={"SLUICE_DISABLE": "1"})
        executed = counter_lines(counter, "real") - n0
        logs_after = len(list((sb.state / "sluice" / "logs").glob("*.log"))) if (sb.state / "sluice" / "logs").exists() else 0
        rep.check("7: SLUICE_DISABLE=1 runs the gate every time, with a broken user config and no daemon",
                  cp1.returncode == 0 and cp2.returncode == 0 and executed == 2, f"exit {cp1.returncode}/{cp2.returncode}, executed {executed} time(s), stderr {cp1.stderr[-200:]!r}")
        rep.check("7: SLUICE_DISABLE=1 starts no daemon and writes no job log", not sluice_daemon_pids(sb) and logs_after == logs_before,
                  f"daemons {sluice_daemon_pids(sb)}, job logs {logs_before} -> {logs_after}")
        sb.write_user_config("[daemon]\nbudget = 8\n")
        rep.check("7: SLUICE_DISABLE=1 wrote no marker", lookup(sb, t7c, "real") == "MISS", f"lookup {lookup(sb, t7c, 'real')}")
        (t7c / "flag.txt").write_text("broken\n")
        cp = sb.run(["sluice", "run", "real"], t7c, extra={"SLUICE_DISABLE": "1"})
        rep.check("7: SLUICE_DISABLE=1 propagates the gate's own exit code", cp.returncode == 1, f"exit {cp.returncode}")
        # Through MCP: cache off, the gate still queues (documented).
        (t7c / "flag.txt").write_text("ok\n")
        sb.git(t7c, "add", "-A")
        sb.run(["sluice", "daemon"], t7c)  # bring the daemon up so the totals below are comparable
        wait_until(lambda: sb.daemon_pid(t7c) is not None, 10)
        ag = McpAgent(sb.env(agent=True, extra={"SLUICE_DISABLE": "1"}), t7c, name="disable")
        n0 = counter_lines(counter, "real")
        runs0 = int(sb.totals(t7c).get("runs", 0))
        r1 = ag.verify(t7c, ["real"])
        r2 = ag.verify(t7c, ["real"])
        ag.close()
        runs1 = int(sb.totals(t7c).get("runs", 0))
        rep.check("7: SLUICE_DISABLE=1 through MCP: cache off (runs twice) but the gate still goes through the daemon",
                  counter_lines(counter, "real") - n0 == 2 and runs1 - runs0 == 2 and not r1["is_error"] and not r2["is_error"],
                  f"executed {counter_lines(counter, 'real') - n0}, daemon runs +{runs1 - runs0}")
    finally:
        left = clean.settle()
        rep.check("no daemon or gate process left behind", not left, f"{left}")
        for s in clean.sandboxes:
            s.close()
        rep.save()
    dur = time.monotonic() - t_start
    rep.check("scenario finished in under 5 minutes", dur < 300, f"{dur:.0f}s")
    rep.save()
    print(f"s3_correctness: {len(rep.results) - rep.failed}/{len(rep.results)} checks passed")
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
