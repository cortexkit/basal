"""Tests for check_mutation_coverage.py: every catalogue row is assigned to
 exactly one CI host, with portable packages on Linux or Windows-only modules on Windows,
and the shards' replay reports contain one successful replay of every row."""

import unittest

from check_mutation_coverage import ARTIFACT_PATTERN, PLATFORMS, partition, verify_reports


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


if __name__ == "__main__":
    unittest.main(verbosity=2)
