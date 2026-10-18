"""Keep basal-core's catalog digests independent of transitive JSON features."""

from pathlib import Path
import tomllib
import unittest


REPOSITORY = Path(__file__).resolve().parents[2]


class CanonicalJsonFeatures(unittest.TestCase):
    def core_json_dependency(self):
        with (REPOSITORY / "crates/basal-core/Cargo.toml").open("rb") as manifest:
            return tomllib.load(manifest)["dependencies"]["serde_json"]

    def test_core_requests_correctly_rounded_json_parsing(self):
        # A transitive dependency can enable this feature in Cargo's unified
        # build. Inspect the owning manifest so removing its request still fails
        # even while the nearest-double and RFC 8785 runtime vectors stay green.
        dependency = self.core_json_dependency()
        self.assertIn("float_roundtrip", dependency.get("features", []))

    def test_core_uses_the_workspace_json_dependency(self):
        self.assertIs(self.core_json_dependency()["workspace"], True)
