#!/usr/bin/env python3
"""Capture indexed source diffs and exact named-test failures for Cargo controls.

The pinned ckdev-mutate runner grades whether catalogue mutations are caught by
their declared test targets. This script adds the applied/restored Git diff and
an exact named-test failure; it does not replace that replay or coverage grading.
"""
import argparse
import json
from pathlib import Path
import shlex
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def command(row):
    args = ["cargo", "test", "--locked", "-p", row["package"], *shlex.split(row["target"])]
    if row.get("features"):
        args.extend(["--features", ",".join(row["features"])])
    if row.get("no_default_features"):
        args.append("--no-default-features")
    if row.get("all_features"):
        args.append("--all-features")
    return args + ["--", row["expect_red"][0], "--exact", "--nocapture"]


def witness(row, output):
    if row["runner"] != "cargo" or len(row["expect_red"]) != 1 or "edits" in row:
        raise SystemExit(f"{row['id']}: witness requires a single-file Cargo control with one oracle")
    path = ROOT / row["file"]
    subprocess.run(["git", "add", "--", row["file"]], cwd=ROOT, check=True)
    if git("diff", "--stat"):
        raise SystemExit("mutation safety: stage the live working state before running witnesses")
    original = path.read_text()
    if original.count(row["old"]) != 1:
        raise SystemExit(f"{row['id']}: expected exactly one anchor")
    args = command(row)
    baseline = subprocess.run(args, cwd=ROOT, text=True, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT, timeout=row.get("build_timeout_s", 3600))
    test = row["expect_red"][0]
    if baseline.returncode or f"test {test} ... ok" not in baseline.stdout:
        (output / f"{row['id']}-baseline.log").write_text(baseline.stdout)
        raise SystemExit(f"{row['id']}: named baseline did not execute and pass")
    result = None
    try:
        if "NON-VACUITY BREAK" not in row["new"]:
            raise SystemExit(f"{row['id']}: mutation lacks its safety marker")
        path.write_text(original.replace(row["old"], row["new"]))
        during = git("diff", "--stat")
        if not during:
            raise SystemExit("mutation safety: empty applied diff")
        try:
            result = subprocess.run(args, cwd=ROOT, text=True, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, timeout=row.get("build_timeout_s", 3600))
            text = result.stdout
        except subprocess.TimeoutExpired as error:
            text = (error.stdout or b"").decode(errors="replace")
    finally:
        subprocess.run(["git", "checkout", "--", row["file"]], cwd=ROOT, check=True)
        path.touch()
        after = git("diff", "--stat")
        if after:
            raise SystemExit("mutation safety: restored diff is not empty")
    (output / f"{row['id']}-mutant.log").write_text(text)
    expected = f"test {test} ... FAILED"
    red = result is not None and result.returncode != 0 and expected in text and "0 passed; 1 failed" in text
    reached = f"test {test} ..." in text
    proof = {
        "control": row["id"],
        "expected_red": test,
        "captured_output": expected + "; 0 passed; 1 failed; all other tests filtered" if red else text[-400:],
        "applied_evidence": f"{row['file']}; during: {during}; after: {after or '(empty)'}",
        "outcome": "reddened" if red else "hung" if result is None else "not_reached" if not reached else "undefended",
    }
    print(json.dumps(proof), flush=True)
    return proof


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", required=True, choices=["linux", "macos", "windows"])
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--shard", default="1/1")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    shard, total = map(int, args.shard.split("/"))
    if not 1 <= shard <= total:
        parser.error("shard must be in 1..total")
    args.output.mkdir(parents=True, exist_ok=True)
    rows = tomllib.loads((ROOT / "mutations.toml").read_text())["control"]
    selected = sorted((row for row in rows if row["platforms"] == [args.platform]
                       and (not args.only or row["id"] in args.only)), key=lambda row: row["id"])
    if not selected or set(args.only) - {row["id"] for row in selected}:
        raise SystemExit("no controls selected or an unknown control requested")
    proofs = []
    for index, row in enumerate(selected):
        if index % total != shard - 1:
            continue
        proofs.append(witness(row, args.output))
        (args.output / "mutation-evidence.json").write_text(json.dumps(proofs, indent=2))
        if proofs[-1]["outcome"] != "reddened":
            raise SystemExit(f"{row['id']}: exact named oracle did not redden")
    print(f"Mutation witnesses: {len(proofs)} exact named failures, all sources restored", flush=True)


if __name__ == "__main__":
    main()
