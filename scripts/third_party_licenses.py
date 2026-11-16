#!/usr/bin/env python3
"""Writes the notices of the Rust crates built into the binary: third_party_licenses.py OUT.txt

Lists every crate in the release build for x86_64 Linux (`cargo metadata --locked`, normal and
build dependencies of the app, not dev-dependencies) with its version, licence and source, then
the licence texts found in the crates' own sources (the manifest's `license-file`, and LICENSE*,
LICENCE*, COPYING*, NOTICE*, UNLICENSE*, COPYRIGHT*), each printed once with the crates it came
from. A crate that ships no licence text gets the standard text of each licence its SPDX
expression names, from scripts/spdx/; for MIT, whose text carries the copyright line, that line
is taken from another crate of the same repository when one has it, and otherwise the notice
points at the repository. Run after the crates are fetched (any cargo build does that). Fails
when a crate declares no licence, or has no text and names a licence scripts/spdx/ lacks; add
the text there then. --self-test checks the logic on made-up data.
"""
import hashlib
import json
import os
import re
import subprocess
import sys

TARGET = "x86_64-unknown-linux-gnu"
NAMES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE", "COPYRIGHT")


def built_packages(meta):
    """The packages reachable from the app through normal and build dependencies."""
    nodes = {node["id"]: node for node in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]
    seen, todo = set(), [root]
    while todo:
        node = nodes[todo.pop()]
        for dep in node["deps"]:
            kinds = {kind["kind"] for kind in dep["dep_kinds"]}
            if kinds == {"dev"} or dep["pkg"] in seen:
                continue
            seen.add(dep["pkg"])
            todo.append(dep["pkg"])
    packages = {package["id"]: package for package in meta["packages"]}
    return sorted((packages[i] for i in seen), key=lambda p: (p["name"].lower(), p["version"]))


SPDX_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "spdx")


def licence_files(package):
    """The licence texts in the crate's own source: `license-file` first, then by name."""
    folder = os.path.dirname(package["manifest_path"])
    paths = []
    if package.get("license_file"):
        paths.append(os.path.normpath(os.path.join(folder, package["license_file"])))
    try:
        names = sorted(os.listdir(folder))
    except OSError:
        names = []
    for name in names:
        path = os.path.join(folder, name)
        if name.upper().startswith(NAMES) and path not in paths:
            paths.append(path)
    found = []
    for path in paths:
        if os.path.isfile(path):
            with open(path, encoding="utf-8", errors="replace") as f:
                found.append(f.read().strip())
    return found


def spdx_ids(expression):
    """The licence ids an SPDX expression names, in order, each once ("MIT/Apache-2.0" too)."""
    ids = []
    words = re.split(r"[\s()/]+", expression or "")
    skip = False
    for word in words:
        if skip:  # the exception after WITH is not a licence
            skip = False
            continue
        if word.upper() == "WITH":
            skip = True
        elif word and word.upper() not in ("OR", "AND") and word not in ids:
            ids.append(word)
    return ids


def spdx_text(licence_id):
    path = os.path.join(SPDX_DIR, licence_id + ".txt")
    if not os.path.isfile(path):
        return None
    with open(path, encoding="utf-8") as f:
        return f.read().strip()


def copyright_lines(texts):
    """The copyright lines of the MIT texts among `texts`, in order, each once."""
    lines = []
    for text in texts:
        if "Permission is hereby granted, free of charge" not in text:
            continue
        for line in text.splitlines():
            line = line.strip()
            if re.match(r"(?i)copyright\b.*\S", line) and "permission notice" not in line.lower():
                if line not in lines:
                    lines.append(line)
    return lines


def standard_texts(package, same_repository):
    """For a crate with no licence text of its own: the standard text of each licence it names.

    `same_repository` are the other packages of its repository, where the MIT copyright line is
    looked for. Returns (texts, missing ids).
    """
    texts, missing = [], []
    for licence_id in spdx_ids(package.get("license")):
        text = spdx_text(licence_id)
        if text is None:
            missing.append(licence_id)
            continue
        if licence_id == "MIT":
            found = []
            for other in same_repository:
                for line in copyright_lines(licence_files(other)):
                    if line not in found:
                        found.append(line)
            where = package.get("repository") or "https://crates.io/crates/" + package["name"]
            notice = "\n".join(found) if found else f"Copyright: see {where}"
            text = text.replace("MIT License\n", "MIT License\n\n" + notice + "\n", 1)
        texts.append(text)
    return texts, missing


def group_texts(crate_texts):
    """[(crate, [text...])] -> [(text, [crate...])], each distinct text once, in first-seen order."""
    order, users = [], {}
    for crate, texts in crate_texts:
        for text in texts:
            key = hashlib.sha256(" ".join(text.split()).encode()).hexdigest()
            if key not in users:
                users[key] = (text, [])
                order.append(key)
            if crate not in users[key][1]:
                users[key][1].append(crate)
    return [users[key] for key in order]


def render(packages, all_packages=()):
    missing = [p["name"] for p in packages if not (p.get("license") or p.get("license_file"))]
    if missing:
        sys.exit(f"error: no licence declared by: {', '.join(missing)}")
    by_repository = {}
    for p in sorted(all_packages, key=lambda p: (p["name"], p["version"])):
        if p.get("repository"):
            by_repository.setdefault(p["repository"].rstrip("/"), []).append(p)
    out = [
        "Green Light is built with the open-source Rust crates listed here. Each line gives the",
        "crate, its version, its licence (SPDX) and where its source is. The licence texts found",
        "in the crates' sources follow the list.",
        "",
    ]
    crate_texts, gaps = [], []
    for p in packages:
        name = f"{p['name']} {p['version']}"
        where = p.get("repository") or "https://crates.io/crates/" + p["name"]
        out.append(f"{name}: {p.get('license') or 'see its licence file'} ({where})")
        texts = licence_files(p)
        if not texts:
            siblings = by_repository.get((p.get("repository") or "").rstrip("/"), [])
            texts, unknown = standard_texts(p, [s for s in siblings if s["id"] != p["id"]])
            if unknown or not texts:
                gaps.append(f"{name} ({', '.join(unknown) or 'no licence id'})")
            name += " (standard licence text; the crate ships none)"
        crate_texts.append((name, texts))
    if gaps:
        sys.exit("error: no licence text in the crate and none in scripts/spdx/ for: "
                 + "; ".join(gaps))
    for text, crates in group_texts(crate_texts):
        out += ["", "=" * 78, "Licence text of: " + ", ".join(crates), "=" * 78, "", text]
    return "\n".join(out) + "\n"


def self_test():
    grouped = group_texts([("a 1", ["MIT text\n", "Apache"]), ("b 2", ["MIT  text"]), ("c 3", [])])
    assert grouped == [("MIT text\n", ["a 1", "b 2"]), ("Apache", ["a 1"])], grouped
    meta = {
        "resolve": {
            "root": "app",
            "nodes": [
                {"id": "app", "deps": [
                    {"pkg": "x", "dep_kinds": [{"kind": None}]},
                    {"pkg": "t", "dep_kinds": [{"kind": "dev"}]},
                    {"pkg": "b", "dep_kinds": [{"kind": "build"}]}]},
                {"id": "x", "deps": [{"pkg": "y", "dep_kinds": [{"kind": None}]}]},
                {"id": "y", "deps": []}, {"id": "t", "deps": []}, {"id": "b", "deps": []},
            ],
        },
        "packages": [{"id": i, "name": i, "version": "1"} for i in ("app", "x", "y", "t", "b")],
    }
    assert [p["name"] for p in built_packages(meta)] == ["b", "x", "y"]
    assert spdx_ids("MIT OR Apache-2.0") == ["MIT", "Apache-2.0"]
    assert spdx_ids("MIT/Apache-2.0") == ["MIT", "Apache-2.0"]
    assert spdx_ids("(Apache-2.0 WITH LLVM-exception OR MIT) AND Zlib") == ["Apache-2.0", "MIT", "Zlib"]
    mit = "MIT License\n\nCopyright (c) 2020 A. Author\n\nPermission is hereby granted, free of charge, to"
    assert copyright_lines([mit, "Apache\nCopyright 2019 Nobody"]) == ["Copyright (c) 2020 A. Author"]
    here = os.path.dirname(os.path.abspath(__file__))
    bare = {"id": "bare", "name": "bare", "version": "1", "license": "MIT OR Apache-2.0",
            "repository": "https://example.org/r", "manifest_path": "/nonexistent/Cargo.toml"}
    texts, unknown = standard_texts(bare, [])
    assert unknown == [] and len(texts) == 2, (texts, unknown)
    assert "Copyright: see https://example.org/r" in texts[0] and "Permission is hereby" in texts[0]
    assert "Apache License" in texts[1]
    odd = dict(bare, license="MIT OR Made-Up-1.0")
    assert standard_texts(odd, [])[1] == ["Made-Up-1.0"], "an unknown licence must be reported"
    try:
        render([odd])
    except SystemExit as e:
        assert "Made-Up-1.0" in str(e)
    else:
        raise AssertionError("a crate with no text and an unknown licence must fail the build")
    assert os.path.isfile(os.path.join(here, "spdx", "MIT.txt"))
    print("third_party_licenses self-test ok")


def main():
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET]))
    text = render(built_packages(meta), meta["packages"])
    with open(sys.argv[1], "w", encoding="utf-8") as f:
        f.write(text)


if __name__ == "__main__":
    main()
