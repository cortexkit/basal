import contextlib
import io
from pathlib import Path
import struct
import tempfile
import unittest

from pe_inventory import inventory
from summarize import summarize


class EvidenceTests(unittest.TestCase):
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
        Path('target').mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir='target') as temp:
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
