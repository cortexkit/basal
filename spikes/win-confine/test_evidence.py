import contextlib
import io
from pathlib import Path
import struct
import tempfile
import unittest

from pe_inventory import inventory
from summarize import summarize
from validate import validate


def complete_report():
    sid = 'S-1-15-2-fixture'
    policies = [('dynamic_code', 1), ('signature', 1), ('image_load', 7), ('win32k', 1), ('strict_handle', 3), ('extension_points', 1), ('child_process', 1)]
    runs = []
    for mode in ('plain', 'lpac', 'full'):
        token = {'token_type': 1, 'lpac': mode != 'plain', 'appcontainer_sid': sid if mode != 'plain' else None, 'capabilities': [], 'integrity': 'S-1-16-0', 'privileges': [], 'restricting_sids': [{'sid': 'S-1-0-0'}], 'groups': [{'sid': 'S-1-1-0', 'attributes': '0x00000010', 'deny_only': True}]}
        child = {'primary_token': token, 'initial_impersonation': {'present': mode == 'full', 'token': {'token_type': 2, 'impersonation_level': 2}}, 'after_revert': {'present': False, 'open_error': 1008}, 'handle_table': [], 'loaded_modules': [{'name': 'kernel32.dll', 'path': 'C:\\Windows\\System32\\kernel32.dll'}], 'mitigations': [{'policy': name, 'flags': f'0x{flags:08x}', 'success': True} for name, flags in policies], 'probes': []}
        job = {'limit_flags': '0x00002508', 'active_process_limit': 1, 'process_memory_limit': 268435456, 'ui_restrictions': '0x000000ff', 'breakaway': False}
        runs.append({'mode': mode, 'attempts': [{'exit_code': '0x00000000', 'child': child, 'job': job}]})
    return {'appcontainer_sid': sid, 'system_root': 'C:\\Windows', 'enumeration': [], 'runs': runs}


class EvidenceTests(unittest.TestCase):
    def test_complete_launch_evidence_is_accepted(self):
        self.assertEqual(validate(complete_report()), [])

    def test_loader_identification_token_is_rejected(self):
        report = complete_report()
        report['runs'][2]['attempts'][0]['child']['initial_impersonation']['token']['impersonation_level'] = 1
        self.assertEqual(validate(report), ['full: loader token is not SecurityImpersonation'])

    def test_full_policy_requires_null_restricting_sid(self):
        report = complete_report()
        report['runs'][2]['attempts'][0]['child']['primary_token']['restricting_sids'] = [{'sid': 'S-1-1-0'}]
        self.assertEqual(validate(report), ['full: restricting SIDs are not exactly NULL'])

    def test_full_policy_requires_every_mitigation(self):
        report = complete_report()
        report['runs'][2]['attempts'][0]['child']['mitigations'][0]['flags'] = '0x00000000'
        self.assertEqual(len(validate(report)), 1)
        self.assertIn('full: mitigation mismatch', validate(report)[0])

    def test_enumeration_error_is_a_coverage_gap(self):
        report = complete_report()
        report['enumeration'] = [{'namespace': '\\RPC Control', 'error': 'denied'}]
        self.assertEqual(validate(report), ['enumeration incomplete: \\RPC Control: denied'])

    def test_attestation_module_path_outside_system32_is_rejected(self):
        report = complete_report()
        report['runs'][2]['attempts'][0]['child']['loaded_modules'][0]['path'] = 'C:\\User\\kernel32.dll'
        self.assertEqual(validate(report), ['full: module loaded outside System32: C:\\User\\kernel32.dll'])

    def test_pe_inventory_distinguishes_imports_and_delay_imports(self):
        # A PE32+ fixture with real descriptors, hint/name thunks and two tables.
        data = bytearray(0x1200)
        struct.pack_into('<I', data, 0x3C, 0x80)
        data[0x80:0x84] = b'PE\0\0'
        struct.pack_into('<H', data, 0x86, 1)
        struct.pack_into('<H', data, 0x94, 240)
        struct.pack_into('<H', data, 0x98, 0x20B)
        struct.pack_into('<Q', data, 0x98 + 24, 0x140000000)
        struct.pack_into('<I', data, 0x98 + 60, 0x200)
        directory = 0x98 + 112
        struct.pack_into('<II', data, directory + 8, 0x1000, 40)
        struct.pack_into('<II', data, directory + 13 * 8, 0x1100, 64)
        struct.pack_into('<IIII', data, 0x98 + 240 + 8, 0x1000, 0x1000, 0x1000, 0x200)
        struct.pack_into('<5I', data, 0x200, 0x1200, 0, 0, 0x1300, 0x1200)
        struct.pack_into('<8I', data, 0x300, 1, 0x1400, 0, 0x1500, 0x1500, 0, 0, 0)
        struct.pack_into('<QQ', data, 0x400, 0x1600, 0)
        struct.pack_into('<QQ', data, 0x700, 0x1700, 0)
        data[0x500:0x50D] = b'KERNEL32.dll\0'
        data[0x600:0x60C] = b'USERENV.dll\0'
        data[0x802:0x80A] = b'ExitNow\0'
        data[0x902:0x90B] = b'DelayNow\0'
        target = Path(__file__).parent / 'target'
        target.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=target) as temp:
            path = Path(temp) / 'fixture.exe'
            path.write_bytes(data)
            result = inventory(path)
        self.assertEqual(result['normal'], [{'dll': 'KERNEL32.dll', 'symbols': [{'name': 'ExitNow'}]}])
        self.assertEqual(result['delay'], [{'dll': 'USERENV.dll', 'symbols': [{'name': 'DelayNow'}]}])

    def test_failed_child_is_not_summarized_as_denial(self):
        report = {'appcontainer_sid': 'S-1-15-2-test', 'enumeration': [], 'runs': [
            {'mode': 'full', 'attempts': [{'sequence': 'CreateProcessAsUserW', 'error': 'Win32 1314'}], 'loopback_observed': {}}
        ]}
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            summarize(report)
        self.assertIn('NO MEASUREMENT', output.getvalue())
        self.assertIn('Win32 1314', output.getvalue())

    def test_summary_lists_every_success_and_not_failed_targets(self):
        probe = {'kind': 'event', 'target': 'fixture', 'access': 'QUERY', 'success': True, 'result': {'code': 0}}
        failed = dict(probe, target='denied', success=False)
        child = {'primary_token': {}, 'after_revert': {}, 'mitigations': [], 'handle_table': [], 'loaded_modules': [], 'probes': [probe, failed]}
        report = {'appcontainer_sid': 'sid', 'enumeration': [], 'runs': [
            {'mode': mode, 'attempts': [{'sequence': 'CreateProcessW', 'child': child}], 'loopback_observed': {}} for mode in ('plain', 'lpac', 'full')
        ]}
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            summarize(report)
        self.assertEqual(output.getvalue().count('| event | fixture | QUERY |'), 4)
        self.assertNotIn('| event | denied |', output.getvalue())
        self.assertIn('| yes | yes | yes |', output.getvalue())


if __name__ == '__main__':
    unittest.main(verbosity=2)
