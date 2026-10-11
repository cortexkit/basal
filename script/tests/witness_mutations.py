"""Mutation witnesses must observe the indexed mutant and a named test failure."""
import importlib.util
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
                         "--lib", "--features", "deviations", "--", "tests::guard", "--exact", "--nocapture"])

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


if __name__ == "__main__":
    unittest.main(verbosity=2)
