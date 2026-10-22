"""Scenario 7: command control (guard rewrite, `sluice exec`, shell shims).

Agents are simulated; no AI session is involved. Ten "agents" work in ten
worktrees of the Node test bed (sindresorhus/slugify) and issue heavy
build-like commands, through the guard path and through shell shims:

  A. Guard path, exactly as the agent client does it: build the PreToolUse
     hook JSON (`tool_name: Bash`, `tool_input.command`, `cwd`,
     `permission_mode`), pipe it into `sluice guard --hook` with
     CLAUDE_PROJECT_DIR set and the settings files in place, parse the
     decision and, when it rewrites, run `updatedInput.command` with `sh -c`
     in that cwd. Once in bypassPermissions mode (allow) and once in default
     mode (ask: a human would have been asked; the command is run anyway).
     Heavy commands: `npm run build` and a fake `docker build`, both a
     work-bounded CPU load that writes `dist/out-<agent>.txt`.
  B. A command equal to a gate's cmd on an already verified tree is rewritten
     to `sluice run <gate> -v` and answered from the cache.
  C. Refusals: pipelines, `$(...)`, heredocs, `sluice queue approve`, deny
     rules, plan mode, SLUICE_GUARD=0.
  D. Shim variant: `sluice shim install`, shim dir first in PATH, ten
     concurrent `make build`; with SLUICE_SHIMS=1 they are queued, with
     SLUICE_SHIMS unset and no agent marker they run directly.
  E. Baseline: the same ten commands raw (no guard, no shims), run first.

The load is synthetic (like s2) and work-bounded: a fixed number of
iterations, so contention makes it slower instead of making it do less.
Budget 8, `[agent] exec_weight = 4`: at most 2 concurrent loads. PSI admission
is disabled so the bound under test is the budget alone.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess as sp
import sys
import threading
import time
from pathlib import Path

from lib import Report, Sandbox, add_worktree, clone_project, link_node_modules
from pygate import Cleanup

TB = Path(os.environ["ALPHA_TESTBEDS"])
AGENTS = int(os.environ.get("ALPHA_AGENTS", "10"))
SECONDS = float(os.environ.get("ALPHA_BURN_SECONDS", "3"))
BUDGET = 8
WEIGHT = 4
LIMIT = BUDGET // WEIGHT
FAIL_NPM = AGENTS - 2  # its `npm run build` fails on purpose (A, E)
FAIL_DOCKER = AGENTS - 1  # its `docker build` / `make build` fails on purpose
FAIL_CODE = 7
TOKEN = f"alpha-s7-{os.getpid()}"

BUILD_JS = """// Build-like load of the alpha harness: fixed work, a marker line, an artifact.
const fs = require('node:fs');
const kind = process.argv[2] || 'build';
const agent = process.env.ALPHA_AGENT || '0';
const iterations = Number(process.env.ALPHA_ITERS || process.argv[3] || 0);
let x = 0;
for (let i = 0; i < iterations; i++) { x += Math.sqrt(x + i); }
if (x < 0) console.log(x);
if (process.env.ALPHA_FAIL === '1') {
	console.log(`BUILD-MARKER agent=${agent} kind=${kind} FAILED`);
	console.error(`build failed (agent ${agent})`);
	process.exit(%(code)d);
}
fs.mkdirSync('dist', {recursive: true});
fs.writeFileSync(`dist/out-${agent}.txt`, `${kind} agent=${agent} pid=${process.pid}\\n`);
console.log(`BUILD-MARKER agent=${agent} kind=${kind} OK`);
""" % {"code": FAIL_CODE}

CHECK_JS = """// Gate body: one line per real execution.
const fs = require('node:fs');
fs.appendFileSync(process.env.ALPHA_COUNTER, `${process.pid}\\n`);
console.log('CHECK-RAN');
"""


def calibrate(seconds: float) -> int:
    code = ("let x=0,i=0;const e=Date.now()+1000;"
            "while(Date.now()<e){for(let k=0;k<1e5;k++,i++){x+=Math.sqrt(x+i);}}console.log(i)")
    per_second = int(sp.run(["node", "-e", code], capture_output=True, text=True, check=True).stdout)
    return int(per_second * seconds)


def lines(path: Path) -> int:
    try:
        return len(path.read_text().splitlines())
    except OSError:
        return 0


class Sampler(threading.Thread):
    """Peak number of concurrently running loads: node processes whose argv[1]
    is this scenario's build script."""

    def __init__(self, script: str) -> None:
        super().__init__(daemon=True)
        self.script = script.encode()
        self.halt = threading.Event()
        self.peak = 0
        self.seen: set[int] = set()

    def count(self) -> int:
        pids = []
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                argv = (entry / "cmdline").read_bytes().split(b"\0")
            except OSError:
                continue
            if len(argv) > 1 and argv[1] == self.script and Path(argv[0].decode(errors="replace")).name.startswith("node"):
                pids.append(int(entry.name))
        self.seen.update(pids)
        return len(pids)

    def run(self) -> None:
        while not self.halt.is_set():
            self.peak = max(self.peak, self.count())
            time.sleep(0.02)


def hook_json(cwd: Path, command: str, mode: str) -> str:
    return json.dumps({
        "session_id": "alpha-s7",
        "transcript_path": "/dev/null",
        "cwd": str(cwd),
        "permission_mode": mode,
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command, "description": "alpha s7", "timeout": 600000, "run_in_background": False},
    })


class World:
    """The sandbox plus everything the scenario shares."""

    def __init__(self, sb: Sandbox) -> None:
        self.sb = sb
        self.tools = sb.root / "tools"
        self.fakebin = sb.root / "fakebin"
        self.shims = sb.root / "shims"
        self.build_js = str(self.tools / f"{TOKEN}-build.js")
        self.base_path = os.environ.get("PATH", "/usr/bin:/bin")

    def path(self, shims: bool = False) -> str:
        parts = ([str(self.shims)] if shims else []) + [str(self.sb.bin), str(self.fakebin), self.base_path]
        return ":".join(parts)

    def env(self, agent_id: int | None, *, agent: bool = True, shims: bool = False, project: Path | None = None,
            extra: dict | None = None) -> dict:
        e = self.sb.env(agent=agent, extra={"PATH": self.path(shims)})
        if agent_id is not None:
            e["ALPHA_AGENT"] = str(agent_id)
        if project is not None:
            e["CLAUDE_PROJECT_DIR"] = str(project)
        if extra:
            e.update(extra)
        return e

    # -- the guard, as the agent client calls it ------------------------------
    def guard(self, cwd: Path, command: str, mode: str, project: Path, extra: dict | None = None) -> dict:
        env = self.env(None, project=project, extra=extra)
        cp = sp.run(["sluice", "guard", "--hook"], input=hook_json(cwd, command, mode), cwd=cwd, env=env,
                    capture_output=True, text=True, timeout=30)
        decision, new_command, reason = None, None, ""
        out = cp.stdout.strip()
        if out:
            try:
                h = json.loads(out)["hookSpecificOutput"]
                decision = h.get("permissionDecision")
                new_command = (h.get("updatedInput") or {}).get("command")
                reason = h.get("permissionDecisionReason", "")
            except (ValueError, KeyError):
                decision = "unparseable"
        return {"rc": cp.returncode, "decision": decision, "command": new_command, "reason": reason,
                "raw": out, "stderr": cp.stderr.strip()}

    def sh(self, command: str, cwd: Path, env: dict, timeout: float = 600) -> dict:
        cp = sp.run(["sh", "-c", command], cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout,
                    stdin=sp.DEVNULL)
        return {"rc": cp.returncode, "out": cp.stdout, "err": cp.stderr}


def prepare(sb: Sandbox, iterations: int) -> tuple[World, Path, list[Path]]:
    w = World(sb)
    for d in (w.tools, w.fakebin):
        d.mkdir()
    Path(w.build_js).write_text(BUILD_JS)
    (w.tools / "check.js").write_text(CHECK_JS)
    # Fake docker and make: the build-like load, only for `build`.
    (w.fakebin / "docker").write_text(
        f'#!/bin/sh\nif [ "$1" = build ]; then exec node {w.build_js} docker {iterations}; fi\n'
        'echo "fake docker: $*"\n')
    (w.fakebin / "make").write_text(
        f'#!/bin/sh\nif [ "$1" = build ]; then exec node {w.build_js} make {iterations}; fi\n'
        'echo "fake make: $*"\n')
    for f in w.fakebin.iterdir():
        f.chmod(0o755)

    repo = clone_project(sb, TB / "node-slugify", "node-s7")
    link_node_modules(TB / "node-slugify" / "node_modules", repo)
    pkg = repo / "package.json"
    data = json.loads(pkg.read_text())
    data.setdefault("scripts", {}).update({
        "build": f"node {w.build_js} npm {iterations}",
        "check": f"node {w.tools / 'check.js'}",
    })
    pkg.write_text(json.dumps(data, indent="\t") + "\n")
    (repo / ".gitignore").write_text("node_modules\ndist\n")
    (repo / ".claude").mkdir()
    (repo / ".claude" / "settings.json").write_text(json.dumps({"permissions": {"allow": []}}))
    counter = sb.root / "gate-count"
    counter.write_text("")
    (repo / ".sluice.toml").write_text(f"""version = 1

[gates.check]
cmd = "npm run check"
weight = 1
timeout = "2m"
env = {{ ALPHA_COUNTER = "{counter}" }}
""")
    sb.git(repo, "add", "-A")
    sb.run(["git", "-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", "chore: s7 setup"], repo, check=True)
    sb.run(["git", "push", "-q", "--no-verify", "origin", "HEAD"], repo, check=True)
    wts = [add_worktree(sb, repo, f"s7-{i}", f"agent/{i}") for i in range(AGENTS)]
    for wt in wts:
        link_node_modules(TB / "node-slugify" / "node_modules", wt)
    # The user's own Claude settings: no rules.
    (sb.home / ".claude").mkdir(exist_ok=True)
    (sb.home / ".claude" / "settings.json").write_text(json.dumps({"permissions": {"allow": []}}))
    return w, repo, wts


def command_for(i: int) -> str:
    return "npm run build" if i % 2 == 0 else f"docker build -t alpha-agent-{i} ."


def kind_of(command: str) -> str:
    return "npm" if command.startswith("npm") else "docker"


def clean_dist(wts: list[Path]) -> None:
    for wt in wts:
        shutil.rmtree(wt / "dist", ignore_errors=True)


def fail_env(i: int, kind: str) -> dict:
    return {"ALPHA_FAIL": "1"} if (kind == "npm" and i == FAIL_NPM) or (kind == "docker" and i == FAIL_DOCKER) else {}


def run_agents(w: World, wts: list[Path], runner) -> tuple[list[dict], float, Sampler]:
    """Run `runner(i, wt)` for every agent at once; sampled peak and wall time."""
    sampler = Sampler(w.build_js)
    sampler.start()
    results: list[dict] = [{} for _ in wts]

    def work(i: int) -> None:
        results[i] = runner(i, wts[i])

    threads = [threading.Thread(target=work, args=(i,)) for i in range(len(wts))]
    t0 = time.monotonic()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.monotonic() - t0
    sampler.halt.set()
    sampler.join()
    return results, wall, sampler


def raw_rc(w: World, wt: Path, command: str, i: int) -> int:
    """The raw exit code of a failing command (tiny load), for comparison."""
    return w.sh(command, wt, w.env(i, extra={"ALPHA_FAIL": "1", "ALPHA_ITERS": "1000"}))["rc"]


def guard_phase(rep: Report, w: World, repo: Path, wts: list[Path], mode: str) -> dict:
    sb = w.sb
    want = "allow" if mode == "bypassPermissions" else "ask"
    tag = f"A[{mode}]"
    clean_dist(wts)
    ref_npm = raw_rc(w, wts[FAIL_NPM], "npm run build", FAIL_NPM)
    ref_docker = raw_rc(w, wts[FAIL_DOCKER], command_for(FAIL_DOCKER), FAIL_DOCKER)
    runs0 = int(sb.totals(repo).get("runs", 0))

    def runner(i: int, wt: Path) -> dict:
        cmd = command_for(i)
        kind = kind_of(cmd)
        g = w.guard(wt, cmd, mode, project=wt)
        final = g["command"] or cmd  # the client runs the original when nothing is returned
        r = w.sh(final, wt, w.env(i, project=wt, extra=fail_env(i, kind)))
        return {**g, "guard_rc": g["rc"], "original": cmd, **r}

    results, wall, sampler = run_agents(w, wts, runner)
    runs1 = int(sb.totals(repo).get("runs", 0))

    rewritten = [r for r in results if r["command"] and r["command"].startswith("sluice exec --label ") and r["original"] in r["command"]]
    rep.check(f"{tag} every heavy command was rewritten to `sluice exec`", len(rewritten) == AGENTS,
              f"{len(rewritten)}/{AGENTS}; e.g. {results[0]['command']!r}, {results[1]['command']!r}; not rewritten: "
              f"{[(r['original'], r['raw'][:80], r['stderr'][:80]) for r in results if r not in rewritten][:2]}")
    rep.check(f"{tag} decision is {want} for every rewrite", all(r["decision"] == want for r in results),
              f"decisions {sorted(set(str(r['decision']) for r in results))}")
    if want == "ask":
        asked = sum(1 for r in results if r["decision"] == "ask" and r["reason"])
        rep.check(f"{tag} a human would have been asked {AGENTS} times (each ask carries a reason); executed here anyway",
                  asked == AGENTS, f"{asked} asks; sample reason: {results[0]['reason'][:120]!r}")
    odd = [(r["original"], r["guard_rc"], r["raw"].count("\n"), r["stderr"][:80]) for r in results if r["guard_rc"] != 0 or "\n" in r["raw"] or not r["raw"]]
    rep.check(f"{tag} the guard exits 0 and prints exactly one JSON line", not odd, f"odd: {odd[:2]}")
    rep.check(f"{tag} peak concurrent loads <= budget/weight ({BUDGET}/{WEIGHT} = {LIMIT})",
              1 <= sampler.peak <= LIMIT, f"peak {sampler.peak}, distinct load processes {len(sampler.seen)}, wall {wall:.1f}s",
              peak=sampler.peak, wall_s=wall)
    healthy = [i for i in range(AGENTS) if i not in (FAIL_NPM, FAIL_DOCKER)]
    missing = [i for i in healthy if not (wts[i] / "dist" / f"out-{i}.txt").is_file()]
    rep.check(f"{tag} every artifact dist/out-<agent>.txt exists after its run", not missing, f"missing: {missing}")
    rep.check(f"{tag} failed builds left no artifact",
              not (wts[FAIL_NPM] / "dist" / f"out-{FAIL_NPM}.txt").exists() and not (wts[FAIL_DOCKER] / "dist" / f"out-{FAIL_DOCKER}.txt").exists())
    bad = [i for i in healthy if results[i]["rc"] != 0]
    rep.check(f"{tag} healthy builds exit 0", not bad, f"non-zero: {[(i, results[i]['rc'], results[i]['err'][-120:]) for i in bad]}")
    rep.check(f"{tag} exit code of a failing docker build propagates ({FAIL_CODE})",
              results[FAIL_DOCKER]["rc"] == ref_docker == FAIL_CODE, f"guarded {results[FAIL_DOCKER]['rc']}, raw {ref_docker}")
    rep.check(f"{tag} exit code of a failing npm run build equals the raw one",
              results[FAIL_NPM]["rc"] == ref_npm != 0, f"guarded {results[FAIL_NPM]['rc']}, raw {ref_npm}")
    no_marker = [i for i in range(AGENTS) if f"BUILD-MARKER agent={i} kind=" not in results[i]["out"]]
    rep.check(f"{tag} output is replayed (stdout holds the build's marker line)", not no_marker, f"without marker: {no_marker}")
    rep.check(f"{tag} the failing builds' stderr is replayed",
              f"build failed (agent {FAIL_DOCKER})" in results[FAIL_DOCKER]["err"] and f"build failed (agent {FAIL_NPM})" in results[FAIL_NPM]["err"])
    rep.check(f"{tag} every command went through the daemon queue (status totals)", runs1 - runs0 == AGENTS, f"runs +{runs1 - runs0}")
    return {"peak": sampler.peak, "wall_s": wall}


def gate_phase(rep: Report, w: World, repo: Path) -> None:
    sb = w.sb
    counter = sb.root / "gate-count"
    cp = sb.run(["sluice", "run", "check"], repo, agent=False)
    rep.check("B: the gate verifies the tree once (sluice run check)", cp.returncode == 0 and lines(counter) == 1,
              f"exit {cp.returncode}, executions {lines(counter)}")
    t0 = sb.totals(repo)
    g = w.guard(repo, "npm run check", "bypassPermissions", project=repo)
    rep.check("B: `npm run check` (the gate's cmd) is rewritten to `sluice run check -v`",
              g["command"] == "sluice run check -v" and g["decision"] == "allow", f"{g['decision']} {g['command']!r}")
    r = w.sh(g["command"] or "npm run check", repo, w.env(0, project=repo))
    t1 = sb.totals(repo)
    rep.check("B: answered from the cache: no new execution, no new run in status totals, one more hit",
              lines(counter) == 1 and int(t1.get("runs", 0)) == int(t0.get("runs", 0))
              and int(t1.get("hits", 0)) == int(t0.get("hits", 0)) + 1 and r["rc"] == 0,
              f"exit {r['rc']}, executions {lines(counter)}, runs {t0.get('runs')}->{t1.get('runs')}, "
              f"hits {t0.get('hits')}->{t1.get('hits')}, stderr tail {r['err'].strip().splitlines()[-1:]!r}")
    g2 = w.guard(repo, "npm run check", "default", project=repo)
    rep.check("B: in default mode the gate rewrite is an ask", g2["decision"] == "ask" and g2["command"] == "sluice run check -v",
              f"{g2['decision']} {g2['command']!r}")
    # A changed tree is a new tree: the same command runs again.
    (repo / "notes.txt").write_text("changed tree\n")
    sb.git(repo, "add", "-A")
    g3 = w.guard(repo, "npm run check", "bypassPermissions", project=repo)
    r3 = w.sh(g3["command"] or "npm run check", repo, w.env(0, project=repo))
    rep.check("B: after the tree changes the same command runs the gate again", lines(counter) == 2 and r3["rc"] == 0,
              f"executions {lines(counter)}, exit {r3['rc']}")
    sb.git(repo, "reset", "-q", "--hard", "HEAD")


def refusal_phase(rep: Report, w: World, wt: Path) -> None:
    alt = w.sb.root / "denyproject"
    (alt / ".claude").mkdir(parents=True)
    (alt / ".claude" / "settings.json").write_text(json.dumps({"permissions": {"deny": ["Bash(npm run build:*)"]}}))
    bp = "bypassPermissions"
    cases = [
        ("pipeline: a heavy command that is not first is not rewritten", "echo x | npm run build", bp, wt, None),
        ("command substitution `$(...)` is not rewritten", "echo $(npm run build)", bp, wt, None),
        ("`$(...)` as an argument is not rewritten", "docker build -t x $(cat tag) .", bp, wt, None),
        ("backticks are not rewritten", "echo `npm run build`", bp, wt, None),
        ("heredoc is not rewritten", "cat <<EOF | docker build -\nFROM scratch\nEOF", bp, wt, None),
        ("input redirection `docker build - < Dockerfile` is not rewritten", "docker build - < Dockerfile", bp, wt, None),
        ("`sh -c` wrapper is not looked into", "sh -c 'npm run build'", bp, wt, None),
        ("background `&` is not rewritten", "npm run build &", bp, wt, None),
        ("an already wrapped command is left alone", "sluice exec -- make build", bp, wt, None),
        ("a deny rule on the original blocks the rewrite (normal flow decides)", "npm run build", bp, alt, None),
        ("plan mode: no rewrite", "npm run build", "plan", wt, None),
        ("SLUICE_GUARD=0 disables the guard", "npm run build", bp, wt, {"SLUICE_GUARD": "0"}),
        ("a light docker subcommand is not rewritten", "docker ps", bp, wt, None),
    ]
    for name, command, mode, project, extra in cases:
        g = w.guard(wt, command, mode, project=project, extra=extra)
        rep.check(f"C: {name}", g["decision"] is None and g["command"] is None and g["rc"] == 0,
                  f"decision {g['decision']!r}, output {g['raw'][:100]!r}")
    g = w.guard(wt, "npm run build | tail -3", bp, project=wt)
    rep.check("C: a heavy command first in a pipeline is rewritten (stdout stays the command's)",
              g["decision"] == "allow" and (g["command"] or "").startswith("sluice exec --label run-build -- npm run build | tail -3"),
              f"{g['decision']} {g['command']!r}")
    for command in ("sluice queue approve t-1 --yes", "true && sluice queue approve t-1", "echo go; sluice queue approve t-1 --yes"):
        for mode in (bp, "default"):
            g = w.guard(wt, command, mode, project=wt)
            rep.check(f"C: `{command}` is denied in {mode} mode", g["decision"] == "deny" and g["command"] is None,
                      f"{g['decision']!r} {g['reason'][:80]!r}")


def shim_phase(rep: Report, w: World, repo: Path, wts: list[Path]) -> dict:
    sb = w.sb
    cp = sb.run(["sluice", "shim", "install", "--dir", str(w.shims)], repo)
    names = sorted(p.name for p in w.shims.iterdir()) if w.shims.is_dir() else []
    rep.check("D: `sluice shim install --dir` writes the shims", cp.returncode == 0 and {"make", "docker", "npm"} <= set(names),
              f"exit {cp.returncode}, {len(names)} shims: {names[:6]}...")
    numbers: dict = {}
    for variant, queued in (("SLUICE_SHIMS=1", True), ("SLUICE_SHIMS unset, no agent marker", False)):
        tag = f"D[{variant}]"
        clean_dist(wts)
        t0 = sb.totals(repo)

        def runner(i: int, wt: Path, queued=queued) -> dict:
            # No agent marker at all: `Sandbox.env(agent=False)` sets SLUICE_AGENT=0, drop it.
            env = w.env(i, agent=False, shims=True, extra={"SLUICE_SHIMS": "1"} if queued else None)
            env.pop("SLUICE_AGENT", None)
            if i == FAIL_DOCKER:
                env["ALPHA_FAIL"] = "1"
            return w.sh("make build", wt, env)

        results, wall, sampler = run_agents(w, wts, runner)
        t1 = sb.totals(repo)
        new_runs = int(t1.get("runs", 0)) - int(t0.get("runs", 0))
        healthy = [i for i in range(AGENTS) if i != FAIL_DOCKER]
        missing = [i for i in healthy if not (wts[i] / "dist" / f"out-{i}.txt").is_file()]
        rep.check(f"{tag} outputs exist", not missing, f"missing {missing}")
        rep.check(f"{tag} exit codes propagate", all(results[i]["rc"] == 0 for i in healthy) and results[FAIL_DOCKER]["rc"] == FAIL_CODE,
                  f"codes {[r['rc'] for r in results]}")
        rep.check(f"{tag} output is replayed (marker line)", all(f"BUILD-MARKER agent={i} kind=make" in results[i]["out"] for i in range(AGENTS)))
        if queued:
            rep.check(f"{tag} concurrency bounded: peak <= {LIMIT}", 1 <= sampler.peak <= LIMIT, f"peak {sampler.peak}, wall {wall:.1f}s")
            rep.check(f"{tag} every call is an exec job in status totals", new_runs == AGENTS, f"runs +{new_runs}")
        else:
            rep.check(f"{tag} runs directly: no exec job in status", new_runs == 0, f"runs +{new_runs}")
            rep.check(f"{tag} runs directly: the loads overlap (not queued), peak recorded", sampler.peak > LIMIT,
                      f"peak {sampler.peak}, wall {wall:.1f}s")
        numbers[variant] = {"peak": sampler.peak, "wall_s": wall}
    return numbers


def baseline_phase(rep: Report, w: World, wts: list[Path]) -> dict:
    clean_dist(wts)

    def runner(i: int, wt: Path) -> dict:
        cmd = command_for(i)
        return w.sh(cmd, wt, w.env(i, extra=fail_env(i, kind_of(cmd))))

    _, wall, sampler = run_agents(w, wts, runner)
    healthy = [i for i in range(AGENTS) if i not in (FAIL_NPM, FAIL_DOCKER)]
    rep.check("E: raw baseline ran every command (artifacts exist)", all((wts[i] / "dist" / f"out-{i}.txt").is_file() for i in healthy),
              f"peak {sampler.peak}, wall {wall:.1f}s", peak=sampler.peak, wall_s=wall)
    return {"peak": sampler.peak, "wall_s": wall}


def main() -> int:
    out = Path(os.environ.get("ALPHA_OUT", "."))
    rep = Report("s7_command_control", out)
    clean = Cleanup()
    clean.tokens.append(TOKEN)
    iterations = calibrate(SECONDS)
    cpus = os.cpu_count() or 1
    print(f"== {AGENTS} agents, load = {iterations} iterations (~{SECONDS}s on an idle core), budget {BUDGET}, "
          f"exec weight {WEIGHT} (limit {LIMIT} concurrent), {cpus} cpus", flush=True)
    sb = Sandbox("s7", base=Path(os.environ["TMPDIR"]) if os.environ.get("TMPDIR") else None)
    clean.sandboxes.append(sb)
    numbers: dict = {}
    try:
        sb.write_user_config(f"[daemon]\nbudget = {BUDGET}\n[admission]\npsi_enabled = false\n[agent]\nexec_weight = {WEIGHT}\n")
        w, repo, wts = prepare(sb, iterations)
        print("-- E: raw baseline (run first, on the idle machine)", flush=True)
        numbers["raw"] = baseline_phase(rep, w, wts)
        print("-- A: guard path", flush=True)
        numbers["guard_bypass"] = guard_phase(rep, w, repo, wts, "bypassPermissions")
        numbers["guard_default"] = guard_phase(rep, w, repo, wts, "default")
        print("-- B: gate through the guard", flush=True)
        gate_phase(rep, w, repo)
        print("-- C: refusals", flush=True)
        refusal_phase(rep, w, wts[0])
        print("-- D: shims", flush=True)
        numbers["shims"] = shim_phase(rep, w, repo, wts)
        on, off = numbers["shims"]["SLUICE_SHIMS=1"], numbers["shims"]["SLUICE_SHIMS unset, no agent marker"]
        rep.check("E: guarded peak <= budget/weight, next to the raw baseline",
                  max(numbers["guard_bypass"]["peak"], numbers["guard_default"]["peak"]) <= LIMIT,
                  f"raw peak {numbers['raw']['peak']} wall {numbers['raw']['wall_s']:.1f}s; "
                  f"guarded bypass peak {numbers['guard_bypass']['peak']} wall {numbers['guard_bypass']['wall_s']:.1f}s; "
                  f"guarded default peak {numbers['guard_default']['peak']} wall {numbers['guard_default']['wall_s']:.1f}s; "
                  f"shims=1 peak {on['peak']} wall {on['wall_s']:.1f}s; shims off peak {off['peak']} wall {off['wall_s']:.1f}s",
                  numbers=numbers, cpus=cpus, load_iterations=iterations)
    finally:
        left = clean.settle()
        rep.check("no daemon or load process left behind", not left, f"{left}")
        for s in clean.sandboxes:
            s.close()
        rep.save()
    print(f"s7_command_control: {len(rep.results) - rep.failed}/{len(rep.results)} checks passed")
    print("numbers:", json.dumps(numbers))
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
