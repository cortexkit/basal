"""Require one replay platform per proof and exactly one successful replay per ID.

The catalogue is read by script/mutation_catalogue.py, the same loader the
other catalogue tools use, so a single file or a mutations.d/ directory both
work, and every error names the file and line that defines the row.

Uses Python 3.11+ for the standard-library TOML parser.
"""

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "script"))
import mutation_catalogue  # noqa: E402  (needs the script directory on the path)


PLATFORMS = ("linux", "macos", "windows")
PORTABLE_PACKAGES = {"basal-proto", "basal-core", "basal-host"}
SUCCESS = {"CAUGHT", "CAUGHT_BROADLY", "HUB", "EQUIVALENT", "UNREACHABLE"}
ARTIFACT_PATTERN = re.compile(r"mutations-(linux|macos|windows)-[1-9][0-9]*")
# These files contain macOS-specific F_GETPATH and verified-open guards. A
# mutation qualifies for macOS only when it replaces a complete cfg(macos)
# function; shared Unix callsites stay on Linux to avoid duplicate shard work.
MACOS_NATIVE_FILES = {
    "crates/basal-host/src/builtins/git.rs",
    "crates/basal-host/src/builtins/fs.rs",
}


def macos_native_row(row):
    """Require every edit to replace one complete macOS-only function."""
    if row.get("package") != "basal-host":
        return False
    edits = list(row.get("edits", []))
    if row.get("file"):
        edits.insert(0, row)
    if not edits:
        return False
    for edit in edits:
        if edit.get("file") not in MACOS_NATIVE_FILES:
            return False
        old = edit.get("old", "").strip()
        if not re.match(r'#\[cfg\(target_os = "macos"\)\]\s+(?:pub(?:\([^)]*\))?\s+)?fn\s+', old):
            return False
        # Whole-function anchors make the cfg boundary explicit in the row.
        # Reject trailing items and unbalanced bodies rather than guessing scope.
        start = old.find("{")
        if start == -1:
            return False
        body = old[start:]
        # Ignore regular literals and line comments while counting syntax
        # braces. More complex Rust raw literals/block comments fail closed.
        if "/*" in body or re.search(r'\b(?:b?r|cr)#*"', body):
            return False
        body = re.sub(r'''//[^\n]*|"(?:\\.|[^"\\])*"|'(?:[^'\\]|\\(?:[nrt0'"\\]|x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}))' ''', "", body, flags=re.VERBOSE)
        depth = 0
        for index, character in enumerate(body):
            if character == "{":
                depth += 1
            elif character == "}":
                depth -= 1
                if depth == 0 and index != len(body) - 1:
                    return False
                if depth < 0:
                    return False
        if depth != 0 or not old.endswith("}"):
            return False
    return True


def label(identity, sources):
    """Name a row by its id and, when known, the file and line defining it."""
    source = (sources or {}).get(identity)
    return f"{identity} ({source})" if source else identity


def partition(rows, sources=None):
    """Fail closed when a proof has no CI host, two hosts, or an unsafe host."""
    selected = {platform: set() for platform in PLATFORMS}
    seen = set()
    for row in rows:
        name = label(row["id"], sources)
        platforms = row.get("platforms", PLATFORMS)
        hosts = set(platforms) & set(PLATFORMS)
        if len(platforms) != 1 or len(hosts) != 1:
            raise ValueError(f"{name}: expected exactly one CI platform, got {sorted(hosts)}")
        host = hosts.pop()
        package = row.get("package")
        file = Path(row.get("file", ""))
        windows_module = "windows" in file.parts or file.name == "windows.rs"
        if package in PORTABLE_PACKAGES and host != "linux" and not (
            host == "windows" and windows_module
            or host == "macos" and macos_native_row(row)
        ):
            raise ValueError(f"{name}: {package} proofs belong on Linux unless targeting a Windows-only module on Windows")
        if row["id"] in seen:
            raise ValueError(f"duplicate catalogue ID: {name}")
        seen.add(row["id"])
        selected[host].add(row["id"])
    return selected


def report_label(row, sources):
    """Name a reported row by the catalogue's source, else the report's own."""
    if row["id"] in (sources or {}):
        return label(row["id"], sources)
    source = row.get("source")
    if isinstance(source, dict) and source.get("file"):
        line = source.get("line")
        return f"{row['id']} ({source['file']}{f':{line}' if line else ''})"
    return row["id"]


def verify_reports(selected, reports, sources=None):
    """Compare actual runner reports, not a second calculation of shard assignment."""
    expected = set.union(*selected.values())
    executed = {platform: Counter() for platform in PLATFORMS}
    reported = {platform: Counter() for platform in PLATFORMS}
    for platform, rows in reports:
        for row in rows:
            identity = row["id"]
            name = report_label(row, sources)
            reported[platform][identity] += 1
            if row["outcome"] == "SKIPPED_PLATFORM":
                if identity in selected[platform]:
                    raise ValueError(f"{name}: unexpectedly skipped on {platform}")
                continue
            if identity not in selected[platform]:
                raise ValueError(f"{name}: replayed on the wrong platform: {platform}")
            if row["outcome"] not in SUCCESS:
                raise ValueError(f"{name}: unsuccessful replay: {row['outcome']}")
            executed[platform][identity] += 1
    for platform in PLATFORMS:
        # Each host shards the entire catalogue; the runner reports the other
        # hosts' rows as skips. Checking every view also detects a missing shard.
        wanted = Counter({identity: 1 for identity in expected})
        if reported[platform] != wanted:
            raise ValueError(f"{platform}: reports must contain every catalogue ID exactly once; "
                             + mismatch(reported[platform], wanted, sources))
        wanted = Counter({identity: 1 for identity in selected[platform]})
        if executed[platform] != wanted:
            raise ValueError(f"{platform}: each selected ID must be replayed exactly once; "
                             + mismatch(executed[platform], wanted, sources))
    return {platform: sum(counts.values()) for platform, counts in executed.items()}


def mismatch(actual, wanted, sources, limit=5):
    """Describe a few missing or repeated IDs, naming the files defining them."""
    missing = sorted(identity for identity in wanted if actual[identity] < wanted[identity])
    extra = sorted(identity for identity in actual if actual[identity] > wanted[identity])
    parts = []
    for title, identities in (("missing", missing), ("unexpected or repeated", extra)):
        if identities:
            shown = ", ".join(label(identity, sources) for identity in identities[:limit])
            more = f" and {len(identities) - limit} more" if len(identities) > limit else ""
            parts.append(f"{title}: {shown}{more}")
    return "; ".join(parts)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalogue", type=Path,
                        help="catalogue file or directory (default: mutations.d/ or mutations.toml at the root)")
    parser.add_argument("--reports", type=Path)
    args = parser.parse_args()
    rows, sources = mutation_catalogue.load(ROOT, args.catalogue)
    selected = partition(rows, sources)
    counts = {platform: len(ids) for platform, ids in selected.items()}
    if args.reports:
        reports = []
        for path in sorted(args.reports.glob("mutations-*/all.json")):
            match = ARTIFACT_PATTERN.fullmatch(path.parent.name)
            if not match:
                raise ValueError(f"unrecognized mutation artifact: {path.parent.name}")
            reports.append((match[1], json.loads(path.read_text())))
        counts = verify_reports(selected, reports, sources)
    print(f"Mutation coverage: linux={counts['linux']}, macos={counts['macos']}, windows={counts['windows']}, "
          f"total={sum(counts.values())} catalogue rows; exactly one platform per row")


if __name__ == "__main__":
    main()
