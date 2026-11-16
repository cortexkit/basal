"""Tests for the Rust source file size guard using temporary repositories."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-file-size.py"


class FileSizeChecks(unittest.TestCase):
    def check(self, files):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "--quiet", str(root)], check=True)
            for name, lines in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("x\n" * lines)
            subprocess.run(["git", "-C", str(root), "add", "--", "."], check=True)
            return subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(root)],
                text=True,
                capture_output=True,
                check=False,
            )

    def test_oversized_sources_are_refused_and_named(self):
        result = self.check({"src/large.rs": 5001, "src/other.rs": 5002})
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("src/large.rs: 5001 lines", result.stderr)
        self.assertIn("src/other.rs: 5002 lines", result.stderr)

    def test_limit_and_exempt_test_files_pass(self):
        result = self.check(
            {
                "src/at_limit.rs": 5000,
                "tests/integration.rs": 5001,
                "src/tests/nested.rs": 5001,
                "src/tests.rs": 5001,
                "src/widget_tests.rs": 5001,
                "benches/bench.rs": 5001,
            }
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_repository_passes(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT)], text=True, capture_output=True, check=False
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
