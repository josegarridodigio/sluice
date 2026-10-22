"""Scenario 1: real stacks and real hook managers.

Node project (sindresorhus/slugify, gates `xo` and `ava`) with husky and
with lefthook; Python project (pallets/itsdangerous, gates pytest and mypy)
with the pre-commit framework and with plain hooks. Each manager is
installed with its own tool and driven by real `git commit` / `git push`:

  1. human commit on a new tree runs the pre-commit gates;
  2. a second commit of the same tree is a cache hit (no new run);
  3. push runs the pre-push gate, a second push of the same tree hits;
  4. an agent commit on an unverified tree fails fast (exit 3) with the
     actionable message, and passes after the agent verifies over MCP;
  5. a broken change is refused with the gate's own exit code and leaves
     no pass marker; the fix passes;
  6. returning to previously verified content hits (content addressed);
  7. hook overhead on a hit is measured.
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

from lib import Report, Sandbox, clone_project, link_node_modules
from mcp_agent import McpAgent

TB = Path(os.environ["ALPHA_TESTBEDS"])
PYLIB = Path(os.environ["ALPHA_PYLIB"])

NODE_TOML = """version = 1

[gates.lint]
cmd = "npx --no-install xo"
weight = 2
timeout = "5m"
hooks = ["pre-commit"]
key_inputs = ["node --version"]

[gates.test]
cmd = "npx --no-install ava"
weight = 3
timeout = "10m"
hooks = ["pre-push"]
key_inputs = ["node --version"]
"""

PY_TOML = f"""version = 1

[gates.test]
cmd = "PYTHONPATH={PYLIB}:src python3 -m pytest -q -p no:cacheprovider"
weight = 2
timeout = "5m"
hooks = ["pre-commit"]
key_inputs = ["python3 --version"]

[gates.types]
cmd = "PYTHONPATH={PYLIB}:src python3 -m mypy --no-incremental src"
weight = 2
timeout = "5m"
hooks = ["pre-push"]
key_inputs = ["python3 --version"]
"""


def setup_node(sb: Sandbox, manager: str) -> Path:
    repo = clone_project(sb, TB / "node-slugify", f"node-{manager}")
    link_node_modules(TB / "node-slugify" / "node_modules", repo)
    (repo / ".sluice.toml").write_text(NODE_TOML)
    if manager == "husky":
        sb.run(["npx", "--no-install", "husky", "init"], repo, check=True)
        (repo / ".husky" / "pre-commit").write_text("sluice run --hook pre-commit lint || exit $?\n")
        (repo / ".husky" / "pre-push").write_text("sluice run --hook pre-push test || exit $?\n")
        sb.run(["git", "add", ".husky", ".sluice.toml"], repo, check=True)
    elif manager == "lefthook":
        (repo / "lefthook.yml").write_text(
            "pre-commit:\n  commands:\n    sluice-lint:\n      run: sluice run --hook pre-commit lint\n"
            "pre-push:\n  commands:\n    sluice-test:\n      run: sluice run --hook pre-push test\n"
        )
        sb.run(["npx", "--no-install", "lefthook", "install"], repo, check=True)
        sb.run(["git", "add", "lefthook.yml", ".sluice.toml"], repo, check=True)
    sb.run(["git", "-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", f"chore: sluice with {manager}"], repo, check=True)
    return repo


def setup_python(sb: Sandbox, manager: str) -> tuple[Path, dict]:
    repo = clone_project(sb, TB / "py-itsdangerous", f"py-{manager}")
    (repo / ".sluice.toml").write_text(PY_TOML)
    extra: dict = {}
    if manager == "pre-commit":
        (repo / ".pre-commit-config.yaml").write_text(
            "repos:\n  - repo: local\n    hooks:\n"
            "      - id: sluice-test\n        name: sluice test\n        entry: sluice run --hook pre-commit test\n"
            "        language: system\n        pass_filenames: false\n        always_run: true\n        stages: [pre-commit]\n"
            "      - id: sluice-types\n        name: sluice types\n        entry: sluice run --hook pre-push types\n"
            "        language: system\n        pass_filenames: false\n        always_run: true\n        stages: [pre-push]\n"
        )
        extra = {"PYTHONPATH": str(PYLIB)}
        sb.run(["python3", "-m", "pre_commit", "install", "--hook-type", "pre-commit", "--hook-type", "pre-push"], repo, extra=extra, check=True)
        sb.run(["git", "add", ".pre-commit-config.yaml", ".sluice.toml"], repo, check=True)
    elif manager == "plain":
        hooks = repo / ".githooks"
        hooks.mkdir()
        (hooks / "pre-commit").write_text("#!/bin/sh\nset -e\nsluice run --hook pre-commit test\n")
        (hooks / "pre-push").write_text("#!/bin/sh\nset -e\nsluice run --hook pre-push types\n")
        for h in hooks.iterdir():
            h.chmod(0o755)
        sb.git(repo, "config", "core.hooksPath", ".githooks")
        sb.run(["git", "add", ".githooks", ".sluice.toml"], repo, check=True)
    sb.run(["git", "-c", "core.hooksPath=/dev/null", "commit", "-q", "-m", f"chore: sluice with {manager}"], repo, check=True)
    return repo, extra


def runs(sb: Sandbox, repo: Path) -> int:
    return int(sb.totals(repo).get("runs", 0))


def flow(sb: Sandbox, rep: Report, label: str, repo: Path, extra: dict, edit, break_it, pre_gates, push_gates) -> None:
    def commit(msg, agent=False, allow_empty=False):
        args = ["git", "commit", "-q", "-m", msg] + (["--allow-empty"] if allow_empty else [])
        t = time.monotonic()
        cp = sb.run(args, repo, agent=agent, extra=extra)
        return cp, time.monotonic() - t

    # 1. new tree, human commit runs the pre-commit gates.
    edit(repo, "one")
    sb.git(repo, "add", "-A")
    before = runs(sb, repo)
    cp, t_miss = commit("feat: first change")
    rep.check(f"{label}: human commit on a new tree runs the gates", cp.returncode == 0 and runs(sb, repo) == before + len(pre_gates),
              f"exit {cp.returncode}, runs +{runs(sb, repo) - before} (expected +{len(pre_gates)}), {t_miss:.2f}s", stderr=cp.stderr[-400:])

    # 2. same tree again: hit, no new run.
    before = runs(sb, repo)
    cp, t_hit = commit("chore: same tree", allow_empty=True)
    rep.check(f"{label}: commit of an already verified tree is a cache hit", cp.returncode == 0 and runs(sb, repo) == before,
              f"exit {cp.returncode}, runs +{runs(sb, repo) - before}, {t_hit:.3f}s", hit_seconds=t_hit, miss_seconds=t_miss)

    # 3. push runs pre-push gates once; a second push of the same tree hits.
    before = runs(sb, repo)
    cp = sb.run(["git", "push", "-q", "origin", "HEAD"], repo, extra=extra)
    first = runs(sb, repo) - before
    sb.run(["git", "commit", "-q", "--allow-empty", "--no-verify", "-m", "chore: empty"], repo)
    before = runs(sb, repo)
    cp2 = sb.run(["git", "push", "-q", "origin", "HEAD"], repo, extra=extra)
    rep.check(f"{label}: push runs pre-push gates once, the next push of the same tree hits",
              cp.returncode == 0 and cp2.returncode == 0 and first == len(push_gates) and runs(sb, repo) == before,
              f"first push +{first} run(s), second push +{runs(sb, repo) - before}", stderr=(cp.stderr + cp2.stderr)[-400:])

    # 4. agent commit on an unverified tree fails fast; verify over MCP, retry hits.
    edit(repo, "two")
    sb.git(repo, "add", "-A")
    before = runs(sb, repo)
    cp, _ = commit("feat: agent change", agent=True)
    # git reports any hook failure as exit 1; Sluice's own exit 3 shows in the text.
    fast = cp.returncode != 0 and "has not passed on this tree" in cp.stderr and "fail-fast" in cp.stderr and "verify" in cp.stderr
    rep.check(f"{label}: agent commit on an unverified tree fails fast with the actionable message",
              fast and runs(sb, repo) == before, f"exit {cp.returncode}, runs +{runs(sb, repo) - before}", stderr=cp.stderr[-400:])
    agent = McpAgent(sb.env(agent=True, extra=extra), repo, name=f"{label}-agent")
    res = agent.verify(repo, pre_gates)
    agent.close()
    ok_verify = not res["is_error"] and (res["structured"] or {}).get("all_passed") is True
    cp, t_agent = commit("feat: agent change", agent=True)
    rep.check(f"{label}: after MCP verify the agent commit passes from the cache",
              ok_verify and cp.returncode == 0, f"verify all_passed={ok_verify}, commit exit {cp.returncode} in {t_agent:.3f}s",
              verify=res["text"][:300])

    # 5. broken change: refused with the gate's exit code, no pass marker, fix passes.
    break_it(repo, True)
    sb.git(repo, "add", "-A")
    cp, _ = commit("feat: broken")
    status = sb.sluice(repo, "cache", "status").stdout
    rep.check(f"{label}: a broken change is refused by the gate and not cached as a pass",
              cp.returncode != 0 and "HIT" not in [ln.split()[1] for ln in status.splitlines() if ln.split() and ln.split()[0] in pre_gates],
              f"commit exit {cp.returncode}", cache_status=status[-500:])
    cp_again, _ = commit("feat: broken")
    rep.check(f"{label}: the broken tree stays refused on retry (no stale pass)", cp_again.returncode != 0, f"exit {cp_again.returncode}")
    break_it(repo, False)
    sb.git(repo, "add", "-A")
    before = runs(sb, repo)
    cp, _ = commit("feat: fixed", allow_empty=True)
    # 6. the fixed content equals the agent-verified tree of step 4: hit.
    rep.check(f"{label}: content identical to a verified tree hits (content addressed)",
              cp.returncode == 0 and runs(sb, repo) == before, f"exit {cp.returncode}, runs +{runs(sb, repo) - before}")
    rep.check(f"{label}: hook overhead on a hit", t_hit < 1.0, f"{t_hit * 1000:.0f} ms for a full git commit with the hook")


def node_edit(repo: Path, tag: str) -> None:
    p = repo / "readme.md"
    p.write_text(p.read_text() + f"\n<!-- alpha {tag} -->\n")


NODE_LINT_BREAK = "\nvar alphaUnused = 1;\n"


def node_break(repo: Path, broken: bool) -> None:
    """Break the pre-commit gate (xo): an unused `var` fails the linter."""
    p = repo / "index.js"
    text = p.read_text()
    p.write_text(text + NODE_LINT_BREAK if broken else text.replace(NODE_LINT_BREAK, "", 1))


def py_edit(repo: Path, tag: str) -> None:
    p = repo / "README.md"
    p.write_text(p.read_text() + f"\n<!-- alpha {tag} -->\n")


def py_break(repo: Path, broken: bool) -> None:
    p = repo / "src" / "itsdangerous" / "encoding.py"
    text = p.read_text()
    marker = "\n\ndef _alpha_broken():\n    pass\n\n\nbase64_encode = lambda s: b'broken'  # noqa\n"
    p.write_text(text + marker if broken else text.replace(marker, ""))


def main() -> int:
    out = Path(os.environ.get("ALPHA_OUT", "."))
    rep = Report("s1_hooks", out)
    for manager in ("husky", "lefthook"):
        print(f"== node + {manager}", flush=True)
        sb = Sandbox(f"node-{manager}")
        try:
            repo = setup_node(sb, manager)
            flow(sb, rep, f"node/{manager}", repo, {}, node_edit, node_break, ["lint"], ["test"])
        finally:
            sb.close()
    for manager in ("pre-commit", "plain"):
        print(f"== python + {manager}", flush=True)
        sb = Sandbox(f"py-{manager}")
        try:
            repo, extra = setup_python(sb, manager)
            flow(sb, rep, f"python/{manager}", repo, extra, py_edit, py_break, ["test"], ["types"])
        finally:
            sb.close()
    rep.save()
    print(f"s1_hooks: {len(rep.results) - rep.failed}/{len(rep.results)} checks passed")
    return 1 if rep.failed else 0


if __name__ == "__main__":
    sys.exit(main())
