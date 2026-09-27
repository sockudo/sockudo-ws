# Stream benchmarks and diagnostics

The default inputs are synthetic; `receive_state_bench` also accepts a caller-supplied UTF-8 fixture. Build and compare revisions with the same profile, features and harness sources. Separate checkouts need independent target directories; one checkout can reuse its target when sequential builds are verified and executables are preserved before switching revisions. Preserve raw runs, A/A controls, paired execution order, executable hashes and runtime configuration.

## Stream benchmark

`stream_bench` uses Criterion to compare unified and split reads and writes over Tokio duplex and TCP transports. Each Criterion iteration contains 1,024 messages; reported time is per batch, not per message. Socket/runtime setup, split construction and final peer join are outside the timer. A single timed loop processes all iterations requested by Criterion on one connection.

```sh
cargo bench --locked --bench stream_bench
```

The benchmark separates timer configurations and includes masked receive sizes around framing boundaries. TCP loopback results do not represent TLS, a physical NIC, an application handler or production tail latency. Multi-worker cases need enough physical cores for runtime workers, the benchmark driver and the peer.

| Path | Timed boundary and validation |
| --- | --- |
| `read_unified`, `read_split` | Consume the requested number of messages; retain the last message for a full-payload check outside the timer. Earlier messages are dropped inside it. The raw producer can start filling the transport before the reader timer starts; this is a receive-drain cost, not emission-to-delivery latency. |
| `send_unified`, `send_split` | Clone/send each message through local completion. The raw peer checks every frame's bytes concurrently; the final join is outside the timer, so final peer delivery is not included. |
| `feed_16` | Feed 16 messages then flush; includes message clones and local flush completion. The same concurrent peer validation applies. |
| `prototype_direct`, `prototype_batch_16` | Data-only protocol/write models, including encoding and local flush. The batch model includes Vec/channel/oneshot costs. They omit production heartbeat, control ownership and close handling and are not API-equivalent replacements for split. |

`current_thread` drives the caller and peer on one runtime thread. `worker_1` and `worker_4` spawn the caller onto their respective runtime; `caller_4` drives the caller through `block_on` outside the four workers. Peer/driver tasks share the runtime workers. The prototypes are control paths, not proposed production optimizations. Append `-- --test` to run the full Criterion smoke matrix without timing samples.

## Controlled receive state

`receive_state_bench` always measures the native split reader. It separates read readiness, input batching, dispatch and retained message ownership from socket scheduling; constructing a unified stream here does not mean the timed path is unified:

```sh
cargo bench --locked --bench receive_state_bench --no-run
# executable arguments:
# fixture connections frames_per_read ready|pending retain typed|boxed
```

Use `-` for the built-in JSON payload. With no arguments the executable runs a short synthetic case. The current-thread runtime processes connections round robin. `ready` provides bytes immediately; `pending` injects one self-waking `Pending` before each read and is not a socket-readiness model. The source repeats the payload in a prebuilt Text frame batch, allowing partial reads when the destination cannot hold the batch. Thus `frames_per_read` is a source-batch size, not a guarantee for arbitrarily large payloads. `typed` uses the concrete input type; `boxed` wraps it in `Stream<Http1>` before creating the same native split reader.

Each connection warms for 512 messages before 64 blocks of 256 messages per connection. Timing includes round-robin reads, retaining messages in a VecDeque and dropping messages beyond the retention limit. Full payloads are checked during warm-up and after all blocks, including when retention is zero; timed blocks do not compare every payload. CSV values are block-average nanoseconds per message, not message tails. Native heartbeat monitoring remains enabled. Treat processes, rather than blocks, as independent replicates. The default run exercises functionality and prints diagnostic times; it is not an A/B comparison or performance gate.

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
  --case 4 split 1 64 2048 default
# arguments: workers unified|split connections burst count default|off
cargo run --locked --release --example send_latency_diagnostic -- --matrix
```

The output separates local send completion, pre-send-to-peer-delivery latency, scheduled lateness and per-connection P99-P1 spreads. The send timer surrounds the send call; the delivery timestamp is embedded immediately before that call and therefore also includes the remaining payload setup. It is not a kernel or wire-emission timestamp. Sender and receiver use the same process-wide monotonic epoch. Warm-up checks the Binary payload; measured messages validate length, sequence and padding, and total send/delivery counts must match.

A zero burst selects saturated traffic and leaves scheduling columns empty. Positive bursts offer 1,000 messages/second per connection, with each send still awaiting local completion; late senders catch up rather than dropping planned messages. Each connection chooses its start after the warm-up barrier, so starts are not an exact shared phase. Absolute scheduled-lateness percentiles depend on timer phase. Use per-connection spreads, send and delivery latency, a separate phase sweep and a control path before attributing a change to scheduler fairness. This tool does not implement that phase sweep or establish a fairness gate.

Percentiles use nearest rank within one case. Send/delivery percentiles pool connections from that case, not independent runs; scheduled/sender-late spread columns report the maximum per-connection P99-P1. Do not pool samples across processes. `messages` is the total sample count; per-connection spread uses `count` observations. The smoke case has only 128 messages per connection (about 1.28 observations in the upper 1%) and, for burst 64, only two burst-first samples. Its percentiles are diagnostics only. For larger runs report support separately for burst-first/nonfirst subsets: per connection they contain `ceil(count / burst)` and `count - ceil(count / burst)` samples.

The default smoke covers workers 1/4, unified/split, one connection and bursts 0/64. `--matrix` adds 16 connections and burst 1 with longer runs; `--case` selects timer mode explicitly. Neither mode changes CPU affinity. Use isolated single-thread placement for receive-state cases and runtime-matched physical cores for multi-worker diagnostics. A smoke pass does not qualify the machine or establish a performance result.
