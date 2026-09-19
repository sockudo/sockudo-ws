# Stream benchmarks and diagnostics

All inputs are synthetic. Build and compare revisions with the same profile,
features, and independent target directories. Preserve raw runs, A/A controls,
paired execution order, executable hashes, and the runtime configuration.

## Stream benchmark

`stream_bench` uses Criterion to compare unified and split reads and writes over
Tokio duplex and TCP transports. Each timed iteration contains 1,024 messages;
setup and teardown remain outside the measured interval.

```sh
cargo bench --locked --bench stream_bench
```

The benchmark separates timer configurations and includes masked receive sizes
around framing boundaries. TCP loopback results do not represent TLS, a physical
NIC, an application handler, or production tail latency. Multi-worker cases need
enough physical cores for runtime workers, the benchmark driver, and the peer.

## Controlled receive state

`receive_state_bench` separates read readiness, input batching, dispatch, and
retained message ownership from socket scheduling:

```sh
cargo bench --locked --bench receive_state_bench --no-run
# executable arguments:
# fixture connections frames_per_read ready|pending retain typed|boxed
```

Use `-` for the built-in JSON payload. With no arguments the executable runs a
short synthetic case. The current-thread runtime processes connections round
robin. `ready` provides bytes immediately; `pending` injects one self-waking
`Pending` before each read. The source repeats the payload in a prebuilt Text
frame batch, allowing partial reads when the destination cannot hold the batch.
Thus `frames_per_read` is a source-batch size, not a guarantee for arbitrarily
large payloads.

Each connection warms for 512 messages before 64 blocks of 256 messages per
connection. CSV values are block-average nanoseconds per message, not message
tails. Native heartbeat monitoring remains enabled. Treat processes, rather
than blocks, as independent replicates.

## Send and delivery diagnostic

`send_latency_diagnostic` is an explicit example instead of a default benchmark
target because its scheduling matrix is diagnostic and can take substantial
time. Run a short synthetic smoke case with no arguments:

```sh
cargo run --locked --release --example send_latency_diagnostic
```

Run one controlled case or the full matrix explicitly:

```sh
cargo run --locked --release --example send_latency_diagnostic -- \
  --case workers unified|split connections burst count timing
cargo run --locked --release --example send_latency_diagnostic -- --matrix
```

The output separates send completion, actual emission-to-delivery latency,
scheduled lateness, and per-connection P99-P1 spreads. A zero burst selects the
saturated case; positive bursts are paced. Absolute scheduled-lateness
percentiles depend on timer phase. Use the per-connection spreads, send and
delivery latency, a phase sweep, and a control path before attributing a change
to scheduler fairness.
