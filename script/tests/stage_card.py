"""Offline placement cards: real strings and SQLite, isolated homes, no builds."""

import json
import os
from pathlib import Path
import shlex
import sqlite3
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "stage.sh"
SOURCE = SCRIPT.read_text()
# Exercise the production option parser, validation and card functions without
# running the build/sign/stage side effects, as the offline flow rig tests do.
HELPERS = (SOURCE.split("# ---------------------------------------------------------------- verify")[0]
           + SOURCE.split("# ---------------------------------------------------------------- marker\n")[1]
           .split("# ---------------------------------------------------------------- prepare card\n")[0]
           + SOURCE.split("# ---------------------------------------------------------------- card\n")[1]
           .split("# ---------------------------------------------------------------- output\n")[0])
SHA = "a" * 40
OPTIONS = ["--basal-marker", "new basal", "--basal-control", "basal control",
           "--worker-marker", "new worker", "--worker-control", "worker control"]


class StageCardChecks(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.stage = self.home / "stage with spaces"
        self.stage.mkdir()
        self.share = self.home / ".local/share/cortexkit"
        self.live = self.share / "bin"
        self.live.mkdir(parents=True)
        (self.home / "controls").mkdir()
        # Deliberately unordered: the newest migration is the maximum, not the
        # last declaration. git's committed-schema read is the only mocked input.
        (self.home / "schema.rs").write_text("version: 1,\nversion: 7,\nversion: 4,\n")
        self.write_binaries()
        for binary, digest in [("ck-basal", "basal-digest"), ("ck-basal-worker", "worker-digest")]:
            (self.stage / (binary + ".sha256")).write_text(digest + "  " + binary + "\n")
        # gh's response depends on its actual event filter. The newer scheduled
        # audit has a different conclusion from the push run for this commit.
        tools = self.home / "tools"
        tools.mkdir()
        gh = tools / "gh"
        gh.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
pathlib.Path(os.environ["HOME"], "gh-args.json").write_text(json.dumps(args))
if "--event" in args and args[args.index("--event") + 1] == "push":
    print("completed success, https://example.test/push")
else:
    print("completed failure, https://example.test/nightly")
''')
        gh.chmod(0o755)
        self.env = {**os.environ, "HOME": str(self.home),
                    "PATH": str(tools) + os.pathsep + os.environ["PATH"],
                    "PYTHONDONTWRITEBYTECODE": "1"}

    def write_binaries(self):
        for binary, marker, control, install_control in [
            ("ck-basal", "new basal", "basal control", "flow.install"),
            ("ck-basal-worker", "new worker", "worker control", "refusing to run unconfined"),
        ]:
            (self.stage / binary).write_bytes(
                b"\x00" + (SHA + "\n" + marker + "\n" + marker + "\n" + control
                          + "\n" + install_control + "\n").encode() + b"\x00")
            (self.live / binary).write_bytes(b"\x00old revision\n" + control.encode() + b"\n\x00")

    def store(self, version):
        path = self.share / "basal/store.db"
        path.parent.mkdir(exist_ok=True)
        with sqlite3.connect(path) as db:
            db.execute("CREATE TABLE cortexkit_schema_version(version INTEGER)")
            db.executemany("INSERT INTO cortexkit_schema_version VALUES (?)", [(1,), (version,)])
            db.execute("CREATE TABLE retained(value TEXT)")
            db.execute("INSERT INTO retained VALUES ('unchanged')")
        return path

    def run_card(self, options=None):
        code = HELPERS + '''
SHA=''' + shlex.quote(SHA) + '''
DIRTY=false
PUSHED='on origin/main (fixture)'
STAGE_DIR="$HOME/stage with spaces"
CONTROL_DIR="$HOME/controls"
codesign_flags() { printf '0x10000(runtime)'; }
git() {
  [ "$*" = "-C $ROOT show HEAD:crates/basal-core/src/schema.rs" ] || exit 90
  /bin/cat "$HOME/schema.rs"
}
write_card
'''
        result = subprocess.run(["/bin/sh", "-c", code, str(SCRIPT)]
                                + (OPTIONS if options is None else options),
                                env=self.env, text=True, capture_output=True, check=False)
        card = self.stage / "card.md"
        return result, card.read_text() if card.exists() else None

    def success(self, options=None):
        result, card = self.run_card(options)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNotNone(card)
        assert card is not None
        return card

    def commands(self, card):
        return [shlex.split(line.strip().strip("`")) for line in card.splitlines()
                if line.strip().startswith("`place-module.sh ")]

    def test_first_install_card_is_unchanged(self):
        for path in self.live.iterdir():
            path.unlink()
        card = self.success([])
        expected = (Path(__file__).parent / "fixtures/first_install_card.md").read_text()
        self.assertEqual(card, expected.replace("@STAGE@", str(self.stage)).replace("@SHA@", SHA))

    def test_update_counts_controls_and_post_placement_checks(self):
        path = self.store(7)
        before = path.read_bytes()
        card = self.success()
        self.assertIn('ck-basal marker "new basal": staged 2 / live 0', card)
        self.assertIn('ck-basal-worker marker "new worker": staged 2 / live 0', card)
        self.assertIn('ck-basal control "basal control": staged 1 / live 1', card)
        self.assertIn('ck-basal-worker control "worker control": staged 1 / live 1', card)
        self.assertNotIn("--install", card)
        for claim in [SHA, "same inode", "sign-worker.sh verify", "schema version 7", "tables intact", "flow.list"]:
            self.assertIn(claim, card)
        self.assertEqual(path.read_bytes(), before)
        with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as db:
            self.assertEqual(db.execute("SELECT * FROM retained").fetchall(), [("unchanged",)])

    def test_migrates_exactly_when_schema_rises(self):
        path = self.store(6)
        for live in [6, 7, 8]:
            with self.subTest(live=live):
                with sqlite3.connect(path) as db:
                    db.execute("UPDATE cortexkit_schema_version SET version = ?", (live,))
                card = self.success()
                commands = self.commands(card)
                basal = next(cmd for cmd in commands if str(self.stage / "ck-basal") in cmd)
                worker = next(cmd for cmd in commands if str(self.stage / "ck-basal-worker") in cmd)
                self.assertEqual("--migrates" in basal, live < 7)
                self.assertNotIn("--migrates", worker)
                self.assertIn(f"build schema 7 / live schema {live}", card)
                if live < 7:
                    self.assertEqual(basal[basal.index("--migrates") + 1], "~/.local/share/cortexkit/basal/store.db")
                    self.assertIn("old binary AND the store backup together", card)
                    self.assertIn("this build knows only up to M", card)

    def test_worker_first_without_restart_then_parent_restart(self):
        self.store(7)
        card = self.success()
        commands = self.commands(card)
        self.assertEqual(len(commands), 2)
        worker, basal = commands
        self.assertEqual(worker[worker.index("--staged") + 1], str(self.stage / "ck-basal-worker"))
        self.assertIn("--no-restart", worker)
        self.assertEqual(worker[worker.index("--dest") + 1], "~/.local/share/cortexkit/bin/ck-basal-worker")
        self.assertEqual(basal[basal.index("--staged") + 1], str(self.stage / "ck-basal"))
        self.assertNotIn("--no-restart", basal)
        for cmd in commands:
            self.assertEqual(cmd[cmd.index("--module") + 1], "basal")
        self.assertIn("Warm workers are spawned by ck-basal", card)
        self.assertIn("worker protocol change is additive", card)

    def test_marker_refusal_writes_no_card(self):
        self.store(7)
        for binary, needle in [("ck-basal", "new basal"), ("ck-basal-worker", "new worker")]:
            for invalid in ["missing staged", "present live"]:
                with self.subTest(binary=binary, invalid=invalid):
                    self.write_binaries()
                    if invalid == "missing staged":
                        path = self.stage / binary
                        path.write_bytes(path.read_bytes().replace(needle.encode(), b"absent"))
                    else:
                        with (self.live / binary).open("ab") as dest:
                            dest.write(needle.encode() + b"\n")
                    result, card = self.run_card()
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("need staged >= 1 / live 0", result.stderr)
                    self.assertIsNone(card)

    def test_control_refusal_writes_no_card(self):
        self.store(7)
        for binary, needle in [("ck-basal", "basal control"), ("ck-basal-worker", "worker control")]:
            for directory in [self.stage, self.live]:
                with self.subTest(binary=binary, directory=directory):
                    self.write_binaries()
                    path = directory / binary
                    path.write_bytes(path.read_bytes().replace(needle.encode(), b"absent"))
                    result, card = self.run_card()
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("need both >= 1", result.stderr)
                    self.assertIsNone(card)

    def test_update_requires_each_option_and_both_live_files(self):
        self.store(7)
        for index in range(0, len(OPTIONS), 2):
            with self.subTest(missing=OPTIONS[index]):
                result, card = self.run_card(OPTIONS[:index] + OPTIONS[index + 2:])
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("an update requires", result.stderr)
                self.assertIsNone(card)
        (self.live / "ck-basal-worker").unlink()
        result, card = self.run_card()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("needs the live destination", result.stderr)
        self.assertIsNone(card)

    def test_missing_store_is_not_created(self):
        (self.share / "basal").mkdir()
        result, card = self.run_card()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot compare", result.stderr)
        self.assertIsNone(card)
        self.assertFalse((self.share / "basal/store.db").exists())

    def test_live_schema_reads_committed_wal(self):
        path = self.store(6)
        with sqlite3.connect(path) as db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("PRAGMA wal_autocheckpoint=0")
            db.execute("INSERT INTO cortexkit_schema_version VALUES (7)")
            db.commit()
            self.assertTrue(Path(str(path) + "-wal").exists())
            card = self.success()
            self.assertIn("build schema 7 / live schema 7", card)
            self.assertNotIn("--migrates", card)

    def test_push_event_ci_run_not_newer_nightly(self):
        self.store(7)
        card = self.success()
        self.assertIn("CI for that commit (push event): completed success, https://example.test/push", card)
        self.assertNotIn("nightly", card)
        args = json.loads((self.home / "gh-args.json").read_text())
        self.assertEqual(args[args.index("--commit") + 1], SHA)
        self.assertEqual(args[args.index("--event") + 1], "push")
        self.assertEqual(args[args.index("--workflow") + 1], "CI")

    def test_literal_needles_are_preserved_in_commands(self):
        self.store(7)
        needle = "new 'basal' $(false)"
        path = self.stage / "ck-basal"
        path.write_bytes(path.read_bytes().replace(b"new basal", needle.encode()))
        options = OPTIONS.copy()
        options[1] = needle
        basal = next(cmd for cmd in self.commands(self.success(options))
                     if str(self.stage / "ck-basal") in cmd)
        self.assertEqual(basal[basal.index("--marker") + 1], needle)


if __name__ == "__main__":
    unittest.main(verbosity=2)
