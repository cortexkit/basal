"""Require one replay platform per proof and exactly one successful replay per ID.

Uses Python 3.11+ for the standard-library TOML parser.
"""

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import tomllib


PLATFORMS = ("linux", "macos")
PORTABLE_PACKAGES = {"basal-proto", "basal-core", "basal-host"}
SUCCESS = {"CAUGHT", "CAUGHT_BROADLY", "HUB", "EQUIVALENT", "UNREACHABLE"}


def partition(rows):
    """Fail closed when a proof has no CI host, two hosts, or an unsafe host."""
    selected = {platform: set() for platform in PLATFORMS}
    seen = set()
    for row in rows:
        hosts = set(row.get("platforms", PLATFORMS)) & set(PLATFORMS)
        if len(hosts) != 1:
            raise ValueError(f"{row['id']}: expected exactly one CI platform, got {sorted(hosts)}")
        host = hosts.pop()
        package = row.get("package")
        if package in PORTABLE_PACKAGES and host != "linux":
            raise ValueError(f"{row['id']}: {package} proofs belong on Linux")
        if row["id"] in seen:
            raise ValueError(f"duplicate catalogue ID: {row['id']}")
        seen.add(row["id"])
        selected[host].add(row["id"])
    return selected


def verify_reports(selected, reports):
    """Compare actual runner reports, not a second calculation of shard assignment."""
    expected = set.union(*selected.values())
    executed = {platform: Counter() for platform in PLATFORMS}
    reported = {platform: Counter() for platform in PLATFORMS}
    for platform, rows in reports:
        for row in rows:
            identity = row["id"]
            reported[platform][identity] += 1
            if row["outcome"] == "SKIPPED_PLATFORM":
                if identity in selected[platform]:
                    raise ValueError(f"{identity}: unexpectedly skipped on {platform}")
                continue
            if identity not in selected[platform]:
                raise ValueError(f"{identity}: replayed on the wrong platform: {platform}")
            if row["outcome"] not in SUCCESS:
                raise ValueError(f"{identity}: unsuccessful replay: {row['outcome']}")
            executed[platform][identity] += 1
    for platform in PLATFORMS:
        # Each host shards the entire catalogue; the runner reports the other
        # host's rows as skips. Checking both views also detects a missing shard.
        if reported[platform] != Counter({identity: 1 for identity in expected}):
            raise ValueError(f"{platform}: reports must contain every catalogue ID exactly once")
        if executed[platform] != Counter({identity: 1 for identity in selected[platform]}):
            raise ValueError(f"{platform}: each selected ID must be replayed exactly once")
    return {platform: sum(counts.values()) for platform, counts in executed.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalogue", type=Path, default=Path("mutations.toml"))
    parser.add_argument("--reports", type=Path)
    args = parser.parse_args()
    with args.catalogue.open("rb") as catalogue:
        selected = partition(tomllib.load(catalogue)["control"])
    counts = {platform: len(ids) for platform, ids in selected.items()}
    if args.reports:
        reports = []
        for path in sorted(args.reports.glob("mutations-*/all.json")):
            match = re.fullmatch(r"mutations-(linux|macos)-[1-9][0-9]*", path.parent.name)
            if not match:
                raise ValueError(f"unrecognized mutation artifact: {path.parent.name}")
            reports.append((match[1], json.loads(path.read_text())))
        counts = verify_reports(selected, reports)
    print(f"Mutation coverage: linux={counts['linux']}, macos={counts['macos']}, "
          f"total={sum(counts.values())} catalogue rows; exactly one platform per row")


if __name__ == "__main__":
    main()
