"""Fail-closed EXIT traps for the POSIX shell entry points."""

from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import unittest


def unguarded_exit_scripts(directory):
    """Find fail-fast scripts without an initialized, checked completion flag."""
    violations = []
    for path in sorted(directory.glob("*.sh")):
        source = re.sub(r"(?m)^[ \t]*#.*$", "", path.read_text())
        errexit = re.search(
            r"(?m)^[ \t]*set[ \t]+(?:-[a-zA-Z]+[ \t]+)*-[a-zA-Z]*e[a-zA-Z]*(?:[ \t;]|$)",
            source,
        )
        exit_trap = re.search(
            r"(?m)^[ \t]*trap[ \t]+[^\n]*[ \t]+(?:EXIT|0)[ \t]*(?:#.*)?$", source
        )
        if not (errexit and exit_trap):
            continue
        flags = re.findall(r"(?m)^[ \t]*([a-zA-Z_][a-zA-Z_0-9]*)=0[ \t]*$", source)
        if not any(
            flag.endswith("finished")
            and re.search(rf"(?m)^[ \t]*{flag}=1[ \t]*$", source)
            and re.search(rf"(?:\[\[?|\btest)[^\n]*\$(?:{flag}\b|\{{{flag}\}})", source)
            for flag in flags
        ):
            violations.append(path.name)
    return violations


class ScriptExitTrapTests(unittest.TestCase):
    CASES = (
        ("stage.sh", 0, "stage_finished", ("cleanup", "on_stage_exit")),
        ("sign-worker.sh", 0, "worker_finished", ("rollback", "on_worker_exit")),
        ("flows-rig.sh", 0, "rig_finished", ("on_rig_exit", "cleanup_rig_test")),
        ("flows-rig.sh", 1, "rig_finished", ("on_rig_exit", "cleanup_rig_test")),
    )

    def _run_trap(self, case, body, success=False, existed=True):
        filename, trap_index, flag, functions = case
        source = (Path(__file__).resolve().parents[1] / filename).read_text()
        # Extract the production handlers and trap registrations, not the build
        # or rig setup. Missing new handlers are allowed so the same harness
        # exercises the old, unguarded traps when checking the regression.
        handlers = []
        for name in functions:
            match = re.search(rf"(?m)^([ \t]*){name}\(\) \{{\n[\s\S]*?^\1\}}\n", source)
            if match:
                handlers.append(match[0])
        traps = re.findall(r"(?m)^[ \t]*trap[ \t]+[^\n]*[ \t]+EXIT[ \t]*$", source)
        scratch_root = Path(__file__).resolve().parents[2] / "target/scratch"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as tmp:
            root = Path(tmp)
            for directory in ("scratch", "control"):
                (root / directory).mkdir()
            (root / "scratch/old").write_text("old")
            for name in ("dest", "kill", "unscoped"):
                (root / name).write_text("new")
            setup = f'''set -eu
unset undefined_exit_trap_variable
{flag}=0
root={shlex.quote(tmp)}
SCRATCH="$root/scratch"
CONTROL_DIR="$root/control"
HELD=""
scratch="$SCRATCH"
dest="$root/dest"
installed=1
accepted={int(success)}
existed={int(existed)}
KILL_FILE="$root/kill"
UNSCOPED_FILE="$root/unscoped"
cmd_stop() {{ printf '%s\\n' stopped > "$root/stopped"; }}
'''
            result = subprocess.run(
                ["/bin/sh", "-c", setup + "".join(handlers) + traps[trap_index] + "\n" + body],
                text=True, capture_output=True, check=False,
            )
            if filename == "stage.sh":
                self.assertFalse((root / "scratch").exists())
                self.assertFalse((root / "control").exists())
            elif filename == "sign-worker.sh":
                self.assertFalse((root / "scratch").exists())
                if success or existed:
                    self.assertEqual((root / "dest").read_text(), "new" if success else "old")
                else:
                    self.assertFalse((root / "dest").exists())
            else:
                self.assertEqual((root / "stopped").read_text(), "stopped\n")
                for name in ("kill", "unscoped"):
                    self.assertEqual((root / name).exists(), trap_index == 0)
            return result

    @unittest.skipUnless(sys.platform == "darwin", "requires macOS /bin/sh's bash 3.2 nounset behavior")
    def test_unbound_variables_fail_closed_under_bin_sh(self):
        # macOS /bin/sh is bash 3.2: nounset can give the EXIT trap status 0.
        for case in self.CASES:
            with self.subTest(script=case[0], trap=case[1]):
                result = self._run_trap(case, 'echo "${undefined_exit_trap_variable}"\n' + case[2] + '=1\n')
                self.assertEqual(result.returncode, 1, result.stderr)

    def test_unfinished_zero_exits_are_failures(self):
        for case in self.CASES:
            with self.subTest(script=case[0], trap=case[1]):
                result = self._run_trap(case, "exit 0\n")
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn(f"FAIL {Path(case[0]).stem} stopped before finishing", result.stderr)

    def test_finished_runs_pass_and_explicit_failures_keep_their_status(self):
        for case in self.CASES:
            with self.subTest(script=case[0], trap=case[1]):
                passed = self._run_trap(case, case[2] + "=1\nexit 0\n", success=True)
                self.assertEqual(passed.returncode, 0, passed.stderr)
                self.assertNotIn("FAIL", passed.stderr)
                for body, status in (("exit 7\n", 7), ("false\n", 1)):
                    failed = self._run_trap(case, body)
                    self.assertEqual(failed.returncode, status, failed.stderr)
                    self.assertNotIn("stopped before finishing", failed.stderr)

    def test_failed_first_placement_removes_the_unaccepted_worker(self):
        result = self._run_trap(self.CASES[1], "exit 7\n", existed=False)
        self.assertEqual(result.returncode, 7, result.stderr)

    def test_all_fail_fast_exit_traps_have_a_finished_flag(self):
        scripts = Path(__file__).resolve().parents[1]
        self.assertEqual(unguarded_exit_scripts(scripts), [])

    def test_source_check_rejects_planted_unguarded_exit_traps(self):
        scratch_root = Path(__file__).resolve().parents[2] / "target/scratch"
        scratch_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch_root) as tmp:
            scripts = Path(tmp)
            for index, options in enumerate(("-e", "-eu", "-euo pipefail", "-u -e")):
                (scripts / f"bad{index}.sh").write_text(
                    f"#!/bin/sh\nset {options}\n# finished=0\n# finished=1\ntrap 'cleanup' EXIT\n"
                )
            (scripts / "bad4.sh").write_text("set -eu\ntrap cleanup 0 # remove scratch\n")
            (scripts / "good.sh").write_text(
                "set -eu\nrun_finished=0\ntrap 'cleanup; [ $run_finished = 1 ] || exit 1' EXIT\nrun_finished=1\n"
            )
            (scripts / "plain-flag.sh").write_text(
                "set -e\nfinished=0\ntrap 'cleanup; test ${finished} = 1 || exit 1' 0\nfinished=1\n"
            )
            (scripts / "no-trap.sh").write_text("set -eu\n")
            (scripts / "no-errexit.sh").write_text("set -u\ntrap cleanup EXIT\n")
            (scripts / "comment.sh").write_text("set -eu\n# trap cleanup EXIT\n")
            self.assertEqual(unguarded_exit_scripts(scripts), [f"bad{i}.sh" for i in range(5)])


if __name__ == "__main__":
    unittest.main()
