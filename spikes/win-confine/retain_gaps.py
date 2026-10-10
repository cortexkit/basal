"""Compact evidence and fail-closed checks for thread tokens and scheduler handles."""
import json
import pathlib
import sys
from typing import Any


def violations(measurement: dict[str, Any]) -> list[str]:
    issues = []
    if measurement.get("exit_code") != "0x00000000":
        issues.append("complete-close worker did not exit successfully")
    primary = measurement.get('primary_token') or {}
    if primary.get('integrity') != 'S-1-16-0' or primary.get('lpac') is not True:
        issues.append('actual primary is not attested Untrusted LPAC')
    if not (measurement.get('self_lowering') or {}).get('adjustment_handle_closed_before_input'):
        issues.append('primary adjustment handle closure not attested')
    threads = measurement.get("threads") or {}
    for phase in ("pre_pool_activity", "post_pool_activity", "post_release"):
        snapshot = threads.get(phase, {})
        rows = snapshot.get("threads", [])
        tids = {t.get("tid") for t in rows}
        if (not snapshot.get("enumeration_complete") or not rows
                or len(rows) != len(tids)
                or len(rows) != snapshot.get("process_reported_thread_count")
                or tids != set(snapshot.get("discovered_toolhelp_tids") or [])):
            issues.append(f"{phase}: incomplete enumeration")
        for row in rows:
            for mode in ("nt_open_as_self", "nt_open_as_client"):
                if row.get(mode, {}).get("status") != "0xc000007c":
                    issues.append(f"{phase}: TID {row.get('tid')} {mode} not STATUS_NO_TOKEN")
            if not isinstance(row.get("win32_start_address"), str):
                issues.append(f"{phase}: TID {row.get('tid')} start address unknown")
    activity = threads.get("forced_pool_activity", {})
    callbacks = activity.get("callback_inspections", [])
    tids = {t.get("tid") for t in threads.get("post_pool_activity", {}).get("threads", [])}
    if activity.get('callbacks_held_during_snapshot') is not True:
        issues.append('callback liveness at snapshot not proved')
    if activity.get("ready_status") != 0 or {c.get("kind") for c in callbacks} != {"work", "wait", "timer", "legacy_work"}:
        issues.append("work/wait/timer/legacy callbacks did not all run")
    if activity.get('callbacks_drained') is not True:
        issues.append('callback drain not proved')
    if not callbacks or any(c.get("tid") not in tids for c in callbacks):
        issues.append("callback TID missing from enumeration")
    for callback in callbacks:
        if any(callback.get(mode, {}).get("status") != "0xc000007c" for mode in ("nt_open_as_self", "nt_open_as_client")):
            issues.append("callback token status unknown or token present")
    scheduler = measurement.get("scheduler_shared_data") or {}
    if scheduler.get("error") or "present" not in scheduler:
        issues.append("scheduler inventory unknown")
    if scheduler.get("present"):
        correlation = measurement.get("scheduler_correlation") or {}
        if not correlation.get("object_resolved"):
            issues.append("scheduler object address unknown")
        for name, operation in scheduler.get("probe", {}).get("operations", {}).get("duplicate_object", {}).items():
            if operation.get("status") == "0x00000000" and "granted_access" not in operation:
                issues.append(f"{name}: successful duplicate grant not queried")
    return issues


def render(measurement: dict[str, Any]) -> str:
    lines = [f"# Windows gap measurements: run {measurement['run_id']} / {measurement['image']}", "",
             f"Source `{measurement['source_sha']}`; recipe `{measurement['sequence']}`; PID {measurement['pid']}; exit `{measurement['exit_code']}`.", "",
             "## Gap 1: all live threads", "",
             "The broker enumerates with SystemProcessInformation and Toolhelp at worker-requested barriers. The confined worker's enumeration errors are retained separately. Callbacks remain alive during the second snapshot; these are not permanently suspended threads."]
    threads = measurement.get("threads") or {}
    symbols = measurement.get("thread_symbols") or {}
    for phase, symbol_key in (("pre_pool_activity", "pre_pool_threads_symbolized"), ("post_pool_activity", "post_pool_threads_symbolized"), ("post_release", "post_release_threads_symbolized")):
        snapshot = threads.get(phase, {})
        lines += ["", f"### {phase}", f"Complete: {snapshot.get('enumeration_complete')}; SPI NumberOfThreads: {snapshot.get('process_reported_thread_count')}; Toolhelp TIDs: {snapshot.get('discovered_toolhelp_tids')}", "",
                  "| TID | Start address | PDB symbol | NtOpenThreadTokenEx self=true | self=false | Token details |", "|---:|---|---|---|---|---|"]
        by_tid = {t['tid']: t for t in symbols.get(symbol_key, [])}
        for t in snapshot.get("threads", []):
            s = by_tid.get(t['tid'], t).get('win32_symbol') or {}
            module = s.get('module') or {}
            module_name = module.get('name', '') if isinstance(module, dict) else str(module)
            symbol_type = module.get('symbol_type') if isinstance(module, dict) else None
            symbol = f"{module_name}!{s.get('symbol', '?')}+{s.get('displacement', '?')} (type {symbol_type}; error {s.get('error')})"
            token = t.get('nt_open_as_self', {}).get('token') or t.get('nt_open_as_client', {}).get('token')
            lines.append(f"| {t['tid']} | `{t.get('win32_start_address')}` | `{symbol}` | `{t.get('nt_open_as_self', {}).get('status')}` | `{t.get('nt_open_as_client', {}).get('status')}` | {json.dumps(token) if token else 'absent only if STATUS_NO_TOKEN'} |")
    activity = threads.get('forced_pool_activity', {})
    lines += ["", f"Callback barrier status: `{activity.get('ready_status')}`; callback TIDs included: `{threads.get('callback_tids_in_enumerated_set')}`.", "",
              "| Callback | TID | self=true | self=false |", "|---|---:|---|---|"]
    for c in activity.get('callback_inspections', []):
        lines.append(f"| {c['kind']} | {c['tid']} | `{c['nt_open_as_self'].get('status')}` | `{c['nt_open_as_client'].get('status')}` |")
    lines += ["", "## Gap 2: SchedulerSharedData", "",
              f"Correlation snapshot: `{json.dumps(measurement.get('scheduler_correlation'), separators=(',', ':'))}`", "",
              "| Operation | Exact measured result (NTSTATUS unless explicitly Win32) |", "|---|---|"]
    scheduler = measurement.get('scheduler_shared_data') or {}
    if not scheduler.get('present'):
        lines.append(f"| inventory | `{json.dumps(scheduler)}` |")
    for name, result in scheduler.get('probe', {}).get('operations', {}).items():
        lines.append(f"| {name} | `{json.dumps(result, separators=(',', ':'))}` |")
    lines += ["", "Loader syscall observations: `" + json.dumps(measurement.get('loader_scheduler_calls'), separators=(',', ':')) + "`", "",
              "Scheduler-close trial: `" + json.dumps(measurement.get('scheduler_close'), separators=(',', ':')) + "`", "",
              "Coverage violations: " + json.dumps(violations(measurement)), "",
              "This validates coverage and token absence, not universal scheduler non-reachability. A successful duplicate is success; its grant and follow-up security operations must be assessed, not labelled denied."]
    return "\n".join(lines) + "\n"


def retain(source, destination, run_id, source_sha, image):
    report = json.loads(pathlib.Path(source).read_text(encoding='utf-8-sig'))
    variants = {d['result'].get('sequence'): d['result'] for r in report['runs'] for d in r.get('diagnostics', [])}
    trace = variants.get('full-gui-connect-trace', {}).get('debug', {}).get('trace') or {}
    close = variants.get('full-gui-close-scheduler', {})
    removable_close = variants.get('full-gui-close-removable-fault-trace', {})
    def crash_summary(variant):
        faults = [e for e in (variant.get('debug', {}).get('trace') or {}).get('events', []) if e.get('event', {}).get('fault_context')]
        return dict(pid=variant.get('pid'), exit_code=variant.get('exit_code'), stderr=variant.get('stderr'), faults=faults,
                    measurement_faults=variant.get('measurement_faults'))
    destination = pathlib.Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    failed = False
    for sequence in ('full-gui-close-removable', 'full-gui-close-combined', 'full-gui-inspect'):
        v = variants.get(sequence, {})
        child = v.get('child', {})
        start = v.get('measurement_start') or {}
        parent = v.get('parent_handle_inspection') or {}
        checkpoints = {c['stage']: c['observation'] for c in v.get('thread_checkpoints', [])}
        partial_threads = dict(pre_pool_activity=checkpoints.get('before_activity', {}), post_pool_activity=checkpoints.get('callbacks_held', {}), post_release=checkpoints.get('after_release', {}))
        compact = dict(schema=2, run_id=str(run_id), source_sha=source_sha, image=image,
                       sequence=sequence, pid=v.get('pid'), exit_code=v.get('exit_code'), stderr=v.get('stderr'),
                       completed_probe_count=len(child.get('probes', [])), successes=[p for p in child.get('probes', []) if p.get('success')],
                       primary_token=child.get('primary_token') or start.get('primary_token'), self_lowering=child.get('self_lowering') or start.get('self_lowering'),
                       threads=child.get('threads') or partial_threads, thread_symbols=parent.get('threads'),
                       scheduler_shared_data=child.get('scheduler_shared_data'), scheduler_correlation=parent.get('scheduler_shared_data'),
                       post_close_handles=child.get('handle_table') or start.get('handle_table'), after_measurement_handles=child.get('after_measurement_handles'),
                       ambient_close=child.get('ambient_close') or start.get('ambient_close'),
                       loader_scheduler_calls=[e for e in trace.get('connections', []) if e.get('api') == 'NtSetInformationProcess'],
                       scheduler_close=crash_summary(close), removable_close=crash_summary(removable_close),
                       measurement_faults=v.get('measurement_faults'),
                       console_attempts=[dict(sequence=a.get('sequence'), exit_code=a.get('exit_code'), error=a.get('error'), stderr=a.get('stderr')) for r in report['runs'] if r['mode'] == 'full' for a in r.get('attempts', [])],
                       profile_cleanup=report.get('profile_cleanup'))
        base = destination / f'{run_id}-{image}-{sequence}'
        base.with_suffix('.json').write_text(json.dumps(compact, indent=2) + '\n', encoding='utf-8')
        base.with_suffix('.md').write_text(render(compact), encoding='utf-8')
        problems = violations(compact)
        print(f'Windows gap coverage {sequence}: 3 snapshots, work/wait/timer/legacy, scheduler duplicate grants; {len(problems)} violations: {problems}')
        failed |= bool(problems)
    return failed


if __name__ == '__main__':
    sys.exit(retain(*sys.argv[1:]))
