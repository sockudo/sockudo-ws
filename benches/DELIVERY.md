# Delivery diagnostic benchmark

`delivery_diagnostic_bench` records per-connection timelines for generated
32-byte sequence messages over WebSocket or a raw TCP control path. It separates
send completion, actual send-to-delivery latency, scheduled lateness, and
per-connection P99-P1 spreads.

The default invocation is a short fixed-rate WebSocket smoke case: one runtime
worker, one connection, 128 messages, and 1,000 messages per second.

```sh
cargo bench --locked --bench delivery_diagnostic_bench
```

Pass an explicit case after `--` for measurement:

```text
sender_workers receiver_workers connections count rate_per_connection [ws|raw off|on]
```

For example:

```sh
cargo bench --locked --bench delivery_diagnostic_bench -- \
  4 4 16 20000 1000 ws off
cargo bench --locked --bench delivery_diagnostic_bench -- \
  4 4 16 20000 1000 raw off
```

A rate of zero selects saturated traffic. Keep saturated and paced results
separate. `on` enables poll/wake tracing and changes the measured path, so use it
only for diagnosis. The raw TCP case is a control for runtime and harness costs;
it does not exercise WebSocket framing.

Absolute scheduled-lateness percentiles depend on timer-tick phase. Gate on
per-connection P99-P1 spreads together with send and delivery latency, and use a
phase sweep plus the raw control before attributing a change to scheduler
fairness. Match CPU affinity to runtime concurrency, leave capacity for the
benchmark driver and in-process peer, and label single-CPU oversubscription as a
contention stress case.

All payloads are generated. No external fixture, private corpus, WebSocket
handshake, physical NIC, or application handler is included. Preserve raw runs,
A/A controls, paired execution order, executable hashes, and the runtime
configuration when comparing revisions.
