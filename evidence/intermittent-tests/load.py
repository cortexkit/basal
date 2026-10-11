"""Run a libtest binary repeatedly beside CPU-heavy processes on Linux."""

import argparse
import concurrent.futures
import hashlib
import multiprocessing
import re
import subprocess


def burn_cpu():
    data = b"basal-load" * 1000
    while True:
        data = hashlib.sha256(data).digest() * 1000


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary")
    parser.add_argument("--filter", default="")
    parser.add_argument("--tests", type=int, required=True)
    parser.add_argument("--iterations", type=int, default=50)
    parser.add_argument("--jobs", type=int, default=1)
    parser.add_argument("--cpu-workers", type=int, default=8)
    parser.add_argument("--label", required=True)
    args = parser.parse_args()
    print(subprocess.check_output(["rustc", "--version"], text=True).strip(), flush=True)
    print(subprocess.check_output(["python3", "--version"], text=True).strip(), flush=True)
    listing = subprocess.check_output([args.binary, args.filter, "--list"], text=True)
    assert re.search(rf"^{args.tests} tests?,", listing, re.M), listing

    def run(attempt):
        result = subprocess.run(
            [args.binary, args.filter, "--test-threads=64", "--nocapture"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=120,
        )
        assert re.search(rf"^running {args.tests} tests?$", result.stdout, re.M), result.stdout
        if result.returncode:
            print(
                f"{args.label} attempt={attempt} exit={result.returncode}\n{result.stdout}",
                flush=True,
            )
        return result.returncode == 0

    workers = [
        multiprocessing.get_context("fork").Process(target=burn_cpu)
        for _ in range(args.cpu_workers)
    ]
    try:
        for worker in workers:
            worker.start()
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            results = list(pool.map(run, range(1, args.iterations + 1)))
        print(
            f"SUMMARY {args.label}: {sum(results)}/{args.iterations} passed, "
            f"{args.iterations - sum(results)}/{args.iterations} failed; "
            f"{args.tests} tests/run; {args.jobs} simultaneous binary runs; "
            f"--test-threads=64; {args.cpu_workers} CPU-heavy workers",
            flush=True,
        )
        return 0 if all(results) else 1
    finally:
        for worker in workers:
            if worker.pid is not None:
                worker.terminate()
                worker.join()


if __name__ == "__main__":
    raise SystemExit(main())
