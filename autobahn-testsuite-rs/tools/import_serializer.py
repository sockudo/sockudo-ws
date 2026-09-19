#!/usr/bin/env python3
"""Export actual marshalled WAMP fixtures from the MIT-licensed Autobahn 0.10.9 definitions.
The reference files retain the original MIT license. No network or third-party
Python packages are needed. Only the classes and fixture generator are executed.
"""
import ast
import json
import re
import types
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
class Subscriber:
    ROLE = 'subscriber'
    def __repr__(self): return 'RoleSubscriberFeatures()'
role = types.SimpleNamespace(RoleSubscriberFeatures=Subscriber, ROLE_NAME_TO_CLASS={'subscriber': Subscriber})
env = dict(re=re, six=types.SimpleNamespace(text_type=str,integer_types=(int,),u=lambda x:x),
           util=types.SimpleNamespace(EqualityMixin=object), ProtocolError=ValueError,
           IMessage=types.SimpleNamespace(register=lambda _:None), ROLE_NAME_TO_CLASS=role.ROLE_NAME_TO_CLASS,
           autobahn=types.SimpleNamespace(wamp=types.SimpleNamespace(role=role)))
module=ast.parse((ROOT/'tools/reference/message.py').read_text())
module.body=[n for n in module.body if not isinstance(n,(ast.Import,ast.ImportFrom))]
exec(compile(module,'message.py','exec'),env)
fixture=ast.parse((ROOT/'tools/reference/test_serializer.py').read_text())
fixture.body=[n for n in fixture.body if isinstance(n,ast.FunctionDef) and n.name=='generate_test_messages']
globals_={'message':types.SimpleNamespace(**env),'role':role}
exec(compile(fixture,'test_serializer.py','exec'),globals_)
values=[dict(name=str(m),rmsg=m.marshal()) for m in globals_['generate_test_messages']()]
assert len(values)==40
(ROOT/'catalog/serializer.json').write_text(json.dumps(values,ensure_ascii=False,indent=2)+'\n')
print('Imported 40 serializer cases')
