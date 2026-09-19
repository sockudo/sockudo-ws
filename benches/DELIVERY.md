# Delivery diagnostics

## Client receive experiments

`client_receive_bench` measures native-split client delivery over TCP or rustls
loopback, using an external UTF-8 fixture without rewriting its contents:

```sh
CARGO_PROFILE_BENCH_LTO=fat CARGO_PROFILE_BENCH_CODEGEN_UNITS=1 \
  cargo bench --locked --bench client_receive_bench \
  --features rustls-webpki-roots,http2 --no-run
# Executable arguments:
# fixture tls|tcp connections count burst retain pause_us typed|boxed [trace]
```

Four runtime workers share the process with the peer; allow at least five
physical cores of the same class when choosing affinity. All connections finish
TLS setup before a shared barrier releases traffic. The connection-local clock
starts before that barrier; delivery subtracts the timestamp immediately before
its matching peer write. Atomic timestamp publication and payload verification
are harness overhead shared by both variants. Verification happens after the
receive timestamp and affects later messages.

`retain=0` releases each message before the next read; a positive value keeps
that many messages across reads. `boxed` uses `Stream<Http1>`;
`typed` preserves the concrete I/O type. Native Ping/idle monitoring remains on.
No WebSocket handshake, application handler, real NIC or application backend is measured.
Text frames cover UTF-8 validation; binary payloads is outside this case.
The emitting peer does not read client control frames. Keep each trial shorter
than the 30-second active Ping interval; these cases measure monitoring overhead,
not the completion of a Ping/Pong cycle.

A nonzero pause sleeps before each burst. These are actual-emission-to-delivery
samples, not scheduled latency or a production arrival replay. A zero pause is
saturated traffic and can include queue buildup. Keep these regimes separate.
The optional `trace` argument wraps client I/O and reports Pending polls and a
histogram of bytes returned per successful read on stderr at transport teardown.
It observes plaintext reads above TLS, not kernel receive calls. Run it
separately from timing: histogram bookkeeping changes the measured path. Byte
counts reveal read batching but do not count frames when reads split a frame.
CSV retains connection, sequence and delivery nanoseconds; exclude the same
predeclared warmup prefix on each connection, and compare burst positions as
well as per-connection tails. Keep fixtures external/local-only, recording their
hashes with experiment results. A/A calibration and paired sessions are required
before claiming a performance improvement; this diagnostic alone is not a gate.


## Synthetic smoke input

Create a small UTF-8 payload in a temporary file, for example:

```sh
payload=$(mktemp)
printf '%s' '{"sequence":1,"value":"synthetic"}' > "$payload"
# Pass "$payload" as the fixture argument to the built executable.
```

`delivery_diagnostic_bench` uses generated 32-byte sequence payloads and needs no
external file. Its arguments are `sender_workers receiver_workers connections
count rate_per_connection [ws|raw] [off|on]`. Tracing is for diagnosis, not timing.
Absolute scheduled-lateness percentiles depend on timer phase; compare
per-connection spreads and actual send-to-delivery latency, with a raw TCP
control, before attributing differences to scheduler fairness.

The standalone diagnostics accept Cargo’s `--bench` argument. With no arguments they run a short synthetic case. For file-based receive diagnostics, `-` selects the built-in JSON payload; batch latency CPU `-` leaves affinity unchanged. These defaults check the harness without external fixtures.
