#!/usr/bin/env python3
"""Refuse hand-written Rust source files over the workspace line limit."""

import argparse
from pathlib import Path
import subprocess
import sys


# Keep source files small so one-line edits rebuild quickly.
MAX_SOURCE_LINES = 5000


def is_exempt(path):
    return (
        "tests" in path.parts
        or "benches" in path.parts
        or path.name == "tests.rs"
        or path.name.endswith("_tests.rs")
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    root = args.root.resolve()
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=root,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        sys.stderr.write(result.stderr.decode(errors="replace"))
        return 2
    violations = 0
    for name in result.stdout.split(b"\0"):
        if not name:
            continue
        path = Path(name.decode(errors="surrogateescape"))
        if is_exempt(path):
            continue
        with (root / path).open("rb") as source:
            lines = sum(1 for _ in source)
        if lines > MAX_SOURCE_LINES:
            print(f"{path}: {lines} lines (limit: {MAX_SOURCE_LINES})", file=sys.stderr)
            violations += 1
    print(f"file size check: {violations} violations")
    return 1 if violations else 0


if __name__ == "__main__":
    raise SystemExit(main())
