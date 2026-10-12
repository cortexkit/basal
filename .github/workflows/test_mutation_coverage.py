"""Tests for check_mutation_coverage.py: every catalogue row is assigned to
 exactly one CI host, with portable packages on Linux or Windows-only modules on Windows,
and the shards' replay reports contain one successful replay of every row."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from check_mutation_coverage import ARTIFACT_PATTERN, PLATFORMS, mutation_catalogue, partition, verify_reports

SCRIPT = Path(__file__).resolve().parent / "check_mutation_coverage.py"


class MutationCoverageChecks(unittest.TestCase):
    def setUp(self):
        self.rows = [
            {"id": "portable", "package": "basal-host", "platforms": ["linux"]},
            {"id": "worker", "package": "basal-worker", "platforms": ["macos"]},
            {"id": "native", "package": "basal-host", "platforms": ["windows"],
             "file": "crates/basal-host/src/builtins/fs/windows.rs"},
        ]
        self.reports = [
            ("linux", [{"id": "portable", "outcome": "CAUGHT"},
                       {"id": "worker", "outcome": "SKIPPED_PLATFORM"},
                       {"id": "native", "outcome": "SKIPPED_PLATFORM"}]),
            ("macos", [{"id": "portable", "outcome": "SKIPPED_PLATFORM"},
                       {"id": "worker", "outcome": "CAUGHT"},
                       {"id": "native", "outcome": "SKIPPED_PLATFORM"}]),
            ("windows", [{"id": "portable", "outcome": "SKIPPED_PLATFORM"},
                         {"id": "worker", "outcome": "SKIPPED_PLATFORM"},
                         {"id": "native", "outcome": "CAUGHT"}]),
        ]

    def test_partition_accepts_one_host_per_row(self):
        self.assertEqual(partition(self.rows), {"linux": {"portable"}, "macos": {"worker"}, "windows": {"native"}})

    def test_partition_rejects_rows_selected_by_no_job_or_both(self):
        for package in (None, "basal-worker", "basal-module", "basal-testkit", "basal-rig"):
            for platforms in ([], ["unknown"], ["linux", "macos"], ["linux", "windows"],
                              ["macos", "windows"], ["windows", "unknown"], ["windows", "windows"], None):
                with self.subTest(package=package, platforms=platforms):
                    row = {"id": "unassigned", "runner": "command", "package": package}
                    if platforms is not None:
                        row["platforms"] = platforms
                    with self.assertRaisesRegex(ValueError, "exactly one CI platform"):
                        partition([row])

    def test_partition_accepts_worker_packages_on_either_host(self):
        for package in ("basal-worker", "basal-module", "basal-testkit", "basal-rig"):
            for host in PLATFORMS:
                with self.subTest(package=package, host=host):
                    row = {"id": "worker", "package": package, "platforms": [host]}
                    expected = {platform: set() for platform in PLATFORMS}
                    expected[host] = {"worker"}
                    self.assertEqual(partition([row]), expected)

    def test_partition_keeps_portable_packages_on_linux(self):
        for package in ("basal-proto", "basal-core", "basal-host"):
            with self.subTest(package=package):
                row = {"id": "portable", "package": package, "platforms": ["linux"]}
                self.assertEqual(partition([row]), {"linux": {"portable"}, "macos": set(), "windows": set()})
                row["platforms"] = ["macos"]
                with self.assertRaisesRegex(ValueError, "belong on Linux"):
                    partition([row])

    def test_windows_portable_rows_require_a_windows_only_module(self):
        for package in ("basal-proto", "basal-core", "basal-host"):
            for file in ("src/windows.rs", "src/windows/guard.rs", "src/nested/windows/check.rs"):
                with self.subTest(package=package, file=file):
                    row = {"id": "native", "package": package, "file": file, "platforms": ["windows"]}
                    self.assertEqual(partition([row]), {"linux": set(), "macos": set(), "windows": {"native"}})
            for file in ("", "src/lib.rs", "src/notwindows/guard.rs", "src/windows_guard.rs", "src/windows.rs.bak"):
                with self.subTest(package=package, file=file):
                    row = {"id": "shared", "package": package, "file": file, "platforms": ["windows"]}
                    with self.assertRaisesRegex(ValueError, "Windows-only module"):
                        partition([row])

    def test_macos_native_allowlist_accepts_only_complete_macos_functions(self):
        native = '#[cfg(target_os = "macos")]\nfn verify() { if true { check(); } }'
        for file in ("crates/basal-host/src/builtins/git.rs", "crates/basal-host/src/builtins/fs.rs"):
            with self.subTest(file=file):
                row = {"id": "mac-native", "package": "basal-host", "platforms": ["macos"],
                       "file": file, "old": native}
                self.assertEqual(partition([row]), {"linux": set(), "macos": {"mac-native"}, "windows": set()})
                for old in ('check();', '#[cfg(unix)]\nfn verify() { check(); }',
                            native + '\nfn shared() {}', native[:-1]):
                    row["old"] = old
                    with self.assertRaisesRegex(ValueError, "belong on Linux"):
                        partition([row])

    def test_macos_native_allowlist_rejects_other_files_and_shared_extra_edits(self):
        native = '#[cfg(target_os = "macos")]\nfn verify() { check(); }'
        for file in ("crates/basal-host/src/lib.rs", "crates/basal-host/src/builtins/net.rs"):
            row = {"id": "other", "package": "basal-host", "platforms": ["macos"], "file": file, "old": native}
            with self.assertRaisesRegex(ValueError, "belong on Linux"):
                partition([row])
        row = {"id": "multi", "package": "basal-host", "platforms": ["macos"],
               "file": "crates/basal-host/src/builtins/fs.rs", "old": native,
               "edits": [{"file": "crates/basal-host/src/builtins/git.rs", "old": "check();"}]}
        with self.assertRaisesRegex(ValueError, "belong on Linux"):
            partition([row])

    def test_literal_braces_cannot_hide_a_shared_function_after_a_macos_function(self):
        row = {"id": "braces", "package": "basal-host", "platforms": ["macos"],
               "file": "crates/basal-host/src/builtins/fs.rs",
               "old": '#[cfg(target_os = "macos")]\nfn verify() { let text = "{}"; }'}
        self.assertEqual(partition([row])["macos"], {"braces"})
        for body in ('fn verify() { let text = "{"; } fn shared() { let text = "}"; }',
                     'fn verify() { // {\n check(); } fn shared() { // }\n check(); }',
                     'fn verify() { let text = r#"{"#; }',
                     'fn verify() { /* { */ check(); }'):
            row["old"] = '#[cfg(target_os = "macos")]\n' + body
            with self.assertRaisesRegex(ValueError, "belong on Linux"):
                partition([row])

    def test_artifact_names_accept_windows_shards_but_not_pr_or_unknown_hosts(self):
        for name in ("mutations-linux-1", "mutations-macos-5", "mutations-windows-1", "mutations-windows-12"):
            self.assertIsNotNone(ARTIFACT_PATTERN.fullmatch(name), name)
        for name in ("mutations-pr-windows", "mutations-windows-0", "mutations-other-1", "mutations-windows-one"):
            self.assertIsNone(ARTIFACT_PATTERN.fullmatch(name), name)

    def test_partition_rejects_duplicate_ids(self):
        for duplicate in (self.rows[0], {"id": "portable", "platforms": ["macos"]}):
            with self.subTest(duplicate=duplicate):
                with self.assertRaisesRegex(ValueError, "duplicate catalogue ID"):
                    partition(self.rows + [duplicate])

    def test_reports_count_replayed_rows_not_platform_skips(self):
        self.assertEqual(verify_reports(partition(self.rows), self.reports),
                          {"linux": 1, "macos": 1, "windows": 1})

    def test_reports_reject_missing_shard(self):
        with self.assertRaisesRegex(ValueError, "every catalogue ID exactly once"):
            verify_reports(partition(self.rows), self.reports[:1])

    def test_reports_reject_duplicate_replays(self):
        self.reports[0][1].append({"id": "portable", "outcome": "CAUGHT"})
        with self.assertRaisesRegex(ValueError, "every catalogue ID exactly once"):
            verify_reports(partition(self.rows), self.reports)

    def test_reports_reject_wrong_platform_replay(self):
        self.reports[0][1][1]["outcome"] = "CAUGHT"
        with self.assertRaisesRegex(ValueError, "wrong platform"):
            verify_reports(partition(self.rows), self.reports)

    def test_reports_reject_unexpected_platform_skip(self):
        self.reports[0][1][0]["outcome"] = "SKIPPED_PLATFORM"
        with self.assertRaisesRegex(ValueError, "unexpectedly skipped"):
            verify_reports(partition(self.rows), self.reports)

    def test_reports_reject_unsuccessful_replay(self):
        self.reports[1][1][1]["outcome"] = "SURVIVED"
        with self.assertRaisesRegex(ValueError, "unsuccessful replay"):
            verify_reports(partition(self.rows), self.reports)

    def sources(self):
        return {"portable": mutation_catalogue.Source("mutations.d/host.toml", 3),
                "worker": mutation_catalogue.Source("mutations.d/worker.toml", 12),
                "native": mutation_catalogue.Source("mutations.d/host-builtins.toml", 7)}

    def test_partition_errors_name_the_file_defining_the_row(self):
        self.rows[1]["platforms"] = ["linux", "macos"]
        with self.assertRaisesRegex(ValueError, r"^worker \(mutations.d/worker.toml:12\): expected exactly one"):
            partition(self.rows, self.sources())
        self.rows[1]["platforms"] = ["macos"]
        self.rows[0]["platforms"] = ["macos"]
        with self.assertRaisesRegex(ValueError, r"^portable \(mutations.d/host.toml:3\): basal-host proofs"):
            partition(self.rows, self.sources())

    def test_report_errors_name_the_catalogue_file(self):
        self.reports[1][1][1]["outcome"] = "SURVIVED"
        with self.assertRaisesRegex(ValueError, r"^worker \(mutations.d/worker.toml:12\): unsuccessful replay"):
            verify_reports(partition(self.rows), self.reports, self.sources())

    def test_report_errors_fall_back_to_the_reports_own_source(self):
        # A row the catalogue no longer defines can only be located by the
        # source the runner wrote into its report.
        self.reports[0][1].append({"id": "stale", "outcome": "CAUGHT",
                                   "source": {"file": "mutations.d/old.toml", "line": 9}})
        with self.assertRaisesRegex(ValueError, r"^stale \(mutations.d/old.toml:9\): replayed on the wrong platform"):
            verify_reports(partition(self.rows), self.reports, self.sources())

    def test_a_missing_report_row_is_named_with_its_file(self):
        del self.reports[2][1][0]
        with self.assertRaisesRegex(ValueError, r"windows: reports must contain every catalogue ID exactly once; "
                                                r"missing: portable \(mutations.d/host.toml:3\)"):
            verify_reports(partition(self.rows), self.reports, self.sources())

    def test_the_command_reads_a_catalogue_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            fragments = Path(temporary) / "mutations.d"
            fragments.mkdir()
            (fragments / "host.toml").write_text('[[control]]\nid = "portable"\npackage = "basal-host"\n'
                                                 'platforms = ["linux"]\n')
            (fragments / "worker.toml").write_text('[[control]]\nid = "worker"\npackage = "basal-worker"\n'
                                                   'platforms = ["macos"]\n')
            result = subprocess.run([sys.executable, str(SCRIPT), "--catalogue", str(fragments)],
                                    text=True, capture_output=True, check=True)
            self.assertIn("linux=1, macos=1, windows=0, total=2", result.stdout)
            (fragments / "copy.toml").write_text('[[control]]\nid = "worker"\nplatforms = ["linux"]\n')
            result = subprocess.run([sys.executable, str(SCRIPT), "--catalogue", str(fragments)],
                                    text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("duplicate catalogue ID worker: defined at", result.stderr)
            self.assertIn("copy.toml:2", result.stderr)
            self.assertIn("worker.toml:2", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
