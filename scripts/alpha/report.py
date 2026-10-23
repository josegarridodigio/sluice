"""Turn the harness's JSON-lines results into markdown tables.

Usage: python3 report.py <out-dir> [<out-dir> ...]

Every scenario writes `<out-dir>/<scenario>.jsonl`, one record per check
({"scenario", "check", "ok", "detail", "data"}). This prints, per
directory, a summary table (scenario, passed, failed) followed by the
failed checks with their detail, ready to paste into
kb/sluice/changes/alpha-readiness.md.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def load(out_dir: Path) -> dict[str, list[dict]]:
    scenarios: dict[str, list[dict]] = {}
    for path in sorted(out_dir.glob("*.jsonl")):
        rows = []
        for line in path.read_text().splitlines():
            line = line.strip()
            if line:
                rows.append(json.loads(line))
        scenarios[path.stem] = rows
    return scenarios


def one_line(text: str, limit: int = 160) -> str:
    text = " ".join(str(text).split()).replace("|", "\\|")
    return text if len(text) <= limit else text[: limit - 1] + "…"


def report(out_dir: Path) -> tuple[str, int]:
    scenarios = load(out_dir)
    lines = [f"Results from `{out_dir.name}`:", "", "| Scenario | Passed | Failed |", "|---|---|---|"]
    failures: list[tuple[str, dict]] = []
    total_failed = 0
    for name, rows in scenarios.items():
        failed = [r for r in rows if not r.get("ok")]
        total_failed += len(failed)
        lines.append(f"| {name} | {len(rows) - len(failed)} | {len(failed)} |")
        failures.extend((name, r) for r in failed)
    if not scenarios:
        lines.append("| (no results) | 0 | 0 |")
    if failures:
        lines += ["", "Failed checks:", "", "| Scenario | Check | Detail |", "|---|---|---|"]
        for name, row in failures:
            lines.append(f"| {name} | {one_line(row.get('check', ''))} | {one_line(row.get('detail', ''))} |")
    return "\n".join(lines) + "\n", total_failed


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    failed = 0
    for arg in argv[1:]:
        text, n = report(Path(arg))
        print(text)
        failed += n
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
