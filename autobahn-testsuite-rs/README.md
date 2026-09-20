# Autobahn Testsuite in Rust

A native Rust implementation of the active WebSocket conformance suite at
[`crossbario/autobahn-testsuite@b8a5120`](https://github.com/crossbario/autobahn-testsuite/tree/b8a5120d905e30470e4475785c48e4cedc35f6cd).
The binary includes all **517 registered WebSocket cases**, all five compression
corpora, and the **40 WAMP serializer fixtures**. Building and running it requires
no Python, Twisted, upstream checkout, or external data downloads.

The port executes the complete active case catalog. It is **not a claim of
byte-for-byte compatibility with every historical tool, flag, or report field**.
See the compatibility notes below and [validation evidence](docs/VALIDATION.md).

## Build and run

Requires Rust 1.88 or newer. The checked-in lockfile fixes dependency versions.

```sh
cargo build --release --locked
./target/release/wstest --list-cases
```

Test an echo server:

```sh
./target/release/wstest -m fuzzingclient -s config/fuzzingclient.json
```

Test clients, including the browser you are using:

```sh
./target/release/wstest -m fuzzingserver -s config/fuzzingserver.json
# Open http://127.0.0.1:8080/ for the browser driver.
```

In another terminal, drive that server using the Rust testee:

```sh
./target/release/wstest -m testeeclient -w ws://127.0.0.1:9001 -i rust-testee
```

`reports/servers/index.html` or `reports/clients/index.html` contains the report.
The fuzzing client exits with status 1 for a failing case or close handshake.
A peer that declines compression is reported as `UNIMPLEMENTED`, never `OK`.

Useful focused runs:

```sh
./target/release/wstest -m fuzzingclient -w ws://localhost:9001 --cases '6.*'
./target/release/wstest -m fuzzingclient -w ws://localhost:9001 --concurrency 8
./target/release/wstest -m fuzzingclient -w ws://localhost:9001 --message-count 3
./target/release/wstest -m serializer -o serializer-vectors.json
```

`--message-count` reduces performance/compression repetitions; the report marks
such runs `reduced`. Defaults preserve the upstream message counts and deadlines.
Keep concurrency at 1 for comparable per-case latency measurements; raise it for
throughput and faster functional runs.

## Modes

| Mode | Behavior |
|---|---|
| `fuzzingclient` | Exercise configured WebSocket servers and write reports |
| `fuzzingserver` | Serve the Autobahn client-testing control endpoints |
| `testeeserver`, `echoserver` | Validate and echo messages, including compression |
| `testeeclient` | Enumerate and run cases on an external fuzzing server |
| `echoclient` | Echo server-originated messages; `--message-count N` runs N latency probes |
| `broadcastserver` | Fan out messages and send one-second tick messages |
| `broadcastclient` | Send periodic greetings and display received broadcasts |
| `massconnect` | Connect in bounded batches, handle pings, hold connections, and close them |
| `serializer` | Generate JSON and MessagePack encodings for the 40 WAMP fixtures |

The server implements `/getCaseCount`, `/getCaseInfo`, `/getCaseStatus`,
`/runCase`, `/updateReports`, and `/stopServer`. Numeric `case` is **one-based
within the selected cases**; `casetuple` selects a dotted ID. `caseId` is also
accepted. Agent/case exclusions and `shutdownOnComplete` are supported.

## TLS and limits

Use `wss://` with `--cert chain.pem --key key.pem` for a TLS server. Clients verify
certificates and hostnames using a shared rustls configuration. `--ca ca.pem`
adds a trusted CA; certificate verification has no insecure bypass.

The JSON specification accepts upstream `cases`, `exclude-cases`,
`exclude-agent-cases`, `servers`, `protocols`, `url`, and `outdir`. Additional keys:

| Setting | Default |
|---|---:|
| `concurrency` | 1 case / handshake at a time |
| `max_connections` | 256 accepted WebSocket connections |
| `max_frame_size`, `max_message_size` | 64 MiB each |
| `handshake_timeout_ms`, `close_timeout_ms` | 10,000 / 1,000 |
| `case_timeout_ms` | Upstream case deadline |
| `max_results` | 10,000 stored agent/case results |
| `webport` / `--webport` | 0 (disabled); example server config enables 8080 |
| `connections`, `hold_ms` | 100 / 1,000 for massconnect |
| `batch_delay_ms`, `retry_delay_ms` | 0 / 1,000 for massconnect |
| `connect_retries` | 0; null means retry until interrupted |

Normal defaults bind loopback. The test/control endpoints intentionally allow
clients to request test cases, generate reports, and stop the test server; use
an isolated test network when binding a non-loopback address.

## Architecture and performance

The fuzzer uses a purpose-built raw frame writer because a normal WebSocket
library would prevent the malformed frames these tests must send. It preserves
reserved bits/opcodes, invalid close payloads, timed partial frames, fragmentation,
TCP chopping, event sequences, and separate close-handshake verdicts.

Tokio tasks handle concurrent sockets, with bounded channels and connection
admission. `Bytes` shares payload slices without copying during fragmentation.
The reader retains partially received data across cancellation. The writer batches
ordinary output into a reusable 64 KiB buffer and honors explicit chop/sync
boundaries. Safe word-at-a-time masking allows compiler vectorization. Compression
uses the Rust `zlib-rs` backend, the original compression level 6, directional
window negotiation, and context reuse. Source crates forbid `unsafe` code;
dependency internals are separate.

Reports run outside async I/O workers. Output is HTML-escaped, filenames use
sanitized labels plus a full digest, and replacement files are written atomically.
Both decompressed payloads and accumulated diagnostic events are bounded.

Run the release microbenchmarks:

```sh
cargo bench --locked --bench codec
```

[Recorded microbenchmarks](docs/benchmark.csv) measure cached-buffer masking,
framing, and repetitive compression. They are **not network-throughput numbers**.
The [reference testing guide](docs/REFERENCE_TESTING.md) describes the separate,
controlled Python comparison and its emulation and JIT limitations.
Across six recorded application workloads, Rust's median case duration was
2.63×–11.92× faster under matching x86-64 Docker emulation. See the scoped
[comparison results](docs/VALIDATION.md#controlled-comparison-with-python);
these are not general bare-metal speedup claims.

## Validation and maintenance

```sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
AUTOBAHN_FULL_WORKLOAD=1 cargo test --release --locked --lib -- --ignored
cargo build --release --locked
python3 tools/smoke_cli.py
```

The complete-catalog test runs all 517 cases in both transport roles, over bounded
in-memory duplex streams. `AUTOBAHN_FULL_WORKLOAD=1` preserves all 1,000-message
loops; without it the explicit full-catalog test uses three-message smoke loops.
Independent tests use tungstenite rather than the port's own echo implementation.
The CLI harness separately checks real TCP, TLS, certificate rejection, reports,
control workflows, browser HTTP delivery, serializer output, and massconnect.
CI definitions cover Linux and macOS; only local results are claimed here.
Reference-runtime and browser checks are documented in
[validation evidence](docs/VALIDATION.md), with reproducible Docker harnesses in
`tools/differential.py` and `tools/compare_performance.py`.

`catalog/provenance.json` records source hashes and the pinned upstream commit.
To reproduce the WebSocket import, clone that exact revision into `upstream/`
and run `python3 tools/import_cases.py`. This development tool records the
inspected Python definitions into typed recipes; it is never invoked by Cargo or
the binary. `python3 tools/import_serializer.py` reproduces the WAMP corpus from
the checked-in, MIT-licensed reference definitions.

## Compatibility notes

- All **active WebSocket case IDs**, payloads, timings, expected event alternatives,
  and workload counts are imported. Case `9.9.1` is disabled in the pinned
  upstream registry and is not registered here either.
- The old WAMP router/fuzzer, database import/export, and wsperf master/controller
  modes are commented out in upstream `wstest.MODES`; those dormant applications
  are not ported. The active WAMP **serializer** mode is implemented.
- JSON reports preserve the agent → case summary layout and standard behavior
  names. Detailed reports use a new schema and bounded event summaries instead
  of the original exhaustive wirelog, CSS, and JavaScript assets.
- All 40 serializer fixtures match the original message objects and decoded
  JSON/MessagePack values. Encodings are not universally byte-identical (32 of
  40 match exactly in each format in the recorded reference run).
- Performance echoes are checked byte-for-byte. Several upstream category 9
  cases only checked type and length, so this port can flag corrupt echoes that
  the original would accept.
- Common `options.failByDrop: false`, positive handshake timeouts, target
  `options.version: 18`/`13`, and legacy massconnect batch/retry options are
  understood. Unsupported Twisted-specific options, pre-RFC protocol versions,
  and infinite handshake timeouts fail explicitly. Specs are not auto-created.
- The optional browser interface is a replacement interface, enabled with
  `--webport`. Legacy debugging and database flags are not emulated.
- Compression windows used by every registered case (9–15) are supported.
  Eight-bit window offers are declined rather than falsely advertised.
- Reference comparisons establish observed verdict compatibility on the tested
  peers, not an independent proof of every possible peer or wire encoding.

The multistage [Dockerfile](Dockerfile) was built for Linux ARM64 and x86-64 using
Rust 1.88.0. Licensing and source attribution are in [LICENSE](LICENSE) and
[NOTICE](NOTICE).
