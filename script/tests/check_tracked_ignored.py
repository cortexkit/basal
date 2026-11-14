"""Planted tests for the tracked ignored .cortexkit guard."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-tracked-ignored.py"
GITIGNORE = ".cortexkit/*\n!.cortexkit/WORKER.md\n"


class TrackedIgnoredChecks(unittest.TestCase):
    def check(self, path, force):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            git = ["git", "-C", str(root)]
            subprocess.run(["git", "init", "--quiet", str(root)], check=True)
            (root / ".gitignore").write_text(GITIGNORE)
            planted = root / path
            planted.parent.mkdir(parents=True, exist_ok=True)
            planted.write_text("x\n")
            add = git + ["add"] + (["-f"] if force else []) + [".gitignore", path]
            subprocess.run(add, check=True, capture_output=True)
            return subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(root)],
                text=True,
                capture_output=True,
                check=False,
            )

    def test_planted_force_added_internal_doc_is_refused(self):
        result = self.check(".cortexkit/internal-docs/designs/note.md", force=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(".cortexkit/internal-docs/designs/note.md", result.stderr)

    def test_reincluded_file_passes(self):
        result = self.check(".cortexkit/WORKER.md", force=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_repository_passes(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT)], text=True, capture_output=True, check=False
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
