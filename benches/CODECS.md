# Codec and fan-out benchmarks

All payloads are generated deterministically; no external corpus is required.
Run a selected suite with `cargo bench --locked --features permessage-deflate --bench NAME` and use identical profiles, features and harness sources for baseline/candidate comparisons. Separate checkouts need independent target directories; a single checkout can reuse its target directory when building revisions sequentially, provided each executable is copied and its revision verified before switching. Keep Criterion output separate for each run.

- `protocol_bench`: framing, fragmented messages and protocol processing.
- `masking_bench` and `client_mask_bench`: in-place and copy masking by size and alignment.
- `deflate_input_bench`: reset/takeover, mixed/repeated input and TCP delivery.
- `deflate_capacity_bench`: fixed/alternating sizes, capacity growth and TCP batches.
- `services_bench`: compression and publication with receiver draining included.
- Existing `websocket_bench` adds size/alignment boundaries; `comparison_bench` validates the roundtrip with separate sender/receiver protocol state.

For smoke validation, append `-- --test` to the Cargo command. This runs setup, assertions, the benchmark body and teardown without collecting performance samples; smoke results are not timings. Run all eight targets listed above. The compression suites require `permessage-deflate`; the six added targets also require `tokio-runtime` (both are default features).

## Measurement boundaries

Criterion reports time per iteration. `Throughput::Elements` and `Throughput::Bytes` supply a throughput denominator; they do not turn that time into per-message latency. Batch time divided by message count is an average cost, not a latency percentile. These suites do not measure send/delivery P99, allocation counts, retained capacity or RSS; collect those separately when an optimization needs those guardrails.

| Case | One iteration and timed work | Untimed preparation / checks |
| --- | --- | --- |
| `receive_container`, `fragmented_text` | Parse the prepared batch; drop messages or clear the retained output Vec; drop the consumed input | Construct and clone wire input; verify decoded payloads/counts and empty input before measurement |
| `cork_slices` | Construct and drop the returned slice list | Build cork segments and check their total byte count |
| `mask_alignment`, `client_mask_encode` | XOR in place, or truncate and encode into a retained destination | Allocate and align storage; scalar XOR or decoded-frame oracle |
| `masked_parse` | Allocate/copy input, parse and drop the frame | Construct wire input and validate one decode |
| `websocket_bench/parse` | Parse prepared input; consumed input is dropped inside the closure; returned frame is dropped outside Criterion's batch timer | Allocate/copy input; validate one decode |
| `message_protocol` | Encode and decode one message, including output destruction and any buffer replenishment | Construct sender/receiver state and message; validate the roundtrip |
| `deflate_input_decode` | Decode and drop one repeated message | Build wire data; prime and verify first/repeated blocks |
| `deflate_capacity_decode` | Decode and drop one whole size cycle (1, 2 or 8 messages) | Build wire data; prime and verify two cycles; fixed 1 MiB message limit |
| `masked_tcp_receive` | Deliver/drop 1024 messages, including producer wakeup; excludes final producer join | Runtime/socket setup and one full-payload warm-up check |
| `client_mask_tcp` | Send and deliver 1024 messages using send or feed/flush batches; includes peer join | Runtime/socket setup and one full-payload warm-up check |
| `deflate_input_tcp`, `deflate_capacity_tcp` | Deliver/drop 256 messages, including producer wakeup and final join | Runtime/socket setup; verify first/repeated blocks or cycles |
| `deflate` | One compression attempt (possibly `None`) or one decompression, including output destruction | Seeded input generation; verify compressed output when present; omit decode case if compression is declined |
| `pubsub_publish_and_drain` | One publication plus draining all 1/100/1000 recipients | Create memberships and verify initial delivery |
| `shared_compression_concurrency` | One compression per worker, amortized across a synchronized batch; includes barrier release and joins | Create threads and pool; verify a pooled compression roundtrip |
| `pubsub_publish_with_churn` | One publication/drain plus one subscribe/unsubscribe pair in a second thread, amortized across the batch; includes barrier release and join | Create threads, subscribers and pool-independent PubSub state |

TCP cases use four Tokio workers, a caller driving `block_on`, and an in-process peer/producer scheduled on those workers. They use loopback with TCP_NODELAY, without HTTP/TLS handshakes. Payload equality is checked during warm-up; timed loops require successful message delivery and consume the configured count, but do not compare every payload. Masked receive offers default-heartbeat and timers-off cases; the other TCP cases disable heartbeat timers. These are saturated batch diagnostics, not paced arrival or production tail-latency measurements.

The shared-compression case has 1/4/8/16 worker threads calling the same synchronous pool. PubSub churn is on a separate topic and runs a finite batch: either side can finish first, so this is combined batch completion time, not publication latency under guaranteed continuous contention.

For async multi-worker TCP trials, provide separate physical cores for workers and the caller, with capacity for the peer/producer, rather than pinning the whole process to a single CPU. For threaded pool trials, match physical cores to worker count and leave capacity for the coordinator. On smaller machines, oversized worker counts are oversubscription stress cases; smoke execution remains useful for correctness but supplies no normal multi-core performance evidence. Keep single-thread comparisons on an isolated core. Interleave fresh baseline/candidate runs and calibrate repeatability before interpreting differences.
