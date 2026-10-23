#!/usr/bin/env python3
"""Capture indexed source diffs and named-test run sets for Cargo controls.

The pinned ckdev-mutate runner replays catalogue mutations against their declared
test targets. This script supplements that replay with applied/restored Git diffs,
a named failure and any declared passing siblings. It does not replace the runner's
catch or coverage grading. Rows without a run set use only their named oracle.
"""
import argparse
import json
from pathlib import Path
import re
import shlex
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mutation_catalogue  # noqa: E402  (needs the script directory on the path)

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
    selection = row.get("select")
    if isinstance(selection, dict) and "tests" in selection:
        filters = [*selection["tests"], "--exact"]
    elif isinstance(selection, dict) and "filter" in selection:
        filters = [selection["filter"]]
    else:
        filters = [row["expect_red"][0], "--exact"]
    # Keep test output captured until libtest finishes its status lines. Live
    # output can interleave with those lines and make the results unparseable.
    return args + ["--", *filters, "--show-output"]


def test_results(text):
    # With --show-output, captured test text follows these headings. Do not
    # mistake a test's own output for libtest's status lines.
    text = re.split(r"^(?:failures|successes):\s*$", text, maxsplit=1, flags=re.MULTILINE)[0]
    return dict(re.findall(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b", text, re.MULTILINE))


def complete_selection(row, results):
    selection = row.get("select")
    if isinstance(selection, dict) and "tests" in selection:
        return set(results) == set(selection["tests"])
    if isinstance(selection, dict) and "filter" in selection:
        return (len(results) >= 2 and row["expect_red"][0] in results
                and all(selection["filter"] in name for name in results))
    return set(results) == {row["expect_red"][0]}


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
    (output / f"{row['id']}-baseline.log").write_text(baseline.stdout)
    test = row["expect_red"][0]
    baseline_results = test_results(baseline.stdout)
    if (baseline.returncode or baseline_results.get(test) != "ok"
            or not complete_selection(row, baseline_results)
            or any(status != "ok" for status in baseline_results.values())):
        raise SystemExit(f"{row['id']}: named baseline did not execute and pass")
    result = None
    try:
        if "NON-VACUITY BREAK" not in row["new"]:
            raise SystemExit(f"{row['id']}: mutation lacks its safety marker")
        path.write_text(original.replace(row["old"], row["new"]))
        during = git("diff", "--stat")
        if not during:
            raise SystemExit("mutation safety: empty applied diff")
        applied = git("diff", "--", row["file"])
        (output / f"{row['id']}-applied.diff").write_text(applied + "\n")
        prefix = f"Applied mutation {row['id']}:\n{applied}\nDuring diff stat:\n{during}\n"
        print(prefix, flush=True)
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
        stats = f"during: {during}\nafter: {after or '(empty)'}\n"
        (output / f"{row['id']}-stats.log").write_text(stats)
        print(f"Restored {row['file']}: {after or '(empty)'}", flush=True)
    (output / f"{row['id']}-mutant.log").write_text(prefix + text)
    results = test_results(text)
    red = (result is not None and result.returncode != 0 and results.get(test) == "FAILED"
           and complete_selection(row, results)
           and all(status == "ok" for name, status in results.items() if name != test))
    reached = test in results
    summary = "; ".join(f"{name}: {status}" for name, status in sorted(results.items()))
    proof = {
        "control": row["id"],
        "expected_red": test,
        "captured_output": summary if red else text[-400:],
        "applied_evidence": f"{row['file']}; during: {during}; after: {after or '(empty)'}",
        "outcome": "reddened" if red else "hung" if result is None else "not_reached" if not reached else "undefended",
    }
    print(json.dumps(proof), flush=True)
    return proof


def select(rows, platform, only):
    """Pick this platform's rows, sorted by id; every requested id must be among them."""
    selected = sorted((row for row in rows if row["platforms"] == [platform]
                       and (not only or row["id"] in only)), key=lambda row: row["id"])
    if not selected or set(only) - {row["id"] for row in selected}:
        raise SystemExit("no controls selected or an unknown control requested")
    return selected


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
    rows, sources = mutation_catalogue.load(ROOT)
    selected = select(rows, args.platform, args.only)
    proofs = []
    for index, row in enumerate(selected):
        if index % total != shard - 1:
            continue
        try:
            proofs.append(witness(row, args.output))
        except SystemExit as error:
            # Name the catalogue file and line, so the failing row can be found.
            raise SystemExit(f"{error} (defined at {sources[row['id']]})") from None
        (args.output / "mutation-evidence.json").write_text(json.dumps(proofs, indent=2))
        if proofs[-1]["outcome"] != "reddened":
            raise SystemExit(f"{row['id']}: exact named oracle did not redden (defined at {sources[row['id']]})")
    print(f"Mutation witnesses: {len(proofs)} exact named failures, all sources restored", flush=True)


if __name__ == "__main__":
    main()
