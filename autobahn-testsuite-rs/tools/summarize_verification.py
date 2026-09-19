#!/usr/bin/env python3
"""Validate completed reference reports and preserve a compact evidence record."""
import collections
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPORTS = ROOT / 'reports/differential'
FIELDS = ('behavior', 'behaviorClose', 'remoteCloseCode')


def read(folder):
    data = json.loads((folder / 'index.json').read_text())
    assert len(data) == 1
    return next(iter(data.values()))


def differences(left, right):
    assert left.keys() == right.keys(), 'catalog mismatch'
    return {case: {key: [left[case].get(key), right[case].get(key)]
                  for key in FIELDS if left[case].get(key) != right[case].get(key)}
            for case in left if any(left[case].get(key) != right[case].get(key) for key in FIELDS)}


def main():
    expected = {c['id'] for c in json.loads((ROOT / 'catalog/cases.json').read_text())}
    labels = ['python-server', 'python-client', 'rust-server', 'rust-client',
              'python-server-rust-testee', 'python-client-rust-testee']
    runs = {label: read(REPORTS / label) for label in labels}
    focused = {role: read(REPORTS / f'rust-{role}-fragment-fix') for role in ('server', 'client')}
    for label, results in runs.items():
        assert set(results) == expected, label
        assert all(r['behavior'] in ('OK', 'INFORMATIONAL')
                   and r['behaviorClose'] in ('OK', 'INFORMATIONAL') for r in results.values()), label
    # Recheck affected cases after the final exact-multiple fragmentation fix.
    for role, results in focused.items():
        assert all(case in expected for case in results)
        runs[f'rust-{role}'].update(results)
    diffs = {role: differences(runs[f'python-{role}'], runs[f'rust-{role}'])
             for role in ('server', 'client')}
    assert not any(diffs.values()), diffs
    frame_diffs = {}
    for role in ('server', 'client'):
        frame_diffs[role] = {}
        for case in sorted(expected, key=lambda value: tuple(map(int, value.split('.')))):
            original = json.loads((REPORTS / f'python-{role}' /
                                   runs[f'python-{role}'][case]['reportfile']).read_text())
            native_folder = f'rust-{role}-fragment-fix' if case in focused[role] else f'rust-{role}'
            native = json.loads((REPORTS / native_folder /
                                 runs[f'rust-{role}'][case]['reportfile']).read_text())
            diff = {direction: [sum(original[f'{direction}FrameStats'].values()),
                                native[f'{direction}Frames']]
                    for direction in ('tx', 'rx')
                    if sum(original[f'{direction}FrameStats'].values()) != native[f'{direction}Frames']}
            if diff:
                assert case.startswith(('12.', '13.')) or case in ('6.4.3', '6.4.4'), (case, diff)
                frame_diffs[role][case] = diff
    browser_reference = read(REPORTS / 'browser-python')
    browser_fixed = read(ROOT / 'reports/browser-fixed')
    browser_diff = differences(browser_reference, browser_fixed)
    # This case intentionally accepts either outcome; TCP coalescing can change
    # whether the preceding ping is handled before the invalid opcode arrives.
    assert all(case == '4.2.5' and set(diff) == {'behavior'}
               and set(diff['behavior']) == {'OK', 'NON-STRICT'}
               for case, diff in browser_diff.items()), browser_diff
    evidence = {
        'upstream_commit': 'b8a5120d905e30470e4475785c48e4cedc35f6cd',
        'reference_image': 'crossbario/autobahn-testsuite@sha256:519915fb568b04c9383f70a1c405ae3ff44ab9e35835b085239c258b6fac3074',
        'reference_runtime': {'python': '2.7.18', 'pypy': '7.3.20', 'autobahn': '0.10.9', 'twisted': '19.10.0'},
        'runs': {label: {'cases': len(results),
                         'behavior': dict(collections.Counter(r['behavior'] for r in results.values())),
                         'behaviorClose': dict(collections.Counter(r['behaviorClose'] for r in results.values()))}
                 for label, results in runs.items()},
        'verdict_and_remote_close_code_differences': diffs,
        'focused_cases_rechecked_after_fragment_fix': {role: sorted(results) for role, results in focused.items()},
        'frame_count_differences': frame_diffs,
        'frame_count_note': 'Different zlib implementations can emit different compressed lengths '
                            'at the same level; fragmentation follows compression. For 6.4.3/6.4.4 '
                            'Rust counts the deliberately incomplete frame header in its diagnostics.',
        'browser': {'agent': next(iter(json.loads((ROOT / 'reports/browser/index.json').read_text()))),
                    'full_cases_executed': len(read(ROOT / 'reports/browser')),
                    'focused_reference_comparison_cases': len(browser_fixed),
                    'remaining_allowed_alternative_difference': browser_diff,
                    'fixes': ['retain invalid remote close codes', 'preserve informational verdicts']},
        'serializer': json.loads((REPORTS / 'serializer-comparison.json').read_text()),
        'source_sha256': {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest()
                          for p in sorted((ROOT / 'src').glob('*.rs'))},
        'scope': 'Observed verdict parity on the pinned Python peer; not a proof of every possible peer or wire encoding.'}
    (ROOT / 'docs/differential.json').write_text(json.dumps(evidence, indent=2) + '\n')
    print('Verified six complete 517-case runs, both-role verdict parity, and focused browser parity.')


if __name__ == '__main__':
    main()
