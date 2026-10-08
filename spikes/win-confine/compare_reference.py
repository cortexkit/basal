"""Compare Chrome token observations to basal's pre-resume and intended final tokens."""
import json
from pathlib import Path
import sys

FIELDS = ('token_type', 'impersonation_level', 'user_sid', 'integrity', 'appcontainer',
          'appcontainer_sid', 'lpac', 'capabilities', 'restricting_sids', 'groups', 'privileges',
          'default_dacl')


def token_diff(reference, candidate):
    return {field: {'chrome': reference.get(field), 'basal': candidate.get(field),
                    'equal': reference[field] == candidate[field]
                    if field in reference and field in candidate else None}
            for field in FIELDS}


def compare(directory):
    directory = Path(directory)
    report = json.loads((directory / 'report.json').read_text(encoding='utf-8-sig'))
    inventory = json.loads((directory / 'chrome-inventory.json').read_text(encoding='utf-8-sig'))
    full = next(run for run in report['runs'] if run['mode'] == 'full')
    birth = full['attempts'][-1]['parent_before_resume']['primary']
    # Constructed lockdown values describe the intended final process token;
    # they do not show that the child transitioned to that token.
    intended = dict(full['attempts'][0]['constructed_tokens']['lockdown'])
    intended.update(appcontainer=True, appcontainer_sid=report['appcontainer_sid'], lpac=True,
                    capabilities=[], integrity='S-1-16-0',
                    lpac_query={'not_measured': 'intended final state'})
    comparisons = []
    for process in inventory['processes']:
        if process['kind'] != 'renderer':
            continue
        observed = json.loads((directory / Path(process['attestation']).name).read_text(encoding='utf-8-sig'))
        token = observed.get('primary_token', {})
        comparisons.append({'pid': process['pid'], 'observed': observed,
                            'startup_context_diff': {'chrome': observed.get('startup_context'),
                                'basal_before_resume': full['attempts'][-1]['parent_before_resume'].get('startup_context')},
                            'against_birth': token_diff(token, birth),
                            'against_intended_final': token_diff(token, intended)})
    return {'schema': 1, 'chrome_inventory': inventory, 'basal_birth': birth,
            'basal_intended_final_not_measured': intended, 'renderers': comparisons,
            'loader_traces': [d['result'] for d in full['diagnostics']
                              if d['result'].get('sequence') == 'loader-trace'],
            'chrome_matched_attempts': [d for d in full['diagnostics']
                                        if d['label'].startswith('Chrome-matched')]}


if __name__ == '__main__':
    result = compare(sys.argv[1])
    Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + '\n', encoding='utf-8')
    print(f"Compared {len(result['renderers'])} renderer tokens across {len(FIELDS)} fields; "
          f"captured {len(result['loader_traces'])} loader trace(s)")
