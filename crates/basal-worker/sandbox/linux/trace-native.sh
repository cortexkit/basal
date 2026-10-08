#!/usr/bin/env bash
# Collect native release-worker evidence without relaxing any confinement policy.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../../.." && pwd)
cd "$root"
[[ $(uname -s) == Linux ]] || { echo 'native Linux required' >&2; exit 1; }
arch=$(uname -m)
case "$arch" in x86_64|aarch64) ;; *) echo "unsupported native architecture: $arch" >&2; exit 1 ;; esac
command -v strace >/dev/null
out="$root/crates/basal-worker/sandbox/linux/traces/$arch"
mkdir -p "$out"
{
    uname -srmo
    cargo --version
    rustc --version
    strace --version
    ldd --version
} > "$out/tools.txt" 2>&1
# Cargo's nested testkit build uses a separate target directory; these invocations
# are sequential. The tests below explicitly launch the release artifact.
cargo build --locked --release -p basal-worker --bin ck-basal-worker --example clock_cost
bin="${CARGO_TARGET_DIR:-$root/target}/release/ck-basal-worker"
clock="${CARGO_TARGET_DIR:-$root/target}/release/examples/clock_cost"
"$clock" > "$out/clock-cost.txt"
export BASAL_WORKER_BIN="$bin"
cargo test --locked -p basal-worker --test budgets --test ipc --test replay --test clock --no-run
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
strace -ff -s 256 -v -o "$scratch/activation" \
    cargo test --locked -p basal-worker --test budgets --test ipc --test replay --test clock -- --test-threads=1 \
    > "$out/scenarios.txt" 2>&1
# Keep only traces containing the successful worker TSYNC installation. Parent,
# cargo and build traces are not evidence for the worker's filter.
python3 - "$scratch" "$out" <<'PY'
import pathlib, re, sys
scratch, out = map(pathlib.Path, sys.argv[1:])
workers = []
for path in sorted(scratch.glob('activation.*'), key=lambda p: int(p.suffix[1:])):
    text = path.read_text()
    if re.search(r'seccomp\(SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, .*\) += 0', text):
        workers.append(text)
if not workers:
    raise SystemExit('no successfully confined release worker traces')
for i, text in enumerate(workers, 1):
    (out / f'activation-{i:02}.strace').write_text(text)
print(f'{len(workers)} confined release workers traced')
PY
# Panic and abort are separate raw probe children using the identical entry.
# stdout/stderr are anonymous pipes (never redirected files in the worker).
python3 - "$bin" "$out" <<'PY'
import pathlib, signal, subprocess, sys
binary, directory = sys.argv[1:]
out = pathlib.Path(directory)
for scenario, expected in [('panic', 101), ('abort', -signal.SIGABRT)]:
    result = subprocess.run(['strace', '-s', '256', '-v', '-o', str(out / f'{scenario}.strace'),
                             binary, '--confinement-probe', f'--syscall={scenario}'],
                            input=b'', stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    (out / f'{scenario}.txt').write_bytes(result.stdout + result.stderr)
    if f'ready: {scenario}'.encode() not in result.stdout:
        raise SystemExit(f'{scenario}: no readiness, exit {result.returncode}')
    if result.returncode != expected:
        raise SystemExit(f'{scenario}: expected {expected}, got {result.returncode}')
print('panic and abort traced')
PY
# Preserve every non-clock syscall and the first/last eight budget samples. Long
# CPU-budget loops produce millions of identical-authority clock calls; record
# their omitted count rather than checking in megabytes of timestamps.
python3 - "$out" <<'PY'
import collections, pathlib, re, sys
out = pathlib.Path(sys.argv[1])
observed = collections.Counter()
for path in sorted(out.glob('*.strace')):
    lines = path.read_text().splitlines()
    installation = next(i for i, line in enumerate(lines) if 'seccomp(SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC,' in line and line.endswith('= 0'))
    body = lines[installation + 1:]
    clocks = [i for i, line in enumerate(body) if line.startswith('clock_gettime(CLOCK_THREAD_CPUTIME_ID,')]
    retained = set(clocks[:8] + clocks[-8:])
    clocks = set(clocks)
    reduced = ['# Native release worker, post-confinement trace. Loader/setup calls omitted.',
               '# All non-clock calls retained; first/last 8 thread-CPU samples retained.', lines[installation]]
    skipped = 0
    for i, line in enumerate(body):
        match = re.match(r'(\w+)\(', line)
        if match:
            observed[match.group(1)] += 1
        if i in clocks and i not in retained:
            skipped += 1
            continue
        if skipped:
            reduced.append(f'# {skipped} additional clock_gettime(CLOCK_THREAD_CPUTIME_ID) samples omitted')
            skipped = 0
        reduced.append(line)
    if skipped:
        reduced.append(f'# {skipped} additional clock_gettime(CLOCK_THREAD_CPUTIME_ID) samples omitted')
    path.write_text('\n'.join(reduced) + '\n')
(out / 'observed-syscalls.txt').write_text('\n'.join(f'{name} {count}' for name, count in sorted(observed.items())) + '\n')
for path in out.glob('*.txt'):
    path.write_text(path.read_text().rstrip() + '\n')
PY
printf 'Evidence written to %s\n' "$out"
