#!/usr/bin/env python3
"""Check the CI matrix against the pinned runner's whole-catalogue sharding."""

from collections import Counter
from pathlib import Path
import json
import subprocess
import sys
import unittest

import check_mutation_coverage as coverage
from mutation_matrix import matrix_for


ROOT = Path(__file__).resolve().parents[2]


def simulated_reports(matrix, rows):
    # ckdev-mutate 0.12.0 sorts all IDs before platform filtering. Execution
    # order groups packages later, without changing these assigned slots.
    ordered = sorted(rows, key=lambda row: row["id"])
    return [
        (job["platform"], [
            {"id": row["id"], "outcome": "CAUGHT" if row["platforms"] == [job["platform"]]
             else "SKIPPED_PLATFORM"}
            for index, row in enumerate(ordered)
            if index % job["shards"] == job["shard"] - 1
        ])
        for job in matrix
    ]


class MutationMatrixChecks(unittest.TestCase):
    def test_push_and_manual_counts_are_unchanged(self):
        for event in ("push", "workflow_dispatch"):
            with self.subTest(event=event):
                self.assertEqual(Counter(job["platform"] for job in matrix_for(event)),
                                 {"linux": 2, "macos": 5, "windows": 2})

    def test_schedule_uses_smaller_shards(self):
        self.assertEqual(Counter(job["platform"] for job in matrix_for("schedule")),
                         {"linux": 16, "macos": 12, "windows": 2})

    def check_assignment(self, event):
        rows, sources = coverage.mutation_catalogue.load(ROOT)
        selected = coverage.partition(rows, sources)
        actual = coverage.verify_reports(selected, simulated_reports(matrix_for(event), rows), sources)
        self.assertEqual(actual, {platform: len(ids) for platform, ids in selected.items()})

    def test_nightly_matrix_covers_every_row_once(self):
        self.check_assignment("schedule")

    def test_push_matrix_covers_every_row_once(self):
        self.check_assignment("push")

    def test_command_emits_the_matrix_used_by_workflow(self):
        script = Path(__file__).with_name("mutation_matrix.py")
        for event in ("schedule", "push", "workflow_dispatch"):
            with self.subTest(event=event):
                output = subprocess.check_output([sys.executable, str(script), "--event", event], text=True)
                self.assertTrue(output.startswith("include="))
                self.assertEqual(json.loads(output.removeprefix("include=")), matrix_for(event))

    def test_workflow_consumes_event_specific_output(self):
        workflow = Path(__file__).with_name("ci.yml").read_text()
        self.assertIn('mutation_matrix.py --event "$EVENT_NAME" >> "$GITHUB_OUTPUT"', workflow)
        self.assertIn("EVENT_NAME: ${{ github.event_name }}", workflow)
        self.assertIn("include: ${{ fromJSON(needs.mutation-matrix.outputs.include) }}", workflow)
        self.assertIn("--shard ${{ matrix.shard }}/${{ matrix.shards }}", workflow)


if __name__ == "__main__":
    unittest.main()
