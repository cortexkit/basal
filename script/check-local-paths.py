#!/usr/bin/env python3
"""Refuse tracked text files containing absolute user-home paths."""

import argparse
from pathlib import Path
import re
import subprocess
import sys


HOME_PATHS = (
    re.compile(r"(?<![\w./-])/(?:Users|home)/[^\r\n/\\<>]+/"),
    re.compile(r"(?<![\w])C:\\Users\\[^\r\n/\\<>]+\\", re.IGNORECASE),
)
# Add only fixtures that genuinely need an absolute home path; keep empty otherwise.
ALLOWLIST = frozenset()


def tracked_files(root):
    result = subprocess.run(
        ["git", "ls-files", "-z"], cwd=root, capture_output=True, check=False
    )
    if result.returncode:
        sys.stderr.write(result.stderr.decode(errors="replace"))
        return None
    return [name for name in result.stdout.split(b"\0") if name]


def text_contents(path):
    try:
        data = path.read_bytes()
    except (FileNotFoundError, IsADirectoryError):
        return None
    if b"\0" in data:
        return None
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        return None


def violations(text):
    for pattern in HOME_PATHS:
        for match in pattern.finditer(text):
            yield text.count("\n", 0, match.start()) + 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    root = args.root.resolve()
    files = tracked_files(root)
    if files is None:
        return 2

    checked = 0
    failures = 0
    for name in files:
        relative = name.decode(errors="surrogateescape")
        if relative in ALLOWLIST:
            continue
        path = root / relative
        if path.is_symlink():
            text = str(path.readlink())
        else:
            text = text_contents(path)
        if text is None:
            continue
        checked += 1
        for line in violations(text):
            failures += 1
            print(f"{relative}:{line}: absolute home path is not allowed", file=sys.stderr)

    print(f"local path check: {checked} text files checked, {failures} violations")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
