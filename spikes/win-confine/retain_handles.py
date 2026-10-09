"""Retain GUI startup handle identities, close trials and probes with source hashes.

The parent target list is omitted because each child's probes already retain the
attempted targets and results.
"""
import hashlib
import json
import pathlib
import sys


def retain(source, destination, run_id, source_sha, image):
    source = pathlib.Path(source)
    destination = pathlib.Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    report = json.loads((source / "report.json").read_text(encoding="utf-8-sig"))
    variants = [
        diagnostic["result"]
        for run in report["runs"]
        for diagnostic in run.get("diagnostics", [])
        if diagnostic.get("result", {}).get("sequence", "").startswith("full-gui-")
    ]
    retained = {
        "run_id": str(run_id), "source_sha": source_sha, "image": image,
        "target_count": len(report["targets"]), "profile_cleanup": report["profile_cleanup"],
        "variants": variants,
    }
    output = destination / f"{run_id}-{image}.json"
    output.write_text(json.dumps(retained, indent=2) + "\n", encoding="utf-8")
    lines = ["# Pre-input handle campaign", "", f"Source `{source_sha}`; run {run_id}; {image}.", "",
             "| Recipe | PID | Exit | Probes | Successful operations | Pre-input handles |",
             "|---|---:|---|---:|---|---:|"]
    for variant in variants:
        child = variant.get("child", {})
        probes = child.get("probes", [])
        successes = [f"{probe['kind']}: {probe['target']}" for probe in probes if probe.get("success")]
        lines.append(f"| {variant['sequence']} | {variant.get('pid', '')} | {variant.get('exit_code', variant.get('error', ''))} | {len(probes)} | {', '.join(successes)} | {len(child.get('handle_table', []))} |")
    summary = destination / f"{run_id}-{image}.md"
    summary.write_text("\n".join(lines) + "\n", encoding="utf-8")
    manifest = {
        "run_id": str(run_id), "source_sha": source_sha, "image": image,
        "raw_report_sha256": hashlib.sha256((source / "report.json").read_bytes()).hexdigest(),
        "retained_sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
        "summary_sha256": hashlib.sha256(summary.read_bytes()).hexdigest(),
    }
    (destination / f"{run_id}-{image}-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print("\n".join(lines))


if __name__ == "__main__":
    retain(*sys.argv[1:])
