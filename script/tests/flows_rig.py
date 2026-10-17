"""Offline checks of the rig's generated policy and explicit vault boundary."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "flows-rig.sh"
HELPERS = SCRIPT.read_text().split("# ---------------------------------------------------------------- main")[0]


class RigChecks(unittest.TestCase):
    def test_rig_root_defaults_to_scratch_and_honors_override(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp).resolve()
            default = home / "Work/Projects/CortexKit-scratch/ckdev-flows"
            result = self.run_shell('guard_all_paths; printf "%s\\n" "$RIG"', home)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), str(default))

            override = home / "alternate" / "ckdev-flows"
            result = self.run_shell(
                'guard_all_paths; printf "%s\\n" "$RIG"', home,
                {"BASAL_RIG_ROOT": str(override)},
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), str(override))

    def test_rig_root_refuses_production_store_and_external_volumes(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp).resolve()
            roots = [
                home / ".local/share/cortexkit/ckdev-flows",
                Path("/Volumes/ckdev-flows"),
            ]
            for root in roots:
                with self.subTest(root=root):
                    result = self.run_shell(
                        "guard_all_paths", home, {"BASAL_RIG_ROOT": str(root)}
                    )
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("refusing: the rig root", result.stderr)

    def test_dry_run_does_not_check_the_host_rig_port(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''
DRY=1
port_listener() { printf '25910\\n'; }
guard_port_free
''', home)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_real_start_refuses_a_taken_rig_port(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''
port_listener() { printf '25910\\n'; }
cmd_start
''', home)
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("refusing: port 8791 is already taken by pid 25910", result.stderr)

    def test_status_retries_until_module_exits_the_subc_trampoline(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''
mkdir -p "$BIN"
: > "$BIN/ckdev-subc"
: > "$BIN/module"
stat() {
  case "$3" in
    "$BIN/ckdev-subc") printf '100\\n' ;;
    "$BIN/module") printf '200\\n' ;;
    *) return 1 ;;
  esac
}
identifier_of() { printf 'fixture-id\\n'; }
image_reads=0
printf '0\\n' > "$HOME/image-reads"
lsof() {
  case "$*" in
    *"-d txt -Fin"*)
      image_reads=$(cat "$HOME/image-reads")
      image_reads=$((image_reads + 1))
      printf '%s\\n' "$image_reads" > "$HOME/image-reads"
      if [ "$image_reads" -eq 1 ]; then
        printf 'i100\\nn%s\\n' "$BIN/ckdev-subc"
      else
        printf 'i200\\nn%s\\n' "$BIN/module"
      fi ;;
    *"-Ftn"*) : ;;
  esac
}
sleep() { :; }
report_process fixture "$BIN/module" 123 fixture-id
printf 'image-reads=%s\\n' "$(cat "$HOME/image-reads")"
''', home)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("running image is the placed file (inode 200)", result.stdout)
            self.assertIn("image-reads=2", result.stdout)

    def test_status_fails_if_module_never_exits_the_subc_trampoline(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''
mkdir -p "$BIN"
: > "$BIN/ckdev-subc"
: > "$BIN/module"
stat() {
  case "$3" in
    "$BIN/ckdev-subc") printf '100\\n' ;;
    "$BIN/module") printf '200\\n' ;;
    *) return 1 ;;
  esac
}
identifier_of() { printf 'fixture-id\\n'; }
image_reads=0
printf '0\\n' > "$HOME/image-reads"
lsof() {
  case "$*" in
    *"-d txt -Fin"*)
      image_reads=$(cat "$HOME/image-reads")
      image_reads=$((image_reads + 1))
      printf '%s\\n' "$image_reads" > "$HOME/image-reads"
      printf 'i100\\nn%s\\n' "$BIN/ckdev-subc" ;;
    *"-Ftn"*) : ;;
  esac
}
sleep() { :; }
status=0
report_process fixture "$BIN/module" 123 fixture-id || status=$?
printf 'report-status=%s image-reads=%s\\n' "$status" "$(cat "$HOME/image-reads")"
''', home)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("running image is NOT the placed file (running inode 100, placed 200)", result.stdout)
            self.assertIn("report-status=1 image-reads=51", result.stdout)

    def test_report_rejects_zero_checks_unless_explicitly_not_run(self):
        with tempfile.TemporaryDirectory() as home:
            report_path = Path(home) / "contract.json"
            def check(case):
                report_path.write_text(json.dumps({"passed": True, "cases": [case]}))
                return self.run_shell('check_contract_report "$HOME/contract.json"', home)

            empty = {"case": "facts: agent.facts through core", "checks": [], "passed": True}
            result = check(empty)
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("zero checks without NOT RUN: facts: agent.facts through core", result.stderr)
            result = check({**empty, "not_run": ""})
            self.assertNotEqual(result.returncode, 0)
            result = check({**empty, "not_run": "The prerequisite flow did not run.", "passed": None})
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            result = check({**empty, "checks": [{"passed": True}]})
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_run_scope_probe_is_built_signed_and_configured_only_on_the_rig(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''guard_all_paths
DRY=1
binaries
modules
place_one runscope-probe /built/ckdev-runscope-probe "$BIN/ckdev-runscope-probe"
cmd_config
''', home)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("runscope-probe\tbasal\tckdev-runscope-probe\tckdev-runscope-probe", result.stdout)
            self.assertIn("ckdev-runscope-probe\tckdev-runscope-probe", result.stdout)
            self.assertIn('"ckdev-runscope-probe": {', result.stdout)
            self.assertIn('/bin/ckdev-runscope-probe"', result.stdout)
            self.assertIn("sign_hardened", result.stdout)
            self.assertIn("ckdev-runscope-probe   (script/signing.sh)", result.stdout)

    def test_build_defaults_to_pinned_run_scope_core_and_builds_probe(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''guard_all_paths
DRY=1
repos() { printf 'basal\t%s\nprefrontal\t%s\ncingulate\t%s\n' "$HOME" "$HOME" "$HOME"; }
git() { printf '%040d\n' 0; }
clone_at() { printf 'clone %s %s\n' "$1" "$3"; }
cargo_build() { printf 'build %s\n' "$*"; }
cmd_build
''', home)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("prefrontal: defe4a72a3ee4bac8ce81645df801c6907a8ce4c", result.stdout)
            self.assertIn("cingulate: c650e9db6e87dbb28fe20dd5fc2815db5deb41e9", result.stdout)
            self.assertIn("--bin ck-callosum-stub --bin ckdev-runscope-probe --bin basal-rig-contract", result.stdout)

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
            root = home / 'Work/Projects/CortexKit-scratch/ckdev-flows'
            root.mkdir(parents=True)
            (root / 'bin').symlink_to(home / '.local/share/cortexkit/bin')
            result = self.run_shell('guard_all_paths', home)
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn('outside', result.stderr)

    def test_bad_staged_copy_leaves_the_placed_binary_unchanged(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp).resolve()
            root = home / 'Work/Projects/CortexKit-scratch/ckdev-flows'
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

    def test_copy_stage_copies_and_verifies_the_stage_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp).resolve()
            stage = home / "basal-0123456789ab"
            stage.mkdir()
            files = {
                "ck-basal": b"basal staged bytes",
                "ck-basal-worker": b"worker staged bytes",
                "card.md": b"deployment card\n",
                "revision": b"0123456789abcdef\n",
            }
            for name, content in files.items():
                (stage / name).write_bytes(content)
            for name in ("ck-basal", "ck-basal-worker"):
                digest = hashlib.sha256(files[name]).hexdigest()
                (stage / f"{name}.sha256").write_text(f"{digest}  {name}\n")

            root = home / "scratch" / "ckdev-flows"
            result = self.run_shell(
                'guard_all_paths; cmd_copy_stage "$HOME/basal-0123456789ab"', home,
                {"BASAL_RIG_ROOT": str(root)},
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            copied = root / "stages" / stage.name
            for name, content in files.items():
                self.assertEqual((copied / name).read_bytes(), content)
            for name in ("ck-basal", "ck-basal-worker"):
                self.assertEqual(
                    (copied / f"{name}.sha256").read_text(),
                    (stage / f"{name}.sha256").read_text(),
                )

    def test_copy_stage_refuses_tampered_sha256(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp).resolve()
            stage = home / "basal-tampered"
            stage.mkdir()
            for name, content in (("ck-basal", b"basal"), ("ck-basal-worker", b"worker")):
                (stage / name).write_bytes(content)
                digest = hashlib.sha256(content).hexdigest()
                (stage / f"{name}.sha256").write_text(f"{digest}  {name}\n")
            (stage / "ck-basal.sha256").write_text("0" * 64 + "  ck-basal\n")
            (stage / "card.md").write_text("card\n")
            (stage / "revision").write_text("revision\n")

            root = home / "scratch" / "ckdev-flows"
            result = self.run_shell(
                'guard_all_paths; cmd_copy_stage "$HOME/basal-tampered"', home,
                {"BASAL_RIG_ROOT": str(root)},
            )
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("fails its SHA-256 sidecar", result.stderr)
            self.assertFalse((root / "stages" / stage.name).exists())

    def run_shell(self, code, home, extra_env=None):
        env = {**os.environ, "HOME": str(home)}
        env.pop("BASAL_RIG_ROOT", None)
        env.update(extra_env or {})
        return subprocess.run(
            ["/bin/sh", "-c", HELPERS + "\n" + code, str(SCRIPT)],
            env=env,
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
            routing = Path(home) / "Work/Projects/CortexKit-scratch/ckdev-flows/bin/ckdev-prefrontal-routing"
            routing.parent.mkdir(parents=True)
            routing.write_text('#!/bin/sh\nprintf \'%s\\n\' "$XDG_CONFIG_HOME" "$@" > "$HOME/check-call"\n')
            routing.chmod(0o755)
            code = 'guard_all_paths; rig_env() { /bin/cat "$HOME/catalog.json"; }; pin_routing'
            result = self.run_shell(code, home)
            self.assertEqual(result.returncode, 0, result.stderr)
            policy = Path(home) / "Work/Projects/CortexKit-scratch/ckdev-flows/config/cortexkit/alfonso-routing.jsonc"
            self.assertEqual((Path(home) / "check-call").read_text().splitlines(), [
                str(Path(home) / "Work/Projects/CortexKit-scratch/ckdev-flows/config"),
                "--check-config",
            ])
            policy_text = policy.read_text()
            self.assertIn("// Rig fixture values, not measured model quality.", policy_text)
            value = json.loads("\n".join(line for line in policy_text.splitlines()
                                         if not line.lstrip().startswith("//")))
            self.assertEqual(value, {"model_routing": {
                "exclude": {"all": ["another", "new-provider", "openai", "-openai/gpt-6-luna"]},
                "models": {"openai/gpt-6-luna": {"elo": 1, "eq": 0, "speed": 0}},
            }})
            policy.unlink()
            catalog.write_text('{"models":{"openai/other":{}}}')
            result = self.run_shell(code, home)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("lacks openai/gpt-6-luna", result.stderr)
            self.assertFalse(policy.exists())

    def test_pin_fails_with_routing_config_validation_output(self):
        with tempfile.TemporaryDirectory() as home:
            catalog = Path(home) / "catalog.json"
            catalog.write_text(json.dumps({"models": {"openai/gpt-6-luna": {}}}))
            routing = Path(home) / "Work/Projects/CortexKit-scratch/ckdev-flows/bin/ckdev-prefrontal-routing"
            routing.parent.mkdir(parents=True)
            routing.write_text('#!/bin/sh\nprintf \'unsupported routing configuration\\n\' >&2\nexit 2\n')
            routing.chmod(0o755)
            result = self.run_shell(
                'guard_all_paths; rig_env() { /bin/cat "$HOME/catalog.json"; }; pin_routing', home
            )
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("unsupported routing configuration", result.stderr)

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
                    self.assertIn(f"--data-dir {home}/Work/Projects/CortexKit-scratch/ckdev-flows/data/cortexkit/claustrum", line)
                    self.assertIn(f"--key-path {home}/Work/Projects/CortexKit-scratch/ckdev-flows/config/claustrum/master.key", line)
                    self.assertIn(f"--subc {home}/Work/Projects/CortexKit-scratch/ckdev-flows/runtime/subc-connection.json", line)
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
            key = Path(home) / "Work/Projects/CortexKit-scratch/ckdev-flows/config/claustrum/master.key"
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
            self.assertIn("model_routing.exclude.all = [every served provider, -openai/gpt-6-luna]", result.stdout)
            self.assertIn("ckdev-prefrontal-routing --check-config", result.stdout)
            path = f"{home}/Work/Projects/CortexKit-scratch/ckdev-flows/runtime/basal-unscoped-send"
            self.assertIn(f'"BASAL_RIG_UNSCOPED_FILE": "{path}"', result.stdout)
            self.assertIn(f"--unscoped-file {path}", result.stdout)

    def test_live_contract_receives_the_registered_isolated_project_root(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''
guard_all_paths
date() { printf '%s\n' 20260101T000000Z; }
daemon_pid() { printf '%s\n' 123; }
check_daemon_identity() { :; }
ensure_project() { printf '%s\n' pj_fixture; }
write_manifest() { mkdir -p "$RESULTS/$1"; }
basal_mode() { printf '%s\n' rig-build; }
mkdir -p "$RIG"
CONTRACT="$RIG/ckdev-contract"
printf '#!/bin/sh\nexit 0\n' > "$CONTRACT"
chmod 755 "$CONTRACT"
rig_env() {
  printf '%s\n' "$@" > "$RIG/captured-argv"
  printf '%s\n' '{"passed":true,"cases":[{"case":"fixture","checks":[{"passed":true}]}]}' > "$dir/contract.json"
}
cmd_test
''', home)
            self.assertEqual(result.returncode, 0, result.stderr)
            rig = Path(home) / "Work/Projects/CortexKit-scratch/ckdev-flows"
            args = (rig / "captured-argv").read_text().splitlines()
            self.assertEqual(args[args.index("--rig-root") + 1], str(rig))
            self.assertEqual(args[args.index("--project-root") + 1],
                             str(rig / "home/projects/basal-rig-20260101t000000z"))

    def test_package_selector_reaches_the_live_contract_without_model_credentials(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''
guard_all_paths
date() { printf '%s\n' 20260101T000000Z; }
daemon_pid() { printf '%s\n' 123; }
check_daemon_identity() { :; }
ensure_project() { printf '%s\n' pj_fixture; }
write_manifest() { mkdir -p "$RESULTS/$1"; }
basal_mode() { printf '%s\n' rig-build; }
require_model_credential() { die "packages must not require model credentials"; }
mkdir -p "$RIG"
CONTRACT="$RIG/ckdev-contract"
printf '#!/bin/sh\nexit 0\n' > "$CONTRACT"
chmod 755 "$CONTRACT"
rig_env() {
  printf '%s\n' "$@" > "$RIG/captured-argv"
  printf '%s\n' '{"passed":true,"cases":[{"case":"fixture","checks":[{"passed":true}]}]}' > "$dir/contract.json"
}
cmd_test --packages
''', home)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            rig = Path(home) / "Work/Projects/CortexKit-scratch/ckdev-flows"
            args = (rig / "captured-argv").read_text().splitlines()
            self.assertEqual(args.count("--packages"), 1)
            self.assertNotIn("--models", args)
            dry = self.run_shell('DRY=1; cmd_test --packages', home)
            self.assertEqual(dry.returncode, 0, dry.stderr)
            self.assertIn("--packages (only setup, packages and cleanup; other groups not run)", dry.stdout)
            mixed = self.run_shell('DRY=1; cmd_test --packages --models', home)
            self.assertNotEqual(mixed.returncode, 0)

    def test_core_revision_comes_from_the_manifest_and_must_match(self):
        with tempfile.TemporaryDirectory() as home:
            manifest = Path(home) / "manifest.json"
            manifest.write_text(json.dumps({"manifest": {"provenance": {"build_git_sha": "expected"}}}))
            good = self.run_shell('/bin/cat "$HOME/manifest.json" | core_revision expected', home)
            self.assertEqual(good.returncode, 0, good.stderr)
            self.assertEqual(good.stdout.strip(), "expected")
            bad = self.run_shell('/bin/cat "$HOME/manifest.json" | core_revision other', home)
            self.assertNotEqual(bad.returncode, 0)
            self.assertIn("does not match", bad.stderr)

    def test_real_cingulate_is_pinned_placed_and_configured_in_the_isolated_rig(self):
        with tempfile.TemporaryDirectory() as home:
            result = self.run_shell('''
guard_all_paths
DRY=1
binaries
modules
cmd_config
''', home)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("cingulate\tcingulate\tck-cingulate\tckdev-cingulate", result.stdout)
            self.assertIn("cingulate\tckdev-cingulate", result.stdout)
            self.assertIn(f'"program": "{home}/Work/Projects/CortexKit-scratch/ckdev-flows/bin/ckdev-cingulate"', result.stdout)
        self.assertIn("cingulate_rev=c650e9db6e87dbb28fe20dd5fc2815db5deb41e9", HELPERS)


if __name__ == "__main__":
    unittest.main(verbosity=2)
