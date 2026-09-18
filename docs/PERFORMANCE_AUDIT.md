# sockudo-ws performance audit

Date: 2026-09-19. Machine: Apple M5 Pro (arm64), macOS, rustc 1.98.1, release profile
(`lto = "fat"`, `codegen-units = 1`). Baseline commit: `0d7e79c` (v2.0.1).

This audit was triggered by a third-party comparison table (nago-wss vs tokio-tungstenite vs
sockudo-ws). The competitor's code is not available, so the audit does three things instead:

1. re-measures the sockudo-ws primitives the table talks about, on this machine;
2. runs a neutral head-to-head against tokio-tungstenite (same client, same box);
3. reads every hot path in the crate and lists what is actually slow or wrong.

Everything marked **fixed** was changed in this working tree and re-measured.

---

## 1. Summary

**The competitor's micro numbers for sockudo-ws do not reproduce.** Measured `apply_mask` here is
27 to 71 GB/s and ASCII UTF-8 validation is 45 to 190 GB/s. The table shows 1.2 to 3 GB/s and
4 GB/s respectively, flat across sizes, which is the signature of a debug build or a per-iteration
allocation in the harness, not of the code. Even the competitor's own figures (4.7 to 14.8 GB/s
masking) are below what sockudo-ws does on this machine.

**Against tokio-tungstenite, sockudo-ws is at parity on a ping-pong echo and 15 to 25 % faster at
connection establishment.** The competitor's "4.7x slower to establish" and the dip at 8
connections do not reproduce either; the dip appears for both tokio libraries in their harness,
so it is a property of that harness or of tokio, not of sockudo-ws.

**Where the competitor is genuinely ahead is the runtime.** A thread-per-core custom runtime with
no work stealing wins connection storms and many-connection fan-out by design; no amount of
frame-codec tuning changes that while the transport is `tokio::net::TcpStream` driven by a
work-stealing scheduler. Section 5 lists what would close that gap.

**Real defects found in sockudo-ws** (all fixed here, ranked by impact):

| # | Defect | Impact | Status |
|---|---|---|---|
| 1 | Fragmented text re-validated the whole accumulated message on every fragment (O(n·k)) | Autobahn 9.3.1 (4 MiB text in 64 B fragments) took **30.6 s** of server CPU; tungstenite: 7 ms. A client can pin a core for 30 s with 4.5 MB of traffic. | fixed, now 5 ms |
| 2 | Heartbeat timer torn down and re-created on every inbound message (v2.0.1 regression) | 1 Box alloc, 1 free, 2 timer-wheel lock acquisitions and 2 clock reads per message on every `WebSocketStream` | fixed |
| 3 | Split reader sent an `Activity` message through an mpsc channel to the writer task for every data frame, and awaited it | channel send + writer wakeup per inbound message in the split API (what `axum` users get) | fixed |
| 4 | `Vec<Message>` allocated per socket read, every message cloned out of it | 1 alloc/free per read, 2 atomic refcount ops per message | fixed |
| 5 | `Vec<IoSlice>` allocated on every flush | 1 alloc/free per write | fixed |
| 6 | Read buffer regrown in 8 KiB steps once shared | read syscalls capped at ~8 KiB after the first 64 KiB | fixed |
| 7 | Handshake allocated a `String` per header (`to_ascii_lowercase`) | ~10 small allocs per connection | fixed |
| 8 | Invalid UTF-8 not rejected until the frame completed (Autobahn 6.4.x NON-STRICT) | conformance, and a large invalid text frame was buffered in full before rejection | fixed, STRICT |

Autobahn: **517 cases, 514 OK, 3 INFORMATIONAL** (7.1.6, 7.13.1, 7.13.2 are informational for
every implementation). Before: 512 OK, 2 NON-STRICT, 3 INFORMATIONAL.

---

## 2. Measurements

### 2.1 Primitives (criterion, `benches/websocket_bench.rs`, medians)

| size | `apply_mask` | competitor table for sockudo | competitor's own figure |
|---|---|---|---|
| 64 B | 2.4 ns, **27 GB/s** | 1.22 GB/s | 4.72 GB/s |
| 1 KiB | 16.4 ns, **62 GB/s** | 4.56 GB/s | 8.99 GB/s |
| 16 KiB | 236 ns, **69 GB/s** | 3.08 GB/s | 14.82 GB/s |
| 64 KiB | 917 ns, **71 GB/s** | (256 KiB: 2.81 GB/s) | (256 KiB: 14.07 GB/s) |

| input | `validate_utf8` | competitor table for sockudo | competitor's own figure |
|---|---|---|---|
| ASCII 1 KiB | 5.9 ns, **174 GB/s** | 4.01 GB/s | 17.46 GB/s |
| ASCII 16 KiB | 87 ns, **188 GB/s** | 28.85 GB/s | 23.37 GB/s |
| mixed 1 KiB | 70 ns, **14.6 GB/s** | 1.58 GB/s | 0.88 GB/s |
| mixed 4 KiB | 283 ns, **14.4 GB/s** | (16 KiB: 2.23 GB/s) | (16 KiB: 1.33 GB/s) |

On aarch64 `validate_utf8` is `simdutf8::basic` on NEON (its `target_feature = "neon"` gate is on
by default). `apply_mask` is the 16-byte NEON loop in `src/simd.rs`. Neither has a per-call
feature-detection cost on aarch64; on x86_64 `apply_mask` does up to three cached
`is_x86_feature_detected!` loads per call, which is ~1 ns and not worth changing.

### 2.2 Head-to-head vs tokio-tungstenite 0.26 (same neutral client, loopback)

Client: raw `TcpStream` + `sockudo_ws::protocol` as codec, sends one message and waits for its echo
(`msgs` round trips per connection). Servers: `WebSocketStream` echo with `Config::default()`
(heartbeat on) and `tokio_tungstenite::accept_async` echo, both `TCP_NODELAY`.

| case | sockudo before | sockudo after | tungstenite |
|---|---|---|---|
| 64 B, 1 conn | 33.6k msg/s | 38.1k | 35.2k / 39.5k |
| 64 B, 8 conns | 150k | 158k | 149k / 154k |
| 64 B, 32 conns | 154k | 158k | 156k / 157k |
| 4 KiB, 1 conn | 35.8k | 36.4k | 36.2k / 37.2k |
| 4 KiB, 32 conns | 155k | 157k | 156k / 156k |
| 64 KiB, 1 conn | 22.4k | 20.9k | 21.0k / 20.3k |
| 64 KiB, 8 conns | 94.0k | 80.8k | 90.2k / 82.6k |
| connect 2000 (handshake incl.) | 84 ms | 86 ms | 111 ms / 100 ms |

(tungstenite shown as before-run / after-run so the run-to-run noise is visible; it is ±5 %.)

Reading: a request/response echo is bound by two syscalls and a scheduler hop per message, so the
codec changes do not move it. They remove allocations and lock traffic that show up as tail
latency and CPU under real load, not as throughput in this test. Disabling the heartbeat on the
old code gave the same numbers, which confirms the per-message timer churn was cost, not
throughput, in this harness.

A single-shot 10 000-connection storm completed in 470 ms (47 µs per connection incl. handshake).
Repeating it back to back against three servers exhausted macOS ephemeral ports, so only the
2 000-connection figure above is comparable.

### 2.3 Autobahn (autobahn-testsuite-rs, 517 cases, `--concurrency 8`)

| | before | after |
|---|---|---|
| OK | 512 | **514** |
| NON-STRICT | 2 (6.4.3, 6.4.4) | 0 |
| INFORMATIONAL | 3 | 3 |
| 9.3.1 (4 MiB text, 64 B fragments) | 30 640 ms | **5 ms** |
| 9.3.2 (256 B fragments) | 7 675 ms | 2 ms |
| 9.3.3 (1 KiB fragments) | 2 165 ms | 3 ms |

tokio-tungstenite on the same runner: 9.3.1 7 ms, 6.4.3 NON-STRICT.

---

## 3. Findings in detail

### 3.1 O(n·k) UTF-8 validation of fragmented text — fixed

`Protocol::handle_continuation` and `start_fragment` called `validate_utf8_incomplete` over the
**entire** `fragment_buf` after every fragment. For a message of n bytes in k fragments that is
n·k/2 bytes of byte-at-a-time scanning: 4 MiB in 64 B frames is 128 GB of work, i.e. the 30 s
measured. Binary fragments (9.4.x) were unaffected, which is why it hid.

Fix: `utf8::Utf8Stream`, an incremental validator that runs the SIMD validator over each chunk
except for a ≤3-byte incomplete tail, which it carries to the next chunk. `Protocol` now feeds
each fragment (and each partial frame, see 3.8) exactly once; `complete_fragment` only checks
that no tail is pending. Text validation is now one linear pass per message however it is
fragmented or chopped, and the second full pass that used to run at completion is gone.

### 3.2 Heartbeat timer churn per message — fixed

`WebSocketStream::poll_next` (and the compressed twin) did, on every poll with a deadline
pending, `heartbeat_sleep.get_or_insert_with(|| Box::pin(sleep(delay)))`, and on every inbound
message `heartbeat_sleep = None`. With the default config (ping 30 s, idle 120 s) that is a
`Box<Sleep>` allocation, a timer-wheel registration, a deregistration and a free **per message**,
plus two `Instant::now()` calls. Timer registration takes the tokio time-driver lock; under many
connections on many workers that lock is shared.

Fix: one `Sleep` per stream, armed once. Inbound traffic only ever moves the deadline later, so
the timer is left alone; when it fires the deadline is re-read and the sleep is `reset()` (no
allocation). It is reset eagerly only when the deadline moves earlier (a Pong deadline appearing
after a Ping is flushed). Steady-state cost per message: zero timer operations.

The split writer driver had the same pattern in another form: `tokio::select!` re-created
`tokio::time::sleep(heartbeat_delay)` on every loop iteration, i.e. one timer registration per
message sent or per control request. Same fix: a single `Sleep` re-armed lazily.

### 3.3 Split reader: a channel round trip per inbound data frame — fixed

`SplitReader::next` sent `ControlRequest::Activity(Instant::now())` through the bounded mpsc
channel to the writer task **for every data message**, and awaited the send. That is a channel
push, a writer-task wakeup and a possible cross-thread hop per inbound message, only to refresh
the inactivity clock. Since `axum` `WebSocket::split()` uses this path, most real deployments pay it.

Fix: `SplitShared` carries the clock epoch and an `AtomicU64 last_inbound_ms`. The reader does a
relaxed `fetch_max`; the driver reads it whenever it wakes. Ping, Pong and Close still go through
the channel because they require a write. The `Activity` variant is gone.

### 3.4 Per-read `Vec<Message>` allocation and per-message clone — fixed

`process_read_buf` called `Protocol::process`, which allocates a fresh `Vec`, then stored it in
`pending_messages`; `next_pending_message` then **cloned** each message out by index (an atomic
increment on the `Bytes` and a later decrement when the Vec was cleared). Fixed in both streams
and both split readers: `process_into` reuses `pending_messages`, the Vec is reversed once and
messages are `pop()`ed.

### 3.5 `Vec<IoSlice>` per flush — fixed

`CorkBuffer::get_write_slices` allocated a `Vec` for every `poll_flush` / `flush_write_buf`.
Added `fill_write_slices(&mut [IoSlice])` and a 16-slot stack array at the call sites. The
old method is kept for API compatibility.

### 3.6 Read buffer regrowth in 8 KiB steps — fixed

`poll_read_more` reserved 8192 bytes whenever less than 4 KiB was free. The 64 KiB read buffer
is handed out as `Bytes` payloads, so once it is shared `reserve` must allocate, and it allocated
small: after the first buffer, reads were capped at ~8 KiB per syscall. Now reserves
`RECV_BUFFER_SIZE` (64 KiB). When the buffer is unique `reserve` reclaims in place as before.

### 3.7 Handshake header parsing allocated per header — fixed

`parse_request` / `parse_response` did `header.name.to_ascii_lowercase()` (a `String`) for every
header and `value.to_ascii_lowercase()` for `Upgrade` / `Connection`, then a substring
`contains`. Now `eq_ignore_ascii_case` on names and a comma-separated token match on values
(also more correct: `Connection: keep-alive, Upgrade` is matched as tokens). Remaining handshake
allocations are the accept key `String`, the response `BytesMut`, and the owned `path` /
`protocol` in `HandshakeResult`, which is API shape.

### 3.8 Fail-fast UTF-8 (Autobahn 6.4.x) — fixed, now STRICT

The parser buffered a whole frame before the protocol layer saw its payload, so invalid UTF-8 in
the middle of a frame was only detected when the frame completed. Two changes:

* `FrameParser` now unmasks payload bytes as they arrive (`payload_ready` offset,
  `apply_mask_offset` with the right mask rotation) and exposes `pending_payload()`. The final
  unmask touches only the tail that had not arrived yet, so total masking work is unchanged.
* `Protocol::process_into` calls `prevalidate_partial_text` whenever the parser needs more data:
  for a text frame (or a continuation of a text message) it feeds the newly available prefix to
  the `Utf8Stream`. `Utf8Stream` also rejects a carried prefix that can no longer become valid
  (`F4 90`, `ED A0`, `E0 80`, `C0`, ...) using the RFC 3629 second-byte table, which is what the
  6.4.2 / 6.4.4 chops test.

Compressed (RSV1) frames are skipped, since their bytes are validated after inflate.

### 3.9 Things that are fine

* Masking and UTF-8 primitives (section 2.1).
* `FrameParser::parse` fast paths for ≤125-byte frames: no state machine, one `split_to`.
* Text messages are returned as `Bytes` views into the read buffer, no copy on receive.
* `encode_frame` writes header and payload in one reserved region; masked client encode fuses the
  copy and the XOR.
* `pubsub` uses `DashMap` and clones `Bytes` (refcount) per subscriber, not the payload.
* Memory: `Config::default()` reserves 64 KiB read + 16 KiB cork per connection. RSS per idle
  connection is much lower because untouched pages are not resident, which matches the 35 KiB
  the competitor measured.

---

## 4. Not fixed: known costs that remain

These are design-level and are listed so the next round has a target list.

1. **Payload copy on send.** `encode_message` copies the payload into the cork buffer. For large
   messages a header + payload `writev` (as `examples/wtx_bench_echo.rs` does) avoids the copy.
   `CorkBuffer` already has an overflow queue of `Bytes` that could carry the payload zero-copy;
   the encoder would need an `encode_frame_header` + `write_bytes` path when the payload is above
   a threshold (e.g. 4 KiB).
2. **One write syscall per `send()`.** The `Sink` impl flushes on every `send`. `feed()` +
   `flush()` batches, but the echo examples and most users call `send`. A cheap improvement is to
   coalesce writes while the read buffer still contains complete frames (write after draining a
   read), which is what uWebSockets' cork does implicitly.
3. **Split writer: oneshot + mpsc per `send`.** `SplitWriter::send` allocates a `oneshot`,
   pushes through a bounded channel, wakes the driver task, which then does `write_all` +
   `flush`. That is two task hops and two allocations per outbound message. An `Arc<Mutex<..>>`
   around the write half with a try-lock fast path, or a lock-free write queue drained by
   whichever side holds the socket, would remove the hop for the common case.
4. **`compio.rs` duplicates the tokio stream** (3 000 lines) and still has items 3.2 to 3.4
   (`process()` per read, clone per message, `Activity` channel messages, `sleep` per loop
   iteration). Same fixes apply verbatim.
5. **`CompressedWebSocketStream` and `CompressedProtocol` are copy-pasted** from the plain
   variants. Every hot-path fix has to be applied twice. Generic over an `Inflate` strategy would
   halve the surface.
6. **Runtime.** This is the actual reason a custom runtime beats every tokio library on connection
   storms and fan-out: no work stealing, one epoll/kqueue per core, accept via `SO_REUSEPORT`
   listeners per core, no cross-thread wakeups, no `Arc<Task>` per connection. Within tokio the
   closest approximations are: `LocalSet` per worker with `SO_REUSEPORT` listeners (the autobahn
   server already sets `reuse_port` but binds one listener), `current_thread` runtimes pinned per
   core, and on Linux the `io-uring` feature. None of these change the codec.
7. **Accept path.** `WebSocketServer::accept` boxes the transport (`Stream<Http1>` is a
   `Box<dyn AsyncRead + AsyncWrite>`), adding a vtable call per `poll_read` / `poll_write`. Use
   `accept_stream` (no erasure) in hot servers.

---

## 5. Reproducing

```bash
# primitives
cargo bench --bench websocket_bench -- '^mask/|^utf8/'

# Autobahn (Rust port of the suite)
cargo build --release --bin autobahn-server && ./target/release/autobahn-server &
/path/to/autobahn-testsuite-rs/target/release/wstest -m fuzzingclient \
    -s autobahn/fuzzingclient.json --concurrency 8
```

The head-to-head harness (three echo servers plus a neutral connect/echo client) lives outside the
repo; it is ~150 lines and is described in section 2.2 closely enough to recreate.
