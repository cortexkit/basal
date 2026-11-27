"""Exercise the release asset format and verification without building Rust."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from zipfile import ZipFile

from script.release import assets


REVISION = "a" * 40
VERSION = "0.1.0"
BINARIES = ("ck-basal", "ck-basal-worker")
TARGETS = ("darwin-arm64", "darwin-x64", "linux-x64", "linux-arm64", "windows-x64")


class ReleaseAssets(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.built = self.root / "built"

    def pack(self, target, binary):
        assets.pack(argparse.Namespace(source=self.source, directory=self.built, target=target, binary=binary))

    def args(self, target):
        return argparse.Namespace(directory=self.built, target=target, revision=REVISION, version=VERSION)

    def build_pair(self, target, worker_revision=REVISION, dirty=False, confinement_ok=False):
        parent = self.source / "ck-basal"
        manifest = {"module_id": "basal", "module_version": VERSION, "provenance": {"build_git_sha": REVISION}}
        parent.write_text(f'''#!/usr/bin/env python3
import pathlib, sys
self = pathlib.Path(sys.argv[0])
assert self.name == "ckdev-basal"
assert self.with_name(self.name + "-worker").is_file()
if sys.argv[1] == "--version":
    print("ck-basal {VERSION} ({REVISION})")
elif sys.argv[1] == "--manifest":
    print({json.dumps(manifest)!r})
else:
    sys.exit(2)
''')
        worker = self.source / "ck-basal-worker"
        worker_version = f"ck-basal-worker 0.2.0 ({worker_revision}{', dirty' if dirty else ''})"
        worker.write_text(f'''#!/usr/bin/env python3
import pathlib, sys
assert pathlib.Path(sys.argv[0]).name == "ckdev-basal-worker"
if sys.argv[1] == "--version":
    print({worker_version!r})
elif sys.argv[1] == "--gate-fixture":
    sys.exit({0 if confinement_ok else 3})
else:
    sys.exit(2)
''')
        for binary in (parent, worker):
            binary.chmod(0o755)
            self.pack(target, binary.name)

    def test_checksums_follow_verifying_both_archive_members_side_by_side(self):
        self.build_pair("linux-x64")
        assets.verify_target(self.args("linux-x64"))
        for binary in BINARIES:
            archive = self.built / f"{binary}-linux-x64.zip"
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            self.assertEqual(archive.with_suffix(".zip.sha256").read_text(), f"{digest}  {archive.name}\n")

    def test_rejects_dirty_or_mismatched_worker(self):
        for index, (revision, dirty) in enumerate((("b" * 40, False), (REVISION, True))):
            with self.subTest(revision=revision, dirty=dirty):
                self.built = self.root / f"built-{index}"
                self.build_pair("linux-x64", worker_revision=revision, dirty=dirty)
                with self.assertRaisesRegex(ValueError, "ck-basal-worker-linux-x64.zip: unexpected --version"):
                    assets.verify_target(self.args("linux-x64"))
                self.assertEqual(list(self.built.glob("*.sha256")), [])

    def test_macos_worker_gate_must_accept_the_extracted_worker(self):
        gate = self.root / "sign-worker.sh"
        gate.write_text('''set -eu
[ "$1" = verify ]
[ "$BASAL_WORKER_IDENTIFIER" = ck-basal-worker ]
"$2" --gate-fixture
''')
        for passes in (False, True):
            with self.subTest(gate_passes=passes), patch.object(assets, "WORKER_GATE", gate):
                self.built = self.root / f"built-{passes}"
                self.build_pair("darwin-arm64", confinement_ok=passes)
                if passes:
                    assets.verify_target(self.args("darwin-arm64"))
                    self.assertEqual(len(list(self.built.glob("*.sha256"))), 2)
                else:
                    with self.assertRaises(subprocess.CalledProcessError) as failure:
                        assets.verify_target(self.args("darwin-arm64"))
                    self.assertEqual(failure.exception.returncode, 3)
                    self.assertEqual(list(self.built.glob("*.sha256")), [])

    def make_inventory(self):
        for target in TARGETS:
            for binary in BINARIES:
                name = binary + (".exe" if target == "windows-x64" else "")
                path = self.source / name
                path.write_text(f"fixture for {binary} on {target}\n")
                path.chmod(0o755)
                self.pack(target, binary)
                archive = self.built / f"{binary}-{target}.zip"
                with ZipFile(archive) as zipped:
                    self.assertEqual(zipped.namelist(), [name])
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                archive.with_suffix(".zip.sha256").write_text(f"{digest}  {archive.name}\n")

    def assemble(self, output, *, dry_run=False, publish_windows=False):
        assets.assemble(argparse.Namespace(directory=self.built, output=output, revision=REVISION,
                                            dry_run=dry_run, publish_windows=publish_windows))

    def test_manual_rehearsal_keeps_windows_but_public_release_omits_it(self):
        self.make_inventory()
        dry = self.root / "dry"
        release = self.root / "release"
        with patch.dict(os.environ, {"BASAL_REQUIRES_CORE": ""}):
            with self.assertRaisesRegex(ValueError, "BASAL_REQUIRES_CORE must be set"):
                self.assemble(release)
            self.assemble(dry, dry_run=True)
        with patch.dict(os.environ, {"BASAL_REQUIRES_CORE": "1.2.3"}):
            self.assemble(release)
        manifest = {"binaries": {"ck-basal": {"reports": REVISION},
                                 "ck-basal-worker": {"reports": None}}}
        self.assertEqual(json.loads((dry / "release-manifest.json").read_text()), manifest)
        self.assertEqual(json.loads((release / "release-manifest.json").read_text()),
                         {**manifest, "requires_core": "1.2.3"})
        self.assertEqual(len(list(dry.glob("*.zip*"))), 20)
        self.assertEqual(len(list(release.glob("*.zip*"))), 16)
        self.assertEqual(sorted(p.name for p in dry.glob("*-windows-x64.zip")),
                         ["ck-basal-windows-x64.zip", "ck-basal-worker-windows-x64.zip"])
        self.assertFalse(list(release.glob("*-windows-x64.zip*")))

    def test_assembly_refuses_corrupt_windows_archive_even_when_withheld(self):
        self.make_inventory()
        archive = self.built / "ck-basal-worker-windows-x64.zip"
        archive.write_bytes(b"not the archived worker")
        with patch.dict(os.environ, {"BASAL_REQUIRES_CORE": "1.2.3"}):
            with self.assertRaisesRegex(ValueError, "ck-basal-worker-windows-x64.zip.sha256: checksum does not match"):
                self.assemble(self.root / "release")
