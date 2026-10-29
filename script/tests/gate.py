"""Selection and snapshot contracts for the local/remote gate boundary."""

from contextlib import contextmanager
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from script import gate_support as gate


@contextmanager
def in_directory(path):
    old = Path.cwd()
    os.chdir(path)
    try:
        yield
    finally:
        os.chdir(old)


class GateTests(unittest.TestCase):
    def setUp(self):
        self.manifest = {"workspace": {"dependencies": {"shared": {"package": "base", "path": "crates/base"}}}}
        self.packages = {
            "base": (Path("crates/base"), {}),
            "consumer": (Path("crates/consumer"), {"dependencies": {"shared": {"workspace": True}}}),
            "leaf": (Path("crates/leaf"), {"target": {"cfg(windows)": {"dev-dependencies": {"consumer": {"path": "../consumer"}}}}}),
            "other": (Path("crates/other"), {}),
        }

    def test_changed_crate_includes_transitive_and_target_dependents(self):
        self.assertEqual(
            gate.select_packages(self.manifest, self.packages, {"crates/base/src/lib.rs"}),
            ["base", "consumer", "leaf"],
        )

    def test_leaf_does_not_select_unrelated_packages(self):
        self.assertEqual(
            gate.select_packages(self.manifest, self.packages, {"crates/leaf/src/lib.rs"}), ["leaf"]
        )

    def test_global_deleted_unknown_and_empty_changes_select_workspace(self):
        for paths in ({"Cargo.lock"}, {"script/gate.sh"}, {"crates/deleted/Cargo.toml"}, set()):
            with self.subTest(paths=paths):
                self.assertEqual(gate.select_packages(self.manifest, self.packages, paths), sorted(self.packages))

    def test_workspace_flag_overrides_leaf_selection(self):
        self.assertEqual(
            gate.select_packages(self.manifest, self.packages, {"crates/leaf/src/lib.rs"}, True), sorted(self.packages)
        )

    def test_worker_selects_testkit_artifact_consumers(self):
        packages = {
            "basal-worker": (Path("crates/basal-worker"), {}),
            "basal-testkit": (Path("crates/basal-testkit"), {}),
            "consumer": (Path("crates/consumer"), {"dev-dependencies": {"basal-testkit": "*"}}),
        }
        self.assertEqual(
            gate.select_packages(self.manifest, packages, {"crates/basal-worker/src/main.rs"}),
            ["basal-testkit", "basal-worker", "consumer"],
        )

    def test_macos_lane_is_path_conditional(self):
        for filename in ("crates/basal-worker/sandbox/macos/mod.rs", "crates/x/src/os_macos.rs", "script/sign-worker.sh", "crates/basal-worker/sandbox/worker.sb", "crates/basal-module/src/process.rs", "script/flows-rig.sh", "Cargo.lock"):
            self.assertTrue(gate.macos_paths_changed({filename}), filename)
        self.assertFalse(gate.macos_paths_changed({"crates/basal-core/src/lib.rs"}))

    def test_fmt_checks_target_roots_and_custom_edition_without_cargo(self):
        with tempfile.TemporaryDirectory() as tmp, in_directory(tmp):
            for filename in ("src/lib.rs", "tests/contract.rs", "src/bin/tool/main.rs", "build.rs", "examples/demo.rs"):
                path = Path("crate") / filename
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("fn main() {}\n")
            packages = {"example": (Path("crate"), {"package": {"edition": {"workspace": True}}})}
            roots = gate.format_roots({"workspace": {"package": {"edition": "2024"}}}, packages)
            self.assertEqual(roots, {"2024": {
                Path("crate/src/lib.rs"), Path("crate/tests/contract.rs"), Path("crate/src/bin/tool/main.rs"),
                Path("crate/build.rs"), Path("crate/examples/demo.rs"),
            }})
            with patch.object(gate, "workspace", return_value=({"workspace": {"package": {"edition": "2024"}}}, packages)), patch.object(gate.subprocess, "check_call") as call:
                gate.check_format()
            commands = [entry.args[0][0] for entry in call.call_args_list]
            self.assertEqual(commands, ["rustfmt", "rustfmt"])
            self.assertIn("--check", call.call_args_list[1].args[0])

    def test_snapshot_includes_dirty_untracked_deletions_but_not_ignored(self):
        with tempfile.TemporaryDirectory() as tmp, in_directory(tmp):
            subprocess.run(["git", "init", "-q", "source"], check=True)
            with in_directory("source"):
                Path("kept").write_text("before\n")
                Path("deleted").write_text("delete me\n")
                Path(".gitignore").write_text("ignored\n")
                subprocess.run(["git", "add", "."], check=True)
                subprocess.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"], check=True)
                base = gate.git("rev-parse", "HEAD").decode().strip()
                Path("kept").write_text("after\n")
                Path("deleted").unlink()
                Path("new").write_text("untracked\n")
                Path("ignored").write_text("not for the runner\n")
                self.assertEqual(gate.changed_paths(base), {"kept", "deleted", "new"})
                gate.snapshot(Path(tmp) / "snapshot")
            snapshot = Path(tmp) / "snapshot"
            self.assertEqual((snapshot / "kept").read_text(), "after\n")
            self.assertEqual((snapshot / "new").read_text(), "untracked\n")
            self.assertFalse((snapshot / "deleted").exists())
            self.assertFalse((snapshot / "ignored").exists())

    def test_ci_action_is_the_only_nats_pin_source(self):
        version, pins = gate.nats_pins()
        self.assertRegex(version, r"^\d+\.\d+\.\d+$")
        self.assertEqual(set(pins), {"linux-amd64", "linux-arm64", "windows-amd64", "windows-arm64", "darwin-amd64", "darwin-arm64"})
        for extension, sha in pins.values():
            self.assertIn(extension, {"tar.gz", "zip"})
            self.assertRegex(sha, "^[a-f0-9]{64}$")

    def test_nats_cache_is_checksum_verified_and_reused(self):
        sha = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        with tempfile.TemporaryDirectory() as tmp, in_directory(tmp):
            cache = Path("target/scratch/gate-downloads")
            cache.mkdir(parents=True)
            archive = "nats-server-v1.2.3-windows-amd64.zip"
            (cache / archive).write_bytes(b"hello")
            with patch.object(gate, "nats_pins", return_value=("1.2.3", {"windows-amd64": ("zip", sha)})), patch.object(gate.subprocess, "check_call") as call:
                gate.prepare_tools("snapshot", ["windows"])
                call.assert_not_called()
            output = Path("snapshot/.gate-inputs/nats")
            self.assertEqual((output / archive).read_bytes(), b"hello")
            self.assertIn(sha, (output / "manifest.sh").read_text())

    def test_nats_rejects_corrupt_cache_and_bad_download(self):
        sha = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        with tempfile.TemporaryDirectory() as tmp, in_directory(tmp):
            cache = Path("target/scratch/gate-downloads")
            cache.mkdir(parents=True)
            (cache / "nats-server-v1.2.3-windows-amd64.zip").write_bytes(b"corrupt")
            def download(command):
                Path(command[-1]).write_bytes(b"still corrupt")
            with patch.object(gate, "nats_pins", return_value=("1.2.3", {"windows-amd64": ("zip", sha)})), patch.object(gate.subprocess, "check_call", side_effect=download) as call:
                with self.assertRaisesRegex(SystemExit, "NATS checksum mismatch"):
                    gate.prepare_tools("snapshot", ["windows"])
                call.assert_called_once()
            self.assertFalse((Path("snapshot/.gate-inputs/nats/nats-server-v1.2.3-windows-amd64.zip")).exists())

    def test_print_plan_is_read_only_and_lists_every_forced_lane(self):
        script = Path(gate.__file__).with_name("gate.sh")
        result = subprocess.run(["bash", str(script), "--print-plan", "--workspace", "--all-platforms", "--base", "HEAD"], text=True, capture_output=True, check=True)
        self.assertIn("script/gate.sh --prepare-tools", result.stdout)
        for lane in ("linux", "windows", "macos", "scripts"):
            self.assertIn(f"bash script/gate.sh --lane {lane}", result.stdout)
        self.assertIn("runon: linux,16c", result.stdout)
        self.assertIn("script/gate.sh --clean-tools", result.stdout)
        self.assertNotIn("Gate logs:", result.stdout)

    def test_offline_lane_refuses_toolchain_older_than_workspace_minimum(self):
        script = Path(gate.__file__).with_name("gate.sh")
        with tempfile.TemporaryDirectory() as tmp:
            for tool, response in (("uname", "MINGW64_NT"), ("rustc", "rustc 1.88.0 (fixture)"), ("cargo", "cargo 1.88.0 (fixture)")):
                path = Path(tmp) / tool
                path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{response}'\n")
                path.chmod(0o755)
            result = subprocess.run(["bash", str(script), "--lane", "windows", "basal-proto"], text=True, capture_output=True, env={**os.environ, "PATH": f"{tmp}:{os.environ['PATH']}"})
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("older than workspace rust-version", result.stderr)
            self.assertNotIn("Missing offline NATS", result.stderr)

    def test_windows_skips_only_unavailable_offline_git_sources(self):
        script = Path(gate.__file__).with_name("gate.sh")
        for message, skipped in (("can't checkout from 'https://example.invalid/repo': you are in the offline mode (--offline)", True), ("package version does not match Cargo.lock", False)):
            with self.subTest(message=message), tempfile.TemporaryDirectory() as tmp:
                for tool, response in (("uname", "MINGW64_NT"), ("rustc", "rustc 1.99.0 (fixture)")):
                    path = Path(tmp) / tool
                    path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{response}'\n")
                    path.chmod(0o755)
                cargo = Path(tmp) / "cargo"
                cargo.write_text('#!/bin/sh\nif [ "$1" = --version ]; then echo "cargo 1.99.0 (fixture)"; exit 0; fi\nprintf "%s\\n" "$FIXTURE_METADATA_ERROR" >&2\nexit 101\n')
                cargo.chmod(0o755)
                result = subprocess.run(["bash", str(script), "--lane", "windows", "basal-proto"], text=True, capture_output=True, env={**os.environ, "PATH": f"{tmp}:{os.environ['PATH']}", "FIXTURE_METADATA_ERROR": message})
                self.assertEqual(result.returncode == 0, skipped)
                self.assertEqual("SKIP windows:" in result.stdout, skipped)
                self.assertNotIn("Missing offline NATS", result.stderr)

    def test_macos_build_dir_is_unique_even_with_inherited_target_dir(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "script").mkdir()
            script = root / "script/gate.sh"
            shutil.copy2(Path(gate.__file__).with_name("gate.sh"), script)
            (root / "Cargo.toml").write_text('[workspace.package]\nrust-version = "1.89.0"\n')
            (root / "bin").mkdir()
            for tool, response in (("uname", "Darwin"), ("rustc", "rustc 1.99.0 (fixture)"), ("cargo", "cargo 1.99.0 (fixture)")):
                path = root / "bin" / tool
                path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{response}'\n")
                path.chmod(0o755)
            directories = []
            for _ in range(2):
                result = subprocess.run(["bash", str(script), "--lane", "macos", "basal-module"], text=True, capture_output=True, env={**os.environ, "PATH": f"{root / 'bin'}:{os.environ['PATH']}", "CARGO_TARGET_DIR": "stale-cache"})
                self.assertIn("Missing offline NATS", result.stderr)
                directory = result.stdout.split("Isolated macOS build directory: ", 1)[1].strip()
                self.assertEqual(Path(directory).parent, root / "target")
                self.assertTrue(Path(directory).is_dir())
                directories.append(directory)
            self.assertNotEqual(*directories)


if __name__ == "__main__":
    unittest.main()
