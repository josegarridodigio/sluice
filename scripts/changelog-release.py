#!/usr/bin/env python3
"""Keep a Changelog helper used by scripts/release.sh and the release workflow.

  changelog-release.py release X.Y.Z [--date YYYY-MM-DD] [--file CHANGELOG.md]
      Move the content of [Unreleased] into a new "## [X.Y.Z] - <date>"
      section, leave a fresh empty Unreleased section and update the compare
      links. Exits 1 and changes nothing when Unreleased has no entries.

  changelog-release.py extract X.Y.Z [--file CHANGELOG.md]
      Print the body of the "## [X.Y.Z]" section (without its heading).
      Exits 1 when the section is missing or empty.
"""
import argparse
import datetime
import re
import sys

PLACEHOLDER = "### Added\n\n### Changed\n\n### Fixed\n"
SECTION = re.compile(r"^## \[(?P<name>[^\]]+)\]", re.M)
LINK = re.compile(r"^\[(?P<name>[^\]]+)\]: (?P<url>\S+)\s*$", re.M)


def die(message):
    print(f"changelog: {message}", file=sys.stderr)
    sys.exit(1)


def split(text):
    """Return (head, sections, links): sections is [(name, body)] in order."""
    first_link = LINK.search(text)
    # The link block is the trailing run of "[x]: url" lines.
    links_start = len(text)
    if first_link:
        tail = text[first_link.start():]
        if all(not line.strip() or LINK.match(line) for line in tail.splitlines()):
            links_start = first_link.start()
    body, links = text[:links_start], text[links_start:]
    marks = list(SECTION.finditer(body))
    if not marks:
        die("no '## [..]' sections found")
    head = body[: marks[0].start()]
    sections = []
    for i, m in enumerate(marks):
        end = marks[i + 1].start() if i + 1 < len(marks) else len(body)
        sections.append((m.group("name"), body[m.end():end]))
    return head, sections, links


def entries(body):
    """The non-blank, non-heading lines of a section body."""
    keep = []
    for line in body.splitlines():
        if line.strip() and not line.startswith("#"):
            keep.append(line)
    return "\n".join(keep)


def drop_empty_subsections(body):
    """Remove '### X' headings that have no entries under them."""
    parts = re.split(r"(?m)^(?=### )", body.strip("\n"))
    out = []
    for part in parts:
        if not part.strip():
            continue
        if part.startswith("###") and not entries(part):
            continue
        out.append(part.strip("\n"))
    return "\n\n".join(out)


def release(args):
    version = args.version
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        die(f"not a X.Y.Z version: {version}")
    date = args.date or datetime.date.today().isoformat()
    with open(args.file, encoding="utf-8") as fh:
        text = fh.read()
    head, sections, links = split(text)
    names = [n for n, _ in sections]
    if names[0] != "Unreleased":
        die("the first section must be [Unreleased]")
    if version in names:
        die(f"[{version}] already has a section")
    if not entries(sections[0][1]):
        die("the [Unreleased] section is empty: add entries before releasing")
    previous = names[1] if len(names) > 1 else None

    released = drop_empty_subsections(sections[0][1])
    out = [head.rstrip("\n") + "\n\n", "## [Unreleased]\n\n" + PLACEHOLDER + "\n"]
    out.append(f"## [{version}] - {date}\n\n{released}\n\n")
    for name, body in sections[1:]:
        out.append(f"## [{name}]" + body.rstrip("\n") + "\n\n")

    link_map = {m.group("name"): m.group("url") for m in LINK.finditer(links)}
    order = [m.group("name") for m in LINK.finditer(links)]
    unreleased = link_map.get("Unreleased")
    if not unreleased or "/compare/" not in unreleased:
        die("the [Unreleased] compare link is missing")
    base = unreleased.split("/compare/")[0]
    link_map["Unreleased"] = f"{base}/compare/v{version}...HEAD"
    if previous:
        link_map[version] = f"{base}/compare/v{previous}...v{version}"
    else:
        link_map[version] = f"{base}/releases/tag/v{version}"
    # Unreleased first, then the new version, then the older links as they were.
    new_order = ["Unreleased", version] + [
        n for n in order if n not in ("Unreleased", version)
    ]
    out.append("\n".join(f"[{n}]: {link_map[n]}" for n in new_order) + "\n")
    with open(args.file, "w", encoding="utf-8") as fh:
        fh.write("".join(out))


def extract(args):
    with open(args.file, encoding="utf-8") as fh:
        text = fh.read()
    _, sections, _ = split(text)
    for name, body in sections:
        if name == args.version:
            body = body.split("\n", 1)[1] if "\n" in body else ""
            body = body.strip("\n")
            if not entries(body):
                die(f"the [{args.version}] section is empty")
            print(body)
            return
    die(f"no section [{args.version}] in {args.file}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="cmd", required=True)
    for name, fn in (("release", release), ("extract", extract)):
        p = sub.add_parser(name)
        p.add_argument("version")
        p.add_argument("--file", default="CHANGELOG.md")
        if name == "release":
            p.add_argument("--date")
        p.set_defaults(fn=fn)
    args = parser.parse_args()
    args.fn(args)


if __name__ == "__main__":
    main()
