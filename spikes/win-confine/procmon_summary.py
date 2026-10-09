"""Retain every non-success child event, without treating expected misses as denials."""
import csv
import json
import sys
from pathlib import Path

RECIPES = ('post-load-primary-control', 'chrome-default-dacl-control',
           'chrome-context-control', 'lpac-context-control')
FOLLOWUPS = ('post-load-cwd-grant-control', 'post-load-detached-control',
             'chrome-detached-control', 'lpac-context-detached-control',
             'chrome-context-detached-control', 'full-detached-control',
             'chrome-untrusted-detached-control', 'full-gui-control',
             'full-gui-close-alpc', 'full-gui-close-file',
             'full-gui-close-pool', 'chrome-untrusted-gui-control')


def attempts(report):
    for run in report['runs']:
        yield from run['attempts']
        for diagnostic in run.get('diagnostics', []):
            yield diagnostic['result']


def safe_row(row):
    row = dict(row)
    if row['Operation'] == 'Process Start' and row['Result'] == 'SUCCESS':
        # Procmon puts inherited variable values in Process Start details;
        # omit them rather than publish potential runner credentials.
        row['Detail'] = row['Detail'].split('Environment:', 1)[0] + 'Environment: [values omitted]'
    return row


def collect(report, rows):
    selected = {str(a['pid']): a for a in attempts(report)
                if a.get('sequence') in RECIPES + FOLLOWUPS and 'pid' in a}
    events = {pid: [] for pid in selected}
    all_rows = []
    for index, row in enumerate(rows, 1):
        pid = row['PID']
        if pid not in selected:
            continue
        row = dict(safe_row(row), capture_order=index)
        all_rows.append(row)
        events[pid].append(row)
    results = []
    for pid, attempt in selected.items():
        rows = events[pid]
        failures = [r for r in rows if r['Result'] != 'SUCCESS']
        denials = [r for r in failures if r['Result'] in
                   ('ACCESS DENIED', 'PRIVILEGE NOT HELD')]
        results.append({'sequence': attempt['sequence'], 'pid': int(pid),
                        'exit_code': attempt['exit_code'], 'event_count': len(rows),
                        'operations': sorted(set(r['Operation'] for r in rows)),
                        'process_start_seen': any(r['Operation'] == 'Process Start' for r in rows),
                        'process_exit_seen': any(r['Operation'] == 'Process Exit' for r in rows),
                        'non_success': failures,
                        'last_denied': denials[-1] if denials else None})
    return results, all_rows


def markdown(results):
    lines = ['# Process Monitor child startup windows', '']
    for result in results:
        lines += [f"## {result['sequence']} (PID {result['pid']})",
                  f"Exit `{result['exit_code']}`; {result['event_count']} events; "
                  f"start seen: {result['process_start_seen']}; exit seen: {result['process_exit_seen']}.",
                  '', '| Capture order | Time | Operation | Path | Result | Detail |',
                  '|---:|---|---|---|---|---|']
        for row in result['non_success']:
            values = [str(row['capture_order']), row['Time of Day'], row['Operation'],
                      row['Path'], row['Result'], row['Detail']]
            lines.append('| ' + ' | '.join(v.replace('|', '&#124;').replace('\n', ' ') for v in values) + ' |')
        lines += ['', f"Last denied event: `{result['last_denied']}`", '']
    return '\n'.join(lines)


def complete(results):
    return (set(RECIPES).issubset(r['sequence'] for r in results) and all(
        r['process_start_seen'] and r['process_exit_seen'] for r in results))


if __name__ == '__main__':
    report, source, output, text = map(Path, sys.argv[1:])
    with source.open(encoding='utf-8-sig', newline='') as f:
        results, rows = collect(json.loads(report.read_text(encoding='utf-8')), csv.DictReader(f))
    output.write_text(json.dumps(results, indent=2), encoding='utf-8')
    text.write_text(markdown(results), encoding='utf-8')
    source.with_name('procmon-child-rows.json').write_text(json.dumps(rows, indent=2), encoding='utf-8')
    print(f"Process Monitor: {len(results)} children, {len(rows)} events, "
          f"{sum(len(r['non_success']) for r in results)} non-success events")
    if not complete(results):
        print('Incomplete early capture: no empty result is accepted as denial evidence', file=sys.stderr)
        sys.exit(1)
