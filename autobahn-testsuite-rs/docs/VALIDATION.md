# Validation evidence

Validated locally on macOS 27 / arm64 with Rust 1.98.1, and Docker Linux with
Rust 1.88.0. The upstream WebSocket
catalog is pinned at `b8a5120d905e30470e4475785c48e4cedc35f6cd`.

## Complete active catalog

Command:

```sh
AUTOBAHN_FULL_WORKLOAD=1 cargo test --release --locked --lib -- --ignored
```

The completed run took **69.54 seconds** using four Tokio runtime workers and at
most eight concurrent case tasks. Transport was bounded in-memory duplex I/O,
not TCP. Both endpoints were the native Rust implementation.

| Testee role | Cases | OK | Informational | Failed | Reduced workloads |
|---|---:|---:|---:|---:|---:|
| Client | 517 | 514 | 3 | 0 | 0 |
| Server | 517 | 514 | 3 | 0 | 0 |

All 1,000-message compression and RTT loops ran at their original counts. The
three informational cases are `7.1.6`, `7.13.1`, and `7.13.2`. Their close results
are informational too; the other 514 close results per role are OK.

Machine-readable evidence is in [validation.json](validation.json). Detailed
local reports are in `reports/native-smoke/index.html` (generated, not versioned).
The self-test verifies executable coverage and internal interoperability. It
cannot independently prove parity with the old Python runner.

## Independent and adversarial tests

`cargo test --locked` passed 20 non-ignored test functions. These include:

- Real tungstenite parsing/echoing in both transport roles, covering empty and
  large text, binary, ping, fragmentation, UTF-8, closing, and performance cases.
- Deliberately corrupted echoes, invalid opcodes/control frames/text, and a peer
  that never reads, to verify failures and bounded deadlines.
- RFC 6455 masking and RFC 7692 compressed `Hello` vectors.
- Frame length boundaries, incremental receive, unmasked/masked invalid UTF-8
  before frame completion, decompression limits, window sizes, and context reuse.
- HTTP upgrade accept-key verification, pipelined frames, duplicate headers,
  directional extension negotiation, report escaping, and legacy spec parsing.
- JSON and MessagePack roundtrips of all 40 WAMP fixtures.
- Invalid close-code reflection in both roles, retaining the actual code and
  preserving the upstream informational-case verdicts.
- Exact-multiple automatic fragmentation, including the original sender's empty
  final continuation frame.

## Executable TCP/TLS checks

`python3 tools/smoke_cli.py` launches actual release binaries and checks:

- `fuzzingclient` → `testeeserver` on loopback TCP and certificate-verified TLS.
- Eight representative cases including delayed fragmentation, fail-fast UTF-8,
  valid close codes, RTT, compression payloads, and compression parameters.
- TLS refusal when the self-signed certificate is not trusted.
- `testeeclient` → `fuzzingserver`, report generation, and browser HTTP delivery.
- Catalog enumeration, serializer mode, ordinary and legacy-spec massconnect.

These network smoke runs intentionally use three-message performance loops and
are labeled reduced. Reports are under `reports/cli-tcp`, `reports/cli-tls`, and
`reports/cli-clients`.

`cargo fmt --check`, strict Clippy (`--all-targets --all-features -- -D warnings`),
and rustdoc with `-D warnings` also passed. The automated security-review skill
could not run because its supporting plugin scripts were absent. Manual checks
and regression tests were performed; this is not a formal security audit.

## Pinned Python differential verification

The frozen reference image was executed with the pinned source mounted
read-only. Runtime versions were Python 2.7.18 / PyPy 7.3.20, Autobahn 0.10.9,
and Twisted 19.10.0. See [reproduction instructions](REFERENCE_TESTING.md) and
[machine-readable evidence](differential.json).

| Fuzzer | Testee | Testee role | Cases | OK | Informational | Failed |
|---|---|---|---:|---:|---:|---:|
| Python | Python | Server | 517 | 514 | 3 | 0 |
| Python | Python | Client | 517 | 514 | 3 | 0 |
| Rust | Python | Server | 517 | 514 | 3 | 0 |
| Rust | Python | Client | 517 | 514 | 3 | 0 |
| Python | Rust | Server | 517 | 514 | 3 | 0 |
| Python | Rust | Client | 517 | 514 | 3 | 0 |

All workloads retained original counts. The Rust and Python fuzzers agree on
application verdict, close verdict, and remote close code for every case when
testing the Python peer in either role. After the final fragmentation correction,
30 affected/representative cases per role were rerun against Python, with no
verdict differences, and the complete native self-test was rerun too. The compact
evidence combines those focused results with the full runs. One earlier Python
baseline ended without a report; it was discarded and repeated successfully.

Verification found and corrected four compatibility issues:

- Invalid peer close codes were discarded instead of reported.
- Informational cases were incorrectly overridden by protocol-error verdicts.
- Compression used level 1 instead of upstream's level 6.
- Exactly divisible fragmented messages lacked upstream's empty final frame.

Matching verdicts are not byte-for-byte wire parity. The Rust and Python zlib
implementations can emit different compressed lengths even at level 6, changing
the number of compressed fragments. For `6.4.3`/`6.4.4`, Rust diagnostics count
the intentionally incomplete frame header, whereas upstream's frame statistics
do not. The evidence records these count differences explicitly.

## Browser and serializer verification

Chrome 152 completed all 517 cases through the browser page and generated its
report. A focused 55-case comparison against the original Python fuzzer exposed
the close-reporting bugs above. After correction, the affected close cases agree.
Case `4.2.5` selected `OK` in the Python run and `NON-STRICT` in the Rust run;
both are expected alternatives depending on whether the preceding ping is handled
before the invalid opcode. Chrome's seven invalid-close-code failures in that
focused run were also failures under the original fuzzer. These are browser
conformance results, not seven failures of the Rust regression suite.

All 40 serializer fixtures match the actual original message objects and decoded
JSON/MessagePack values. Exactly 32 of 40 encodings are byte-identical in each
format. The original CLI cannot format one Unicode display name and its image
omits optional MessagePack; the comparison installed `msgpack==0.6.2` in a
disposable container and invoked the original serializers directly.

## Linux and minimum compiler

Docker images built successfully for Linux ARM64 and x86-64 with Rust 1.88.0.
The final 20-test regression suite passed on Linux ARM64 and macOS, and the final
source built on both Linux architectures. The x86-64 image executes under Docker
Desktop emulation on this ARM64 host. CI was extended with a Rust 1.88 job;
remote CI execution is not claimed.

## Performance measurements

[benchmark.csv](benchmark.csv) records release microbenchmarks with each workload
running for at least 250 ms. Representative measurements on this machine:

| Operation | Payload | Throughput |
|---|---:|---:|
| Safe masking | 1 MiB | 66.848 GiB/s |
| Decode including input-buffer copy | 1 MiB | 77.621 GiB/s |
| Repetitive deflate with context takeover, level 6 | 40 KiB | 4.390 GiB/s |

These operate on repeated hot buffers and a highly compressible payload. They
are useful for regression tracking, but do not predict socket throughput,
incompressible-data speed, or end-to-end latency. Re-run on the intended
deployment hardware before choosing limits. The previous level-1 compression
measurement was superseded when the reference audit identified that upstream
uses level 6.

### Controlled comparison with Python

Three measured fresh-process trials per implementation, after one discarded
environment-warmup pair, produced these median per-case durations:

| Workload | Python | Rust | Python / Rust |
|---|---:|---:|---:|
| One 16 MiB binary echo (`9.2.6`) | 130 ms | 10.91 ms | 11.92× |
| 1,000 empty text echoes (`9.7.1`) | 145 ms | 34.91 ms | 4.15× |
| 1,000 × 64-byte text echoes (`9.7.3`) | 112 ms | 35.28 ms | 3.17× |
| 1,000 × 1,024-byte text echoes (`9.7.5`) | 101 ms | 34.98 ms | 2.89× |
| 1,000 × 256-character compressed JSON echoes (`12.1.3`) | 251 ms | 49.67 ms | 5.05× |
| 1,000 × 65,536-character compressed JSON echoes (`12.1.9`) | 917 ms | 349.06 ms | 2.63× |

All cases passed and message counts were checked. Both runners were Linux
x86-64 under Docker Desktop emulation on the same ARM Mac, with a one-CPU quota,
one Rust worker, one case at a time, compression level 6, and the same persistent
ARM64 echo server. No other verification workload ran during these trials.
Run order alternated. Per-case durations exclude startup and report generation;
each fresh PyPy process still incurs JIT warmup. Original wire-log defaults and
Rust's bounded diagnostics/stricter echo validation differ. Compression backends
can produce different wire sizes for the same application payload.

The measured **2.63×–11.92×** advantage applies to these six workloads and this
environment. It is not a general native-hardware speedup claim. Raw trials,
image IDs, and methodology are in [performance-comparison.json](performance-comparison.json)
and [REFERENCE_TESTING.md](REFERENCE_TESTING.md).

## Remaining verification

- No Windows execution or bare-metal Linux x86-64 performance measurement.
- No formal security audit or proof of compatibility with every possible peer.
- Dormant WAMP fuzzing/router and wsperf tools remain unported. This was verified
  directly against the commented-out entries in the pinned `wstest.MODES`.
- Full original wire-log/report schemas and byte-identical serializer output
  are intentionally not provided. Browser behavior outside the tested Chrome
  version remains unverified.
