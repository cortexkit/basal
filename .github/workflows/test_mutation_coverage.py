"""Tests for check_mutation_coverage.py: every catalogue row is assigned to
exactly one CI host (Linux or macOS), with portable packages kept on Linux,
and the shards' replay reports contain one successful replay of every row."""

import unittest

from check_mutation_coverage import partition, verify_reports


class MutationCoverageChecks(unittest.TestCase):
    def setUp(self):
        self.rows = [
            {"id": "portable", "package": "basal-host", "platforms": ["linux"]},
            {"id": "worker", "package": "basal-worker", "platforms": ["macos"]},
        ]
        self.reports = [
            ("linux", [{"id": "portable", "outcome": "CAUGHT"},
                       {"id": "worker", "outcome": "SKIPPED_PLATFORM"}]),
            ("macos", [{"id": "portable", "outcome": "SKIPPED_PLATFORM"},
                       {"id": "worker", "outcome": "CAUGHT"}]),
        ]

    def test_partition_accepts_one_host_per_row(self):
        self.assertEqual(partition(self.rows), {"linux": {"portable"}, "macos": {"worker"}})

    def test_partition_rejects_rows_selected_by_no_job_or_both(self):
        for package in (None, "basal-worker", "basal-module", "basal-testkit", "basal-rig"):
            for platforms in ([], ["windows"], ["linux", "macos"], None):
                with self.subTest(package=package, platforms=platforms):
                    row = {"id": "unassigned", "runner": "command", "package": package}
                    if platforms is not None:
                        row["platforms"] = platforms
                    with self.assertRaisesRegex(ValueError, "exactly one CI platform"):
                        partition([row])

    def test_partition_accepts_worker_packages_on_either_host(self):
        for package in ("basal-worker", "basal-module", "basal-testkit", "basal-rig"):
            for host in ("linux", "macos"):
                with self.subTest(package=package, host=host):
                    row = {"id": "worker", "package": package, "platforms": [host]}
                    expected = {"linux": set(), "macos": set()}
                    expected[host] = {"worker"}
                    self.assertEqual(partition([row]), expected)

    def test_partition_keeps_portable_packages_on_linux(self):
        for package in ("basal-proto", "basal-core", "basal-host"):
            with self.subTest(package=package):
                row = {"id": "portable", "package": package, "platforms": ["linux"]}
                self.assertEqual(partition([row]), {"linux": {"portable"}, "macos": set()})
                row["platforms"] = ["macos"]
                with self.assertRaisesRegex(ValueError, "belong on Linux"):
                    partition([row])

    def test_partition_rejects_duplicate_ids(self):
        for duplicate in (self.rows[0], {"id": "portable", "platforms": ["macos"]}):
            with self.subTest(duplicate=duplicate):
                with self.assertRaisesRegex(ValueError, "duplicate catalogue ID"):
                    partition(self.rows + [duplicate])

    def test_reports_count_replayed_rows_not_platform_skips(self):
        self.assertEqual(verify_reports(partition(self.rows), self.reports),
                         {"linux": 1, "macos": 1})

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
