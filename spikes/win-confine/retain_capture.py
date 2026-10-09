"""Keep PID-linked capture evidence and content hashes without duplicate probe inputs."""
import hashlib
import json
import sys
from pathlib import Path
from procmon_summary import RECIPES, FOLLOWUPS, attempts, safe_row


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def retain(source, destination, run_id, source_sha, image):
    destination.mkdir(parents=True, exist_ok=True)
    files = {str(p.relative_to(source)): sha256(p) for p in sorted(source.rglob('*')) if p.is_file()}
    report = json.loads((source / 'report.json').read_text(encoding='utf-8'))
    selected = [a for a in attempts(report) if a.get('sequence') in RECIPES + FOLLOWUPS]
    kept = {'run_id': run_id, 'compiled_source_sha': source_sha, 'image': image,
            'parent_token': report['parent_token'], 'appcontainer_sid': report['appcontainer_sid'],
            'profile_cleanup': report['profile_cleanup'], 'selected_attempts': selected,
            'controls': [r for r in report['runs'] if r['mode'] == 'lpac']}
    path = destination / f'{run_id}-{image}-births.json'
    path.write_text(json.dumps(kept, separators=(',', ':')), encoding='utf-8')
    retained = [path]
    for name in ['procmon-metadata.json', 'procmon-events.json', 'procmon-child-rows.json',
                 'procmon-events.md', 'denied-object-acls.json', 'mutation-evidence.json',
                 'targeted-change.json', 'loader-snaps-cleanup.txt']:
        original = source / name
        if original.exists():
            path = destination / f'{run_id}-{image}-{name}'
            if name == 'procmon-child-rows.json':
                rows = json.loads(original.read_text())
                path.write_text(json.dumps([safe_row(row) for row in rows], indent=2), encoding='utf-8')
            else:
                path.write_bytes(original.read_bytes())
            retained.append(path)
    for name in ['imports.json', 'imports-gui.json', 'kernel-trace-setup.txt',
                 'kernel-trace-export.txt', 'kernel-providers.txt']:
        original = source / name
        if original.exists():
            path = destination / f'{run_id}-{image}-{name}'
            path.write_bytes(original.read_bytes())
            retained.append(path)
    manifest_path = destination / 'manifest.json'
    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {'runs': []}
    manifest['runs'] = [r for r in manifest['runs'] if (r['run_id'], r['image']) != (run_id, image)]
    manifest['runs'].append({'run_id': run_id, 'source_sha': source_sha, 'image': image,
                             'artifact_url': f'https://github.com/cortexkit/basal/actions/runs/{run_id}',
                             'artifact_file_sha256': files,
                             'retained_file_sha256': {p.name: sha256(p) for p in retained},
                             'omitted': 'Runner-wide PML/ZIP/executable and unrelated control reports remain in Actions artifacts; births retains requested recipes and trace-driven followups only. Successful Process Start details omit environment values.'})
    manifest_path.write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    print(f'Retained {len(retained)} files for {image} run {run_id}; hashed {len(files)} artifact files')


if __name__ == '__main__':
    source, destination, run_id, source_sha, image = sys.argv[1:]
    retain(Path(source), Path(destination), int(run_id), source_sha, image)
