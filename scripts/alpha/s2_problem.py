"""Scenario 2: the original problem, measured.

N simulated agents, each in its own worktree of a real Node project
(sindresorhus/slugify), follow the same workflow at the same time:

  0. run the tests on the fresh checkout before touching anything
     (every agent sees the same tree);
  1. K iterations of: edit, verify (lint + test), commit (pre-commit: lint);
  2. push (pre-push: test).

One agent introduces a failing change in its last iteration: its verify
must fail, its commit must be refused and nobody else may be affected.

The workflow runs twice on identical repositories:
  * baseline: verify runs the commands directly and the hooks run them too;
  * sluice:   verify is the MCP `verify` tool (agents speak MCP over stdio,
              no language model), hooks call `sluice run --hook ...`.

The `test` gate is the project's real `ava` suite followed by a CPU load
(`.alpha/alpha-burn.js`, THREADS worker threads for SECONDS each) that
stands in for the heavy suites of larger projects; the load is synthetic and
reported as such. Measured: gate executions, wall time, peak concurrent
heavy suites (sampled from /proc), peak 1-minute load average.
"""

from __future__ import annotations

import os
import sys
import threading
import time
from pathlib import Path

from lib import Report, Sandbox, add_worktree, clone_project, link_node_modules, loadavg1, procs_matching
from mcp_agent import McpAgent

TB = Path(os.environ["ALPHA_TESTBEDS"])
AGENTS = int(os.environ.get("ALPHA_AGENTS", "8"))
ITER = int(os.environ.get("ALPHA_ITER", "2"))
THREADS = int(os.environ.get("ALPHA_BURN_THREADS", "4"))
SECONDS = float(os.environ.get("ALPHA_BURN_SECONDS", "6"))
BROKEN_AGENT = 0

BURN_JS = """// Synthetic CPU load standing in for a heavy test suite (alpha harness).
// Work-bounded like a real suite: each thread performs a fixed number of
// iterations, so CPU contention makes it take longer instead of doing less.
import {Worker, isMainThread} from 'node:worker_threads';
if (isMainThread) {
	const [threads, iterations] = process.argv.slice(2).map(Number);
	const workers = Array.from({length: threads}, () => new Worker(new URL(import.meta.url), {argv: [iterations]}));
	await Promise.all(workers.map(w => new Promise(r => w.on('exit', r))));
} else {
	const n = Number(process.argv[2]);
	let x = 0;
	for (let i = 0; i < n; i++) { x += Math.sqrt(x + i); }
	if (x < 0) console.log(x);
}
"""


def calibrate(seconds: float) -> int:
    """Iterations one idle core runs in `seconds` (measured over 1 s)."""
    import subprocess as sp
    code = ("let x=0,i=0;const e=Date.now()+1000;"
            "while(Date.now()<e){for(let k=0;k<1e5;k++,i++){x+=Math.sqrt(x+i);}}console.log(i)")
    per_second = int(sp.run(["node", "-e", code], capture_output=True, text=True, check=True).stdout)
    return int(per_second * seconds)


ITERATIONS = calibrate(SECONDS)

TEST_CMD = f"echo run >> \"$ALPHA_COUNTER\" && npx --no-install ava && node .alpha/alpha-burn.js {THREADS} {ITERATIONS}"
LINT_CMD = "npx --no-install xo"

SLUICE_TOML = f"""version = 1

[gates.lint]
cmd = "{LINT_CMD}"
weight = 1
timeout = "5m"
hooks = ["pre-commit"]
key_inputs = ["node --version"]

[gates.test]
cmd = "{TEST_CMD.replace('"', '\\"')}"
env = {{ ALPHA_COUNTER = "@COUNTER@" }}
weight = {THREADS}
timeout = "15m"
hooks = ["pre-push"]
key_inputs = ["node --version"]
"""


def prepare(sb: Sandbox, mode: str) -> tuple[Path, list[Path]]:
    repo = clone_project(sb, TB / "node-slugify", f"node-{mode}")
    link_node_modules(TB / "node-slugify" / "node_modules", repo)
    (repo / ".alpha").mkdir()
    (repo / ".alpha" / "alpha-burn.js").write_text(BURN_JS)
    hooks = repo / ".githooks"
    hooks.mkdir()
    counter = sb.root / "suite-runs"
    counter.write_text("")
    if mode == "sluice":
        (repo / ".sluice.toml").write_text(SLUICE_TOML.replace("@COUNTER@", str(counter)))
        (hooks / "pre-commit").write_text("#!/bin/sh\nexec sluice run --hook pre-commit lint\n")
        (hooks / "pre-push").write_text("#!/bin/sh\nexec sluice run --hook pre-push test\n")
    else:
        (hooks / "pre-commit").write_text(f"#!/bin/sh\n{LINT_CMD}\n")
        (hooks / "pre-push").write_text(f"#!/bin/sh\nexport ALPHA_COUNTER='{counter}'\n{TEST_CMD}\n")
    for h in hooks.iterdir():
        h.chmod(0o755)
    # xo lints every .js file: keep the synthetic load out of it.
    import json as _json
    pkg = repo / "package.json"
    data = _json.loads(pkg.read_text())
    data.setdefault("xo", {})["ignores"] = [".alpha"]
    pkg.write_text(_json.dumps(data, indent="\t") + "\n")
    sb.git(repo, "add", "-A")
    sb.run(["git", "-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", "chore: alpha bench setup"], repo, check=True)
    sb.run(["git", "push", "-q", "--no-verify", "origin", "HEAD"], repo, check=True)
    sb.git(repo, "config", "core.hooksPath", ".githooks")
    wts = [add_worktree(sb, repo, f"{mode}-{i}", f"agent/{i}") for i in range(AGENTS)]
    for wt in wts:
        link_node_modules(TB / "node-slugify" / "node_modules", wt)
        sb.git(wt, "config", "core.hooksPath", str(hooks))
    return repo, wts


def burn_pids() -> list[int]:
    """Main processes of running heavy suites: argv = node .alpha/alpha-burn.js."""
    out = []
    for p in procs_matching("alpha-burn.js"):
        try:
            argv = Path(f"/proc/{p}/cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if len(argv) > 1 and Path(argv[0].decode()).name == "node" and argv[1].endswith(b"alpha-burn.js"):
            out.append(p)
    return out


class Sampler(threading.Thread):
    def __init__(self) -> None:
        super().__init__(daemon=True)
        self.stop = threading.Event()
        self.peak_suites = 0
        self.peak_load = 0.0

    def run(self) -> None:
        while not self.stop.is_set():
            # One main process per running heavy suite (`node .alpha/alpha-burn.js`).
            n = len(burn_pids())
            self.peak_suites = max(self.peak_suites, n)
            self.peak_load = max(self.peak_load, loadavg1())
            time.sleep(0.1)


def agent_workflow(sb: Sandbox, mode: str, idx: int, wt: Path, log: dict) -> None:
    rec = log.setdefault(idx, {"verify_s": [], "events": []})
    agent = McpAgent(sb.env(agent=True), wt, name=f"a{idx}") if mode == "sluice" else None

    def verify() -> bool:
        t = time.monotonic()
        if agent:
            res = agent.verify(wt, ["lint", "test"], timeout=1800)
            ok = not res["is_error"] and (res["structured"] or {}).get("all_passed") is True
            if not ok:
                tails = [(r.get("gate"), r.get("status"), "\n".join(r.get("tail") or [])[-500:]) for r in (res["structured"] or {}).get("results", [])]
                rec.setdefault("verify_errors", []).append(str(tails) or res["text"][-500:])
        else:
            cp = sb.run(f"{LINT_CMD} && {TEST_CMD}", wt, agent=True, timeout=1800, extra={"ALPHA_COUNTER": str(sb.root / "suite-runs")})
            ok = cp.returncode == 0
            if not ok:
                rec.setdefault("verify_errors", []).append((cp.stdout + cp.stderr)[-600:])
        rec["verify_s"].append(time.monotonic() - t)
        return ok

    rec["events"].append(("verify-base", verify()))
    for k in range(ITER):
        note = wt / "notes" / f"agent-{idx}.md"
        note.parent.mkdir(exist_ok=True)
        note.write_text(f"agent {idx} iteration {k}\n")
        broken = idx == BROKEN_AGENT and k == ITER - 1
        if broken:
            p = wt / "index.js"
            p.write_text(p.read_text().replace("\treturn string;\n}", "\treturn string + '-alpha';\n}", 1))
        sb.git(wt, "add", "-A")
        ok = verify()
        rec["events"].append((f"verify-{k}", ok))
        # The pre-commit gate is lint only, so even the broken change commits.
        cp = sb.run(["git", "commit", "-q", "-m", f"feat: agent {idx} iteration {k}"], wt, agent=True, timeout=1800)
        rec["events"].append((f"commit-{k}", cp.returncode == 0))
    cp = sb.run(["git", "push", "-q", "origin", f"agent/{idx}"], wt, agent=True, timeout=1800)
    rec["events"].append(("push", cp.returncode == 0))
    if cp.returncode != 0:
        rec["push_stderr"] = cp.stderr[-400:]
    if idx == BROKEN_AGENT:
        rec["broken_push_stderr"] = cp.stderr[-400:]
    if agent:
        agent.close()


def cool_down(limit: float = 2.0, timeout: float = 300) -> float:
    """Wait until the 1-minute load average drops below `limit`."""
    end = time.monotonic() + timeout
    while loadavg1() > limit and time.monotonic() < end:
        time.sleep(2)
    return loadavg1()


def run_mode(mode: str) -> dict:
    start_load = cool_down()
    sb = Sandbox(f"problem-{mode}")
    try:
        if mode == "sluice":
            sb.write_user_config("[mcp]\nprogress_interval = \"5s\"\n")
        repo, wts = prepare(sb, mode)
        # The test command writes nothing that git tracks; count executions
        # from the processes that ran (one `alpha-burn` main per suite).
        counter = sb.root / "suite-count"
        counter.write_text("")
        wrapper_note = "count via /proc sampling of distinct burn pids"
        seen: set[int] = set()
        stop = threading.Event()

        def count_suites() -> None:
            while not stop.is_set():
                seen.update(burn_pids())
                time.sleep(0.05)

        sampler = Sampler()
        counter_t = threading.Thread(target=count_suites, daemon=True)
        sampler.start()
        counter_t.start()
        log: dict = {}
        threads = [threading.Thread(target=agent_workflow, args=(sb, mode, i, wt, log)) for i, wt in enumerate(wts)]
        t0 = time.monotonic()
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        wall = time.monotonic() - t0
        sampler.stop.set()
        stop.set()
        totals = sb.totals(repo) if mode == "sluice" else {}
        return {
            "mode": mode,
            "wall_s": wall,
            "heavy_suite_executions": len((sb.root / "suite-runs").read_text().splitlines()),
            "burn_processes_seen": len(seen),
            "peak_concurrent_suites": sampler.peak_suites,
            "peak_load1": sampler.peak_load,
            "totals": totals,
            "log": log,
            "note": wrapper_note,
            "start_load1": start_load,
            "remote_branches": sb.git(repo, "ls-remote", "--heads", "origin").count("refs/heads/agent/"),
            "verify_errors": {i: rec.get("verify_errors") for i, rec in log.items() if rec.get("verify_errors")},
        }
    finally:
        sb.close()


def main() -> int:
    out = Path(os.environ.get("ALPHA_OUT", "."))
    rep = Report("s2_problem", out)
    print(f"== {AGENTS} agents x {ITER} iterations, heavy suite = ava + {THREADS} threads x {ITERATIONS} iterations "
          f"(~{SECONDS}s on an idle core)", flush=True)
    results = {}
    order = ("sluice", "baseline") if os.environ.get("ALPHA_SLUICE_FIRST") else ("baseline", "sluice")
    for mode in order:
        print(f"-- {mode}", flush=True)
        results[mode] = run_mode(mode)
        r = results[mode]
        print(f"   wall {r['wall_s']:.1f}s, heavy suites {r['heavy_suite_executions']}, peak concurrent {r['peak_concurrent_suites']}, "
              f"peak load {r['peak_load1']:.1f}, totals {r['totals']}", flush=True)
    b, s = results["baseline"], results["sluice"]
    expected_base = AGENTS * (ITER + 2)  # base verify + K verifies + push (the broken push runs and fails)
    rep.check("baseline runs the heavy suite for every request", b["heavy_suite_executions"] == expected_base,
              f"{b['heavy_suite_executions']} (expected {expected_base})")
    # Sluice: the base verify of all agents is one tree (1 run); each
    # iteration's verify is a distinct tree; a healthy push tree was verified
    # (hit) and the broken push tree has a failure record (fail-fast, no run).
    expected_sluice = 1 + AGENTS * ITER
    rep.check("sluice runs each distinct tree once", s["heavy_suite_executions"] == expected_sluice,
              f"{s['heavy_suite_executions']} (expected {expected_sluice})", totals=s["totals"])
    saved = 1 - s["heavy_suite_executions"] / max(1, b["heavy_suite_executions"])
    rep.check("sluice executes fewer heavy suites", s["heavy_suite_executions"] < b["heavy_suite_executions"], f"{saved:.0%} fewer")
    rep.check("sluice keeps concurrent heavy suites within the budget", s["peak_concurrent_suites"] <= max(1, (os.cpu_count() - 1) // THREADS),
              f"peak {s['peak_concurrent_suites']} vs baseline {b['peak_concurrent_suites']}")
    rep.check("sluice wall time is not worse than baseline", s["wall_s"] <= b["wall_s"] * 1.05,
              f"{s['wall_s']:.1f}s vs {b['wall_s']:.1f}s")
    for mode, r in results.items():
        log = r["log"]
        good = all(ok for i, rec in log.items() if i != BROKEN_AGENT for _, ok in rec["events"]) and len(log) == AGENTS
        rep.check(f"{mode}: every healthy agent verified, committed and pushed", good,
                  "; ".join(f"a{i}: {[e for e, ok in rec['events'] if not ok]}" for i, rec in log.items() if i != BROKEN_AGENT and not all(ok for _, ok in rec["events"])),
                  push_errors={i: rec.get("push_stderr", "") for i, rec in log.items()})
        broken = dict(log[BROKEN_AGENT]["events"])
        rep.check(f"{mode}: the broken change failed verify and its push was refused",
                  broken.get(f"verify-{ITER - 1}") is False and broken.get("push") is False,
                  str(log[BROKEN_AGENT]["events"]), stderr=log[BROKEN_AGENT].get("broken_push_stderr", ""))
    tot = s["totals"]
    rep.check("sluice coalesced the identical base-tree requests",
              int(tot.get("coalesced", 0)) + int(tot.get("hits", 0)) >= AGENTS - 1, f"coalesced {tot.get('coalesced')}, hits {tot.get('hits')}")
    rep.check("results recorded", True, "", results={k: {kk: vv for kk, vv in v.items() if kk != "log"} for k, v in results.items()},
              verify_latency={k: {i: rec["verify_s"] for i, rec in v["log"].items()} for k, v in results.items()})
    rep.save()
    print(f"s2_problem: {len(rep.results) - rep.failed}/{len(rep.results)} checks passed")
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
