"""Run under the reference PyPy2 with msgpack==0.6.2 installed.

The upstream CLI crashes formatting a Unicode fixture's display name. Compare
the actual generated message objects and serializers without calling str(msg).
"""
from __future__ import print_function
import binascii
import json
import msgpack
from autobahn.wamp.serializer import JsonSerializer, MsgPackSerializer
from autobahn.wamp.test.test_serializer import generate_test_messages

with open('/reports/rust-serializer.json') as source:
    rust = json.load(source)
messages = list(generate_test_messages())
assert len(messages) == len(rust) == 40
json_serializer, msgpack_serializer = JsonSerializer(), MsgPackSerializer()
same_json = same_msgpack = 0
for message, vector in zip(messages, rust):
    reference_json = json_serializer.serialize(message)[0]
    reference_msgpack = msgpack_serializer.serialize(message)[0]
    assert message.marshal() == vector['rmsg']
    assert json.loads(reference_json) == json.loads(vector['json'])
    assert msgpack.unpackb(reference_msgpack, raw=False) == msgpack.unpackb(
        binascii.unhexlify(vector['msgpack']), raw=False)
    same_json += reference_json == vector['json'].encode('utf8')
    same_msgpack += binascii.hexlify(reference_msgpack) == vector['msgpack']
evidence = {'fixtures': 40, 'raw_messages_equal': 40, 'json_semantically_equal': 40,
            'msgpack_semantically_equal': 40, 'json_byte_identical': same_json,
            'msgpack_byte_identical': same_msgpack,
            'reference': 'Autobahn 0.10.9 actual message objects, msgpack 0.6.2',
            'note': 'Original CLI display-name formatting crashes on Unicode; '
                    'this check directly calls the original serializers.'}
with open('/reports/serializer-comparison.json', 'w') as output:
    json.dump(evidence, output, indent=2)
print(json.dumps(evidence))
