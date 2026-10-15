"""CI witnesses: a disclaimed cat must fail calibration; a plain worker must fail isolation."""

import json
import os
from pathlib import Path
import re
import subprocess
import tomllib


def main():
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.uname().sysname != "Darwin":
        raise SystemExit("Mutation witnesses require a GitHub macOS runner")
    out = Path(os.environ["RUNNER_TEMP"]) / "privacy-mutations"
    out.mkdir()
    rows = tomllib.loads(Path("mutations.d/module.toml").read_text())["control"]
    ids = ["plain-privacy-calibration-disclaims-child", "production-worker-inherits-parent-privacy-identity"]
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
        assert region.count(row["old"]) == 1, id
        path.write_text(prefix + region.replace(row["old"], row["new"], 1))
        applied = subprocess.check_output(["git", "diff", "--stat"], text=True)
        assert applied, id
        try:
            result = subprocess.run(["cargo", "test", "--locked", "-p", "basal-module", "--test", "e2e",
                                     "--", "--nocapture"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                    text=True, timeout=600)
            (out / f"{id}.log").write_text(result.stdout)
            failed = re.findall(r"^test (\S+) \.\.\. FAILED$", result.stdout, re.MULTILINE)
            passed = re.findall(r"^test (\S+) \.\.\. ok$", result.stdout, re.MULTILINE)
        finally:
            subprocess.run(["git", "checkout", "--", str(path)], check=True)
            path.touch()
            restored = subprocess.check_output(["git", "diff", "--stat"], text=True)
            assert not restored, restored
        report = {"id": id, "path": str(path), "applied_diff_stat": applied,
                  "restored_diff_stat": restored, "failed": failed, "passed": passed,
                  "returncode": result.returncode}
        reports.append(report)
        (out / "summary.json").write_text(json.dumps(reports, indent=2) + "\n")
        assert result.returncode != 0 and failed == row["expect_red"] and len(passed) == 5, report
        print(json.dumps(report), flush=True)


if __name__ == "__main__":
    main()
