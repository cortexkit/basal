"""Offline tests for sign-worker.sh sandbox denials and macOS signature checks."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "sign-worker.sh"
PROFILE = SCRIPT.parents[1] / "crates/basal-worker/sandbox/worker.sb"


class SignWorkerVerifyChecks(unittest.TestCase):
    def test_connection_refused_fails_gate(self):
        result = self.run_gate("refused")
        self.assert_gate_rejected(result)
        self.assertIn("EPERM", result.stderr)

    def test_connection_accepted_fails_gate(self):
        result = self.run_gate("accepted")
        self.assert_gate_rejected(result)
        self.assertIn("connect", result.stderr)

    def test_missing_report_fails_gate(self):
        result = self.run_gate("missing")
        self.assert_gate_rejected(result)
        self.assertIn("did not report", result.stderr)

    def test_unparsable_report_fails_gate(self):
        result = self.run_gate("unparsable")
        self.assert_gate_rejected(result)
        self.assertIn("not valid JSON", result.stderr)

    def test_crashed_probe_fails_gate(self):
        result = self.run_gate("crashed")
        self.assert_gate_rejected(result)
        self.assertIn("died by signal", result.stderr)

    def test_unexpected_exit_code_fails_gate(self):
        result = self.run_gate("exit")
        self.assert_gate_rejected(result)
        self.assertIn("exited with code 23", result.stderr)

    @unittest.skipUnless(sys.platform.startswith("linux"), "SIGSYS is a Linux seccomp signal")
    def test_sigsys_before_ready_fails_gate(self):
        result = self.run_gate("setup-sigsys")
        self.assert_gate_rejected(result)
        self.assertIn("died by signal", result.stderr)

    def test_correct_denial_passes(self):
        result = self.run_gate("sigsys")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("signature gate passed", result.stdout)

    def test_errno_denial_passes(self):
        result = self.run_gate("errno-denied")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("signature gate passed", result.stdout)

    def assert_gate_rejected(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def run_gate(self, scenario):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            binary = directory / "ck-basal-worker"
            binary.write_text(STAND_IN, encoding="utf-8")
            binary.write_bytes(
                binary.read_bytes() + b"_embedded_profile = b'''" + PROFILE.read_bytes() + b"'''\n"
            )
            binary.chmod(0o755)

            tools = directory / "tools"
            tools.mkdir()
            codesign = tools / "codesign"
            codesign.write_text(
                "#!/bin/sh\n"
                "case \"$1\" in\n"
                "  --verify) exit 0 ;;\n"
                "  -dv)\n"
                "    echo 'Identifier=ck-basal-worker' >&2\n"
                "    echo 'CodeDirectory v=20400 n=-1 flags=0x10000(runtime)' >&2\n"
                "    exit 0 ;;\n"
                "  -d) exit 0 ;;\n"
                "  *) exit 1 ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            codesign.chmod(0o755)

            environment = os.environ.copy()
            environment["PATH"] = str(tools) + os.pathsep + environment["PATH"]
            environment["SIGN_WORKER_SCENARIO"] = scenario
            environment["BASAL_WORKER_IDENTIFIER"] = "ck-basal-worker"
            return subprocess.run(
                ["/bin/sh", str(SCRIPT), "verify", str(binary)],
                capture_output=True,
                text=True,
                env=environment,
                check=False,
            )


STAND_IN = r'''#!/usr/bin/env python3
import json
import os
import signal
import socket
import sys

args = sys.argv[1:]
scenario = os.environ["SIGN_WORKER_SCENARIO"]
linux = sys.platform.startswith("linux")
if linux:
    selected = next(value for value in args if value.startswith("--syscall="))
    syscall_name = selected.split("=", 1)[1]
    attempt = {"open": "read", "tcp": "connect", "exec": "exec"}[syscall_name]
    raw_name = {"open": "openat", "tcp": "socket", "exec": "execve"}[syscall_name]
else:
    attempt = "read"
    syscall_name = "open"
    raw_name = "openat"

if scenario == "missing":
    sys.exit(0)
if scenario == "unparsable":
    print("{not a report}", flush=True)
    sys.exit(0)

if linux:
    print(
        '{"confinement":"linux","seccomp":true,"landlock":null,'
        '"open_descriptors":[],"sigsys_handler":false}',
        flush=True,
    )
    if scenario == "setup-sigsys":
        os.kill(os.getpid(), signal.SIGSYS)
    print(f"ready: {syscall_name} syscall: {raw_name}", flush=True)
    if scenario == "crashed":
        os.kill(os.getpid(), signal.SIGSEGV)
    if scenario == "sigsys":
        os.kill(os.getpid(), signal.SIGSYS)
    if scenario == "exit":
        sys.exit(23)
    if attempt == "connect" and scenario == "accepted":
        port = int(next(value.split("=", 1)[1] for value in args if value.startswith("--port=")))
        with socket.create_connection(("127.0.0.1", port), timeout=2):
            pass
        print("result: 3 errno: 0", flush=True)
    elif attempt == "connect" and scenario == "refused":
        print("result: -1 errno: 111", flush=True)
    else:
        print("result: -1 errno: 1", flush=True)
else:
    report = {
        "confinement": "seatbelt",
        "open_descriptors": [],
        "read": {"ok": False, "error": "Operation not permitted (os error 1)"},
        "connect": {"ok": False, "error": "Operation not permitted (os error 1)"},
        "exec": {"ok": False, "error": "Operation not permitted (os error 1)"},
    }
    if scenario == "refused":
        report["connect"]["error"] = "Connection refused (os error 61)"
    elif scenario == "accepted":
        report["connect"] = {"ok": True}
        endpoint = args[args.index("--connect") + 1]
        host, port = endpoint.rsplit(":", 1)
        with socket.create_connection((host, int(port)), timeout=2):
            pass
    print(json.dumps(report), flush=True)
    if scenario == "crashed":
        os.kill(os.getpid(), signal.SIGSEGV)
    if scenario == "exit":
        sys.exit(23)
'''


if __name__ == "__main__":
    unittest.main(verbosity=2)
