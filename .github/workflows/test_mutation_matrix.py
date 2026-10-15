#!/usr/bin/env python3
"""Check the CI matrix against the pinned runner's whole-catalogue sharding."""

from collections import Counter
from pathlib import Path
import json
import os
import re
import subprocess
import sys
import textwrap
import unittest
from unittest.mock import patch

import check_mutation_coverage as coverage
from mutation_matrix import matrix_for


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = Path(__file__).with_name("ci.yml")
NATS_ACTION = ROOT / ".github" / "actions" / "install-nats-server" / "action.yml"


def workflow_job(name):
    jobs = re.split(r"(?m)^  ([\w-]+):\n", WORKFLOW.read_text())
    return dict(zip(jobs[1::2], jobs[2::2]))[name]


def workflow_steps(name):
    return re.split(r"(?m)^      - ", workflow_job(name))[1:]


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
    def test_push_and_manual_use_eight_linux_shards(self):
        for event in ("push", "workflow_dispatch"):
            with self.subTest(event=event):
                self.assertEqual(Counter(job["platform"] for job in matrix_for(event)),
                                 {"linux": 8, "macos": 5, "windows": 2})

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

    def test_eight_push_shards_partition_the_linux_catalogue_exactly_once(self):
        rows, _ = coverage.mutation_catalogue.load(ROOT)
        reports = simulated_reports(matrix_for("push"), rows)
        linux = [report for platform, report in reports if platform == "linux"]
        self.assertEqual(len(linux), 8)
        expected = {row["id"] for row in rows if row["platforms"] == ["linux"]}
        actual = Counter(row["id"] for report in linux for row in report if row["outcome"] == "CAUGHT")
        self.assertEqual(actual, Counter({row_id: 1 for row_id in expected}))

    def test_command_emits_the_matrix_used_by_workflow(self):
        script = Path(__file__).with_name("mutation_matrix.py")
        for event in ("schedule", "push", "workflow_dispatch"):
            with self.subTest(event=event):
                output = subprocess.check_output([sys.executable, str(script), "--event", event], text=True)
                self.assertTrue(output.startswith("include="))
                self.assertEqual(json.loads(output.removeprefix("include=")), matrix_for(event))

    def test_workflow_consumes_event_specific_output(self):
        workflow = WORKFLOW.read_text()
        self.assertIn('mutation_matrix.py --event "$EVENT_NAME" >> "$GITHUB_OUTPUT"', workflow)
        self.assertIn("EVENT_NAME: ${{ github.event_name }}", workflow)
        self.assertIn("include: ${{ fromJSON(needs.mutation-matrix.outputs.include) }}", workflow)
        self.assertIn("--shard ${{ matrix.shard }}/${{ matrix.shards }}", workflow)


class WorkflowSchedulingChecks(unittest.TestCase):
    def test_every_nats_install_uses_a_checksum_keyed_cache(self):
        workflow = WORKFLOW.read_text()
        self.assertIn("uses: ./.github/actions/install-nats-server", workflow_job("checks"))
        for job in ("checks", "mutations-pr", "mutations-main", "windows-baseline"):
            with self.subTest(job=job):
                install = [step for step in workflow_steps(job) if "install-nats" in step]
                self.assertEqual(len(install), 1)
                self.assertTrue("*install-nats" in install[0] or
                                "uses: ./.github/actions/install-nats-server" in install[0])

        action = NATS_ACTION.read_text()
        action_steps = re.split(r"(?m)^    - ", action)[1:]
        restore_index = next(index for index, step in enumerate(action_steps)
                             if "actions/cache/restore@" in step)
        restore = action_steps[restore_index]
        self.assertIn(
            "key: nats-server-v2.15.0-${{ steps.archive.outputs.platform }}-${{ steps.archive.outputs.sha256 }}",
            restore,
        )
        checksum_indexes = [
            index for index, step in enumerate(action_steps)
            if "sha256sum --check -" in step or "Get-FileHash" in step
        ]
        self.assertEqual(len(checksum_indexes), 2)
        self.assertTrue(all(index > restore_index for index in checksum_indexes))

    def test_cache_saves_only_on_main_push(self):
        workflow = WORKFLOW.read_text()
        self.assertNotIn("actions/cache@", workflow)
        saves = []
        for job in ("checks", "mutations-pr", "mutations-main", "minimum-rust", "windows-baseline"):
            for step in workflow_steps(job):
                if "actions/cache/save@" in step:
                    saves.append(step)
                    self.assertIn("if: github.event_name == 'push' && github.ref == 'refs/heads/main'", step)
                    self.assertNotIn("github.sha", step)
        self.assertEqual(len(saves), 6)

        action_steps = re.split(r"(?m)^    - ", NATS_ACTION.read_text())[1:]
        nats_saves = [step for step in action_steps if "actions/cache/save@" in step]
        self.assertEqual(len(nats_saves), 1)
        self.assertIn("if: steps.nats-cache.outputs.cache-hit != 'true' && github.event_name == 'push' && github.ref == 'refs/heads/main'", nats_saves[0])
        self.assertNotIn("github.sha", nats_saves[0])

    def test_main_build_cache_namespace_and_run_handoff(self):
        for job, namespace in (("checks", "checks"), ("minimum-rust", "minimum")):
            steps = workflow_steps(job)
            restore = next(step for step in steps if "Restore Cargo build cache" in step)
            save = next(step for step in steps if "Save Cargo build cache" in step)
            key = f"-{namespace}-main-${{{{ github.run_id }}}}"
            self.assertIn(key, restore)
            self.assertIn(key, save)
            self.assertIn(f"-{namespace}-main-\n", restore)
        for job in ("mutations-pr", "mutations-main"):
            restore = next(step for step in workflow_steps(job) if "actions/cache/restore@" in step)
            self.assertIn("-checks-main-${{ github.run_id }}", restore)
            self.assertIn("-checks-main-\n", restore)
            self.assertIn("-windows-baseline-windows-2022-main-\n", restore)
            self.assertIn("-windows-baseline-windows-2022-\n", restore)

    def test_mutation_runner_cache_ignores_workspace_lockfile(self):
        for job in ("checks", "mutations-pr", "mutations-main", "windows-baseline"):
            cache = next(step for step in workflow_steps(job) if "id: mutate-cache" in step)
            self.assertIn("${{ runner.os }}-${{ runner.arch }}-ckdev-mutate-${{ steps.rust.outputs.version }}-${{ env.CK_MUTATE_REV }}", cache)
            self.assertNotIn("hashFiles", cache)

    def test_build_free_gates_run_once_in_lint(self):
        lint = workflow_job("lint")
        checks = workflow_job("checks")
        self.assertNotIn("actions/cache", lint)
        for command in ("cargo fmt --all --check", "shellcheck\" -x script/*.sh",
                        "/bin/sh -n script/stage.sh", "script.tests.flows_rig",
                        "script/check-path-deps.py", "script.tests.check_path_deps",
                        "script/check-ckdev-names.py", "script.tests.check_ckdev_names",
                        "script/check-local-paths.py", "script.tests.check_local_paths",
                        "script.tests.stage_card", "check_mutation_coverage.py",
                        "unittest discover -s .github/workflows", "script.tests.mutation_catalogue",
                        "script.tests.witness_mutations"):
            with self.subTest(command=command):
                self.assertEqual(lint.count(command), 1)
                self.assertNotIn(command, checks)
        self.assertIn("run: ckdev-mutate check", checks)
        self.assertIn("check_mutation_coverage.py --reports", workflow_job("mutation-coverage"))
        self.assertIn("needs: [checks, lint]", workflow_job("mutations-pr"))
        self.assertIn("needs: [checks, lint]", workflow_job("mutation-matrix"))

    def test_hook_tests_skip_arm_but_hook_clippy_does_not(self):
        steps = workflow_steps("checks")
        portable = next(step for step in steps if "Test portable crates with the rig kill hook" in step)
        worker = next(step for step in steps if "Test confined worker packages with the rig kill hook" in step)
        clippy = next(step for step in steps if "cargo clippy" in step and "rig-kill-hook" in step)
        self.assertIn("if: matrix.platform == 'linux'", portable)
        self.assertIn("if: matrix.platform != 'linux-arm64'", worker)
        self.assertNotIn("if:", clippy)

    def test_windows_split_preserves_package_and_feature_test_commands(self):
        job = workflow_job("windows-baseline")
        self.assertIn("os: [windows-latest, windows-2022]", job)
        self.assertIn("suite: [launch-worker-host, module-core-testkit-rig]", job)
        expected = {
            "Test the Windows launcher with its test variants": ("launch-worker-host", [
                "cargo test -p basal-launch --locked -- --show-output",
                "cargo test -p basal-launch --features deviations --locked -- --show-output"]),
            "Test the worker on its own": ("launch-worker-host", [
                "cargo test -p basal-worker --locked --no-fail-fast -- --show-output"]),
            "Test the worker with its test variants": ("launch-worker-host", [
                "cargo test -p basal-worker --features deviations --locked --no-fail-fast -- --show-output"]),
            "Test the module and the core": ("module-core-testkit-rig", [
                "cargo test -p basal-module -p basal-core --locked --no-fail-fast -- --show-output"]),
            "Test the shared test kit on its own": ("module-core-testkit-rig", [
                "cargo test -p basal-testkit --locked --no-fail-fast"]),
            "Test the integration rig on its own": ("module-core-testkit-rig", [
                "cargo test -p basal-rig --locked --no-fail-fast"]),
            "Test the workspace": ("launch-worker-host", [
                "cargo test --workspace --exclude basal-launch --exclude basal-worker --exclude basal-module --exclude basal-core --exclude basal-testkit --exclude basal-rig --locked"]),
        }
        steps = workflow_steps("windows-baseline")
        for name, (suite, commands) in expected.items():
            step = next(step for step in steps if step.startswith(f"name: {name}\n"))
            self.assertIn(f"if: matrix.suite == '{suite}'", step)
            actual = re.findall(r"(cargo test .+?) 2>&1", step)
            self.assertEqual(actual, commands)
        self.assertNotIn("cargo test -p basal-host", job)
        self.assertNotIn("--exclude basal-host", job)
        self.assertIn("Witness the startup-handle and import-inventory mutants", job)
        self.assertIn("cargo check --workspace --all-targets --locked", job)

    def run_pr_witness_selection(self, report):
        step = next(step for step in workflow_steps("mutations-pr")
                    if step.startswith("name: Capture exact Windows mutation"))
        code = textwrap.dedent(step.split("python - <<'PY'\n", 1)[1].split("          PY\n", 1)[0])
        rows = [{"id": "windows-a", "platforms": ["windows"]},
                {"id": "windows-b", "platforms": ["windows"]},
                {"id": "linux-a", "platforms": ["linux"]}]
        with patch.object(coverage.mutation_catalogue, "load", return_value=(rows, {})), \
             patch.object(Path, "read_text", return_value=json.dumps(report)), \
             patch.dict(os.environ, {"RUNNER_TEMP": "/tmp/witness-selection"}), \
             patch("subprocess.run") as run, patch("builtins.print") as output:
            exec(code, {})
        return run, output

    def test_pr_windows_witnesses_only_diff_selected_windows_ids(self):
        run, output = self.run_pr_witness_selection([
            {"id": "linux-a", "outcome": "SKIPPED_PLATFORM"},
            {"id": "windows-b", "outcome": "CAUGHT"}])
        run.assert_called_once_with([
            sys.executable, "script/witness-mutations.py", "--platform", "windows",
            "--output", "/tmp/witness-selection/windows-mutation-evidence",
            "--only", "windows-b"], check=True)
        output.assert_not_called()

    def test_pr_windows_witnesses_skip_when_diff_selects_no_windows_rows(self):
        for report in ([], [{"id": "linux-a", "outcome": "SKIPPED_PLATFORM"}]):
            with self.subTest(report=report):
                run, output = self.run_pr_witness_selection(report)
                run.assert_not_called()
                output.assert_called_once_with(
                    "Skipping Windows witnesses: the PR diff selected no Windows catalogue rows.")

    def test_push_replay_and_windows_witnesses_still_cover_full_catalogue(self):
        job = workflow_job("mutations-main")
        self.assertIn("ckdev-mutate run --all --shard ${{ matrix.shard }}/${{ matrix.shards }}", job)
        self.assertIn("script/witness-mutations.py --platform windows --shard ${{ matrix.shard }}/${{ matrix.shards }}", job)
        self.assertNotIn("--only", job)
        self.assertNotIn("--diff", job)


if __name__ == "__main__":
    unittest.main()
