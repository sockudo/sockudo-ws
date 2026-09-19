#!/usr/bin/env python3
"""Development-only importer for the pinned Apache-2.0 upstream corpus.

Executes inspected case definitions with a recording transport, never network I/O.
The Rust binary consumes only the checked-in JSON and test payloads.
"""
import ast
import binascii
import hashlib
import json
import re
import shutil
import subprocess
import types
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = ROOT / 'upstream'
PACKAGE = UPSTREAM / 'autobahntestsuite/autobahntestsuite'
SOURCE = PACKAGE / 'case'
PIN = 'b8a5120d905e30470e4475785c48e4cedc35f6cd'
assert subprocess.check_output(['git', '-C', str(UPSTREAM), 'rev-parse', 'HEAD'], text=True).strip() == PIN


def octets(value):
    return value if isinstance(value, bytes) else value.encode('latin1')


def payload(value, length=None):
    data = octets(value)
    n = len(data) if length is None else length
    # Keep repeated multi-megabyte test inputs compact, without changing bytes.
    for width in range(1, min(len(data), 64) + 1):
        if data == (data[:width] * ((len(data) + width - 1) // width))[:len(data)]:
            data = data[:width]
            break
    return {'hex': data.hex(), 'len': n}


class Validator:
    def reset(self): pass
    def validate(self, value):
        try:
            octets(value).decode('utf-8')
            return True, True
        except UnicodeDecodeError:
            return False, False


class Protocol:
    CLOSE_STATUS_CODE_NORMAL = 1000
    CLOSE_STATUS_CODE_PROTOCOL_ERROR = 1002
    CLOSE_STATUS_CODE_INVALID_PAYLOAD = 1007
    STATE_OPEN = 1
    MESSAGE_TYPE_TEXT = 1
    def __init__(self):
        self.state = 1
        self.factory = types.SimpleNamespace(isServer=True)
        self.autoFragmentSize = 0
        self.actions = []
        self.timers = []
        self.time = 0
        self.deadline = 1
    def emit(self, kind, **kw): self.actions.append({'at': int(self.time * 1000), 'kind': kind, **kw})
    def sendFrame(self, opcode, payload='', fin=True, rsv=0, payload_len=None, chopsize=None, sync=False):
        self.emit('frame', opcode=opcode, payload=globals()['payload'](payload, payload_len), fin=fin, rsv=rsv, chop=chopsize or 0, sync=sync)
    def sendMessage(self, payload, isBinary=False, fragmentSize=None):
        self.emit('message', opcode=2 if isBinary else 1, payload=globals()['payload'](payload), fragment=fragmentSize or self.autoFragmentSize)
    def sendClose(self, code=None, reason=None):
        self.sendCloseFrame(code, reason or '')
    def sendCloseFrame(self, code=None, reasonUtf8=''):
        data = (b'' if code is None else code.to_bytes(2, 'big')) + octets(reasonUtf8 or '')
        self.emit('close', payload=payload(data))
    def killAfter(self, delay):
        self.deadline = max(self.deadline, self.time + delay)
        self.actions.append({'at': int((self.time + delay)*1000), 'kind': 'kill'})
    def closeAfter(self, delay):
        self.deadline = max(self.deadline, self.time + delay + 1)
        self.actions.append({'at': int((self.time + delay)*1000), 'kind': 'close', 'payload': payload('')})
    def continueLater(self, delay, fn, tag=None): self.timers.append((self.time + delay, fn))
    def beginMessage(self, opcode=1): self.opcode = opcode
    def beginMessageFrame(self, length): self.emit('header', opcode=self.opcode, length=length, fin=False)
    def sendMessageFrameData(self, data): self.emit('data', payload=payload(data))
    def endMessage(self): self.sendFrame(0)
    def enableWirelog(self, enabled): pass


class Case:
    OK = 'OK'
    FAILED = 'FAILED'
    NON_STRICT = 'NON-STRICT'
    def __init__(self, protocol):
        self.p = protocol
        self.received = []
        self.expected = {}
        self.expectedClose = {}
        self.suppressClose = False
        self.init()
    def init(self): pass


class Offer:
    def __init__(self, requestNoContextTakeover=False, requestMaxWindowBits=0, **kw):
        self.requestNoContextTakeover = requestNoContextTakeover
        self.requestMaxWindowBits = requestMaxWindowBits


modules = {}
def load(name):
    if name in modules: return modules[name]
    text = (SOURCE / (name + '.py')).read_text()
    # Preserve Python 2 byte-string source encoding before Python 3 parses it.
    text = ''.join(c if ord(c) < 128 else ''.join('\\x%02x' % b for b in c.encode('utf8')) for c in text)
    if name == 'case6_x_x':
        text = text[:text.index('def test_utf8(')] + text[text.index('Case6_X_X = []'):text.index('\nimport array')]
    text = re.sub(r'\(object, Case, \)', '(Case,)', text)
    env = dict(Case=Case, WebSocketProtocol=Protocol, xrange=range, Utf8Validator=Validator,
               binascii=types.SimpleNamespace(b2a_hex=lambda x: octets(x).hex()),
               PerMessageDeflateOffer=Offer, os=__import__('os'),
               pkg_resources=types.SimpleNamespace(resource_filename=lambda _, path: str(PACKAGE / path)))
    lines = []
    for line in text.splitlines():
        if line.startswith('from case'):
            dep = line.split()[1]
            if dep != 'case': env.update(load(dep))
        elif line.startswith('from ') or line.startswith('import '): pass
        else: lines.append(line)
    exec(compile('\n'.join(lines), name, 'exec'), env)
    modules[name] = env
    return env


index = (SOURCE / '__init__.py').read_text()
env = {}
for name in re.findall(r'^from (case\S+) import', index, re.M):
    if name != 'case9_9_1': env.update(load(name))
start = index.index('Cases = []')
env['CaseSubCategories'] = {}
exec(index[start:], env)


def event(e):
    return {'kind': e[0], 'payload': payload(e[1]), 'binary': e[2] if len(e)>2 else False}


result = []
for cls in env['Cases']:
    ident = cls.__name__[4:].replace('_', '.')
    def readable(value):
        try: return value.encode('latin1').decode('utf8')
        except (UnicodeEncodeError, UnicodeDecodeError): return value
    item = dict(id=ident, description=readable(cls.DESCRIPTION), expectation=readable(cls.EXPECTATION))
    if ident.startswith(('12.', '13.')):
        item.update(engine='compression', length=cls.LEN, count=cls.COUNT, timeout_ms=cls.WAITSECS*1000,
                    fragment=cls.AUTOFRAGSIZE, file=cls.TESTDATA['file'], binary=cls.TESTDATA['binary'],
                    parameter=int(ident.split('.')[1]) if ident.startswith('13.') else 1)
    elif ident.startswith('9.'):
        c = cls(Protocol())
        is_rtt = hasattr(c, 'COUNT')
        group = int(ident.split('.')[1])
        data = ('\xfe' if c.BINARY else '*') if is_rtt else c.PAYLOAD
        length = c.LEN if is_rtt else c.DATALEN
        item.update(engine='performance', payload=payload(data, length), count=c.COUNT if is_rtt else 1,
                    timeout_ms=c.WAITSECS*1000, binary=(group%2==0), fragment=getattr(c,'FRAGSIZE',0), chop=getattr(c,'chopsize',0))
    else:
        p = Protocol(); c = cls(p); c.onOpen()
        while p.timers:
            p.timers.sort(key=lambda t:t[0]); p.time, fn = p.timers.pop(0)
            before = len(c.received); mark_at = len(p.actions); fn()
            for e in reversed(c.received[before:]):
                p.actions.insert(mark_at, dict(at=int(p.time*1000),kind='mark',event=event(e)))
        item.update(engine='script', actions=sorted(p.actions,key=lambda a:a['at']),
                    expected={k:[event(e) for e in v] for k,v in c.expected.items()},
                    close=c.expectedClose, suppress_close=c.suppressClose, timeout_ms=int((p.deadline+2)*1000),
                    informational=ident in ('7.1.6','7.13.1','7.13.2'), wrong_code_fatal=ident.startswith('7.'))
    result.append(item)
assert len(result) == 517, len(result)
(ROOT/'catalog/cases.json').write_text(json.dumps(result, ensure_ascii=True, indent=2)+'\n')
(ROOT/'catalog/provenance.json').write_text(json.dumps(dict(repository='https://github.com/crossbario/autobahn-testsuite',commit=PIN,cases=len(result),source_sha256={str(p.relative_to(UPSTREAM)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(SOURCE.glob('*.py'))}),indent=2)+'\n')
(ROOT/'catalog/testdata').mkdir(exist_ok=True)
for f in sorted({x['file'] for x in result if 'file' in x}): shutil.copy2(PACKAGE/'testdata'/f, ROOT/'catalog/testdata'/f)
shutil.copy2(UPSTREAM/'LICENSE', ROOT/'LICENSE')
print(f'Imported {len(result)} cases from {PIN}')
