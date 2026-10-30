#!/usr/bin/env bash
# The same entry point is used before a push and before a worker delivery.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
export CARGO_TERM_COLOR=never PYTHONDONTWRITEBYTECODE=1

# The Linux lane starts basal's sandboxed worker, which requires Landlock.
# Every MOTOR Linux job runs on MOTOR's own servers, whose kernels have it.
linux_cpus=8

usage() {
    echo 'Usage: script/gate.sh [--workspace] [--base REF] [--all-platforms] [--print-plan] [--allow-unavailable]'
    echo 'Default: changed packages and their workspace dependents since origin/main.'
    echo '--workspace tests all packages; --all-platforms also forces the macOS lane.'
    echo 'Requires ck motor, rustfmt, shellcheck 0.11.0, gitleaks and Python >=3.11.'
    echo 'Logs: .cortexkit/internal-docs/audits/gate-logs/<run>/. Never pushes.'
    echo 'Workers: --print-plan lists runon commands; --prepare-tools stages offline NATS inputs.'
    echo 'After runon checks, --clean-tools removes only those transient inputs (cache is retained).'
    echo '--allow-unavailable permits only unreachable, draining or full runners refused before start, never failed tests.'
    echo '--skip-lane NAME leaves a lane to the CI run on main, printed as SKIP.'
}

run() {
    printf '\n+ '; printf '%q ' "$@"; printf '\n'
    "$@"
}

system_scratch() {
    # MOTOR bundles Git dependencies only for top-level checkouts, not nested lockfiles.
    # Ignore an inherited TMPDIR that might point inside a checkout.
    mktemp -d "/tmp/$1.XXXXXX"
}

lane_scratch() {
    # Lane snapshots are submitted to MOTOR as checkouts, so they must be
    # top-level (not nested in this checkout, or Git dependencies don't ship)
    # and under a root MOTOR accepts (it refuses /tmp: "destination outside
    # runner-owned roots"). The fleet's side-checkout folder is both.
    local parent="$HOME/Work/Projects/CortexKit-scratch"
    [[ -d $parent ]] || parent="$HOME/.cache"
    mkdir -p "$parent"
    mktemp -d "$parent/$1.XXXXXX"
}

install_nats() {
    local platform extension sha archive
    if [[ ! -f $root/.gate-inputs/nats/manifest.sh ]]; then
        echo 'Missing offline NATS input: run script/gate.sh --prepare-tools before dispatch' >&2
        return 1
    fi
    # Pins are generated from the CI action and archives are verified again on the runner.
    # shellcheck disable=SC1091
    . "$root/.gate-inputs/nats/manifest.sh"
    # MOTOR's Linux and Windows images ship nats-server at CI's pin. Use it when
    # its version matches; otherwise (macOS today) unpack the verified archive.
    local pinned=${archive#nats-server-v}
    pinned=${pinned%%-*}
    if command -v nats-server >/dev/null && nats-server --version | grep -qF "v$pinned"; then
        run nats-server --version
        return 0
    fi
    local directory="$root/target/gate-nats"
    mkdir -p "$directory"
    if [[ $platform == darwin-* ]]; then
        echo "$sha  $root/.gate-inputs/nats/$archive" | shasum -a 256 --check -
    else
        echo "$sha  $root/.gate-inputs/nats/$archive" | sha256sum --check -
    fi
    if [[ $extension == zip ]]; then
        run unzip -o -q "$root/.gate-inputs/nats/$archive" -d "$directory"
    else
        run tar -xzf "$root/.gate-inputs/nats/$archive" -C "$directory"
    fi
    export PATH="$directory/${archive%."$extension"}:$PATH"
    run nats-server --version
}

package_args() {
    args=()
    local package
    for package in "$@"; do
        if [[ " $selected " == *" $package "* ]]; then
            args+=(-p "$package")
        fi
    done
}

check_toolchain() {
    local minimum version major minor patch required_major required_minor required_patch
    minimum=$(sed -n 's/^rust-version = "\([^"]*\)".*/\1/p' Cargo.toml)
    [[ -n $minimum ]] || { echo 'Cannot read workspace rust-version' >&2; return 1; }
    run rustc --version
    run cargo --version
    version=$(rustc --version)
    version=${version#rustc }
    version=${version%% *}
    IFS=. read -r major minor patch <<< "${version%%-*}"
    IFS=. read -r required_major required_minor required_patch <<< "$minimum"
    if (( major < required_major || (major == required_major && minor < required_minor) || (major == required_major && minor == required_minor && ${patch:-0} < ${required_patch:-0}) )); then
        echo "Installed Rust $version is older than workspace rust-version $minimum; provision the runner" >&2
        return 1
    fi
}

unix_tests() {
    local platform=$1
    package_args basal-proto basal-core basal-host basal-launch
    if [[ $platform == linux && ${#args[@]} -gt 0 ]]; then
        run cargo test --locked "${args[@]}"
        if [[ " $selected " == *' basal-host '* ]]; then
            run cargo test --locked "${args[@]}" --features basal-host/rig-kill-hook
        fi
    fi
    package_args basal-worker basal-module basal-testkit basal-rig
    if [[ ${#args[@]} -gt 0 ]]; then
        run cargo test --locked "${args[@]}"
        if [[ " $selected " == *' basal-module '* ]]; then
            run cargo test --locked "${args[@]}" --features basal-module/rig-kill-hook
        fi
    fi
}

remote_lane() {
    local platform=$1
    shift
    selected="$*"
    echo "BASAL_GATE_LANE_STARTED $platform"
    case "$platform:$(uname -s)" in
        linux:Linux|scripts:Linux|windows:MINGW*|windows:MSYS*|macos:Darwin) ;;
        *) echo "Lane $platform refuses to build on $(uname -s)" >&2; return 1 ;;
    esac
    if [[ $platform == scripts ]]; then
        run shellcheck -x script/*.sh
        run /bin/sh -n script/stage.sh
        run python3 script/check-ckdev-names.py
        run python3 script/check-local-paths.py
        run python3 -m unittest script.tests.check_ckdev_names script.tests.check_local_paths script.tests.gate script.tests.flows_rig script.tests.stage_card script.tests.sign_worker script.tests.codemode_manifest -v
        return
    fi
    check_toolchain
    if [[ $platform == macos ]]; then
        # MOTOR's workspace cache can retain fingerprints without their rlibs.
        # Keep output outside the cached checkout until MOTOR fixes cache seeding.
        export CARGO_TARGET_DIR
        CARGO_TARGET_DIR=$(system_scratch basal-gate-macos)
        echo "Isolated macOS build directory: $CARGO_TARGET_DIR"
    fi
    install_nats
    case "$platform" in
        linux)
            run python3 script/check-path-deps.py
            run python3 -m unittest script.tests.check_path_deps -v
            run cargo fmt --all --check
            run cargo clippy --workspace --all-targets --locked -- -D warnings
            run cargo clippy --workspace --all-targets --locked --features basal-module/rig-kill-hook -- -D warnings
            unset BASAL_REQUIRE_VISIBLE_PROC
            echo 'NOTE visible-proc enforcement is CI-only: MOTOR hides other processes in /proc'
            unix_tests linux
            if rustup toolchain list | grep -q '^1\.89\.0[- ]'; then
                run rustc +1.89.0 --version
                run cargo +1.89.0 check --workspace --all-targets --locked
                run cargo +1.89.0 check --workspace --all-targets --locked --features basal-module/rig-kill-hook
            else
                echo 'SKIP minimum-rust: Rust 1.89.0 is not provisioned; MOTOR rustup homes are read-only'
            fi
            ;;
        macos)
            run cargo clippy --workspace --all-targets --locked --features basal-module/rig-kill-hook -- -D warnings
            unix_tests macos
            ;;
        windows)
            if [[ " $selected " == *' basal-launch '* ]]; then
                run cargo test -p basal-launch --locked -- --show-output
                run cargo test -p basal-launch --features deviations --locked -- --show-output
            fi
            if [[ " $selected " == *' basal-worker '* ]]; then
                run cargo test -p basal-worker --locked --no-fail-fast -- --show-output
                run cargo test -p basal-worker --features deviations --locked --no-fail-fast -- --show-output
            fi
            package_args basal-module basal-core
            if [[ ${#args[@]} -gt 0 ]]; then
                run cargo test "${args[@]}" --locked --no-fail-fast -- --show-output
            fi
            local package
            for package in basal-testkit basal-rig; do
                package_args "$package"
                if [[ ${#args[@]} -gt 0 ]]; then
                    run cargo test "${args[@]}" --locked --no-fail-fast
                fi
            done
            if ! run cargo check --workspace --all-targets --locked; then
                run cargo check --workspace --all-targets --locked --keep-going
                return 1
            fi
            package_args basal-proto basal-host
            if [[ ${#args[@]} -gt 0 ]]; then
                if ! run cargo test "${args[@]}" --locked; then
                    run cargo test "${args[@]}" --locked --no-fail-fast
                    return 1
                fi
            fi
            ;;
    esac
}

# Internal entry point: only the parent gate dispatches this in remote snapshots.
if [[ ${1:-} == --lane ]]; then
    shift
    remote_lane "$@"
    exit
fi

mode=touched
base=origin/main
all_platforms=no
print_plan=no
prepare_tools=no
clean_tools=no
allow_unavailable=no
skip_lanes=" "
while [[ $# -gt 0 ]]; do
    case "$1" in
        --workspace) mode=workspace ;;
        --all-platforms) all_platforms=yes ;;
        --print-plan) print_plan=yes ;;
        --prepare-tools) prepare_tools=yes ;;
        --clean-tools) clean_tools=yes ;;
        --allow-unavailable) allow_unavailable=yes ;;
        --skip-lane) shift; skip_lanes+="${1:?--skip-lane requires a lane name} " ;;
        --base) shift; base=${1:?--base requires a Git ref} ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
    shift
done
base=$(git merge-base "$base" HEAD)
python=
for candidate in python3.14 python3.13 python3.12 python3.11 python3; do
    if command -v "$candidate" >/dev/null && "$candidate" -c 'import sys; sys.exit(sys.version_info < (3, 11))'; then
        python=$candidate
        break
    fi
done
[[ -n $python ]] || { echo 'Python >=3.11 is required (tomllib); none found' >&2; exit 1; }
if [[ $clean_tools == yes ]]; then
    "$python" script/gate_support.py clean-tools
    exit
fi
if [[ $prepare_tools == yes ]]; then
    "$python" script/gate_support.py tools "$root"
    echo 'Offline inputs prepared in .gate-inputs/nats; run --clean-tools before committing.'
    exit
fi
if [[ $print_plan == yes ]]; then
    read -r macos selected <<< "$("$python" script/gate_support.py plan "$base" "$mode")"
    echo "# Selected packages: $selected"
    echo '# Local preparation (downloads/checksums from the CI NATS action; no Cargo):'
    echo 'script/gate.sh --prepare-tools'
    echo '# runon: linux,2c (metadata-only preflight, before build lanes)'
    echo 'cargo metadata --locked --format-version 1 > /dev/null'
    for lane in linux windows macos; do
        if [[ $lane == macos && $macos == no && $all_platforms == no ]]; then
            echo '# SKIP macos: no macOS-specific paths changed (use --all-platforms to force)'
            continue
        fi
        target=$lane
        [[ $lane != linux ]] || target=linux,${linux_cpus}c
        echo "# runon: $target (entry point runs the same commands as the head gate)"
        echo "bash script/gate.sh --lane $lane $selected"
    done
    echo '# runon: linux,2c (Cargo-free script suites; parallel with platform lanes)'
    echo 'bash script/gate.sh --lane scripts'
    echo '# Local cleanup; do not commit transient binary inputs:'
    echo 'script/gate.sh --clean-tools'
    echo '# SKIP native linux-arm64/trace and second Windows image: no MOTOR selector'
    echo '# Windows locked Git dependencies are bundled from the top-level checkout; fetch errors fail normally.'
    echo '# minimum-rust runs on Linux only if Rust 1.89.0 is already provisioned; otherwise an explicit skip is logged.'
    exit
fi
for tool in ck rustfmt shellcheck gitleaks; do
    command -v "$tool" >/dev/null || { echo "Missing gate prerequisite: $tool" >&2; exit 1; }
done

mkdir -p target/scratch .cortexkit/internal-docs/audits/gate-logs
scratch=$(lane_scratch basal-gate)
# Lane snapshots are full checkout copies outside this repository; remove them
# however the gate ends. Logs live under .cortexkit/internal-docs, not here.
trap 'rm -rf "$scratch"' EXIT
export TMPDIR="$scratch"
mkdir -p "$scratch/no-cargo"
printf '#!/bin/sh\necho "Cargo is forbidden in local gate checks; use a remote lane" >&2\nexit 99\n' >"$scratch/no-cargo/cargo"
chmod +x "$scratch/no-cargo/cargo"
export PATH="$scratch/no-cargo:$PATH"
logs="$root/.cortexkit/internal-docs/audits/gate-logs/$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p "$logs"
started=$SECONDS
echo "Gate logs: $logs"
echo "Snapshot HEAD: $(git rev-parse HEAD); base: $base; mode: $mode"

fast() {
    run "$python" --version
    run "$python" script/gate_support.py fmt
    run shellcheck --version
    shellcheck --version | grep -q 'version: 0.11.0' || { echo 'shellcheck 0.11.0 required for CI parity' >&2; return 1; }
    run shellcheck -x script/*.sh
    run /bin/sh -n script/stage.sh
    run "$python" script/check-ckdev-names.py
    run "$python" -m unittest script.tests.check_ckdev_names script.tests.check_local_paths script.tests.gate -v
    local tracked_ignored
    tracked_ignored=$(git ls-files -ci --exclude-standard -- .cortexkit)
    if [[ -n $tracked_ignored ]]; then
        printf 'Ignored .cortexkit files are tracked:\n%s\n' "$tracked_ignored" >&2
        return 1
    fi
    run "$python" script/gate_support.py author "$base"
    run gitleaks version
    run "$python" script/gate_support.py snapshot "$scratch/linux"
    # A disposable commit lets gitleaks inspect uncommitted files without
    # rescanning unchanged fixture secrets whose ignores are commit-specific.
    run git -C "$scratch/linux" add --all
    run git -C "$scratch/linux" -c user.name=ualtinok -c user.email=94532+ualtinok@users.noreply.github.com commit --quiet --allow-empty -m 'gate working-tree snapshot'
    (cd "$scratch/linux"; run "$python" script/check-local-paths.py)
    (cd "$scratch/linux"; run gitleaks git --no-banner --redact --log-opts="$base..HEAD" .)
}

# Do not call fast in an if-condition: Bash would disable errexit inside it.
set +e
(set -e; fast) >"$logs/fast.log" 2>&1
result=$?
set -e
if [[ $result -ne 0 ]]; then
    cat "$logs/fast.log" >&2
    echo "FAIL fast phase after $((SECONDS - started))s; no remote lanes started" >&2
    exit "$result"
fi
echo "PASS local fast phase ($((SECONDS - started))s)"

read -r macos selected <<< "$(cd "$scratch/linux"; "$python" script/gate_support.py plan "$base" "$mode")"
echo "Selected packages: $selected"
outcomes=()
# Cargo's resolver is authoritative; keep Cargo off the local Mac entirely.
if ck motor run --cwd "$scratch/linux" --platform linux --vcpus 2 --timeout 300 --secret-file "$scratch/locked.secret" -- 'echo BASAL_GATE_LANE_STARTED locked; cargo metadata --locked --format-version 1 > /dev/null' >"$logs/locked.log" 2>&1; then
    echo "PASS locked metadata preflight ($((SECONDS - started))s since start)"
else
    result=$?
    reason=$("$python" script/gate_support.py unavailable "$result" "$logs/locked.log")
    if [[ -n $reason ]]; then
        echo "UNAVAILABLE locked metadata preflight: $reason" | tee -a "$logs/locked.log" >&2
        [[ $allow_unavailable == yes ]] || exit "$result"
        outcomes+=(unavailable)
    else
        cat "$logs/locked.log" >&2
        echo "FAIL locked metadata preflight ($((SECONDS - started))s)" >&2
        exit "$result"
    fi
fi

lanes=(linux windows)
if [[ $macos == yes || $all_platforms == yes ]]; then
    lanes+=(macos)
else
    echo 'SKIP macos: no macOS-specific paths changed (use --all-platforms to force)' | tee "$logs/macos.log"
fi
echo 'SKIP linux-arm64 and native syscall trace: MOTOR has no architecture/image selector' | tee "$logs/linux-arm64.log"
echo 'NOTE Windows uses the configured MOTOR image, not both GitHub hosted images' | tee "$logs/windows-images.log"
# The operator can leave a lane to main's CI when its runner can't run it,
# such as a Windows image that refuses the confined worker's window station.
# The skip is printed in the summary, never silent.
kept=()
for lane in "${lanes[@]}"; do
    if [[ $skip_lanes == *" $lane "* ]]; then
        echo "SKIP $lane: skipped by --skip-lane; main's CI covers it" | tee "$logs/$lane.log"
    else
        kept+=("$lane")
    fi
done
lanes=("${kept[@]}")
pids=()
for lane in "${lanes[@]}"; do
    if [[ $lane != linux ]]; then
        (cd "$scratch/linux"; "$python" script/gate_support.py snapshot "$scratch/$lane")
    fi
done
for lane in "${lanes[@]}"; do
    "$python" script/gate_support.py tools "$scratch/$lane" "$lane"
done
for lane in "${lanes[@]}"; do
    cpus=2
    [[ $lane != linux ]] || cpus=$linux_cpus
    # Separate checkouts avoid MOTOR's per-workspace single-job lock.
    # Package names come from Cargo manifests and are passed as argv, not shell code.
    read -r -a packages <<< "$selected"
    (
        lane_started=$SECONDS
        set +e
        ck motor run --cwd "$scratch/$lane" --platform "$lane" --vcpus "$cpus" --timeout 7200 --secret-file "$scratch/$lane.secret" -- bash script/gate.sh --lane "$lane" "${packages[@]}" &
        motor_pid=$!
        trap 'kill "$motor_pid" 2>/dev/null || true; wait "$motor_pid" || true; exit 130' INT TERM
        wait "$motor_pid"
        result=$?
        echo "$((SECONDS - lane_started))" >"$logs/$lane.seconds"
        exit "$result"
    ) >"$logs/$lane.log" 2>&1 &
    pids+=("$!")
done
# Offline fixture suites can take over a minute; do not delay remote builds for them.
lanes+=(scripts)
(
    lane_started=$SECONDS
    set +e
    "$python" -m unittest script.tests.flows_rig script.tests.stage_card script.tests.sign_worker script.tests.codemode_manifest -v &
    suite_pid=$!
    trap 'kill "$suite_pid" 2>/dev/null || true; wait "$suite_pid" || true; exit 130' INT TERM
    wait "$suite_pid"
    result=$?
    echo "$((SECONDS - lane_started))" >"$logs/scripts.seconds"
    exit "$result"
) >"$logs/scripts.log" 2>&1 &
pids+=("$!")
# Invoked by the signal trap below rather than a direct command.
# shellcheck disable=SC2329
cancel_lanes() {
    trap - INT TERM
    kill "${pids[@]}" 2>/dev/null || true
    for pid in "${pids[@]}"; do wait "$pid" || true; done
    exit 130
}
trap 'cancel_lanes' INT TERM
for ((index=0; index<${#lanes[@]}; index++)); do
    if wait "${pids[$index]}"; then
        read -r duration <"$logs/${lanes[$index]}.seconds"
        echo "PASS ${lanes[$index]} (${duration}s lane duration)"
        outcomes+=(pass)
        grep '^SKIP ' "$logs/${lanes[$index]}.log" || true
    else
        result=$?
        duration=unknown
        if [[ -f $logs/${lanes[$index]}.seconds ]]; then read -r duration <"$logs/${lanes[$index]}.seconds"; fi
        reason=$("$python" script/gate_support.py unavailable "$result" "$logs/${lanes[$index]}.log")
        if [[ -n $reason ]]; then
            echo "UNAVAILABLE ${lanes[$index]}: $reason (${duration}s lane duration)" | tee -a "$logs/${lanes[$index]}.log" >&2
            outcomes+=(unavailable)
        else
            echo "FAIL ${lanes[$index]} (exit $result, ${duration}s lane duration); log: $logs/${lanes[$index]}.log" >&2
            # Include complete failure context, including transport refusals and compiler diagnostics.
            cat "$logs/${lanes[$index]}.log" >&2
            outcomes+=(fail)
        fi
    fi
done
echo "Gate finished in $((SECONDS - started))s; logs: $logs"
if [[ $allow_unavailable == yes ]]; then
    echo 'NOTE --allow-unavailable: only runner-unavailable lanes may be deferred to CI on main'
fi
exit "$("$python" script/gate_support.py result "$allow_unavailable" "${outcomes[@]}")"
