"""Scenario 8: zero config (agent plugin session start, heavy commands, init).

Replays, on a fixture, the incident that motivated the zero-config plan
(`kb/sluice/plan/zero-config-plan.md`, sections 1 and 7): an Angular-like
project whose heavy commands (`npx ng test`, `npx ng build`, a package script
that runs Playwright) ran outside Sluice, from subshells, `$(...)`, `sh -c`
and shell scripts, plus a husky pre-commit hook. No AI session is involved:
the "agent" is a script that sets `CLAUDECODE=1` and sources the session's
`CLAUDE_ENV_FILE` before every shell command, as Claude Code does.

  1. Fresh sandbox (HOME, XDG_*), the release binary, NO user config beyond
     the daemon budget (test infrastructure, see below). `plugin/hooks/
     session-start.sh` runs with hook JSON on stdin and a temp
     CLAUDE_ENV_FILE: shims installed, PATH entry present once after a
     startup and a resume, additionalContext variants (init proposal before
     `sluice init`, the rules with the gate names after it).
  2. Fixture "angular-like", generic names only: a fake `ng` whose `build`,
     `test` and `lint` burn busy worker threads for a fixed time and print a
     marker line (`serve` and `test --watch` sleep like dev servers), a fake
     `playwright`, fake `npm` and `npx` (local `node_modules/.bin` first), the
     package.json scripts of the incident and a husky-style hook whose
     wrapper puts `node_modules/.bin` FIRST in PATH, like real husky does.
  3. `sluice init --dry-run` writes nothing; `sluice init` yields the gates
     `lint` and `test` and rewires the hook.
  4. Baseline: the same commands raw (`SLUICE_SHIMS=0`). Then they all run
     concurrently as the agent: `npx ng test --watch=false`,
     `(time npx ng build -c mock) > out.log 2>&1`, `x=$(npx ng build)`,
     `sh -c 'npm run axe'`, `./scripts/build.sh`, a failing build, `npm
     start` and `npx ng test --watch` (both stay direct) and a `git commit`
     whose rewired hook goes through `sluice run`. Sampled while running:
     admitted weight, and the nice value of every process of every job.
  5. `weight = "auto"` on the `test` gate drops after three runs.

Daemon budget 8 comes from the sandbox user config: it is test
infrastructure (so that a handful of fake jobs saturate the daemon), not
configuration the product needs. Everything else is the product's defaults.
"""

from __future__ import annotations

import json
import math
import os
import re
import stat
import subprocess as sp
import sys
import threading
import time
from pathlib import Path

from lib import REPO_ROOT, Report, Sandbox, wait_until
from pygate import Cleanup

BUDGET = 8
OVERCOMMIT = 1.5  # [admission] overcommit default
LIMIT = math.floor(OVERCOMMIT * BUDGET)
AUTO_CEILING = math.ceil(BUDGET / 2)
NICE = 10  # [daemon] nice default
THREADS = int(os.environ.get("ALPHA_BURN_THREADS", "4"))
SECONDS = float(os.environ.get("ALPHA_BURN_SECONDS", "3"))
FAIL_CODE = 7
SERVER_SECONDS = 3.0  # how long the dev-server stubs run before the agent kills them


def build_binary() -> None:
    """The release binary of this checkout, unless SLUICE_BIN is given."""
    if os.environ.get("SLUICE_BIN"):
        return
    print("== cargo build --release -p sluice-gate", flush=True)
    sp.run(["cargo", "build", "--release", "-p", "sluice-gate"], cwd=REPO_ROOT, check=True)
    os.environ["SLUICE_BIN"] = str(REPO_ROOT / "target" / "release" / "sluice")


# -- the fixture ------------------------------------------------------------------
TOOL_JS = r"""#!/usr/bin/env node
// Fake %(name)s of the alpha harness. Build-like subcommands burn busy worker
// threads for a fixed time and print a marker line; dev-server subcommands sleep.
const fs = require('node:fs');
const { Worker } = require('node:worker_threads');
const CTL = %(ctl)s;
const NAME = %(name_js)s;
const args = process.argv.slice(2);
const sub = args[0] || '';
const rest = args.slice(1).join(' ');
const num = (file, dflt) => { try { return Number(fs.readFileSync(`${CTL}/${file}`, 'utf8').trim()) || dflt; } catch { return dflt; } };
fs.appendFileSync(`${CTL}/invocations.log`, `${NAME} ${sub} ${rest} pid=${process.pid}\n`);
const watch = args.some((a) => a === '--watch' || a === '--watch=true');
if (sub === 'serve' || (sub === 'test' && watch && NAME === 'ng')) {
	console.log(`${%(marker)s} ${sub} ${rest} WATCHING`);
	setInterval(() => {}, 1000);
} else if (%(heavy)s.includes(sub)) {
	const light = sub === 'lint';
	const threads = num(`threads-${sub}`, light ? 1 : num('threads', %(threads)d));
	const seconds = num(`seconds-${sub}`, light ? 1 : num('seconds', %(seconds)s));
	const end = Date.now() + seconds * 1000;
	let live = threads;
	for (let i = 0; i < threads; i++) {
		const w = new Worker('const e = require("node:worker_threads").workerData; while (Date.now() < e) {}', { eval: true, workerData: end });
		w.on('exit', () => {
			if (--live > 0) return;
			if (rest.split(' ').includes('fail')) {
				console.log(`${%(marker)s} ${sub} ${rest} FAILED`);
				console.error(`${NAME} ${sub} failed`);
				process.exit(%(code)d);
			}
			console.log(`${%(marker)s} ${sub} ${rest} OK`);
		});
	}
} else {
	console.log(`${%(marker)s} ${sub} ${rest} (nothing to do)`);
}
"""

NPM_SH = r"""#!/bin/sh
# Fake npm: runs package.json scripts through sh with node_modules/.bin first (like npm does).
root=$(pwd)
while [ ! -f "$root/package.json" ] && [ "$root" != / ]; do root=$(dirname "$root"); done
case "$1" in
    --version|-v) echo 10.0.0; exit 0 ;;
    run|run-script) name=$2 ;;
    test|start|stop) name=$1 ;;
    *) echo "fake npm: unsupported: $*" >&2; exit 1 ;;
esac
cmd=$(node -e 'const s=require(process.argv[1]+"/package.json").scripts||{};if(!(process.argv[2] in s))process.exit(3);console.log(s[process.argv[2]])' "$root" "$name") \
    || { echo "fake npm: missing script: $name" >&2; exit 1; }
cd "$root" || exit 1
export PATH="$root/node_modules/.bin:$PATH"
exec sh -c "$cmd"
"""

NPX_SH = r"""#!/bin/sh
# Fake npx: a local node_modules/.bin first, then PATH.
while [ $# -gt 0 ]; do
    case "$1" in
        --) shift; break ;;
        -*) shift ;;
        *) break ;;
    esac
done
name=$1
shift
d=$(pwd)
while :; do
    if [ -x "$d/node_modules/.bin/$name" ]; then exec "$d/node_modules/.bin/$name" "$@"; fi
    [ "$d" = / ] && break
    d=$(dirname "$d")
done
exec "$name" "$@"
"""

HUSKY_H = """#!/usr/bin/env sh
# husky 9 runner: node_modules/.bin FIRST in PATH, then the hook through sh -e.
n=$(basename "$0")
s=$(dirname "$(dirname "$0")")/$n
export PATH="node_modules/.bin:$PATH"
sh -e "$s" "$@"
"""

PRE_COMMIT = """npx pretty-quick --staged
ng lint
ng test --watch=false
"""

PACKAGE_JSON = {
    "name": "angular-like",
    "version": "0.0.0",
    "private": True,
    "scripts": {
        "test": "ng test",
        "build": "ng build",
        "axe": "npm run axe:a",
        "axe:a": "playwright test",
        "start": "ng serve",
        "lint": "ng lint",
    },
}

ANGULAR_JSON = {
    "version": 1,
    "projects": {"app": {"projectType": "application", "architect": {"build": {}, "serve": {}, "test": {}, "lint": {}}}},
}


def write_exec(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def js_str(value: str) -> str:
    return json.dumps(value)


class World:
    def __init__(self, sb: Sandbox) -> None:
        self.sb = sb
        self.ctl = sb.root / "ctl"
        self.fakebin = sb.root / "fakebin"
        self.repo = sb.root / "ng-app"
        self.envfile = sb.root / "claude-env.sh"
        self.shims = sb.home / ".local" / "share" / "sluice" / "shims"
        self.base_path = os.environ.get("PATH", "/usr/bin:/bin")
        self.hook = REPO_ROOT / "plugin" / "hooks" / "session-start.sh"

    def path(self) -> str:
        return ":".join([str(self.sb.bin), str(self.fakebin), self.base_path])

    def env(self, extra: dict | None = None, agent: bool = True) -> dict:
        e = self.sb.env(agent=agent, extra={"PATH": self.path(), "CLAUDE_PROJECT_DIR": str(self.repo)})
        if extra:
            e.update(extra)
        return e

    def agent_argv(self, command: str) -> list[str]:
        """What the Bash tool runs: the session's env file first, then the command."""
        return ["bash", "-c", f'. "{self.envfile}"\n{command}']

    def agent_run(self, command: str, cwd: Path | None = None, extra: dict | None = None, timeout: float = 120) -> sp.CompletedProcess:
        return sp.run(self.agent_argv(command), cwd=cwd or self.repo, env=self.env(extra), capture_output=True, text=True,
                      timeout=timeout, stdin=sp.DEVNULL)

    def hook_json(self, source: str) -> str:
        return json.dumps({"session_id": "alpha-s8", "transcript_path": "/dev/null", "cwd": str(self.repo),
                           "hook_event_name": "SessionStart", "source": source})

    def session_start(self, source: str) -> tuple[sp.CompletedProcess, str]:
        cp = sp.run(["sh", str(self.hook)], input=self.hook_json(source), cwd=self.repo,
                    env=self.env({"CLAUDE_ENV_FILE": str(self.envfile)}), capture_output=True, text=True, timeout=30)
        context = ""
        try:
            context = json.loads(cp.stdout)["hookSpecificOutput"]["additionalContext"]
        except (ValueError, KeyError, TypeError):
            pass
        return cp, context


def prepare(sb: Sandbox) -> World:
    w = World(sb)
    w.ctl.mkdir()
    (w.ctl / "markers").mkdir()
    write_exec(w.fakebin / "npm", NPM_SH)
    write_exec(w.fakebin / "npx", NPX_SH)
    repo = w.repo
    repo.mkdir()
    bins = repo / "node_modules" / ".bin"
    write_exec(bins / "ng", TOOL_JS % {"name": "ng", "name_js": js_str("ng"), "ctl": js_str(str(w.ctl)), "marker": js_str("NG-MARKER").replace('"', "'"),
                                       "heavy": "['build','test','lint']", "threads": THREADS, "seconds": SECONDS, "code": FAIL_CODE})
    write_exec(bins / "playwright", TOOL_JS % {"name": "playwright", "name_js": js_str("playwright"), "ctl": js_str(str(w.ctl)),
                                               "marker": js_str("PW-MARKER").replace('"', "'"), "heavy": "['test']",
                                               "threads": THREADS - 1, "seconds": SECONDS, "code": FAIL_CODE})
    write_exec(bins / "pretty-quick", "#!/bin/sh\necho 'PRETTY-QUICK-MARKER' \"$@\"\n")
    write_exec(repo / "scripts" / "build.sh", "#!/bin/sh\nnpx ng build\n")
    (repo / "package.json").write_text(json.dumps(PACKAGE_JSON, indent=2) + "\n")
    (repo / "package-lock.json").write_text("{}\n")
    (repo / "angular.json").write_text(json.dumps(ANGULAR_JSON, indent=2) + "\n")
    (repo / ".gitignore").write_text("node_modules\ndist\nout-*.log\n")
    (repo / "src").mkdir()
    (repo / "src" / "stamp.txt").write_text("0\n")
    write_exec(repo / ".husky" / "pre-commit", PRE_COMMIT)
    write_exec(repo / ".husky" / "_" / "h", HUSKY_H)
    write_exec(repo / ".husky" / "_" / "pre-commit", '#!/usr/bin/env sh\n. "$(dirname "$0")/h"\n')
    (repo / ".husky" / "_" / ".gitignore").write_text("*\n")
    sb.git(repo, "init", "-q", "-b", "main")
    sb.git(repo, "config", "commit.gpgsign", "false")
    sb.git(repo, "config", "core.hooksPath", ".husky/_")
    sb.git(repo, "add", "-A")
    sb.run(["git", "-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", "chore: fixture"], repo, check=True)
    return w


# -- sampling ---------------------------------------------------------------------
def read_procs() -> dict[int, dict]:
    table: dict[int, dict] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            raw = (entry / "stat").read_text()
        except OSError:
            continue
        comm = raw[raw.index("(") + 1: raw.rindex(")")]
        f = raw[raw.rindex(")") + 2:].split()
        # f[0] state, f[1] ppid, f[2] pgrp, f[16] nice (field 19), f[19] starttime (field 22)
        table[int(entry.name)] = {"comm": comm, "ppid": int(f[1]), "pgrp": int(f[2]), "nice": int(f[16]), "start": f[19]}
    return table


class Sampler(threading.Thread):
    """While the agents run: admitted weight (status) and the nice of every
    process of every job (/proc: processes of the process group of each job
    leader, a child of the daemon that leads its own group, plus descendants)."""

    def __init__(self, w: World, expected_nice: int) -> None:
        super().__init__(daemon=True)
        self.w = w
        self.expected = expected_nice
        self.halt = threading.Event()
        self.peak_weight = 0
        self.peak_jobs = 0
        self.peak_queued = 0
        self.samples = 0
        self.seen: dict[tuple[int, str], dict] = {}
        self.first_bad: dict[tuple[int, str], float] = {}
        self.violations: list[dict] = []
        self.nice_values: dict[int, int] = {}
        self.threads_seen: dict[tuple[int, str], int] = {}
        self.weights: list[dict] = []

    def status(self) -> dict:
        return self.w.sb.status_json(self.w.repo)

    def sample_nice(self, daemon_pid: int) -> None:
        procs = read_procs()
        leaders = {p for p, d in procs.items() if d["ppid"] == daemon_pid and d["pgrp"] == p}
        if not leaders:
            return
        members = {p for p, d in procs.items() if d["pgrp"] in leaders}
        stack = list(leaders)
        while stack:
            cur = stack.pop()
            for p, d in procs.items():
                if d["ppid"] == cur and p not in members:
                    members.add(p)
                    stack.append(p)
        now = time.monotonic()
        for p in members:
            d = procs[p]
            key = (p, d["start"])
            self.seen[key] = d
            if d["comm"] in ("nice", "ionice"):
                continue  # the wrapper itself, before it sets the priority and execs
            self.nice_values[d["nice"]] = self.nice_values.get(d["nice"], 0) + 1
            if d["nice"] == self.expected:
                self.first_bad.pop(key, None)
            elif key not in self.first_bad:
                self.first_bad[key] = now
            elif now - self.first_bad[key] >= 0.15:
                v = {"pid": p, "comm": d["comm"], "nice": d["nice"]}
                if v not in self.violations:
                    self.violations.append(v)
            # Threads too: the priority is per task in Linux (the busy workers are threads).
            try:
                tids = os.listdir(f"/proc/{p}/task")
            except OSError:
                continue
            for tid in tids:
                if tid == str(p):
                    continue
                try:
                    raw = Path(f"/proc/{p}/task/{tid}/stat").read_text()
                except OSError:
                    continue
                f = raw[raw.rindex(")") + 2:].split()
                tkey = (int(tid), f[19])
                self.threads_seen[tkey] = int(f[16])
                if int(f[16]) != self.expected and tkey not in self.first_bad:
                    self.first_bad[tkey] = now
                elif int(f[16]) != self.expected and now - self.first_bad[tkey] >= 0.15:
                    v = {"pid": p, "tid": int(tid), "nice": int(f[16])}
                    if v not in self.violations:
                        self.violations.append(v)

    def run(self) -> None:
        while not self.halt.is_set():
            st = self.status()
            running = st.get("running") or []
            self.samples += 1
            total = sum(int(j.get("weight_effective") or j.get("weight") or 0) for j in running)
            self.peak_weight = max(self.peak_weight, total)
            self.peak_jobs = max(self.peak_jobs, len(running))
            self.peak_queued = max(self.peak_queued, len(st.get("queued") or []))
            for j in running:
                self.weights.append({"gate": j.get("gate"), "weight": j.get("weight"), "cfg": j.get("weight_cfg"),
                                     "effective": j.get("weight_effective"), "auto": j.get("weight_auto")})
            pid = (st.get("daemon") or {}).get("pid")
            if pid:
                self.sample_nice(int(pid))
            time.sleep(0.05)


# -- agent commands -----------------------------------------------------------------
class Agent:
    """One agent shell command running in the background."""

    def __init__(self, w: World, name: str, command: str, extra: dict | None = None, kill_after: float | None = None) -> None:
        self.name = name
        self.command = command
        self.kill_after = kill_after
        self.out = self.err = ""
        self.rc: int | None = None
        self.wall = 0.0
        self.proc = sp.Popen(w.agent_argv(command), cwd=w.repo, env=w.env(extra), stdin=sp.DEVNULL, stdout=sp.PIPE, stderr=sp.PIPE,
                             text=True, start_new_session=True)
        self.t0 = time.monotonic()
        self.thread = threading.Thread(target=self._wait, daemon=True)
        self.thread.start()
        if kill_after:
            threading.Timer(kill_after, self._kill).start()

    def _kill(self) -> None:
        if self.proc.poll() is None:
            try:
                os.killpg(self.proc.pid, 9)
            except ProcessLookupError:
                pass

    def _wait(self) -> None:
        self.out, self.err = self.proc.communicate()
        self.rc = self.proc.returncode
        self.wall = time.monotonic() - self.t0


def markers(text: str) -> list[str]:
    return [ln.strip() for ln in text.splitlines() if "-MARKER" in ln]


SET = {
    "test": "npx ng test --watch=false",
    "time": "(time npx ng build -c mock) > out-time.log 2>&1",
    "subst": 'x=$(npx ng build); echo "captured: $x"',
    "sh-c": "sh -c 'npm run axe'",
    "script": "./scripts/build.sh",
    "fail": "npx ng build -c fail",
    "start": "npm start",
    "watch": "npx ng test --watch",
}
DIRECT = ("start", "watch")


def run_set(w: World, extra: dict | None, commit: bool, sampler: Sampler | None) -> dict[str, Agent]:
    """Launch every command at once (the servers are killed after a short time)."""
    agents: dict[str, Agent] = {}
    if sampler:
        sampler.start()
    for name, cmd in SET.items():
        agents[name] = Agent(w, name, cmd, extra, kill_after=SERVER_SECONDS if name in DIRECT else None)
        time.sleep(0.05)
    if commit:
        # The agent's workflow: an unverified commit is refused by the hook (agent_hook_policy
        # fail-fast), it verifies the tree, then the same commit finds the gates in the cache.
        agents["commit"] = Agent(
            w, "commit",
            "git commit -q -m 'feat: change'; echo first-commit-rc=$?; sluice run lint; sluice run test; git commit -q -m 'feat: change'; echo second-commit-rc=$?",
            extra)
    for a in agents.values():
        a.thread.join(timeout=300)
    if sampler:
        sampler.halt.set()
        sampler.join()
    return agents


def timeline(w: World) -> list[str]:
    cp = w.sb.sluice(w.repo, "logs", "--timeline", "--since", "1h")
    return [ln for ln in cp.stdout.splitlines() if ln.strip()]


def toml_gates(path: Path) -> dict[str, str]:
    """{gate: its table text} of a .sluice.toml."""
    gates: dict[str, str] = {}
    name = None
    for line in path.read_text().splitlines():
        m = re.match(r"^\[gates\.([^\]]+)\]\s*$", line)
        if m:
            name = m.group(1).strip('"')
            gates[name] = ""
        elif line.startswith("["):
            name = None
        elif name is not None:
            gates[name] += line + "\n"
    return gates


def tree_state(w: World) -> tuple[str, str]:
    files = []
    for p in sorted(w.repo.rglob("*")):
        if ".git" in p.parts or "node_modules" in p.parts or not p.is_file():
            continue
        files.append(f"{p.relative_to(w.repo)} {p.stat().st_mode:o} {p.stat().st_size} {p.stat().st_mtime_ns}")
    return "\n".join(files), w.sb.git(w.repo, "status", "--porcelain")


# -- phases ---------------------------------------------------------------------------
def session_phase(rep: Report, w: World) -> None:
    sb = w.sb
    cp, ctx1 = w.session_start("startup")
    rep.check("S: session-start.sh exits 0 and prints a SessionStart additionalContext", cp.returncode == 0 and bool(ctx1),
              f"exit {cp.returncode}, stdout {cp.stdout[:100]!r}, stderr {cp.stderr[:100]!r}")
    names = sorted(p.name for p in w.shims.iterdir()) if w.shims.is_dir() else []
    execs = [n for n in names if os.access(w.shims / n, os.X_OK)]
    rep.check("S: shims installed in the stable directory (ng, npm, npx, playwright, ...)",
              {"ng", "npm", "npx", "playwright"} <= set(execs), f"{len(execs)} shims in {w.shims}: {execs[:8]}")
    rep.check("S: no user config beyond the budget: the sandbox config holds only [daemon] budget",
              (sb.config / "sluice" / "config.toml").read_text().strip().splitlines() == ["version = 1", "[daemon]", f"budget = {BUDGET}"],
              (sb.config / "sluice" / "config.toml").read_text().replace("\n", " | "))
    rep.check("S: a repository without .sluice.toml gets the init proposal (dry run first, only after the user agrees)",
              "sluice init --dry-run" in ctx1 and "sluice init" in ctx1 and "after the user agrees" in ctx1 and "Gates of this repository" not in ctx1,
              ctx1[:160])
    cp2, _ = w.session_start("resume")
    shim = str(w.shims)
    text = w.envfile.read_text()
    rep.check("S: the PATH entry is in the env file exactly once after a startup and a resume", cp2.returncode == 0 and len(text.splitlines()) == 1 and text.count("export PATH=") == 1,
              f"{len(text.splitlines())} line(s), {text.count('export PATH=')} export(s)")
    shell = w.agent_run('. "' + str(w.envfile) + '"; printf "%s\\n" "$PATH"; command -v npm; command -v ng || true')
    path_entries = shell.stdout.splitlines()[0].split(":") if shell.stdout else []
    rep.check("S: an agent shell has the shim directory first in PATH, once (the file sourced twice)",
              path_entries[:1] == [shim] and path_entries.count(shim) == 1, f"first {path_entries[:2]}, count {path_entries.count(shim)}")
    rep.check("S: `npm` in an agent shell resolves to the shim", shell.stdout.splitlines()[1:2] == [str(w.shims / "npm")], f"{shell.stdout.splitlines()[1:3]}")


def init_phase(rep: Report, w: World) -> None:
    sb, repo = w.sb, w.repo
    before = tree_state(w)
    hooks_path = sb.git(repo, "config", "core.hooksPath")
    cp = sb.sluice(repo, "init", "--dry-run", agent=True)
    after = tree_state(w)
    rep.check("I: `sluice init --dry-run` writes nothing (tree, modes, mtimes, status, hooksPath unchanged; no .sluice.toml)",
              cp.returncode == 0 and before == after and not (repo / ".sluice.toml").exists() and sb.git(repo, "config", "core.hooksPath") == hooks_path,
              f"exit {cp.returncode}, stderr {cp.stderr[:100]!r}")
    out = cp.stdout
    rep.check("I: the dry run shows the gates lint and test and the rewired hook lines",
              re.search(r"\blint\b", out) is not None and re.search(r"\btest\b", out) is not None and "sluice run --hook pre-commit lint" in out
              and "sluice run --hook pre-commit test" in out, out.replace("\n", " | ")[:300])
    cp = sb.sluice(repo, "init", agent=True)
    gates = toml_gates(repo / ".sluice.toml") if (repo / ".sluice.toml").exists() else {}
    rep.check("I: `sluice init` writes .sluice.toml at the root with exactly the gates lint and test", cp.returncode == 0 and set(gates) == {"lint", "test"},
              f"exit {cp.returncode}, gates {sorted(gates)}, stderr {cp.stderr[:100]!r}")
    rep.check("I: lint is weight 2 and test is weight = \"auto\"", re.search(r"weight\s*=\s*2\b", gates.get("lint", "")) is not None
              and re.search(r'weight\s*=\s*"auto"', gates.get("test", "")) is not None, f"lint: {gates.get('lint', '')!r} test: {gates.get('test', '')[:120]!r}")
    lines = (repo / ".husky" / "pre-commit").read_text().splitlines()
    rep.check("I: the hook is rewired in place: pretty-quick line kept, ng lint and ng test became `sluice run --hook pre-commit ...`",
              lines == ["npx pretty-quick --staged", "sluice run --hook pre-commit lint", "sluice run --hook pre-commit test"], f"{lines}")
    rep.check("I: hook file mode kept (executable) and the husky wrapper untouched",
              os.access(repo / ".husky" / "pre-commit", os.X_OK) and (repo / ".husky" / "_" / "h").read_text() == HUSKY_H)
    # Commit the init result so every later tree starts clean.
    sb.git(repo, "add", "-A")
    sb.run(["git", "-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", "chore: sluice init"], repo, check=True)
    cp, ctx = w.session_start("resume")
    rep.check("S: after init the context carries the rules with the gate names",
              cp.returncode == 0 and "lint" in ctx and "test" in ctx and "Gates of this repository" in ctx and "verify" in ctx and "sluice init --dry-run" not in ctx,
              ctx[:200])


def learn_phase(rep: Report, w: World) -> None:
    """`weight = "auto"`: admitted at the ceiling, lower after three runs."""
    sb, repo = w.sb, w.repo
    (w.ctl / "threads-test").write_text("1\n")
    seen: list[int | None] = []
    cfgs: list[int | None] = []
    for i in range(1, 6):
        (repo / "src" / "stamp.txt").write_text(f"learn-{i}\n")
        proc = sb.popen(["sluice", "run", "test"], repo)
        observed = None
        autos = None
        t_end = time.monotonic() + 60
        while proc.poll() is None and time.monotonic() < t_end:
            for j in sb.status_json(repo).get("running") or []:
                if j.get("gate") == "test":
                    observed = j.get("weight_effective", j.get("weight"))
                    cfgs_i = j.get("weight_cfg")
                    autos = j.get("weight_auto")
                    if cfgs_i is not None:
                        cfg = cfgs_i
            time.sleep(0.05)
        proc.communicate()
        seen.append(observed)
        rc = proc.returncode
        if rc != 0:
            rep.check(f"W: run {i} of the test gate passes", False, f"exit {rc}")
    sb.git(repo, "checkout", "-q", "--", "src/stamp.txt")
    rep.check(f"W: the first run of the `weight = \"auto\"` test gate is admitted at ceil(budget/2) = {AUTO_CEILING}", seen[:1] == [AUTO_CEILING], f"observed {seen}")
    rep.check(f"W: after 3 runs the effective weight of the test gate is below the auto ceiling {AUTO_CEILING} (fake uses 1 thread)",
              seen[3] is not None and seen[3] < AUTO_CEILING, f"effective weights over runs 1..5: {seen}", effective_weights=seen)
    rep.check("W: status marks the gate as auto (weight_auto) while it runs", autos is True, f"weight_auto {autos}")
    (w.ctl / "threads-test").unlink()


def heavy_phase(rep: Report, w: World) -> dict:
    sb, repo = w.sb, w.repo
    # Baseline: the same commands raw (SLUICE_SHIMS=0), on the idle machine.
    learned_ids = {m.group(1) for ln in timeline(w) if (m := re.match(r"^(\S+) ", ln))}
    t0 = sb.totals(repo)
    raw_sampler = None
    print("-- baseline (SLUICE_SHIMS=0)", flush=True)
    base_t0 = time.monotonic()
    raw = run_set(w, {"SLUICE_SHIMS": "0"}, commit=False, sampler=raw_sampler)
    base_wall = time.monotonic() - base_t0
    t1 = sb.totals(repo)
    rep.check("B: with SLUICE_SHIMS=0 nothing reaches the daemon (escape hatch) and every raw command ran",
              int(t1.get("runs", 0)) == int(t0.get("runs", 0)) and all(markers(raw[n].out) for n in ("test", "subst", "sh-c", "script", "fail", "start", "watch")),
              f"runs {t0.get('runs')}->{t1.get('runs')}, wall {base_wall:.1f}s")
    for f in repo.glob("out-*.log"):
        f.rename(w.ctl / f"raw-{f.name}")
    (w.ctl / "invocations.log").rename(w.ctl / "invocations-raw.log")

    print("-- agent script with the session's shims", flush=True)
    sampler = Sampler(w, NICE)
    (repo / "src" / "stamp.txt").write_text("change\n")  # staged before the agents start: one tree for the whole phase
    sb.git(repo, "add", "src/stamp.txt")
    t0 = sb.totals(repo)
    t_start = time.monotonic()
    ag = run_set(w, None, commit=True, sampler=sampler)
    wall = time.monotonic() - t_start
    t1 = sb.totals(repo)
    tl = timeline(w)
    for line in tl:
        print("    " + line[:170], flush=True)
    rows = [(m.group(1), m.group(2), ln) for ln in tl if (m := re.match(r"^(\S+) (.+?) ng-app: ", ln))]
    exec_rows = [r for r in rows if r[1].startswith("exec:")]
    exec_labels = sorted(r[1] for r in exec_rows)
    gate_rows = [r for r in rows if r[1] in ("lint", "test")]
    inv = (w.ctl / "invocations.log").read_text()
    log_text = ""
    for p in sb.state.rglob("client.log"):
        log_text += p.read_text(errors="replace")

    # -- every heavy command went through the daemon; the direct ones did not
    # `npx ng test --watch=false` is exactly the cmd of the `test` gate: the shim sends it to `sluice run test`.
    expect_exec = ["exec:ng build"] * 4 + ["exec:run axe"]
    rep.check("H: ng build (subshell with time, $(...), script, failing) and the axe package script are exec jobs in `sluice logs --timeline`",
              exec_labels == sorted(expect_exec), f"{exec_labels}")
    test_rows = [r for r in rows if r[1] == "test" and r[0] not in learned_ids]
    rep.check("H: `npx ng test --watch=false` (exactly the test gate's cmd) was queued as the gate `test`, through the cache path",
              len(test_rows) >= 1, f"{len(test_rows)} new test gate job(s) in the timeline")
    direct_hit = [r[2][:90] for r in rows if re.search(r"serve|start|watch", r[1])]
    rep.check("H: `npm start` and `npx ng test --watch` stayed direct: not in the timeline, but they ran (their markers are in the output)",
              not direct_hit and any("serve" in m and "WATCHING" in m for m in markers(ag["start"].out))
              and any("test" in m and "--watch" in m and "WATCHING" in m for m in markers(ag["watch"].out)),
              f"timeline lines naming them: {direct_hit}; start {markers(ag['start'].out)}; watch {markers(ag['watch'].out)}")
    rep.check("H: every heavy invocation reached the fake tools (ng build x4, playwright test x1) and ng test at least once",
              inv.count("ng build") == 4 and inv.count("ng test --watch=false") >= 1 and inv.count("playwright test") == 1,
              f"{inv.count('ng build')} builds, {inv.count('ng test --watch=false')} tests, {inv.count('playwright test')} playwright")

    # -- the rewired hook goes through sluice run
    c = ag["commit"]
    rep.check("H: the first `git commit` is stopped by the rewired hook (fail-fast: the tree has not passed), the second one, after `sluice run lint` and `test`, succeeds",
              "first-commit-rc=1" in c.out and "second-commit-rc=0" in c.out and sb.git(repo, "log", "-1", "--format=%s") == "feat: change"
              and "PRETTY-QUICK-MARKER" in c.out + c.err,
              f"out {c.out[-200:]!r}, err {c.err[-200:]!r}")
    hook_lines = [ln for ln in log_text.splitlines() if "hook=pre-commit" in ln]
    seen = {(re.search(r"gate=(\S+)", ln) or [None, "?"])[1] + ":" + (re.search(r"decision=(\S+)", ln) or [None, "?"])[1] for ln in hook_lines}
    rep.check("H: client.log records the hook through sluice run: hook=pre-commit, gate lint, a miss-fail-fast first and a cache hit after",
              {"lint:miss-fail-fast", "lint:hit"} <= seen, f"{sorted(seen)}")
    rep.check("H: the commit's gates lint and test are in the timeline", {r[1] for r in gate_rows} >= {"lint", "test"}, f"{sorted({r[1] for r in gate_rows})}")

    # -- concurrency bound
    rep.check(f"H: peak concurrent admitted weight <= floor(overcommit x budget) = {LIMIT}", 1 <= sampler.peak_weight <= LIMIT,
              f"peak weight {sampler.peak_weight}, peak running {sampler.peak_jobs}, peak queued {sampler.peak_queued}, {sampler.samples} samples, wall {wall:.1f}s",
              peak_weight=sampler.peak_weight, peak_running=sampler.peak_jobs, wall_s=wall)
    rep.check("H: the daemon queued work: more jobs asked than ran at once (queue was exercised)", sampler.peak_queued >= 1 and sampler.peak_jobs < 7,
              f"peak queued {sampler.peak_queued}, peak running {sampler.peak_jobs}")

    # -- outputs and exit codes equal the raw runs
    # The test command is the gate's cmd: the shim runs `sluice run test -v`, whose stdout is the one-line
    # verdict and whose stderr carries the gate log (design, command-control.md 4.4.1). Exit code equal and
    # the tool's marker visible to the agent are what is checked there.
    rep.check("O: `npx ng test --watch=false` (gate path): same exit code as raw, the tool's marker line is shown to the agent (stderr), stdout is sluice's verdict",
              ag["test"].rc == raw["test"].rc == 0 and markers(raw["test"].out)[0] in ag["test"].err and "sluice: test PASS" in ag["test"].out,
              f"queued rc {ag['test'].rc} out {ag['test'].out[-80:]!r} err tail {ag['test'].err[-160:]!r}; raw rc {raw['test'].rc}")
    for name in ("subst", "sh-c", "script", "fail"):
        rep.check(f"O: `{SET[name]}` has the raw stdout and exit code", ag[name].out == raw[name].out and ag[name].rc == raw[name].rc,
                  f"queued rc {ag[name].rc} out {ag[name].out[-100:]!r}; raw rc {raw[name].rc} out {raw[name].out[-100:]!r}")
    new_log = (repo / "out-time.log").read_text() if (repo / "out-time.log").exists() else ""
    raw_log = (w.ctl / "raw-out-time.log").read_text() if (w.ctl / "raw-out-time.log").exists() else ""
    rep.check("O: `(time npx ng build -c mock) > out.log 2>&1`: the log has the marker and the time report, like the raw run",
              markers(new_log) == markers(raw_log) != [] and "real" in new_log and "real" in raw_log and ag["time"].rc == raw["time"].rc == 0,
              f"queued {new_log[-120:]!r}; raw {raw_log[-120:]!r}")
    rep.check(f"O: a failing build's exit code propagates ({FAIL_CODE}) and its stderr is replayed",
              ag["fail"].rc == raw["fail"].rc == FAIL_CODE and "ng build failed" in ag["fail"].err, f"queued rc {ag['fail'].rc}, raw {raw['fail'].rc}, stderr {ag['fail'].err[-100:]!r}")
    rep.check("O: the servers are killed by the agent in both runs (direct commands behave the same)",
              ag["start"].rc == raw["start"].rc and ag["watch"].rc == raw["watch"].rc and ag["start"].rc not in (0, None),
              f"start {ag['start'].rc}/{raw['start'].rc}, watch {ag['watch'].rc}/{raw['watch'].rc}")

    # -- nice
    bad = sampler.violations
    procs = [(k, v) for k, v in sampler.seen.items() if v["comm"] not in ("nice", "ionice")]
    rep.check(f"N: every process of every job runs with the configured nice {NICE} (sampled from /proc)",
              not bad and len(procs) >= 5, f"{len(procs)} processes and {len(sampler.threads_seen)} extra threads seen in jobs, nice histogram {sampler.nice_values}, violations {bad[:3]}",
              nice_values=sampler.nice_values, processes=len(procs))
    return {"raw_wall_s": base_wall, "queued_wall_s": wall, "peak_weight": sampler.peak_weight, "peak_running": sampler.peak_jobs,
            "peak_queued": sampler.peak_queued, "exec_jobs": len(exec_rows), "processes_sampled": len(procs), "threads_sampled": len(sampler.threads_seen),
            "nice_values": sampler.nice_values, "limit": LIMIT, "timeline": tl}


def main() -> int:
    out = Path(os.environ.get("ALPHA_OUT", "."))
    rep = Report("s8_zero_config", out)
    clean = Cleanup()
    build_binary()
    sb = Sandbox("s8", base=Path(os.environ["TMPDIR"]) if os.environ.get("TMPDIR") else None)
    clean.sandboxes.append(sb)
    numbers: dict = {}
    print(f"== zero config: budget {BUDGET}, overcommit limit {LIMIT}, fake load {THREADS} threads x {SECONDS}s, {os.cpu_count()} cpus", flush=True)
    try:
        sb.write_user_config(f"[daemon]\nbudget = {BUDGET}\n")
        w = prepare(sb)
        clean.tokens.append(str(sb.root / "ng-app" / "node_modules"))
        print("-- S: session start", flush=True)
        session_phase(rep, w)
        print("-- I: sluice init", flush=True)
        init_phase(rep, w)
        print("-- W: weight = auto", flush=True)
        learn_phase(rep, w)
        print("-- H: heavy commands", flush=True)
        numbers = heavy_phase(rep, w)
    finally:
        left = clean.settle()
        rep.check("no daemon, job or fake tool process left behind", not left, f"{left}")
        for s in clean.sandboxes:
            s.close()
        rep.save()
    print(f"s8_zero_config: {len(rep.results) - rep.failed}/{len(rep.results)} checks passed")
    print("numbers:", json.dumps({k: v for k, v in numbers.items() if k != "timeline"}))
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
