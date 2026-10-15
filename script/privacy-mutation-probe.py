"""Run three deliberate faults and require the privacy test to fail.

The faults give the control cat its own responsibility identity, let a worker
inherit the module's identity, and stop giving the observer an explicit live
identity. Each fault is applied to indexed source and restored after its test.
"""

import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import subprocess
import tomllib


def main():
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.uname().sysname != "Darwin":
        raise SystemExit("Mutation witnesses require a GitHub macOS runner")
    out = Path(os.environ["RUNNER_TEMP"]) / "privacy-mutations"
    out.mkdir()
    rows = tomllib.loads(Path("mutations.d/module.toml").read_text())["control"]
    ids = ["plain-privacy-calibration-disclaims-child", "production-worker-inherits-parent-privacy-identity",
           "privacy-probe-borrows-inherited-originator"]
    reports = []
    for id in ids:
        row = next(row for row in rows if row["id"] == id)
        path = Path(row["file"])
        subprocess.run(["git", "add", str(path)], check=True)
        before = subprocess.check_output(["git", "diff", "--stat"], text=True)
        assert not before, before
        source = path.read_text()
        start = source.index(row["scope"]["after"]) + len(row["scope"]["after"]) if "scope" in row else 0
        prefix, region = source[:start], source[start:]
        for edit in [{"old": row["old"], "new": row["new"]}, *row["edits"]]:
            assert region.count(edit["old"]) == 1, id
            region = region.replace(edit["old"], edit["new"], 1)
        path.write_text(prefix + region)
        applied = subprocess.check_output(["git", "diff", "--stat"], text=True)
        assert applied, id
        try:
            process = subprocess.Popen([str(Path(os.environ["RUNNER_TEMP"]) / "privacy-launch"),
                "--orphan", shutil.which("cargo"), "test", "--locked", "-p", "basal-module",
                "--test", "e2e", "--", "--nocapture"],
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
            process.wait(timeout=600)
            prefix = b""
            while True:
                ready, _, _ = select.select([process.stdout], [], [], 30)
                if not ready:
                    raise subprocess.TimeoutExpired(process.args, 30)
                chunk = os.read(process.stdout.fileno(), 65536)
                if not chunk:
                    raise RuntimeError("originator exited without suspended cargo")
                prefix += chunk
                child = re.search(rb"ORIGINATOR_PROBE child=(\d+) result=0", prefix)
                if child:
                    os.kill(int(child[1]), signal.SIGCONT)
                    break
            output = prefix.decode(errors="replace") + process.communicate(timeout=600)[0]
            (out / f"{id}.log").write_text(output)
            failed = re.findall(r"^test (\S+) \.\.\. FAILED$", output, re.MULTILINE)
            passed = re.findall(r"^test (\S+) \.\.\. ok$", output, re.MULTILINE)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=30)
            raise
        finally:
            subprocess.run(["git", "checkout", "--", str(path)], check=True)
            path.touch()
            restored = subprocess.check_output(["git", "diff", "--stat"], text=True)
            assert not restored, restored
        report = {"id": id, "path": str(path), "applied_diff_stat": applied,
                  "restored_diff_stat": restored, "failed": failed, "passed": passed,
                  "launcher_returncode": process.returncode}
        reports.append(report)
        (out / "summary.json").write_text(json.dumps(reports, indent=2) + "\n")
        assert failed == row["expect_red"] and len(passed) == 5, report
        if id == "privacy-probe-borrows-inherited-originator":
            assert "plain control inheritance:" in output, output
        print(json.dumps(report), flush=True)


if __name__ == "__main__":
    main()
