#!/usr/bin/env python3
"""Exercise the actual Rust executables over loopback TCP and verified TLS."""
import json
import signal
import socket
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]
BIN=ROOT/'target/release/wstest'
CASES=['1.1.7','2.3','5.19','6.4.3','7.7.2','9.7.3','12.1.1','13.5.1']

def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1',0));return s.getsockname()[1]

def start(args, port, log):
    proc=subprocess.Popen([str(BIN),*args],cwd=ROOT,stdout=log,stderr=log)
    for _ in range(100):
        if proc.poll() is not None:raise RuntimeError('server exited before ready')
        try:
            with socket.create_connection(('127.0.0.1',port),timeout=.1):return proc
        except OSError:time.sleep(.05)
    proc.terminate();raise RuntimeError('server did not become ready')

def stop(proc):
    if proc.poll() is None:
        proc.send_signal(signal.SIGINT)
        try:proc.wait(timeout=10)
        except subprocess.TimeoutExpired:proc.kill();proc.wait()

def run(args, expected=0):
    p=subprocess.run([str(BIN),*args],cwd=ROOT,capture_output=True,text=True,timeout=90)
    assert p.returncode==expected,(args,p.stdout,p.stderr)
    return p

def check_report(directory,agent=None):
    report=json.loads((directory/'index.json').read_text())
    results=report[agent] if agent else next(iter(report.values()))
    assert set(results)==set(CASES),set(results)
    for case,result in results.items():
        assert result['behavior'] in ('OK','NON-STRICT'),(case,result)
        assert result['behaviorClose']=='OK',(case,result)
    assert (directory/'index.html').is_file()

with tempfile.TemporaryDirectory(prefix='autobahn-rust-cli-') as temp:
    temp=Path(temp)
    listing=json.loads(run(['--list-cases','--json']).stdout)
    assert len(listing)==517
    vectors=temp/'serializer.json';run(['-m','serializer','-o',str(vectors)])
    assert len(json.loads(vectors.read_text()))==40
    cert=temp/'cert.pem';key=temp/'key.pem'
    subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=localhost','-addext','subjectAltName=DNS:localhost,IP:127.0.0.1','-addext','basicConstraints=critical,CA:FALSE','-keyout',str(key),'-out',str(cert)],check=True,capture_output=True)
    for secure in (False,True):
        port=free_port();url=f'{"wss" if secure else "ws"}://127.0.0.1:{port}'
        with (temp/f'echo-{secure}.log').open('w') as log:
            args=['-m','testeeserver','-w',url]
            if secure:args+=['--cert',str(cert),'--key',str(key)]
            server=start(args,port,log)
            try:
                report=ROOT/'reports'/('cli-tls' if secure else 'cli-tcp')
                spec=temp/'client.json';spec.write_text(json.dumps(dict(url=url,cases=CASES,message_count=3,concurrency=4,outdir=str(report),ca=str(cert) if secure else None)))
                run(['-m','fuzzingclient','-s',str(spec)])
                check_report(report)
                if secure:
                    run(['-m','fuzzingclient','-w',url,'--cases','1.1.1','--outdir',str(temp/'untrusted')],expected=1)
                else:
                    run(['-m','massconnect','-w',url,'--connections','12','--concurrency','4','--hold-ms','10'])
                    legacy=temp/'mass.json';legacy.write_text(json.dumps(dict(options=dict(connections=4,batchsize=2,batchdelay=1,retrydelay=1),servers=[dict(name='legacy',uri=url,desc='echo')],hold_ms=10)))
                    run(['-m','massconnect','-s',str(legacy)])
            finally:stop(server)
        print(f'PASS {"TLS with certificate verification" if secure else "TCP"}: fuzzingclient + testeeserver')
    port=free_port();webport=free_port();url=f'ws://127.0.0.1:{port}';report=ROOT/'reports/cli-clients';spec=temp/'server.json'
    spec.write_text(json.dumps(dict(url=url,cases=CASES,message_count=3,outdir=str(report),webport=webport)))
    with (temp/'fuzzingserver.log').open('w') as log:
        server=start(['-m','fuzzingserver','-s',str(spec)],port,log)
        try:
            page=urllib.request.urlopen(f'http://127.0.0.1:{webport}/').read().decode()
            assert 'Test this browser' in page and url in page
            run(['-m','testeeclient','-w',url,'-i','CLI testee'])
            check_report(report,'CLI testee')
            assert b'CLI testee' in urllib.request.urlopen(f'http://127.0.0.1:{webport}/reports/index.html').read()
        finally:stop(server)
    print('PASS fuzzingserver + testeeclient + reports')
    print('PASS catalog, serializer, massconnect, and untrusted-TLS failure')
