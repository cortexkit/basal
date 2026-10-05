#!/usr/bin/env python3
"""Refuse Cargo path dependencies that resolve outside the selected workspace."""

import argparse
import json
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    default_manifest = Path(__file__).resolve().parents[1] / "Cargo.toml"
    parser.add_argument(
        "--manifest-path",
        type=Path,
        default=default_manifest,
        help="workspace manifest to inspect (default: repository root)",
    )
    args = parser.parse_args()

    command = ["cargo", "metadata", "--format-version", "1", "--locked", "--offline"]
    command.extend(["--manifest-path", str(args.manifest_path.resolve())])
    result = subprocess.run(command, text=True, capture_output=True, check=False)
    if result.returncode:
        sys.stderr.write(result.stderr)
        return result.returncode

    metadata = json.loads(result.stdout)
    root = Path(metadata["workspace_root"]).resolve()
    violations = []
    for package in metadata["packages"]:
        if package["source"] is not None:
            continue
        manifest_path = Path(package["manifest_path"]).resolve()
        if manifest_path != root and root not in manifest_path.parents:
            violations.append((package["name"], manifest_path))

    for name, manifest_path in violations:
        print(f"{name}: local package resolves outside workspace root: {manifest_path}", file=sys.stderr)
    return 1 if violations else 0


if __name__ == "__main__":
    raise SystemExit(main())
