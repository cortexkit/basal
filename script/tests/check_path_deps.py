"""Tests for the Cargo path dependency boundary.

The scratch workspaces have no registry dependencies, so they resolve offline.
The check against basal's own workspace resolves online, as it does in CI.
"""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "check-path-deps.py"
REPOSITORY = Path(__file__).resolve().parents[2]


class PathDependencyChecks(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        # macOS resolves temporary directories through /var, which is a
        # symlink; compare real workspace paths to real package paths.
        self.temp = Path(os.path.realpath(temporary.name))

    def write_crate(self, directory, name, extra_manifest=""):
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "Cargo.toml").write_text(
            f'[package]\nname = "{name}"\nversion = "1.0.0"\nedition = "2021"\n'
            + extra_manifest
        )
        source = directory / "src"
        source.mkdir(exist_ok=True)
        (source / "lib.rs").write_text("pub fn value() -> u8 { 1 }\n")

    def prepare_lockfile(self, manifest):
        result = subprocess.run(
            ["cargo", "generate-lockfile", "--offline", "--manifest-path", str(manifest)],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def run_check(self, root):
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--manifest-path",
                str(root / "Cargo.toml"),
                "--offline",
            ],
            text=True,
            capture_output=True,
            check=False,
        )

    def test_clean_workspace_passes(self):
        root = self.temp / "clean-workspace"
        self.write_crate(root, "clean-workspace")
        self.prepare_lockfile(root / "Cargo.toml")

        result = self.run_check(root)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")

    def test_outside_path_dependency_is_refused_and_named(self):
        root = self.temp / "dependency-workspace"
        outside = self.temp / "outside-dependency"
        self.write_crate(outside, "outside-dependency")
        self.write_crate(
            root,
            "dependency-workspace",
            '\n[dependencies]\noutside-dependency = { path = "../outside-dependency" }\n',
        )
        self.prepare_lockfile(root / "Cargo.toml")

        result = self.run_check(root)

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("outside-dependency", result.stderr)
        self.assertIn(str((outside / "Cargo.toml").resolve()), result.stderr)

    def test_outside_crates_io_patch_is_refused_and_named(self):
        root = self.temp / "patch-workspace"
        outside = self.temp / "patched-outside"
        self.write_crate(outside, "patched-outside")
        self.write_crate(
            root,
            "patch-workspace",
            '\n[dependencies]\npatched-outside = "1.0"\n'
            '\n[patch.crates-io]\npatched-outside = { path = "../patched-outside" }\n',
        )
        self.prepare_lockfile(root / "Cargo.toml")

        result = self.run_check(root)

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("patched-outside", result.stderr)
        self.assertIn(str((outside / "Cargo.toml").resolve()), result.stderr)

    def test_inside_path_dependency_passes(self):
        root = self.temp / "inside-workspace"
        dependency = root / "crates" / "inside-dependency"
        self.write_crate(dependency, "inside-dependency")
        self.write_crate(
            root,
            "inside-workspace",
            '\n[dependencies]\ninside-dependency = { path = "crates/inside-dependency" }\n',
        )
        self.prepare_lockfile(root / "Cargo.toml")

        result = self.run_check(root)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")

    def test_basal_workspace_passes(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--manifest-path", str(REPOSITORY / "Cargo.toml")],
            text=True,
            capture_output=True,
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")


if __name__ == "__main__":
    unittest.main(verbosity=2)
