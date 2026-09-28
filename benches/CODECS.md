# Codec and fan-out benchmarks

All payloads are generated deterministically; no external corpus is required.
Run a selected suite with `cargo bench --locked --features permessage-deflate --bench NAME` and use identical profiles, features and harness sources for baseline/candidate comparisons. Separate checkouts need independent target directories; a single checkout can reuse its target directory when building revisions sequentially, provided each executable is copied and its revision verified before switching. Keep Criterion output separate for each run.

- `kernels`: in-place and copy masking, UTF-8 references, prepared/copy-inclusive parsing and encoding.
- `protocol`: message containers, fragmented messages, validation and protocol roundtrip.
- `deflate`: reset/takeover, threshold, changing-size input, configuration and shared/dedicated encoders.
- `pubsub`: publication with receiver draining and subscription changes.
- `transport`: original TCP codec experiments, plus runtime-specific transport coverage.

For smoke validation, append `-- --test` to the Cargo command. This runs setup, assertions, the benchmark body and teardown without collecting performance samples; smoke results are not timings. See [suite entry point](README.md) for manual feature-specific validation commands. Compression cases require `permessage-deflate`; TCP cases additionally require their runtime. Pure kernel/protocol work does not require Tokio.

## Measurement boundaries

Criterion reports time per iteration. `Throughput::Elements` and `Throughput::Bytes` supply a throughput denominator; they do not turn that time into per-message latency. Batch time divided by message count is an average cost, not a latency percentile. These Criterion suites do not measure send/delivery P99, allocation counts, retained capacity or RSS. The separate `bench_memory` example records allocator-requested bytes and allocation calls; it does not report RSS.

| Case | One iteration and timed work | Untimed preparation / checks |
| --- | --- | --- |
| `output_container`, `fragmented_steady` | Parse the prepared batch; drop messages or clear the retained output Vec; drop the consumed input | Construct and clone wire input; verify decoded payloads/counts and empty input before measurement |
| `protocol/extended/cork` | Fill a stack array of up to 16 IoSlices without allocating | Build actual segments; check count, byte order and partial consume across the slice limit |
| `mask_alignment`, `copy_mask_alignment` | XOR in place, or truncate and encode into a retained destination | Allocate and align storage; scalar XOR or decoded-frame oracle |
| `parse_copy` | Allocate/copy input, parse and drop the frame | Construct wire input and validate one decode |
| `kernels/*/parse_prepared` | Parse prepared input; consumed input is dropped inside the closure; returned frame is dropped outside Criterion's batch timer | Allocate/copy input; validate one decode |
| `protocol/extended/parser_partial` | Parse incomplete input, append its final byte without growing the buffer, then parse the complete frame; consumed input is dropped inside the closure and returned frame outside Criterion's batch timer | Prepare a unique buffer with capacity for the complete wire and copy all but its last byte; verify both the incomplete result and final payload |
| `roundtrip` | Encode and decode one message, including output destruction and any buffer replenishment | Construct sender/receiver state and message; validate the roundtrip |
| `input_decode` | Decode and drop one repeated message | Build wire data; prime and verify first/repeated blocks |
| `capacity_decode` | Decode and drop one whole size cycle (1, 2 or 8 messages) | Build wire data; prime and verify two cycles; fixed 1 MiB message limit |
| `masked_tcp_receive` | Deliver/drop 1024 messages, including producer wakeup; excludes final producer join | Runtime/socket setup and one full-payload warm-up check |
| `client_mask_tcp` | Send and deliver 1024 messages using send or feed/flush batches; includes peer join | Runtime/socket setup and one full-payload warm-up check |
| `deflate_input_tcp`, `deflate_capacity_tcp` | Deliver/drop 256 messages, including producer wakeup and final join | Runtime/socket setup; verify first/repeated blocks or cycles |
| `deflate` | One compression attempt (possibly `None`) or one decompression, including output destruction | Seeded input generation; verify compressed output when present; omit decode case if compression is declined |
| `deflate/core/json_*` | One message encoded or decoded, with destruction inside timing; advance through a 64 KiB corpus | Generate input and compressed periods; prime dictionaries and validate repeated decoding plus actual history dependency for takeover |
| `deflate/extended/configuration` | Encode one message from a structured JSON cycle, including output destruction; compare whole presets | Generate matching inputs; prime and validate the preset encoder/decoder over one cycle |
| `deflate/core/random_reset` | One compression attempt, including output destruction if compression is accepted | Generate seeded input and verify compressed output when present |
| `deflate/extended/encode_*` | Four successive compression attempts, including output destruction | Seeded/repeated input generation and matching encoder/decoder validation |
| `publish_drain` | One publication plus draining all 1/100/1000 recipients | Create memberships and verify initial delivery |
| `shared_pool` | One compression per worker, amortized across a synchronized batch; includes barrier release and joins | Create threads, initialize/validate encoders and pool; all workers reach an untimed barrier before release |
| `finite_churn` | One publication/drain plus one subscribe/unsubscribe pair in a second thread, amortized across the batch; includes barrier release and join | Create threads, subscribers and pool-independent PubSub state |

TCP cases use four Tokio workers, a caller driving `block_on`, and an in-process peer/producer scheduled on those workers. They use loopback with TCP_NODELAY, without HTTP/TLS handshakes. Payload equality is checked during warm-up; timed loops require successful message delivery and consume the configured count, but do not compare every payload. Masked receive offers default-heartbeat and timers-off cases; the other TCP cases disable heartbeat timers. These are saturated batch diagnostics, not paced arrival or production tail-latency measurements.

The shared-compression case has 1/4/8/16 worker threads calling the same synchronous pool. PubSub churn is on a separate topic and runs a finite batch: either side can finish first, so this is combined batch completion time, not publication latency under guaranteed continuous contention.

For async multi-worker TCP trials, provide separate physical cores for workers and the caller, with capacity for the peer/producer, rather than pinning the whole process to a single CPU. For threaded pool trials, match physical cores to worker count and leave capacity for the coordinator. On smaller machines, oversized worker counts are oversubscription stress cases; smoke execution remains useful for correctness but supplies no normal multi-core performance evidence. Keep single-thread comparisons on an isolated core. Interleave fresh baseline/candidate runs and calibrate repeatability before interpreting differences.
