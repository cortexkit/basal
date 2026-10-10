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
    metadata = [{
        "sequence": variant["sequence"], "pid": variant.get("pid"),
        "exit_code": variant.get("exit_code"), "error": variant.get("error"),
        "completed_probe_count": len(variant["child"]["probes"]) if "probes" in variant.get("child", {}) else None,
        "child": {key: value for key, value in variant.get("child", {}).items() if key != "probes"},
        "parent_handle_inspection": variant.get("parent_handle_inspection"),
        "handle_trace_enablement": variant.get("handle_trace_enablement"),
        "loader_threads": variant.get("loader_threads"), "loader_setting": variant.get("loader_setting"),
        "connections": variant.get("debug", {}).get("trace", {}).get("connections") if isinstance(variant.get("debug", {}).get("trace"), dict) else None,
        "handles_before_resume": variant.get("parent_before_resume", {}).get("handles"),
        "stdio_handles": variant.get("stdio_handles"),
    } for variant in variants]
    metadata_output = destination / f"{run_id}-{image}-metadata.json"
    metadata_output.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
    lines = ["# Pre-input handle campaign", "", f"Source `{source_sha}`; run {run_id}; {image}.", "",
             "Each row is a separately launched GUI worker. Counts include only reported probe results and the reported pre-input inventory (including stdio). A missing report is not an empty inventory.", "",
             "| Recipe | PID | Exit | Reported probes | Successful operations | Reported pre-input handles |",
             "|---|---:|---|---:|---|---:|"]
    for variant in variants:
        child = variant.get("child", {})
        probes = child.get("probes", [])
        successes = [f"{probe['kind']}: {probe['target']}" for probe in probes if probe.get("success")]
        probe_count = len(child["probes"]) if "probes" in child else "not reported"
        handle_count = len(child["handle_table"]) if "handle_table" in child else "not reported"
        lines.append(f"| {variant['sequence']} | {variant.get('pid', '')} | {variant.get('exit_code', variant.get('error', ''))} | {probe_count} | {', '.join(successes)} | {handle_count} |")

    lines += ["", "Thread and scheduler gap evidence is rendered separately by retain_gaps.py. No token or capability conclusion is inferred from missing query results."]
    summary = destination / f"{run_id}-{image}.md"
    summary.write_text("\n".join(lines) + "\n", encoding="utf-8")
    manifest = {
        "run_id": str(run_id), "source_sha": source_sha, "image": image,
        "raw_report_sha256": hashlib.sha256((source / "report.json").read_bytes()).hexdigest(),
        "retained_sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
        "summary_sha256": hashlib.sha256(summary.read_bytes()).hexdigest(),
        "metadata_sha256": hashlib.sha256(metadata_output.read_bytes()).hexdigest(),
    }
    (destination / f"{run_id}-{image}-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print("\n".join(lines))


if __name__ == "__main__":
    retain(*sys.argv[1:])
