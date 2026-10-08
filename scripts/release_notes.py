#!/usr/bin/env python3
"""Print one version's CHANGELOG.md section: the release page's text.

usage: release_notes.py <version> [CHANGELOG.md]

The section is everything under `## <version> — <date>` up to the next `## ` heading, as written
(the heading itself left out: the release is titled with the version). Exits 1 when the version has
no section or it is empty, so a release never goes out without its notes.
"""
import re
import sys


def notes(changelog: str, version: str) -> str:
    lines = changelog.splitlines()
    head = re.compile(r"^## " + re.escape(version) + r" — \d{4}-\d{2}-\d{2}\s*$")
    start = next((i for i, line in enumerate(lines) if head.match(line)), None)
    if start is None:
        return ""
    body = []
    for line in lines[start + 1:]:
        if line.startswith("## "):
            break
        body.append(line)
    return unwrap("\n".join(body).strip())


def unwrap(text: str) -> str:
    """Joins the changelog's hard-wrapped lines: GitHub shows every newline of a release's text as a
    line break. A list item's continuation lines (indented) and a paragraph's lines become one line;
    blank lines, list items and code fences stay as they are."""
    out = []
    fenced = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("```"):
            fenced = not fenced
            out.append(line)
            continue
        joinable = out and out[-1].strip() and not fenced and not out[-1].strip().startswith("```")
        starts_block = not stripped or stripped.startswith(("- ", "* ", "#", "|")) or re.match(r"\d+\. ", stripped)
        if joinable and stripped and not starts_block:
            out[-1] = out[-1].rstrip() + " " + stripped
        else:
            out.append(line)
    return "\n".join(out)


def _self_test():
    text = "# T\n\n## Unreleased\n\n- x\n\n## 1.2.3 — 2026-10-05\n\nIntro.\n\n- **A.** one\n  two\n\n## 1.2.2 — 2026-10-01\n\n- old\n"
    assert notes(text, "1.2.3") == "Intro.\n\n- **A.** one two", notes(text, "1.2.3")
    assert unwrap("a\nb\n\n- x\n  y\n- z\n```\nk\nl\n```") == "a b\n\n- x y\n- z\n```\nk\nl\n```"
    assert notes(text, "1.2.2") == "- old"
    assert notes(text, "1.2") == ""
    print("release_notes self-test ok")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        _self_test()
        sys.exit(0)
    if not 2 <= len(sys.argv) <= 3:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    path = sys.argv[2] if len(sys.argv) == 3 else "CHANGELOG.md"
    text = notes(open(path, encoding="utf-8").read(), sys.argv[1].lstrip("v"))
    if not text:
        print(f"no CHANGELOG.md section for {sys.argv[1]}", file=sys.stderr)
        sys.exit(1)
    print(text)
