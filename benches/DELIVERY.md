# Delivery diagnostic benchmark

`delivery_diagnostic_bench` records per-connection timelines for generated 32-byte sequence messages over WebSocket or a raw TCP control path. It separates pre-send-to-local-completion cost, pre-send-to-peer-delivery latency, scheduled latency and sender lateness, with per-connection P99-P1 spreads for the last two.

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

## Timing and output contract

All message timestamps share one process-wide monotonic epoch. `sent_ns` is sampled before the final payload timestamp write and send call; it is not kernel or wire emission. Both send and delivery costs include the remaining payload setup. Raw mode writes and validates a fixed 34-byte record (two WebSocket-like header bytes plus 32 payload bytes) through TCP/BufReader, bypassing WebSocket encoding/parsing. Its receiver buffer capacity matches the peer WebSocket default, but it still has different copy/allocation work; it is a control, not a pure subtraction of codec cost.

Connections establish sockets with TCP_NODELAY, exchange 256 warm-up messages and meet at a sender barrier before each chooses a start 50 ms later. Starts are not an exact common timer phase. Paced mode offers the configured per-connection rate, but each send awaits completion; a late sender catches up without dropping scheduled messages. Saturated mode has no independent arrival schedule, so its four scheduling summary fields are empty. Full warm-up contents and measured sequence, length and padding are checked; post-run arithmetic rejects reversed timestamps. Analysis/output starts only after every connection completes.

`connection` rows contain the following fields after their row tag:

```text
id,workers,count,rate,delivery_p99_ns,send_p99_ns,late_p99_ns,first_sent_ns,last_received_ns,max_gap_ns,gap_before_ns,gap_after_ns,scheduled_p99_ns,scheduled_p99_p1_ns,sender_late_p99_p1_ns
```

`late` means sender lateness (`sent - due`); scheduled latency means `received - due`. Percentiles use nearest rank within each connection; spread is P99 minus P1. No samples are pooled across connections or runs. The default 128-message smoke has about 1.28 observations in the upper 1%; `--test` uses only 16 messages and also checks wake forwarding. Neither supplies meaningful tail-performance evidence. Record sample support and run independent processes for comparisons. The tool does not perform a phase sweep.

`slow` rows retain up to 20 messages with the highest delivery latency, including their original due/sent/completed/received timestamps. Trace mode also records read/task polls during warm-up; summaries cover all those polls, while emitted event rows are a selected subset around the worst delivery gap or events meeting the 1 ms threshold. A recorded wake is the first notification since the prior poll boundary, not proof that a scheduler caused a delay: self-wakes, coalescing and notifications arriving during a poll are included. The first parent waker is retained because the traced future remains in the same spawned task. Tracing adds clock reads, atomic updates, buffering and possible reallocations; never compare trace-on times with trace-off times as a library performance delta.

`receiver_workers=0` shares the sender runtime; a positive value creates an additional runtime and re-registers the receiving socket there. Total runtime workers then equal sender plus receiver workers, with additional capacity needed for the caller/coordinator. The `workers` CSV field is the sender worker count only; preserve the full command, including receiver workers, protocol and trace mode. The 4+4 examples require at least eight worker cores plus coordinating capacity. On smaller machines use fewer workers; oversubscription is a stress test, not normal multi-core evidence. No affinity or system policy is configured by this executable.

This harness is a diagnostic tool. It does not measure handshake, physical-network or production application latency, and its raw path does not exercise the library's WebSocket receive implementation. A smoke pass validates execution and checks, not performance qualification.
