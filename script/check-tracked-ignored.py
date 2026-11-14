#!/usr/bin/env python3
"""Refuse tracked files that the repository's ignore rules exclude under .cortexkit/.

Internal documents and maintainer scripts live in ignored .cortexkit/ folders and
must never be force-added. Only the paths .gitignore re-includes (WORKER.md,
release notes, skills, rules) may be tracked there.
"""

import argparse
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    result = subprocess.run(
        ["git", "ls-files", "-ci", "--exclude-standard", "-z", "--", ".cortexkit"],
        cwd=args.root.resolve(),
        capture_output=True,
        check=False,
    )
    if result.returncode:
        sys.stderr.write(result.stderr.decode(errors="replace"))
        return 2
    tracked = [name.decode(errors="surrogateescape") for name in result.stdout.split(b"\0") if name]
    for name in tracked:
        print(f"{name}: ignored .cortexkit file is tracked", file=sys.stderr)
    print(f"tracked ignored check: {len(tracked)} violations")
    return 1 if tracked else 0


if __name__ == "__main__":
    raise SystemExit(main())
