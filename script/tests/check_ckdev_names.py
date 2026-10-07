"""Tests for script/check-ckdev-names.py, which fails when code launches a
production-named basal binary (ck-basal, ck-basal-worker) instead of a ckdev-
copy. Each "planted" case writes such a launch into a scratch file and
expects the check to refuse it; documentation and signing inputs that merely
mention the names must pass."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-ckdev-names.py"


class CkdevNameChecks(unittest.TestCase):
    def check_source(self, relative, source):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / relative
            path.parent.mkdir(parents=True)
            path.write_text(source)
            return subprocess.run([sys.executable, str(SCRIPT), "--root", str(root)],
                                  text=True, capture_output=True, check=False)

    def test_planted_cargo_launch_is_refused_and_named(self):
        result = self.check_source("crates/example/tests/launch.rs",
                                   'Command::new(env!("CARGO_BIN_EXE_ck-basal")).spawn(); // NON-VACUITY BREAK\n')
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("crates/example/tests/launch.rs:1", result.stderr)
        self.assertIn("bypasses dev_binary", result.stderr)

    def test_helper_routed_cargo_launch_passes(self):
        result = self.check_source("crates/example/tests/launch.rs",
                                   'Command::new(dev_binary(env!("CARGO_BIN_EXE_ck-basal"))).spawn();\n')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_planted_literal_test_paths_are_refused(self):
        for source in ['Command::new("target/debug/ck-basal").spawn();',
                       'let binary = dir.join("ck-basal-worker"); Command::new(binary).spawn();']:
            with self.subTest(source=source):
                result = self.check_source("crates/example/tests/launch.rs", source)
                self.assertEqual(result.returncode, 1, result.stderr)

    def test_planted_script_launches_are_refused(self):
        for source in ['manifest=$("$SCRATCH/ck-basal" --manifest)',
                       'debugger_probe "$SCRATCH/$bin" --version',
                       'sh script/sign-worker.sh verify "$BIN/ck-basal-worker"',
                       'basal-worker\tbasal\tck-basal-worker\tck-basal-worker']:
            with self.subTest(source=source):
                result = self.check_source("script/smoke.sh", source)
                self.assertEqual(result.returncode, 1, result.stderr)

    def test_planted_python_launch_is_refused(self):
        result = self.check_source("script/smoke.py", 'subprocess.run(["target/debug/ck-basal", "--version"])')
        self.assertEqual(result.returncode, 1, result.stderr)

    def test_signing_build_inputs_docs_and_placed_paths_pass(self):
        source = '''# ck-basal is the production module
cargo build --bin ck-basal
sign_hardened "$SCRATCH/ck-basal" ck-basal
manifest=$("$SCRATCH/ckdev-basal" --manifest)
sh script/sign-worker.sh verify "$HOME/.local/share/cortexkit/bin/ck-basal-worker"
"$HOME/.local/share/cortexkit/bin/ck-basal" --manifest
basal\tbasal\tck-basal\tckdev-basal
'''
        result = self.check_source("script/smoke.sh", source)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_repository_passes(self):
        result = subprocess.run([sys.executable, str(SCRIPT)], text=True, capture_output=True, check=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
