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
    def test_build_preserves_repository_paths_with_spaces(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp).resolve()
            (home / 'source with spaces').mkdir()
            result = self.run_shell('''guard_all_paths
DRY=1
repos() { printf 'basal\t%s/source with spaces\n' "$HOME"; }
git() { printf '%040d\n' 0; }
clone_at() { printf '%s\n' "$2" > "$HOME/cloned"; }
cargo_build() { :; }
cmd_build --prefrontal-rev HEAD
''', home)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual((home / 'cloned').read_text().strip(), str(home / 'source with spaces'))

    def test_dangling_symlink_cannot_redirect_a_rig_write(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp).resolve()
            root = home / '.local/share/cortexkit/ckdev-flows'
            root.mkdir(parents=True)
            (root / 'bin').symlink_to(home / '.local/share/cortexkit/bin')
            result = self.run_shell('guard_all_paths', home)
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn('outside', result.stderr)

    def test_bad_staged_copy_leaves_the_placed_binary_unchanged(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp).resolve()
            root = home / '.local/share/cortexkit/ckdev-flows'
            dest = root / 'bin/ckdev-basal'
            dest.parent.mkdir(parents=True)
            dest.write_bytes(b'previous verified binary')
            staged = home / 'staged'
            staged.write_bytes(b'bad binary')
            staged.with_suffix('.sha256').write_text('0' * 64 + '  staged\n')
            result = self.run_shell('''guard_all_paths
verify_hardened() { return 1; }
place_staged basal "$HOME/staged" "$BIN/ckdev-basal" ck-basal
''', home)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(dest.read_bytes(), b'previous verified binary')

    def test_config_failure_stops_its_temporary_daemon(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''guard_all_paths
guard_port_free() { :; }; daemon_pid() { :; }
write_file() { /bin/cat > /dev/null; }
rig_auth() { :; }
cmd_start() { touch "$HOME/running"; }
cmd_stop() { rm -f "$HOME/running"; }
configure_live() { die "deliberate fixture failure"; }
cmd_config
''', home)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((Path(home) / 'running').exists())

    def run_shell(self, code, home):
        return subprocess.run(
            ["/bin/sh", "-c", HELPERS + "\n" + code, str(SCRIPT)],
            env={**os.environ, "HOME": str(home)},
            text=True,
            capture_output=True,
            check=False,
        )

    def test_all_rig_files_use_development_names_with_basal_signing_identities(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''
guard_all_paths
binaries
DRY=1
place_one basal /built/ck-basal "$BIN/ckdev-basal"
place_one basal-worker /built/ck-basal-worker "$BIN/ckdev-basal-worker"
place_staged basal /stage/ck-basal "$BIN/ckdev-basal" ck-basal
place_staged basal-worker /stage/ck-basal-worker "$BIN/ckdev-basal-worker" ck-basal-worker
basal_identifier ck-basal
basal_identifier ck-basal-worker
''', home)
            self.assertEqual(result.returncode, 0, result.stderr)
            rows = [line.split("\t") for line in result.stdout.splitlines() if "\t" in line]
            self.assertTrue(rows)
            self.assertTrue(all(row[3].startswith("ckdev-") for row in rows), rows)
            placed = {row[0]: row[3] for row in rows}
            self.assertEqual(placed["basal"], "ckdev-basal")
            self.assertEqual(placed["basal-worker"], "ckdev-basal-worker")
            self.assertIn("verify_hardened", result.stdout)
            self.assertIn("ckdev-basal ck-basal", result.stdout)
            self.assertIn("ckdev-basal-worker ck-basal-worker", result.stdout)
            self.assertTrue(result.stdout.endswith("ck-basal\nck-basal-worker\n"))

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
            policy_text = policy.read_text()
            self.assertIn("// Rig fixture values, not measured model quality.", policy_text)
            value = json.loads("\n".join(line for line in policy_text.splitlines()
                                         if not line.lstrip().startswith("//")))
            self.assertEqual(value, {"model_routing": {
                "exclude": ["another", "new-provider", "openai", "-openai/gpt-6-luna"],
                "models": {"openai/gpt-6-luna": {"elo": 1, "eq": 0, "speed": 0}},
            }})
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
        with tempfile.TemporaryDirectory() as tmp:
            # Resolve the temp dir first: on macOS it sits under /var, a
            # symlink to /private/var, and an unresolved home would be refused
            # for that reason alone, so the test couldn't tell whether the
            # check follows the key file's own symlink.
            home = os.path.realpath(tmp)
            key = Path(home) / ".local/share/cortexkit/ckdev-flows/config/claustrum/master.key"
            key.parent.mkdir(parents=True)
            key.symlink_to(Path(home) / "outside-master.key")
            result = self.run_shell("guard_all_paths; DRY=1; rig_auth status", home)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("auth path resolves outside the rig", result.stderr)
            self.assertNotIn(" auth status ", result.stdout)

    def test_config_grants_only_after_the_temporary_daemon_starts(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''
guard_all_paths
guard_port_free() { :; }; daemon_pid() { :; }
write_file() { /bin/cat > /dev/null; }
ready=0
cmd_start() { ready=1; }
cmd_stop() { ready=0; }
rig_auth() { [ "$1" = bootstrap ] || [ "$ready" = 1 ] || return 1; if [ "$1" = grants ]; then printf 'no grants\\n'; else printf '%s\\n' "$*"; fi; }
pin_routing() { [ "$ready" = 1 ]; }
cmd_config
[ "$ready" = 0 ]
''', home)
            self.assertEqual(result.returncode, 0, result.stderr)
            grants = [line for line in result.stdout.splitlines() if line.startswith("grant ")]
            self.assertEqual(grants, [
                "grant --principal reserved:broca --selector-kind exact --selector apikey:openai --operation read",
                "grant --principal reserved:prefrontal-routing --selector-kind category --selector llm-provider --operation list",
            ])

    def test_grant_plan_is_exact_and_idempotent(self):
        with tempfile.TemporaryDirectory() as home:
            inventory = Path(home) / "grants"
            code = '/bin/cat "$HOME/grants" | missing_grants'
            inventory.write_text("no grants\n")
            result = self.run_shell(code, home)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.splitlines(), [
                "broca|exact|apikey:openai|read", "prefrontal-routing|category|llm-provider|list",
            ])
            valid = ("KIND PRINCIPAL SELECTOR KIND SELECTOR OP REACHES GRANTED\n"
                     "reserved broca exact apikey:openai read 1 timestamp\n"
                     "reserved prefrontal-routing category llm-provider list 1 timestamp\n")
            inventory.write_text(valid)
            result = self.run_shell(code, home)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "")
            inventory.write_text(valid + "reserved broca category llm-provider read 1 timestamp\n")
            result = self.run_shell(code, home)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("unexpected grants", result.stderr)

    def test_config_and_contract_share_the_isolated_unscoped_arming_file(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('guard_all_paths; DRY=1; cmd_config; cmd_test --models', home)
            self.assertEqual(result.returncode, 0, result.stderr)
            path = f"{home}/.local/share/cortexkit/ckdev-flows/runtime/basal-unscoped-send"
            self.assertIn(f'"BASAL_RIG_UNSCOPED_FILE": "{path}"', result.stdout)
            self.assertIn(f"--unscoped-file {path}", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
