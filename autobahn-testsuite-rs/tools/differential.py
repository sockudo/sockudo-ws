#!/usr/bin/env python3
"""Run both fuzzers against the pinned Python testee, in both roles, in Docker.

The original package is mounted read-only over the frozen image's dependencies.
No case counts, delays, or workloads are reduced. Requires Docker and both images.
"""
import argparse
import collections
import concurrent.futures
import fnmatch
import json
import subprocess
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
# Frozen 25.10.1 image. Source itself comes from the pinned checkout below.
PYTHON_IMAGE = 'crossbario/autobahn-testsuite@sha256:519915fb568b04c9383f70a1c405ae3ff44ab9e35835b085239c258b6fac3074'
RUST_IMAGE = 'autobahn-rust-verify:local'


def docker(*args, **kwargs):
    return subprocess.run(['docker', *args], text=True, capture_output=True,
                          check=True, **kwargs).stdout.strip()


def base(name, kind, network):
    return ['run', '--name', name, '--network', network,
            '-v', f'{ROOT}/upstream:/reference:ro',
            '-v', f'{ROOT}/reports/differential:/reports',
            '-e', 'PYTHONPATH=/reference/autobahntestsuite',
            '-e', 'PYTHONUNBUFFERED=1',
            '--entrypoint', 'wstest',
            PYTHON_IMAGE if kind == 'python' else RUST_IMAGE]


def run_pair(kind, role, network, cases, suffix='', testee='python'):
    label = f'{kind}-{role}{suffix}'
    server = f'{network}-{label}-server'
    client = f'{network}-{label}-client'
    directory = ROOT / 'reports/differential' / label
    directory.mkdir(parents=True, exist_ok=True)
    (directory / 'index.json').unlink(missing_ok=True)
    server_kind = testee if role == 'server' else kind
    client_kind = kind if role == 'server' else testee
    url = f'ws://{server}:9001'
    spec = {'url': 'ws://0.0.0.0:9001', 'outdir': f'/reports/{label}',
            'cases': cases, 'exclude-cases': [], 'exclude-agent-cases': {}}
    if role == 'server':
        spec['servers'] = [{'url': url, 'agent': 'reference'}]
    (directory / 'spec.json').write_text(json.dumps(spec))
    args = (['-m', 'testeeserver', '-w', 'ws://0.0.0.0:9001', '-u', '0']
            if role == 'server' else
            ['-m', 'fuzzingserver', '-s', f'/reports/{label}/spec.json', '-u', '0'])
    command = base(server, server_kind, network)
    command.insert(1, '-d')
    try:
        docker(*command, *args)
        # Probe inside the same Docker network; don't time container startup.
        for _ in range(60):
            state = json.loads(docker('inspect', server))[0]['State']
            if not state['Running']:
                raise RuntimeError(docker('logs', server))
            probe = subprocess.run(['docker', 'run', '--rm', '--network', network,
                '--entrypoint', 'pypy', PYTHON_IMAGE, '-c',
                f'import socket; socket.create_connection(("{server}",9001),1).close()'],
                capture_output=True)
            if probe.returncode == 0:
                break
            time.sleep(.25)
        else:
            raise RuntimeError('server never became ready')
        args = (['-m', 'fuzzingclient', '-s', f'/reports/{label}/spec.json']
                if role == 'server' else
                ['-m', 'testeeclient', '-w', url, '-i', 'reference'])
        started = time.monotonic()
        with (directory / 'client.log').open('w') as log:
            result = subprocess.run(['docker', *base(client, client_kind, network), *args],
                                    stdout=log, stderr=log, timeout=1800)
        elapsed = time.monotonic() - started
        (directory / 'client-state.json').write_text(docker('inspect', client) + '\n')
        if not (directory / 'index.json').is_file():
            raise RuntimeError(f'{label}: missing report (exit {result.returncode}), see client.log and client-state.json')
        report = json.loads((directory / 'index.json').read_text())
        assert len(report) == 1, report.keys()
        results = next(iter(report.values()))
        catalog = json.loads((ROOT / 'catalog/cases.json').read_text())
        expected_ids = {case['id'] for case in catalog
                        if any(fnmatch.fnmatchcase(case['id'], pattern) for pattern in cases)}
        assert set(results) == expected_ids, f'{label}: incomplete or unexpected case catalog'
        summary = {'runner': kind, 'testee': testee, 'testee_role': role, 'wall_seconds': elapsed,
                   'exit_code': result.returncode, 'cases': len(results),
                   'behavior': dict(collections.Counter(r['behavior'] for r in results.values())),
                   'behaviorClose': dict(collections.Counter(r['behaviorClose'] for r in results.values()))}
        (directory / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
        print(json.dumps(summary), flush=True)
        return label, results, summary
    finally:
        subprocess.run(['docker', 'stop', '-t', '2', server], capture_output=True)
        logs = subprocess.run(['docker', 'logs', server], text=True, capture_output=True)
        (directory / 'server.log').write_text(logs.stdout + logs.stderr)
        subprocess.run(['docker', 'rm', '-f', server, client], capture_output=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cases', nargs='+', default=['*'])
    parser.add_argument('--runner', choices=['python', 'rust', 'both'], default='both')
    parser.add_argument('--role', choices=['server', 'client', 'both'], default='both')
    parser.add_argument('--testee', choices=['python', 'rust'], default='python')
    parser.add_argument('--jobs', type=int, default=1,
                        help='Concurrent pairs; keep 1 on memory-constrained Docker VMs')
    parser.add_argument('--suffix', default='', help='Separate report label for focused regressions')
    args = parser.parse_args()
    network = 'autobahn-verify-' + uuid.uuid4().hex[:8]
    (ROOT / 'reports/differential').mkdir(parents=True, exist_ok=True)
    docker('network', 'create', network)
    try:
        kinds = ['python', 'rust'] if args.runner == 'both' else [args.runner]
        roles = ['server', 'client'] if args.role == 'both' else [args.role]
        suffix = ('' if args.testee == 'python' else '-rust-testee') + args.suffix
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            futures = [pool.submit(run_pair, kind, role, network, args.cases, suffix, args.testee)
                       for kind in kinds for role in roles]
            runs = [f.result() for f in concurrent.futures.as_completed(futures)]
        by_label = {label: results for label, results, _ in runs}
        differences = {}
        for role in roles:
            if args.runner != 'both':
                continue
            python, rust = by_label[f'python-{role}{suffix}'], by_label[f'rust-{role}{suffix}']
            assert python.keys() == rust.keys(), 'case catalog mismatch'
            differences[role] = {case: {key: [python[case].get(key), rust[case].get(key)]
                for key in ('behavior', 'behaviorClose', 'remoteCloseCode')
                if python[case].get(key) != rust[case].get(key)} for case in python}
            differences[role] = {case: diff for case, diff in differences[role].items() if diff}
        evidence = {'cases_requested': args.cases, 'runs': [s for _, _, s in runs],
                    'differences_python_then_rust': differences,
                    'note': 'Correctness runs; wall times are not benchmark results.'}
        (ROOT / f'reports/differential/comparison{suffix}.json').write_text(json.dumps(evidence, indent=2) + '\n')
        print(json.dumps(differences, indent=2))
        failed = any(r['behavior'] not in ('OK', 'NON-STRICT', 'INFORMATIONAL')
                     or r['behaviorClose'] not in ('OK', 'INFORMATIONAL')
                     for _, results, _ in runs for r in results.values())
        if any(differences.values()) or failed:
            raise SystemExit(1)
    finally:
        docker('network', 'rm', network)


if __name__ == '__main__':
    main()
