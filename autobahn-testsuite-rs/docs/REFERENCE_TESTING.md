# Reproduce reference verification

These checks require Docker Desktop (or Docker Engine), Python 3 for orchestration,
and the exact upstream checkout. Python is not a runtime dependency of the Rust
binary. Run commands from the crate root.

```sh
git clone https://github.com/crossbario/autobahn-testsuite upstream
git -C upstream checkout b8a5120d905e30470e4475785c48e4cedc35f6cd
docker pull crossbario/autobahn-testsuite@sha256:519915fb568b04c9383f70a1c405ae3ff44ab9e35835b085239c258b6fac3074
docker build -t autobahn-rust-verify:local .
docker build --platform linux/amd64 -t autobahn-rust-verify:amd64 .
python3 tools/differential.py
python3 tools/differential.py --runner python --testee rust
python3 tools/differential.py --runner rust --suffix=-fragment-fix --cases '6.2.*' '6.3.2' '6.4.*' '9.3.*' '9.4.*' '12.1.11' '12.1.16' '13.5.11'
```

The first command pair of runs compares Python and Rust fuzzers against the same
Python implementation, in both testee roles. The second checks Rust's testee
against the original fuzzer. Each complete run executes 517 cases with the
original message counts and deadlines. No speed conclusions should be drawn
from these functional runs. The harness defaults to one pair at a time because
the original fuzzer retains extensive wire logs; `--jobs 2` is optional.

The frozen image is x86-64 and contains Python 2.7.18 / PyPy 7.3.20, Twisted
19.10.0 and Autobahn 0.10.9. The image's Dockerfile comments describe an older
PyPy build; the versions above were read from the running image. `PYTHONPATH`
loads the mounted pinned source, not whichever suite revision the image ships.
Reports and process state are saved under `reports/differential`.

## Controlled performance comparison

After all other test/build workloads finish:

```sh
python3 tools/compare_performance.py
```

Both fuzzers run as Linux x86-64 processes with a one-CPU quota; Rust has one
Tokio worker and one case at a time. Both use compression level 6. They contact
the same persistent native ARM64 Rust echo server over the same Docker bridge.
An environment warmup pair is discarded, followed by three measured pairs in
alternating order. Each trial starts a fresh fuzzer process, so PyPy's JIT warms
up again. Per-case durations exclude process startup and report generation.
The output is `docs/performance-comparison.json`.
Original per-case wire-log/statistics defaults remain enabled; the port uses
bounded summaries and stricter byte-for-byte echo validation. These are product
default comparisons, not isolated language or codec comparisons.

On ARM Macs both fuzzers use Docker's x86-64 emulation. Matching architecture
removes the obvious native-versus-emulated asymmetry, but these results still
cannot establish a native-hardware or steady-state language speedup.

## Browser and serializer checks

For browser testing, start the Rust fuzzing server with `--webport`, open the
page in Chrome, and run its full catalog. To compare the original server, expose
the same internal and external TCP port: the old handshake rejects a Host port
that differs from its configured listening port. Point the same browser driver
at that server. The recorded focused comparison selects `3.*`, `4.*`, `5.15`,
and `7.*` (55 cases). Case `4.2.5` can legitimately choose `OK` or `NON-STRICT`
depending on whether the ping preceding the invalid opcode is processed.

The original serializer CLI has two reference-runtime issues: the frozen image
lacks optional MessagePack, and converting a Unicode test message to its display
name raises `UnicodeEncodeError`. `tools/verify_serializer_reference.py` calls
the real original message generator and serializers without formatting names.
Run it inside a disposable reference container with `msgpack==0.6.2` installed,
mounting the generated Rust vectors at `/reports/rust-serializer.json`.
It verifies raw message equality and decoded JSON/MessagePack equality and
separately counts byte-identical encodings.

`tools/summarize_verification.py` checks the six complete reports plus the saved
browser and serializer comparisons and writes `docs/differential.json`. It
fails on missing cases, unexpected failures, or unexplained verdict differences.
