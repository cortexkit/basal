"""Exercise the catalogue partition and real-report coverage fences."""

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
        for platforms in (["windows"], ["linux", "macos"], None):
            with self.subTest(platforms=platforms):
                row = {"id": "unassigned", "runner": "command"}
                if platforms is not None:
                    row["platforms"] = platforms
                with self.assertRaisesRegex(ValueError, "exactly one CI platform"):
                    partition([row])

    def test_partition_keeps_worker_packages_on_macos(self):
        self.rows[1]["platforms"] = ["linux"]
        with self.assertRaisesRegex(ValueError, "require macOS"):
            partition(self.rows)

    def test_partition_keeps_portable_packages_on_linux(self):
        self.rows[0]["platforms"] = ["macos"]
        with self.assertRaisesRegex(ValueError, "belong on Linux"):
            partition(self.rows)

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
