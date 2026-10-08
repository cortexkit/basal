"""Print successful targets; absence of child evidence is never treated as denial."""
import json
from pathlib import Path
import sys


def escape(value):
    return str(value).replace('|', '\\|').replace('\n', ' ')


def summarize(report):
    print('# Native Windows reachability')
    print(f'Profile SID: `{report["appcontainer_sid"]}`')
    print('## Enumeration')
    for entry in report['enumeration']:
        print(f'- `{entry["namespace"]}`: {entry.get("count", entry.get("error"))}')
    for run in report['runs']:
        print(f'## {run["mode"]}')
        for attempt in run['attempts']:
            print(f'- Sequence: `{attempt["sequence"]}`; exit: `{attempt.get("exit_code")}`; error: {attempt.get("error", "none")}')
            print(f'- Parent tokens before resume: `{json.dumps(attempt.get("parent_before_resume"))}`')
            print(f'- Constructed tokens: `{json.dumps(attempt.get("constructed_tokens"))}`')
            child = attempt.get('child', {})
            if 'probes' not in child:
                print(f'- **NO MEASUREMENT**: {escape(child or attempt)}')
                continue
            print(f'- Primary token: `{json.dumps(child["primary_token"])}`')
            print(f'- Impersonation after revert: `{json.dumps(child["after_revert"])}`')
            print(f'- Mitigations: `{json.dumps(child["mitigations"])}`')
            print(f'- Job: `{json.dumps(attempt.get("job"))}`')
            print(f'- Initial handles: `{json.dumps(child["handle_table"])}`')
            print(f'- Loaded modules: `{json.dumps(child["loaded_modules"])}`')
            probes = child['probes']
            print(f'- Probes: {len(probes)}; succeeded: {sum(p["success"] for p in probes)}')
            print('| Kind | Successful target | Access | Result |')
            print('|---|---|---|---|')
            for probe in sorted((p for p in probes if p['success']), key=lambda p: (p['kind'], p['target'], p['access'])):
                print('| ' + ' | '.join(escape(probe[key]) for key in ('kind', 'target', 'access', 'result')) + ' |')
        print(f'- Parent observed loopback: `{json.dumps(run["loopback_observed"])}`')
    print('## Layer comparison')
    modes = {}
    for run in report['runs']:
        child = next((a['child'] for a in reversed(run['attempts']) if 'probes' in a.get('child', {})), None)
        if child:
            modes[run['mode']] = {(p['kind'], p['target'], p['access']): p['success'] for p in child['probes']}
    print('| Kind | Target | Access | Plain | LPAC | Full |')
    print('|---|---|---|---|---|---|')
    for key in sorted(set().union(*(mode.keys() for mode in modes.values()))):
        if not any(mode.get(key) is True for mode in modes.values()):
            continue
        cells = [escape(cell) for cell in key]
        cells += ['yes' if modes.get(mode, {}).get(key) is True else 'no' if modes.get(mode, {}).get(key) is False else 'not measured' for mode in ('plain', 'lpac', 'full')]
        print('| ' + ' | '.join(cells) + ' |')


if __name__ == '__main__':
    summarize(json.loads(Path(sys.argv[1]).read_text(encoding='utf-8')))
