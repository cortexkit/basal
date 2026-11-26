#!/usr/bin/env python3
"""Refuse tracked files under .cortexkit/ outside the allowlist.

Internal documents and maintainer scripts live in ignored .cortexkit/ folders and
must never be tracked. Only WORKER.md, release notes, skills, rules and top-level
project settings files (`.cortexkit/<module>.jsonc`, which the daemon reads) may be.
The check lists every tracked .cortexkit file rather than only tracked files the
ignore rules exclude, because a stray `!` line in any .gitignore would un-ignore
a private file and hide it from an ignore-based check.
"""

import argparse
from pathlib import Path
import re
import subprocess
import sys

ALLOWED = re.compile(r"^\.cortexkit/(release-notes/|skills/|rules/|WORKER\.md$|[a-z0-9-]+\.jsonc$)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    result = subprocess.run(
        ["git", "ls-files", "-z", "--", ".cortexkit"],
        cwd=args.root.resolve(),
        capture_output=True,
        check=False,
    )
    if result.returncode:
        # A failed listing must not read as an empty, passing one.
        sys.stderr.write(result.stderr.decode(errors="replace"))
        print("tracked .cortexkit check: git ls-files failed", file=sys.stderr)
        return 2
    tracked = [name.decode(errors="surrogateescape") for name in result.stdout.split(b"\0") if name]
    bad = [name for name in tracked if not ALLOWED.match(name)]
    for name in bad:
        print(f"{name}: tracked outside the .cortexkit allowlist", file=sys.stderr)
    print(f"tracked .cortexkit check: {len(bad)} violations")
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main())
