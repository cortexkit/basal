"""Planted tests for the tracked local-path guard."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-local-paths.py"


class LocalPathChecks(unittest.TestCase):
    def check_source(self, source):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "--quiet", str(root)], check=True)
            planted = root / "capture.txt"
            planted.write_text(source)
            subprocess.run(["git", "-C", str(root), "add", "capture.txt"], check=True)
            return subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(root)],
                text=True,
                capture_output=True,
                check=False,
            )

    def test_planted_macos_home_path_is_refused(self):
        source = "/" + "Users" + "/" + "alice" + "/Library"
        result = self.check_source(source)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("capture.txt:1", result.stderr)

    def test_planted_linux_home_path_is_refused(self):
        source = "/" + "home" + "/" + "alice" + "/build"
        result = self.check_source(source)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("capture.txt:1", result.stderr)

    def test_planted_windows_home_path_is_refused(self):
        source = "C:" + "\\" + "Users" + "\\" + "Alice" + "\\" + "build"
        result = self.check_source(source)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("capture.txt:1", result.stderr)

    def test_repository_passes(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT)], text=True, capture_output=True, check=False
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
