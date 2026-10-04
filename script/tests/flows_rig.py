"""Offline checks of the rig's generated policy and explicit vault boundary."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "flows-rig.sh"
HELPERS = SCRIPT.read_text().split("# ---------------------------------------------------------------- main")[0]


class RigChecks(unittest.TestCase):
    def run_shell(self, code, home):
        return subprocess.run(
            ["/bin/sh", "-c", HELPERS + "\n" + code, str(SCRIPT)],
            env={**os.environ, "HOME": str(home)},
            text=True,
            capture_output=True,
            check=False,
        )

    def test_pin_uses_served_providers_and_refuses_missing_luna(self):
        with tempfile.TemporaryDirectory() as home:
            catalog = Path(home) / "catalog.json"
            catalog.write_text(json.dumps({"models": {
                "new-provider/some-model": {}, "openai/gpt-6-luna": {},
                "openai/other": {}, "another/model/with/slashes": {},
            }}))
            code = 'guard_all_paths; rig_env() { /bin/cat "$HOME/catalog.json"; }; pin_routing'
            result = self.run_shell(code, home)
            self.assertEqual(result.returncode, 0, result.stderr)
            policy = Path(home) / ".local/share/cortexkit/ckdev-flows/config/cortexkit/alfonso-routing.jsonc"
            self.assertEqual(json.loads(policy.read_text()), {"model_routing": {"exclude": [
                "another", "new-provider", "openai", "-openai/gpt-6-luna",
            ]}})
            policy.unlink()
            catalog.write_text('{"models":{"openai/other":{}}}')
            result = self.run_shell(code, home)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("lacks openai/gpt-6-luna", result.stderr)
            self.assertFalse(policy.exists())

    def test_credential_inventory_requires_one_active_openai_binding(self):
        with tempfile.TemporaryDirectory() as home:
            inventory = Path(home) / "inventory"
            code = 'rig_auth() { /bin/cat "$HOME/inventory"; }; require_model_credential'
            header = "STATE          VER   CREDENTIAL  CATEGORIES  PROVIDERS\n"
            valid = "active v1 apikey:openai llm-provider openai\n"
            for rows, expected in [(valid, 0), ("", 1), (valid + valid, 1),
                                   (valid.replace("openai\n", "-\n"), 1),
                                   (valid.replace("active", "retired"), 1)]:
                inventory.write_text(header + rows + "\n")
                result = self.run_shell(code, home)
                self.assertEqual(result.returncode, expected, result.stderr)

    def test_auth_dry_runs_name_all_three_rig_paths_without_reading_payload(self):
        with tempfile.TemporaryDirectory() as home:
            code = 'guard_all_paths; DRY=1; rig_auth status; cmd_credential --key-file "$RIG_HOME/.secrets/key with spaces"'
            result = self.run_shell(code, home)
            self.assertEqual(result.returncode, 0, result.stderr)
            for line in result.stdout.splitlines():
                if " auth " in line:
                    self.assertIn(f"--data-dir {home}/.local/share/cortexkit/ckdev-flows/data/cortexkit/claustrum", line)
                    self.assertIn(f"--key-path {home}/.local/share/cortexkit/ckdev-flows/config/claustrum/master.key", line)
                    self.assertIn(f"--subc {home}/.local/share/cortexkit/ckdev-flows/runtime/subc-connection.json", line)
            self.assertIn("--provider-id openai", result.stdout)
            self.assertIn("--payload-file", result.stdout)
            self.assertIn("key with spaces", result.stdout)

    def test_auth_refuses_a_final_key_symlink_outside_the_rig(self):
        with tempfile.TemporaryDirectory() as home:
            key = Path(home) / ".local/share/cortexkit/ckdev-flows/config/claustrum/master.key"
            key.parent.mkdir(parents=True)
            key.symlink_to(Path(home) / "outside-master.key")
            result = self.run_shell("guard_all_paths; DRY=1; rig_auth status", home)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("auth path resolves outside the rig", result.stderr)
            self.assertNotIn(" auth status ", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
