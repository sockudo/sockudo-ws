#!/usr/bin/env python3
"""Sequential, alternating full-workload trials with matching x86-64 fuzzers.

Both runners use the same persistent ARM64 Rust echo server. Container startup
and report generation are excluded from per-case durations. Trial zero warms the
environment; each measured trial starts a fresh runner (including a fresh PyPy
JIT). These are Docker Desktop/emulation results, not bare-metal throughput.
"""
import json
import statistics
import subprocess
import time
import uuid
from pathlib import Path
from differential import ROOT, PYTHON_IMAGE, docker

CASES = ['9.2.6', '9.7.1', '9.7.3', '9.7.5', '12.1.3', '12.1.9']


def main():
    network = 'autobahn-perf-' + uuid.uuid4().hex[:8]
    directory = ROOT / 'reports/performance'
    directory.mkdir(parents=True, exist_ok=True)
    server = network + '-echo'
    images = {'python': PYTHON_IMAGE, 'rust': 'autobahn-rust-verify:amd64'}
    evidence = {'cases': CASES, 'trials': [], 'images': {
        kind: json.loads(docker('image', 'inspect', image))[0]['Id']
        for kind, image in images.items()}}
    assert all(json.loads(docker('image', 'inspect', i))[0]['Architecture'] == 'amd64'
               for i in images.values()), 'runner architectures must match'
    docker('network', 'create', network)
    try:
        docker('run', '-d', '--name', server, '--network', network,
               'autobahn-rust-verify:local', '-m', 'testeeserver', '-w', 'ws://0.0.0.0:9001')
        time.sleep(1)
        for trial in range(4):
            for kind in (['python', 'rust'] if trial % 2 == 0 else ['rust', 'python']):
                label = f'{kind}-{trial}'
                folder = directory / label
                folder.mkdir(exist_ok=True)
                spec = {'outdir': f'/reports/{label}', 'cases': CASES,
                        'exclude-cases': [], 'exclude-agent-cases': {},
                        'servers': [{'url': f'ws://{server}:9001', 'agent': 'common-echo'}]}
                (folder / 'spec.json').write_text(json.dumps(spec))
                started = time.monotonic()
                with (folder / 'run.log').open('w') as log:
                    subprocess.run(['docker', 'run', '--rm', '--platform', 'linux/amd64',
                        '--network', network, '--cpus', '1',
                        '-e', 'TOKIO_WORKER_THREADS=1',
                        '-e', 'PYTHONPATH=/reference/autobahntestsuite',
                        '-v', f'{ROOT}/upstream:/reference:ro', '-v', f'{directory}:/reports',
                        '--entrypoint', 'wstest', images[kind], '-m', 'fuzzingclient',
                        '-s', f'/reports/{label}/spec.json'], stdout=log, stderr=log,
                        timeout=600, check=True)
                results = next(iter(json.loads((folder / 'index.json').read_text()).values()))
                assert set(results) == set(CASES)
                assert all(r['behavior'] == r['behaviorClose'] == 'OK' for r in results.values()), results
                for case in CASES:
                    detail = json.loads((folder / results[case]['reportfile']).read_text())
                    expected_count = 1 if case == '9.2.6' else 1000
                    if kind == 'rust':
                        assert detail['messages'] == expected_count and not detail['reduced']
                    else:
                        opcode = '2' if case == '9.2.6' else '1'
                        assert detail['rxFrameStats'][opcode] == expected_count
                record = {'runner': kind, 'trial': trial, 'warmup': trial == 0,
                          'process_wall_seconds': time.monotonic() - started,
                          'duration_ms': {c: results[c]['duration'] for c in CASES}}
                evidence['trials'].append(record)
                print(json.dumps(record), flush=True)
        evidence['median_ms'] = {kind: {c: statistics.median(
            r['duration_ms'][c] for r in evidence['trials']
            if r['runner'] == kind and not r['warmup']) for c in CASES} for kind in images}
        evidence['python_over_rust_ratio'] = {c: evidence['median_ms']['python'][c] /
            evidence['median_ms']['rust'][c] for c in CASES}
        evidence['environment'] = {
            'runner_platform': 'linux/amd64 under Docker Desktop emulation on Apple arm64',
            'cpu_quota_per_runner': 1, 'rust_workers': 1, 'case_concurrency': 1,
            'testee': 'same persistent linux/arm64 Rust testeeserver over Docker bridge TCP',
            'limits': 'Three measured fresh-process trials; includes PyPy JIT warmup per process. '
                      'Not a native-hardware or steady-state microbenchmark. '
                      'Original diagnostic logging defaults retained; Rust uses bounded summaries '
                      'and stricter byte-for-byte echo checks.'}
        (ROOT / 'docs/performance-comparison.json').write_text(json.dumps(evidence, indent=2) + '\n')
    finally:
        subprocess.run(['docker', 'rm', '-f', server], capture_output=True)
        docker('network', 'rm', network)


if __name__ == '__main__':
    main()
