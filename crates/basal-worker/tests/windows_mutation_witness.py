#!/usr/bin/env python3
"""Temporarily remove a Windows guard, require its named test to fail, then
restore the source from Git's index and require the test to pass again.
"""
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[3]
OUTPUT = pathlib.Path(sys.argv[1])
OUTPUT.mkdir(parents=True, exist_ok=True)

CONTROLS = [
    {
        "file": "crates/basal-worker/src/confinement/windows/startup.rs",
        "old": "    allowlist::check(&table).map_err(|error| Refusal::new(reason::HANDLE_NOT_ALLOWED, error))?;",
        "new": "    let _ = allowlist::check(&table); // NON-VACUITY BREAK",
        "target": "windows_probes",
        "test": "planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture",
        "description": "remove startup allowlist enforcement",
        "witness": "fixture bytes [101, 115, 99, 97, 112, 101, 100]",
    },
    {
        "file": "crates/basal-worker/tests/data/windows-imports.txt",
        "old": "kernel32.dll\n",
        "new": "# NON-VACUITY BREAK\n",
        "target": "windows_image",
        "test": "the_worker_is_a_gui_image_without_gui_com_or_c_runtime_imports",
        "description": "remove the real kernel32 import from the committed inventory",
        "witness": "kernel32.dll is not in",
    },
]

def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()

proofs = []
for control in CONTROLS:
    path = ROOT / control["file"]
    subprocess.run(["git", "add", "--", control["file"]], cwd=ROOT, check=True)
    if git("diff", "--stat"):
        raise SystemExit("mutation safety: working changes remain after staging")
    text = path.read_text()
    if text.count(control["old"]) != 1:
        raise SystemExit("mutation safety: expected exactly one guard")
    try:
        path.write_text(text.replace(control["old"], control["new"]))
        during = git("diff", "--stat")
        if not during:
            raise SystemExit("mutation safety: empty applied diff")
        command = ["cargo", "test", "-p", "basal-worker", "--locked", "--test", control["target"], "--", control["test"], "--exact", "--nocapture"]
        result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=300)
        (OUTPUT / f"mutation-{control['test']}.log").write_text(result.stdout)
        expected = f"test {control['test']} ... FAILED"
        reached = control["witness"] in result.stdout
        red = result.returncode != 0 and expected in result.stdout and "0 passed; 1 failed" in result.stdout and reached
    finally:
        subprocess.run(["git", "checkout", "--", control["file"]], cwd=ROOT, check=True)
        path.touch()
        after = git("diff", "--stat")
        if after:
            raise SystemExit("mutation safety: restored diff is not empty")
    proof = {
        "control": control["file"] + ": " + control["description"],
        "expected_red": control["test"],
        "captured_output": expected + "; 0 passed; 1 failed; other tests filtered; " + control["witness"],
        "applied_evidence": f"{control['file']}; during: {during}; after: {after or '(empty)'}",
        "outcome": "reddened" if red else "not_reached" if not reached else "undefended",
    }
    proofs.append(proof)
    (OUTPUT / "mutation-evidence.json").write_text(json.dumps(proofs, indent=2))
    print(json.dumps(proof), flush=True)
    if not red:
        raise SystemExit("named mutation oracle did not redden with the fixture-write witness")
    subprocess.run(command, cwd=ROOT, check=True)
