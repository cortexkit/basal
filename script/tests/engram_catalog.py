import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit("usage: engram_catalog.py <written-catalog> <golden-file>")

    diff = subprocess.run(
        ["git", "diff", "--stat"], cwd=ROOT, capture_output=True, text=True, check=True
    )
    print("mutation diff stat while catalog mutant is applied:\n" + diff.stdout)

    written = Path(sys.argv[1]).read_bytes()
    golden = Path(sys.argv[2]).read_bytes()
    if written != golden:
        raise SystemExit("written catalog bytes do not match the checked-in golden file")

    descriptor = json.loads(written)
    if descriptor.get("schema_version") != 1 or descriptor.get("module_id") != "basal":
        raise SystemExit("catalog schema version or module id is incorrect")
    if len(descriptor.get("entries", [])) != 4:
        raise SystemExit("catalog must describe the two store files and two excluded folders")

    print("catalog bytes match the golden descriptor")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
