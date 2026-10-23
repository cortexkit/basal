"""Mutation witnesses must observe the indexed mutant and a named test failure."""
import contextlib
import importlib.util
import io
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

PATH = Path(__file__).resolve().parents[1] / "witness-mutations.py"
SPEC = importlib.util.spec_from_file_location("witness_mutations", PATH)
assert SPEC is not None and SPEC.loader is not None
WITNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WITNESS)


class MutationWitnessChecks(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        subprocess.run(["git", "init", "--quiet", str(self.root)], check=True)
        self.source = self.root / "guard.rs"
        self.source.write_text("guard();\n")
        self.output = self.root / "proofs"
        self.output.mkdir()
        self.row = {"id": "guard", "runner": "cargo", "package": "fixture", "target": "--lib",
                    "file": "guard.rs", "old": "guard();", "new": "// NON-VACUITY BREAK",
                    "expect_red": ["tests::guard"], "features": ["deviations"]}
        self.real_run = subprocess.run
        self.root_patch = mock.patch.object(WITNESS, "ROOT", self.root)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)

    def cargo(self, args, **kwargs):
        if args[0] != "cargo":
            return self.real_run(args, **kwargs)
        if "NON-VACUITY BREAK" in self.source.read_text():
            return subprocess.CompletedProcess(args, 101, "test tests::guard ... FAILED\n0 passed; 1 failed; 9 filtered out\n")
        return subprocess.CompletedProcess(args, 0, "test tests::guard ... ok\n1 passed; 0 failed\n")

    def test_feature_flags_apply_to_the_named_cargo_oracle(self):
        self.assertEqual(WITNESS.command(self.row), ["cargo", "test", "--locked", "-p", "fixture",
                         "--lib", "--features", "deviations", "--", "tests::guard", "--exact", "--show-output"])

    def cargo_with_sibling(self, args, **kwargs):
        result = self.cargo(args, **kwargs)
        if args[0] != "cargo":
            return result
        text = "test tests::sibling ... ok\n" + result.stdout
        text = text.replace("0 passed; 1 failed", "1 passed; 1 failed")
        text = text.replace("1 passed; 0 failed", "2 passed; 0 failed")
        return subprocess.CompletedProcess(args, result.returncode, text)

    def test_explicit_run_set_executes_and_records_the_passing_sibling(self):
        self.row["select"] = {"tests": ["tests::guard", "tests::sibling"]}
        self.row["only"] = True
        console = io.StringIO()
        def run(args, **kwargs):
            if args[0] == "cargo":
                self.assertEqual(args[-4:], ["tests::guard", "tests::sibling", "--exact", "--show-output"])
                if "NON-VACUITY BREAK" in self.source.read_text():
                    self.assertIn("+// NON-VACUITY BREAK", console.getvalue())
                    self.assertIn("1 file changed", console.getvalue())
            return self.cargo_with_sibling(args, **kwargs)
        with contextlib.redirect_stdout(console), mock.patch.object(WITNESS.subprocess, "run", side_effect=run):
            proof = WITNESS.witness(self.row, self.output)
        self.assertEqual(proof["outcome"], "reddened")
        self.assertIn("tests::guard: FAILED", proof["captured_output"])
        self.assertIn("tests::sibling: ok", proof["captured_output"])
        self.assertIn("test tests::sibling ... ok", (self.output / "guard-baseline.log").read_text())
        self.assertIn("+// NON-VACUITY BREAK", (self.output / "guard-mutant.log").read_text())
        self.assertIn("+// NON-VACUITY BREAK", (self.output / "guard-applied.diff").read_text())
        self.assertIn("after: (empty)", (self.output / "guard-stats.log").read_text())
        self.assertEqual(self.source.read_text(), "guard();\n")

    def test_filter_run_set_runs_without_exact_and_records_its_sibling(self):
        self.row["select"] = {"filter": "tests::"}
        self.assertEqual(WITNESS.command(self.row)[-3:], ["--", "tests::", "--show-output"])
        with mock.patch.object(WITNESS.subprocess, "run", side_effect=self.cargo_with_sibling):
            proof = WITNESS.witness(self.row, self.output)
        self.assertEqual(proof["outcome"], "reddened")
        self.assertIn("tests::sibling: ok", proof["captured_output"])

    def test_captured_test_output_is_not_a_libtest_status(self):
        for heading in ["successes", "failures"]:
            with self.subTest(heading=heading):
                text = (f"test tests::guard ... ok\n\n{heading}:\n"
                        "test tests::guard ... FAILED\ntest tests::sibling ... ok\n")
                self.assertEqual(WITNESS.test_results(text), {"tests::guard": "ok"})

    def test_a_missing_declared_sibling_is_refused_in_the_baseline(self):
        self.row["select"] = {"tests": ["tests::guard", "tests::sibling"]}
        with mock.patch.object(WITNESS.subprocess, "run", side_effect=self.cargo):
            with self.assertRaisesRegex(SystemExit, "named baseline did not execute"):
                WITNESS.witness(self.row, self.output)
        self.assertEqual(self.source.read_text(), "guard();\n")

    def test_a_missing_sibling_in_the_mutant_is_not_a_complete_catch(self):
        self.row["select"] = {"tests": ["tests::guard", "tests::sibling"]}
        def run(args, **kwargs):
            if args[0] == "cargo" and "NON-VACUITY BREAK" in self.source.read_text():
                return self.cargo(args, **kwargs)
            return self.cargo_with_sibling(args, **kwargs)
        with mock.patch.object(WITNESS.subprocess, "run", side_effect=run):
            proof = WITNESS.witness(self.row, self.output)
        self.assertEqual(proof["outcome"], "undefended")
        self.assertEqual(self.source.read_text(), "guard();\n")

    def test_a_failing_sibling_is_not_a_named_mutation_catch(self):
        self.row["select"] = {"tests": ["tests::guard", "tests::sibling"]}
        self.row["only"] = True
        def run(args, **kwargs):
            result = self.cargo_with_sibling(args, **kwargs)
            if args[0] == "cargo" and "NON-VACUITY BREAK" in self.source.read_text():
                return subprocess.CompletedProcess(args, 101, result.stdout.replace(
                    "test tests::sibling ... ok", "test tests::sibling ... FAILED"))
            return result
        with mock.patch.object(WITNESS.subprocess, "run", side_effect=run):
            proof = WITNESS.witness(self.row, self.output)
        self.assertEqual(proof["outcome"], "undefended")
        self.assertEqual(self.source.read_text(), "guard();\n")

    def test_witness_stages_live_source_and_restores_it_after_a_named_failure(self):
        self.source.write_text("guard();\n// live implementation\n")
        with mock.patch.object(WITNESS.subprocess, "run", side_effect=self.cargo):
            proof = WITNESS.witness(self.row, self.output)
        self.assertEqual(proof["outcome"], "reddened")
        self.assertIn("guard.rs", proof["applied_evidence"])
        self.assertIn("after: (empty)", proof["applied_evidence"])
        self.assertEqual(self.source.read_text(), "guard();\n// live implementation\n")
        self.assertEqual(WITNESS.git("diff", "--stat"), "")
        self.assertIn("1 file changed", proof["applied_evidence"])

    def test_a_build_failure_is_not_a_named_mutation_catch(self):
        def fail_build(args, **kwargs):
            if args[0] == "cargo" and "NON-VACUITY BREAK" in self.source.read_text():
                return subprocess.CompletedProcess(args, 101, "error: could not compile fixture\n")
            return self.cargo(args, **kwargs)
        with mock.patch.object(WITNESS.subprocess, "run", side_effect=fail_build):
            proof = WITNESS.witness(self.row, self.output)
        self.assertEqual(proof["outcome"], "not_reached")
        self.assertEqual(self.source.read_text(), "guard();\n")
        self.assertEqual(WITNESS.git("diff", "--stat"), "")

    def test_a_zero_test_baseline_is_refused(self):
        def absent(args, **kwargs):
            if args[0] == "cargo":
                return subprocess.CompletedProcess(args, 0, "0 passed; 0 failed; 10 filtered out\n")
            return self.real_run(args, **kwargs)
        with mock.patch.object(WITNESS.subprocess, "run", side_effect=absent):
            with self.assertRaisesRegex(SystemExit, "named baseline did not execute"):
                WITNESS.witness(self.row, self.output)
        self.assertEqual(self.source.read_text(), "guard();\n")

    def test_a_nonunique_anchor_is_refused_before_mutating(self):
        self.source.write_text("guard();\nguard();\n")
        with self.assertRaisesRegex(SystemExit, "exactly one anchor"):
            WITNESS.witness(self.row, self.output)
        self.assertEqual(self.source.read_text(), "guard();\nguard();\n")


    def write_catalogue(self):
        fragments = self.root / "mutations.d"
        fragments.mkdir()
        (fragments / "host.toml").write_text(
            '# host rows\n\n[[control]]\nid = "guard"\nplatforms = ["windows"]\n'
            '\n[[control]]\nid = "linux-only"\nplatforms = ["linux"]\n')
        (fragments / "worker.toml").write_text('[[control]]\nid = "other"\nplatforms = ["windows"]\n')

    def run_main(self, *args):
        with mock.patch.object(WITNESS.sys, "argv", ["witness-mutations.py", *args]):
            WITNESS.main()

    def test_rows_are_read_from_every_catalogue_fragment(self):
        self.write_catalogue()
        seen = []
        def record(row, output):
            seen.append(row["id"])
            return {"outcome": "reddened"}
        with mock.patch.object(WITNESS, "witness", side_effect=record):
            self.run_main("--platform", "windows", "--output", str(self.output))
        self.assertEqual(seen, ["guard", "other"])

    def test_a_failing_row_names_the_fragment_and_line_defining_it(self):
        self.write_catalogue()
        with mock.patch.object(WITNESS, "witness", side_effect=SystemExit("guard: expected exactly one anchor")):
            with self.assertRaisesRegex(SystemExit, r"^guard: expected exactly one anchor "
                                                    r"\(defined at mutations.d/host.toml:4\)$"):
                self.run_main("--platform", "windows", "--only", "guard", "--output", str(self.output))

    def test_an_unreddened_row_names_its_fragment(self):
        self.write_catalogue()
        with mock.patch.object(WITNESS, "witness", return_value={"outcome": "undefended"}):
            with self.assertRaisesRegex(SystemExit, r"other: exact named oracle did not redden "
                                                    r"\(defined at mutations.d/worker.toml:2\)"):
                self.run_main("--platform", "windows", "--only", "other", "--output", str(self.output))

    def test_an_id_on_another_platform_is_an_unknown_request(self):
        self.write_catalogue()
        with self.assertRaisesRegex(SystemExit, "unknown control requested"):
            self.run_main("--platform", "windows", "--only", "linux-only", "--output", str(self.output))


if __name__ == "__main__":
    unittest.main(verbosity=2)
