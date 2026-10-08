"""Validate requested launch layers without treating residual reachability as failure."""
import json
from pathlib import Path, PureWindowsPath
import sys


EXPECTED_POLICIES = {
    'dynamic_code': 1,
    'signature': 1,
    'image_load': 7,
    'win32k': 1,
    'strict_handle': 3,
    'extension_points': 1,
    'child_process': 1,
}


def validate(report):
    errors = []
    expected_sid = report['appcontainer_sid']
    for mode in ('plain', 'lpac', 'full'):
        run = next((r for r in report['runs'] if r['mode'] == mode), None)
        if not run:
            errors.append(f'{mode}: missing run')
            continue
        attempt = run['attempts'][-1]
        child = attempt.get('child', {})
        if attempt.get('exit_code') != '0x00000000' or 'probes' not in child:
            errors.append(f'{mode}: child did not produce a completed measurement')
            continue
        token = child['primary_token']
        if token.get('token_type') != 1:
            errors.append(f'{mode}: primary token type was not attested')
        if child['after_revert'] != {'present': False, 'open_error': 1008}:
            errors.append(f'{mode}: absence of impersonation was not attested')
        if token.get('lpac') != (mode != 'plain'):
            errors.append(f'{mode}: LPAC identity mismatch')
        if mode != 'plain':
            if token.get('appcontainer_sid') != expected_sid:
                errors.append(f'{mode}: package SID mismatch')
            if token.get('capabilities') != []:
                errors.append(f'{mode}: capabilities not empty')
        if not isinstance(child.get('handle_table'), list) or not isinstance(child.get('loaded_modules'), list):
            errors.append(f'{mode}: handle/module inventory incomplete')
        for probe in child['probes']:
            if 'not_attempted' in probe['result']:
                errors.append(f'{mode}: unsupported target: {probe["target"]} ({probe["kind"]})')
        if mode != 'full':
            continue
        if token.get('integrity') != 'S-1-16-0':
            errors.append('full: integrity is not Untrusted')
        if token.get('privileges') != []:
            errors.append('full: privileges not removed')
        restricting = [entry['sid'] for entry in token.get('restricting_sids', [])]
        if restricting != ['S-1-0-0']:
            errors.append('full: restricting SIDs are not exactly NULL')
        if not token.get('groups') or any(not entry['deny_only'] for entry in token['groups'] if not int(entry['attributes'], 16) & 32):
            errors.append('full: access groups are not all deny-only')
        if child['initial_impersonation']['present'] is not True:
            errors.append('full: loader impersonation was absent')
        loader=child['initial_impersonation'].get('token') or {}
        if (loader.get('token_type'),loader.get('impersonation_level'))!=(2,2):
            errors.append('full: loader token is not SecurityImpersonation')
        actual = {p['policy']: int(p['flags'], 16) if p['success'] else None for p in child['mitigations']}
        if actual != EXPECTED_POLICIES:
            errors.append(f'full: mitigation mismatch: {actual}')
        job = attempt.get('job') or {}
        if (job.get('limit_flags'), job.get('active_process_limit'), job.get('process_memory_limit'), job.get('ui_restrictions'), job.get('breakaway')) != ('0x00002508', 1, 268435456, '0x000000ff', False):
            errors.append('full: parent-owned job limit readback mismatch')
        if isinstance(child.get('loaded_modules'), list):
            for module in child['loaded_modules']:
                path = PureWindowsPath(module['path'])
                system32 = PureWindowsPath(report['system_root']) / 'System32'
                if str(path) != report.get('child_image') and path.parent != system32:
                    errors.append(f'full: module loaded outside System32: {path}')
    for entry in report['enumeration']:
        if 'error' in entry:
            errors.append(f'enumeration incomplete: {entry["namespace"]}: {entry["error"]}')
    return errors


if __name__ == '__main__':
    report = json.loads(Path(sys.argv[1]).read_text(encoding='utf-8'))
    errors = validate(report)
    for error in errors:
        print(f'INCOMPLETE: {error}')
    print(f'launch evidence: {len(report["runs"])} modes checked, {len(errors)} violations/gaps')
    sys.exit(bool(errors))
