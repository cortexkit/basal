#!/bin/sh
# Signing policy for ck-basal-worker on macOS.
#
# The worker is ad-hoc signed with the hardened runtime, an explicit
# identifier and no entitlements at all. QuickJS is an interpreter, so the
# worker needs neither allow-jit nor allow-unsigned-executable-memory, and
# its OS sandbox is the Seatbelt profile it applies to itself at startup, not
# an App Sandbox entitlement.
#
# Usage:
#   script/sign-worker.sh sign   BINARY
#   script/sign-worker.sh verify BINARY
#   script/sign-worker.sh place  BINARY DESTINATION
#
# `place` copies the binary to DESTINATION, signs it there and verifies it
# there, restoring any previous file if verification fails. It never
# restarts anything.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="$ROOT/crates/basal-worker/sandbox/worker.sb"
# The signing itself, and the signature half of the gate, are the policy every
# basal binary shares (script/signing.sh); this script adds the worker's own
# checks on top: the embedded profile and the live confinement probe.
# shellcheck source=script/signing.sh
. "$ROOT/script/signing.sh"
# BASAL_WORKER_IDENTIFIER exists for isolated rigs (script/flows-rig.sh). A rig
# build signs its copies under ckdev- identifiers so it can never be mistaken for
# a production binary; a rig run on staged bytes (place --from-stage) keeps the
# production identifier, because it tests exactly what will be placed. Every
# other check is unchanged.
IDENTIFIER="${BASAL_WORKER_IDENTIFIER:-ck-basal-worker}"

usage() {
  echo "usage: $0 sign|verify BINARY | place BINARY DESTINATION" >&2
  exit 2
}

sign() {
  sign_hardened "$1" "$IDENTIFIER"
}

refuse() {
  echo "REFUSED: $1" >&2
  exit 1
}

verify() {
  binary=$1
  system=$(uname -s)
  case "$system" in
    Darwin)
      # Require a valid strict signature, the expected identifier, hardened
      # runtime flags, and no entitlements before checking confinement.
      verify_hardened "$binary" "$IDENTIFIER" || exit 1
      # The sandbox profile compiled into the binary must be the checked-in one.
      python3 - "$binary" "$PROFILE" <<'PY' || refuse "embedded sandbox profile differs from $PROFILE"
import sys
binary = open(sys.argv[1], "rb").read()
profile = open(sys.argv[2], "rb").read()
sys.exit(0 if profile in binary else 1)
PY
      probe_platform=darwin
      ;;
    Linux)
      probe_platform=linux
      ;;
    *)
      refuse "unsupported worker verification platform: $system"
      ;;
  esac
  # A live listener makes a refused connection distinguishable from a sandbox
  # denial, which must report EPERM or EACCES.
  python3 - "$binary" "$PROFILE" "$probe_platform" <<'PY'
import json
import re
import signal
import socket
import subprocess
import sys
import threading

binary, profile, platform = sys.argv[1:]


class GateFailure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise GateFailure(message)


def is_denial_errno(error_number):
    return error_number in (1, 13)


def error_number(error):
    if isinstance(error, int) and not isinstance(error, bool):
        return error
    if isinstance(error, str):
        match = re.search(r"\(os error ([0-9]+)\)$", error)
        if match:
            return int(match.group(1))
    return None


class LoopbackListener:
    def __init__(self):
        self.server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.server.bind(("127.0.0.1", 0))
        self.server.listen()
        self.server.settimeout(0.1)
        self.port = self.server.getsockname()[1]
        self.accepted = threading.Event()
        self.stopping = threading.Event()
        self.thread = threading.Thread(target=self._accept, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stopping.set()
        self.server.close()
        self.thread.join(timeout=1)

    def _accept(self):
        while not self.stopping.is_set():
            try:
                connection, _ = self.server.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            self.accepted.set()
            connection.close()

    def require_unused(self, attempt):
        if self.accepted.wait(0.2):
            raise GateFailure(
                f"confinement probe: {attempt} connection reached the verification listener"
            )


def run_probe(arguments, attempt):
    try:
        return subprocess.run(
            [binary, "--confinement-probe", *arguments],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except subprocess.TimeoutExpired:
        raise GateFailure(f"confinement probe: {attempt} timed out")
    except OSError as error:
        raise GateFailure(f"confinement probe could not start: {error}")


def require_exit_zero(result, attempt):
    if result.returncode < 0:
        raise GateFailure(
            f"confinement probe {attempt} died by signal {-result.returncode}"
        )
    require(result.returncode == 0,
            f"confinement probe {attempt} exited with code {result.returncode}: "
            f"{result.stderr.strip()}")


def first_report(stdout, attempt):
    lines = stdout.splitlines()
    require(bool(lines) and bool(lines[0].strip()),
            f"confinement probe: {attempt} did not report confinement")
    try:
        report = json.loads(lines[0])
    except (json.JSONDecodeError, TypeError):
        raise GateFailure(f"confinement probe: {attempt} report is not valid JSON: {lines[0]}")
    require(isinstance(report, dict),
            f"confinement probe: {attempt} report is not a JSON object")
    return report, lines


def check_macos(listener):
    result = run_probe(
        ["--read", profile, "--connect", f"127.0.0.1:{listener.port}",
         "--exec", "/bin/echo"],
        "read/connect/exec",
    )
    require_exit_zero(result, "macOS")
    report, _ = first_report(result.stdout, "macOS")
    require(report.get("confinement") == "seatbelt",
            f"confinement probe did not report seatbelt: {result.stdout.strip()}")
    for attempt in ("read", "connect", "exec"):
        entry = report.get(attempt)
        require(isinstance(entry, dict) and entry.get("ok") is False,
                f"confinement probe: {attempt} was not denied: {result.stdout.strip()}")
    connect_error = error_number(report["connect"].get("error"))
    require(is_denial_errno(connect_error),
            f"confinement probe: connect did not report EPERM or EACCES: "
            f"{report['connect']!r}")
    listener.require_unused("connect")


def check_linux(listener):
    probes = (
        ("read", "open", "openat", [f"--path={profile}"]),
        ("connect", "connect", "connect", [f"--port={listener.port}"]),
        ("exec", "exec", "execve", []),
    )
    for attempt, syscall_name, raw_name, extra in probes:
        result = run_probe([f"--syscall={syscall_name}", *extra], attempt)
        report, lines = first_report(result.stdout, attempt)
        require(report.get("confinement") == "linux" and report.get("seccomp") is True,
                f"confinement probe did not report Linux seccomp confinement: {lines[0]}")
        ready = f"ready: {syscall_name} syscall: {raw_name}"
        if result.returncode < 0:
            caught_signal = -result.returncode
            if caught_signal == signal.SIGSYS and ready in lines:
                listener.require_unused(attempt)
                continue
            raise GateFailure(
                f"confinement probe {attempt} died by signal {caught_signal}"
            )
        require(result.returncode == 0,
                f"confinement probe {attempt} exited with code {result.returncode}: "
                f"{result.stderr.strip()}")
        require(ready in lines,
                f"confinement probe did not reach the {attempt} syscall: {result.stdout.strip()}")
        result_line = next((line for line in reversed(lines)
                            if line.startswith("result: ")), None)
        match = re.fullmatch(r"result: (-?[0-9]+) errno: ([0-9]+)", result_line or "")
        require(match is not None,
                f"confinement probe did not report the {attempt} result: {result.stdout.strip()}")
        return_code, error = (int(value) for value in match.groups())
        require(return_code == -1 and is_denial_errno(error),
                f"confinement probe: {attempt} was not denied with EPERM or EACCES: "
                f"{result_line}")
        listener.require_unused(attempt)


try:
    with LoopbackListener() as listener:
        if platform == "darwin":
            check_macos(listener)
        else:
            check_linux(listener)
except GateFailure as error:
    print(f"REFUSED: {error}", file=sys.stderr)
    sys.exit(1)
except OSError as error:
    print(f"REFUSED: could not open loopback verification listener: {error}", file=sys.stderr)
    sys.exit(1)
PY
  echo "signature gate passed: $binary ($IDENTIFIER)"
}

operation=${1:-}
case "$operation" in
  sign)
    [ $# -eq 2 ] || usage
    sign "$2"
    ;;
  verify)
    [ $# -eq 2 ] || usage
    verify "$2"
    ;;
  place)
    [ $# -eq 3 ] || usage
    binary=$2
    dest=$3
    mkdir -p "$(dirname "$dest")"
    scratch=$(mktemp -d "$(dirname "$dest")/.worker-placement.XXXXXX")
    installed=0
    accepted=0
    existed=0
    rollback() {
      if [ "$installed" = 1 ] && [ "$accepted" = 0 ]; then
        if [ "$existed" = 1 ]; then mv -f "$scratch/old" "$dest"; else rm -f "$dest"; fi
        echo "REFUSED: placed worker failed the gate; previous file restored" >&2
      fi
      rm -rf "$scratch"
    }
    trap rollback EXIT
    trap 'exit 1' INT TERM
    cp "$binary" "$scratch/new"
    if [ -e "$dest" ]; then cp -p "$dest" "$scratch/old"; existed=1; fi
    mv -f "$scratch/new" "$dest"
    installed=1
    # Sign at the final path and inspect that path, not a staging copy.
    sign "$dest"
    verify "$dest"
    accepted=1
    ;;
  *)
    usage
    ;;
esac
