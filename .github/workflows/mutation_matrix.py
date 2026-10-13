#!/usr/bin/env python3
"""Emit the mutation job matrix as a GitHub Actions output."""

import argparse
import json


RUNNERS = {"linux": "ubuntu-latest", "macos": "macos-latest", "windows": "windows-2022"}
REGULAR_SHARDS = {"linux": 2, "macos": 5, "windows": 2}
# Broad replays exceed the six-hour hosted-job limit at the regular counts.
# Timing estimates and the censored Linux measurements are recorded in
# evidence/mutation-nightly-breadth-repair.md.
NIGHTLY_SHARDS = {"linux": 16, "macos": 12, "windows": 2}


def matrix_for(event):
    counts = NIGHTLY_SHARDS if event == "schedule" else REGULAR_SHARDS
    return [
        {"platform": platform, "os": RUNNERS[platform], "shard": shard, "shards": count}
        for platform, count in counts.items()
        for shard in range(1, count + 1)
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", required=True)
    args = parser.parse_args()
    print("include=" + json.dumps(matrix_for(args.event), separators=(",", ":")))


if __name__ == "__main__":
    main()
