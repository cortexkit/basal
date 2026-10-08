import unittest
from compare_reference import FIELDS, token_diff


class ReferenceDiffTests(unittest.TestCase):
    def test_integrity_difference_is_not_hidden_by_equal_user(self):
        result = token_diff({'integrity': 'S-1-16-0', 'user_sid': 'S-1-5-21-1'},
                            {'integrity': 'S-1-16-4096', 'user_sid': 'S-1-5-21-1'})
        self.assertFalse(result['integrity']['equal'])
        self.assertTrue(result['user_sid']['equal'])
        self.assertEqual(result['integrity']['chrome'], 'S-1-16-0')
        self.assertEqual(result['integrity']['basal'], 'S-1-16-4096')

    def test_missing_query_is_unknown_not_equal_or_denied(self):
        result = token_diff({'error': 'OpenProcessToken: Win32 5'}, {'privileges': []})
        self.assertIsNone(result['privileges']['equal'])
        self.assertIsNone(result['privileges']['chrome'])
        self.assertEqual(result['privileges']['basal'], [])
        self.assertEqual(set(result), set(FIELDS))

    def test_deny_only_flag_and_restricting_sid_are_compared(self):
        result = token_diff({'groups': [{'sid': 'S-1-1-0', 'deny_only': False}],
                             'restricting_sids': [{'sid': 'S-1-5-12'}]},
                            {'groups': [{'sid': 'S-1-1-0', 'deny_only': True}],
                             'restricting_sids': [{'sid': 'S-1-0-0'}]})
        self.assertFalse(result['groups']['equal'])
        self.assertFalse(result['restricting_sids']['equal'])


if __name__ == '__main__':
    unittest.main()
