"""Witness the token-handle fence against a real token handle on native Windows."""
import json
from pathlib import Path
import subprocess
import sys

TEST = 'native::tests::live_token_handle_is_visible_to_handle_inventory'
SOURCE = Path('src/native.rs')


def command(args):
    result = subprocess.run(args, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=180)
    print(result.stdout, end='')
    return result


def stat():
    result = subprocess.run(['git', 'diff', '--stat'], check=True, text=True, stdout=subprocess.PIPE)
    print(result.stdout or '(empty git diff --stat)')
    return result.stdout.strip()


if __name__ == '__main__':
    command(['rustc', '--version'])
    subprocess.run(['git', 'add', str(SOURCE)], check=True)
    before = stat()
    if before:
        sys.exit('refusing mutation: working changes are not safely staged')
    original = SOURCE.read_text(encoding='utf-8')
    start = original.index('pub fn token_handles_absent(')
    end = original.index('\npub fn object_probe', start)
    mutant = 'pub fn token_handles_absent(handles: &[Value]) -> bool {\n    let _ = handles;\n    true // NON-VACUITY BREAK: neutralize the token-handle fence\n}\n'
    red = None
    during = ''
    try:
        SOURCE.write_text(original[:start] + mutant + original[end:], encoding='utf-8')
        during = stat()
        if not during:
            raise RuntimeError('mutant did not change the checked source')
        red = command(['cargo', 'test', '--locked', TEST, '--', '--exact', '--nocapture'])
    finally:
        subprocess.run(['git', 'checkout', '--', str(SOURCE)], check=True)
        SOURCE.touch()
        after = stat()
        if after:
            raise RuntimeError('mutation restoration left a working diff')
    named_red = f'test {TEST} ... FAILED' in red.stdout or f'{TEST} ... FAILED' in red.stdout
    expected_only = red.returncode != 0 and named_red and '1 failed' in red.stdout
    output = '\n'.join(line for line in red.stdout.splitlines() if TEST in line or 'assertion failed' in line or 'test result:' in line)
    evidence = [{
        'control': 'neutralize token_handles_absent while the test holds a live TOKEN_QUERY handle',
        'expected_red': TEST,
        'captured_output': output[:400],
        'applied_evidence': f'spikes/win-confine/src/native.rs: before empty; during {during}; after empty',
        'outcome': 'reddened' if expected_only else 'undefended',
    }]
    Path('evidence').mkdir(exist_ok=True)
    Path('evidence/mutation-evidence.json').write_text(json.dumps(evidence, indent=2), encoding='utf-8')
    if not expected_only:
        sys.exit('token-handle witness did not fail by the named assertion')
    restored = command(['cargo', 'test', '--locked'])
    if restored.returncode:
        sys.exit(restored.returncode)
    print('token-handle fence: one targeted test reddened; restored native suite passed')
