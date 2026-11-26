"""Planted tests for the tracked .cortexkit allowlist guard."""

from pathlib import Path
import os
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-tracked-ignored.py"
GITIGNORE = ".cortexkit/*\n!.cortexkit/WORKER.md\n"


def isolated_git_env(home):
    """Git with no system or global config, so a host hook or setting can't
    change what the fixture repository holds."""
    return {
        "PATH": os.environ["PATH"],
        "HOME": str(home),
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": os.devnull,
    }


class TrackedIgnoredChecks(unittest.TestCase):
    def check(self, path, force, gitignore=GITIGNORE):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = isolated_git_env(root)
            git = ["git", "-C", str(root)]
            subprocess.run(["git", "init", "--quiet", str(root)], check=True, env=env)
            (root / ".gitignore").write_text(gitignore)
            planted = root / path
            planted.parent.mkdir(parents=True, exist_ok=True)
            planted.write_text("x\n")
            add = git + ["add"] + (["-f"] if force else []) + [".gitignore", path]
            subprocess.run(add, check=True, capture_output=True, env=env)
            return subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(root)],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )

    def test_planted_force_added_internal_doc_is_refused(self):
        result = self.check(".cortexkit/internal-docs/designs/note.md", force=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(".cortexkit/internal-docs/designs/note.md", result.stderr)

    def test_planted_unignored_internal_doc_is_refused(self):
        # A stray `!` rule makes the file tracked but not ignored; an
        # ignore-based check would miss it.
        gitignore = GITIGNORE + "!.cortexkit/notes.md\n"
        result = self.check(".cortexkit/notes.md", force=False, gitignore=gitignore)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(".cortexkit/notes.md", result.stderr)

    def test_failed_listing_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--root", directory],
                env=isolated_git_env(Path(directory)),
                text=True,
                capture_output=True,
                check=False,
            )
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)

    def test_project_settings_file_passes(self):
        gitignore = GITIGNORE + "!.cortexkit/basal.jsonc\n"
        result = self.check(".cortexkit/basal.jsonc", force=False, gitignore=gitignore)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

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
