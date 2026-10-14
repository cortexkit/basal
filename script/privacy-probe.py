"""Temporary stress driver restricted to GitHub-hosted macOS CI runners."""

import concurrent.futures
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.uname().sysname != "Darwin":
        raise SystemExit("This stress probe is restricted to GitHub macOS runners")
    tmp = Path(os.environ["RUNNER_TEMP"])
    out = tmp / "privacy-probe"
    out.mkdir()
    artifacts = [json.loads(line) for line in (tmp / "build.jsonl").read_text().splitlines()]
    binaries = [a["executable"] for a in artifacts if a.get("reason") == "compiler-artifact"
                and a.get("target", {}).get("name") == "e2e" and a.get("executable")
                and "basal-module" in a.get("package_id", "")]
    assert len(binaries) == 1, binaries
    env = dict(os.environ, DYLD_INSERT_LIBRARIES=str(tmp / "privacy-spawn-probe.dylib"))
    for label, child_env in [("injected", env), ("plain", os.environ)]:
        cat = subprocess.run(["/bin/cat"], input=b"R", stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, env=child_env)
        (out / f"cat-preflight-{label}.json").write_text(json.dumps({
            "status": cat.returncode, "stdout": cat.stdout.decode(errors="replace"),
            "stderr": cat.stderr.decode(errors="replace")}) + "\n")
    cores = os.cpu_count() or 2
    results = []
    loads = []
    started = time.monotonic()

    def run(slot, iteration, full):
        name = f"{'full' if full else 'single'}-{slot}-{iteration}"
        args = [binaries[0], "--nocapture"]
        if not full:
            args += ["--exact", "privacy::production_worker_is_its_own_responsible_process"]
        before = time.monotonic()
        with (out / f"{name}.log").open("w") as log:
            try:
                status = subprocess.run(args, env=os.environ if full else env,
                                        stdout=log, stderr=subprocess.STDOUT,
                                        timeout=360).returncode
            except subprocess.TimeoutExpired:
                status = "timeout"
        return {"name": name, "status": status, "seconds": time.monotonic() - before}

    try:
        loads = [subprocess.Popen(["/usr/bin/yes"], stdout=subprocess.DEVNULL) for _ in range(cores)]
        with (out / "workspace.log").open("w") as workspace_log:
            workspace = subprocess.Popen(["cargo", "test", "--locked", "--workspace"],
                                         stdout=workspace_log, stderr=subprocess.STDOUT)
            try:
                def lane(slot):
                    return [run(slot, iteration, slot % 2 == 0) for iteration in range(100)]

                with concurrent.futures.ThreadPoolExecutor(max_workers=cores * 2) as pool:
                    for lane_results in pool.map(lane, range(cores * 2)):
                        results.extend(lane_results)
                workspace_status = workspace.wait(timeout=600)
            finally:
                if workspace.poll() is None:
                    workspace.kill()
                    workspace.wait()
    finally:
        for load in loads:
            load.terminate()
        for load in loads:
            load.wait()
    summary = {"cores": cores, "runs": len(results), "results": results,
               "failed": sum(r["status"] != 0 for r in results),
               "workspace_status": workspace_status, "seconds": time.monotonic() - started}
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({k: v for k, v in summary.items() if k != "results"}), flush=True)
    raise SystemExit(1 if summary["failed"] or workspace_status else 0)


if __name__ == "__main__":
    main()
