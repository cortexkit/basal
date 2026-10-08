import unittest
from procmon_summary import collect, markdown


class ProcessMonitorTests(unittest.TestCase):
    def report(self):
        return {'runs': [{'attempts': [], 'diagnostics': [{'result': {
            'sequence': 'post-load-primary-control', 'pid': 42,
            'exit_code': '0xc0000142'}}]}]}

    def row(self, operation, result='SUCCESS', pid='42', path='C:\\fixture'):
        return {'PID': pid, 'Operation': operation, 'Result': result,
                'Path': path, 'Detail': 'Desired Access: Read', 'Time of Day': '12:00'}

    def test_retains_all_non_success_and_last_denial_in_capture_order(self):
        results, rows = collect(self.report(), [
            self.row('Process Start'), self.row('CreateFile', 'ACCESS DENIED'),
            self.row('RegOpenKey', 'NAME NOT FOUND'),
            self.row('CreateFile', 'PRIVILEGE NOT HELD'),
            self.row('QueryDirectory', 'NO MORE FILES'), self.row('Process Exit')])
        self.assertEqual(len(rows), 6)
        result = results[0]
        self.assertTrue(result['process_start_seen'])
        self.assertTrue(result['process_exit_seen'])
        self.assertEqual([r['Result'] for r in result['non_success']],
                         ['ACCESS DENIED', 'NAME NOT FOUND', 'PRIVILEGE NOT HELD', 'NO MORE FILES'])
        self.assertEqual(result['last_denied']['capture_order'], 4)
        self.assertIn('Desired Access: Read', markdown(results))

    def test_does_not_substitute_same_name_other_pid(self):
        results, rows = collect(self.report(), [self.row('Process Exit', pid='100')])
        self.assertEqual(rows, [])
        self.assertEqual(results[0]['event_count'], 0)
        self.assertFalse(results[0]['process_start_seen'])
        self.assertFalse(results[0]['process_exit_seen'])
        self.assertIsNone(results[0]['last_denied'])

    def test_expected_miss_after_denial_is_not_called_last_denial(self):
        results, _ = collect(self.report(), [self.row('CreateFile', 'ACCESS DENIED'),
                                             self.row('RegOpenKey', 'NAME NOT FOUND')])
        self.assertEqual(results[0]['last_denied']['Operation'], 'CreateFile')
        self.assertEqual(len(results[0]['non_success']), 2)


if __name__ == '__main__':
    unittest.main()
