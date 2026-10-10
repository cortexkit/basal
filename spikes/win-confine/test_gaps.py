import unittest
from typing import Any
from retain_gaps import violations, render


def complete() -> dict[str, Any]:
    def row(tid) -> dict[str, Any]:
        return dict(tid=tid, win32_start_address='0x1234', nt_open_as_self=dict(status='0xc000007c'), nt_open_as_client=dict(status='0xc000007c'))
    def snapshot(tids) -> dict[str, Any]:
        return dict(enumeration_complete=True, process_reported_thread_count=len(tids), discovered_toolhelp_tids=tids, threads=[row(tid) for tid in tids])
    callbacks = [dict(row(tid), kind=kind) for tid, kind in zip([2, 3, 4], ['work', 'wait', 'timer'])]
    return dict(run_id='test', source_sha='test', image='test', pid=1, sequence='test', exit_code='0x00000000',
                threads=dict(pre_pool_activity=snapshot([1]), post_pool_activity=snapshot([1, 2, 3, 4]), post_release=snapshot([1]), forced_pool_activity=dict(ready_status=0, callback_inspections=callbacks)),
                scheduler_shared_data=dict(present=True, probe=dict(operations=dict(duplicate_object=dict(generic_all=dict(status='0x00000000', granted_access='0x000f0001'))))),
                scheduler_correlation=dict(object_resolved=True))


class GapEvidenceTests(unittest.TestCase):
    def test_complete_thread_and_scheduler_coverage(self):
        self.assertEqual(violations(complete()), [])

    def test_callback_tid_missing_from_enumeration_is_rejected(self):
        measurement = complete()
        measurement['threads']['forced_pool_activity']['callback_inspections'][0]['tid'] = 999
        self.assertIn('callback TID missing from enumeration', violations(measurement))

    def test_single_thread_fallback_is_incomplete(self):
        measurement = complete()
        measurement['threads']['pre_pool_activity']['enumeration_complete'] = False
        self.assertIn('pre_pool_activity: incomplete enumeration', violations(measurement))

    def test_process_reported_count_must_match_rows(self):
        measurement = complete()
        measurement['threads']['post_pool_activity']['process_reported_thread_count'] = 1
        self.assertIn('post_pool_activity: incomplete enumeration', violations(measurement))

    def test_access_denied_is_not_no_token(self):
        measurement = complete()
        measurement['threads']['pre_pool_activity']['threads'][0]['nt_open_as_self']['status'] = '0xc0000022'
        self.assertTrue(any('not STATUS_NO_TOKEN' in e for e in violations(measurement)))

    def test_wait_and_timer_must_execute(self):
        measurement = complete()
        measurement['threads']['forced_pool_activity']['callback_inspections'].pop()
        self.assertIn('work/wait/timer callbacks did not all run', violations(measurement))

    def test_successful_duplicate_requires_measured_grant(self):
        measurement = complete()
        del measurement['scheduler_shared_data']['probe']['operations']['duplicate_object']['generic_all']['granted_access']
        self.assertIn('generic_all: successful duplicate grant not queried', violations(measurement))

    def test_render_does_not_label_success_as_access_denied(self):
        text = render(complete())
        self.assertIn('0x000f0001', text)
        self.assertNotIn('STATUS_ACCESS_DENIED', text)


if __name__ == '__main__':
    unittest.main()
