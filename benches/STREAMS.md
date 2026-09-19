# Latency experiments

`batch_latency_bench` compares individual client sends, explicit `feed`/`flush`,
the native split writer, and futures' generic split on Linux TCP loopback. It does not change the
library's send policy or introduce a timer to fill batches.

Build with the same profile for every strategy:

```sh
CARGO_PROFILE_BENCH_LTO=fat CARGO_PROFILE_BENCH_CODEGEN_UNITS=1 \
  cargo bench --locked --bench batch_latency_bench --no-run
```

Invoke the resulting executable with:

```text
--case mode bytes burst groups sender_cpu peer_cpu idle_us nodelay
```

- `mode`: `send`, `batch`, `split`, or `futures`.
- Payload size must be at least 8 bytes for the sequence number.
- Discover the CPU topology first. Sender and peer must use different physical
  cores of the same class; leave their SMT siblings idle. Each thread sets and
  reports its own affinity before measuring.
- The per-case encoded-byte limit accommodates the whole burst, so this tests
  flush policy rather than rejection by the hard buffer limit.
- All messages in a burst exist before the shared readiness timestamp. The
  sender waits for the peer to consume the complete burst, then idles before
  the next burst. This is a closed-loop burst experiment, not an open-loop
  arrival or throughput-capacity test.
- Sixteen initial bursts warm the connection. Connection setup, burst ACK,
  payload construction, and the preceding idle wait are outside readiness-to-
  delivery latency. Idle still affects CPU and socket state.
- The peer checks every sequence, payload length and content. Content checking
  follows the receive timestamp and can affect later messages in the burst.

CSV fields are integer nanoseconds from a shared `std::time::Instant` epoch:
`ready_ns`, `begin_ns`, `completed_ns`, and `received_ns`. For `batch`, completion
is the shared flush boundary, not each `feed` return. Compare
`received_ns - ready_ns` for all messages **and separately for index zero**;
otherwise faster later messages can hide delayed first delivery. Report queue
waiting (`begin_ns - ready_ns`) separately when diagnosing the result.

Preserve raw runs, A/A comparisons, paired execution order, binary hashes and
runtime environment. A shorter batch-average duration does not establish lower
per-message latency. Loopback measurements exclude TLS, physical NICs and the
remote service. No batching strategy should become a default based on these
mechanism experiments alone.

### Controlled receive state

`receive_state_bench` separates read readiness, batching, dispatch and retained
message ownership from socket scheduling:

```sh
cargo bench --locked --bench receive_state_bench --no-run
# fixture connections frames_per_read ready|pending retain typed|boxed
```

The current-thread runtime processes connections round-robin. `ready` provides
bytes immediately; `pending` injects one self-waking Pending before each read.
The source repeats the external fixture in a prebuilt Text-frame batch, allowing
partial reads when the destination cannot hold the batch. Thus `frames_per_read`
is a source-batch size, not a guarantee for arbitrarily large fixtures. Use a
batch smaller than the receive window when testing an exact read/frame ratio.
Each connection warms for 512 messages before 64 blocks of 256 messages per
connection. CSV reports block-average nanoseconds per message, not message tails.
Native heartbeat monitoring stays enabled. There is no independent producer;
one physical core is sufficient. This is a mechanism diagnostic, not a model of
network arrival latency. Treat processes, not blocks, as independent replicates.

### Pending writer control churn

`pending_write_bench` times native-split sends while the transport accepts a
three-byte prefix and then blocks. The peer injects the requested number of
Pings through the real reader/control queue before releasing the write gate:

```sh
cargo bench --locked --bench pending_write_bench --no-run
# number_of_peer_pings (zero selects the immediately writable control)
```

The current-thread runtime and default heartbeat/idle monitoring are active.
Setup and teardown are outside each sample; the blocked case includes spawning
and joining the send task. Sixteen warmup samples precede 256 completion samples.
Compare each case against the same case in the other binary; zero and nonzero
cases have different harness overhead. This exercises partial-write/control
churn costs, not real TCP backpressure or production Ping frequency. Preserve
A/A runs, fixed paired order, executable hashes and environment with results.

The standalone diagnostics accept Cargo’s `--bench` argument. With no arguments they run a short synthetic case. For file-based receive diagnostics, `-` selects the built-in JSON payload; batch latency CPU `-` leaves affinity unchanged. These defaults check the harness without external fixtures.
