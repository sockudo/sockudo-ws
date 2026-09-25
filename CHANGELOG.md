# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Compressed Tokio streams now accept post-handshake frame bytes through `client_with_leftover` and `server_with_leftover`, including when split before the first read; existing constructors continue to start with an empty receive buffer.
- `WebSocketServer<Http1>::protocols` configures HTTP/1 subprotocol selection in server preference order while preserving the existing first-offered default when no list is configured.

### Changed

- **Breaking:** Built-in Tokio and Compio HTTP/1 server handshakes validate request targets and normalize absolute HTTP/HTTPS targets to a resource path and query. `HandshakeRequest.path` is now `Cow<'_, str>` (use `.as_ref()` for a borrowed `&str`), and `host` reports the absolute target's authority when present. A Host header remains required. The existing `http` dependency is now mandatory, including in default-feature builds. Axum's upgrade extractor is unaffected. Path/query characters retain `http::Uri` compatibility rules (including raw UTF-8 and JSON path characters), with additional strict `%XX` validation; this is not full RFC 3986 validation. Invalid percent escapes, fragments, unsupported target forms/schemes, userinfo, empty absolute hosts and nonnumeric ports now return errors.
- Built-in Tokio HTTP/1 URL clients and listener servers enable `TCP_NODELAY` on their TCP sockets. Client socket-option errors propagate; the listener server reports failures for the affected connection and continues its handshake. Caller-provided streams retain their socket settings.
- **Breaking:** Built-in Tokio and Compio HTTP/3 endpoints now apply configured QUIC idle timeout, stream receive window and maximum accepted UDP payload size. Default window (1,250,000 bytes) and payload limit (1472 bytes) preserve Quinn's previously implicit defaults. Extended CONNECT can be disabled; unsupported 0-RTT requests are rejected and early data is disabled on endpoints created by the library. Caller-provided endpoints retain their transport and TLS settings. Previously ignored out-of-range QUIC limits, a zero stream receive window, and `enable_0rtt = true` now return errors.
- **Breaking:** native io_uring read/write methods now require mutable access to preserve ordering with poll I/O, so these methods no longer support concurrent reads and writes through a shared stream. Exclusively direct I/O through `get_ref` still supports concurrent reads and writes, provided it is never mixed with bridge I/O or the wrapper's native methods.
- `io_uring::has_recommended_kernel()` now checks Linux 5.10 or later, matching tokio-uring 0.5's minimum requirement; Linux 5.6–5.9 now returns `false`.
- PubSub selects publication recipients atomically with membership changes and enqueues messages after releasing the membership lock. Removal after selection does not cancel that publication's already selected deliveries; socket-ID exclusion uses the same snapshot.
- **Breaking:** Compio 0.19 HTTP/2 entry points require Splittable; wrap other transports with compio::io::util::Split::new. Automatic Ping requires pending custom reads to cooperate with cancellation; an existing idle/Pong deadline remains terminal. With idle timeout disabled, nonzero pong_timeout also bounds read-buffer recovery from Ping's due time; expiry reports HeartbeatTimeout even if Ping has not been sent. Setting both timeouts to zero leaves recovery unbounded.
- **Breaking:** DEFLATE encoder windows use `DeflateWindowBits` (9–15), including the public window constants and codec configuration fields. An unsupported 8-bit encoder limit is rejected instead of panicking or widening it; server negotiation can still receive an 8-bit client stream with a larger decoder window. A server policy below 15 client window bits declines a `permessage-deflate` offer that omits `client_max_window_bits` rather than exceeding the configured policy. Public offer parsing now rejects duplicate or empty parameters, malformed quoted values, leading zeroes, and non-ASCII optional whitespace.

- Tokio `SinkExt::send()` and `SinkExt::flush()` now always complete the transport flush, including while parsed inbound messages remain unread. Use `feed()` followed by `flush()` to batch frames, and flush before waiting for replies or pausing reads. Readiness drains at the smaller of the high-water mark and `max_backpressure`, or before accepting another frame when `write_coalescing` is disabled; partial drains still wait for the transport flush to complete.
- Server-side data payloads of 8 KiB or more are queued by reference behind
  their frame header and sent with vectored I/O instead of being copied into
  the write buffer (`CorkBuffer::push_segment`, `cork::ZERO_COPY_MIN`).
- `CorkBuffer` is now an ordered list of `Bytes` segments plus an open tail
  buffer; `write_bytes` keeps output order and `write` no longer spills into a
  separate overflow queue.
- `SplitWriter` / `CompressedSplitWriter` write directly to the transport
  through a sink shared with the connection's control driver, instead of a
  channel plus a oneshot completion per `send()`. Automatic Pong/Ping/Close
  frames interleave at frame boundaries. `SplitWriter::send` now requires
  `S: AsyncWrite + Unpin` (which `split()` already required).
- Frame masking uses an auto-vectorised 64-byte block loop on aarch64 and
  other non-x86 targets (aligned above 2 KiB): 1 KiB 53 -> 84 GB/s,
  16 KiB 67 -> 127 GB/s, 64 B 12 -> 19 GB/s on an Apple M5 Pro.
- Compio streams and split readers reuse the message Vec across reads, pop
  messages instead of cloning them, and publish inbound activity through a
  shared cell instead of a channel message per data frame.

### Fixed

- Accepted non-final data frames, including empty continuations and compressed fragments, refresh inbound activity for Tokio and Compio streams and split readers. Partial frame bytes and repeated polls do not extend inactivity deadlines, and fragment activity does not postpone an outstanding Pong or Close deadline.
- HTTP/1 `Stream` forwards vectored writes and reports the underlying transport's vectored-write capability; Axum `UpgradedStream` now reports that capability as well. Partial writes, pending operations and transport errors retain their underlying semantics.
- HTTP/1 handshake nonces now use the selected RNG backend (`getrandom`, then `rand_rng`, then `fastrand`) instead of a timestamp-seeded byte loop. The default fastrand nonce generator forks the thread RNG once, then keeps separate state from frame masking; no-RNG builds also keep separate fallback states. Native fastrand seeds from a clock and thread ID, not OS entropy. These non-cryptographic backends do not provide a cryptographic isolation guarantee; use `getrandom` or `rand_rng` when cryptographically secure output is required.
- Tokio HTTP/3 servers now advertise `SETTINGS_ENABLE_CONNECT_PROTOCOL = 1` when Extended CONNECT is enabled, including with the default configuration.
- Drive io_uring completion operations across poll calls, flush buffered writes before shutdown, and enable the required Tokio integration for the `io-uring` feature.
- PubSub subscriber, socket-ID, and topic indexes now update atomically, preventing duplicate socket IDs and stale membership under concurrent changes. Publication and removal release the membership lock before waking channel receivers so their wakers can reenter membership operations.
- Cancelled native Compio HTTP/3 DATA writes now abort both directions of the affected WebSocket stream with `H3_REQUEST_CANCELLED`; subsequent operations on that stream fail with `ConnectionAborted`, while the multiplexed connection can open new streams.
- Built-in HTTP/1 WebSocket handshakes now reject repeated request `Sec-WebSocket-Key` and `Sec-WebSocket-Version` fields and repeated response `Sec-WebSocket-Accept` and `Sec-WebSocket-Extensions` fields. Repeated response `Sec-WebSocket-Protocol` was already rejected; request protocol and extension field handling is unchanged.
- Built-in HTTP/1 WebSocket server handshakes now reject nonzero or invalid `Content-Length` values and any `Transfer-Encoding` request header; absent and zero-valued lengths remain accepted. Checked HTTP/1 client request construction now rejects custom `Content-Length` and `Transfer-Encoding` headers.
- The built-in HTTP/1 handshake parsers and Tokio/Compio clients and servers now require HTTP/1.1, a nonempty request Host, a request key that decodes to 16 bytes, and an exact `Upgrade: websocket` response with a `Connection: Upgrade` token. Checked request construction, including client connect with an empty Host, now rejects invalid required fields before sending; token matching treats only SP/HTAB as optional whitespace. The separate Axum upgrade extractor is unchanged.
- HTTP/1 upgrade handshakes enforce the 8 KiB limit on the request or response header itself, not on WebSocket frame bytes read with it; oversized incomplete headers remain rejected.
- Frame parsers with compression enabled, including unified and split readers, now reject RSV1 on continuation and control frames as soon as the base header arrives, without waiting for the payload; RSV1 remains valid on the first text or binary frame of a compressed message.
- Splitting a Tokio or Compio stream now preserves partially parsed frames and receive-side fragment/UTF-8 state. Compressed protocol splitting retains parser progress while applying the supplied frame and message limits, including a lowered frame limit for an already accepted header.
- Frame size limits now apply equally to complete and partially received short frames. Single-frame text and binary messages honor the message size limit in typed, raw, and compression-capable protocols, including uncompressed input to compressed readers. Exact-limit payloads remain accepted.
- Resuming typed protocol processing during a fragmented text message now validates bytes accumulated by raw calls without losing split UTF-8 code points. Messages completed through the raw API remain unvalidated.
- Tokio Sink readiness now drains queued encoded output at `max_backpressure`, continuing partial drains before accepting another message. This is a soft queue threshold, not a message size limit: individual messages may exceed it, and zero drains any pending output. `feed`, `send_all`, and `forward` may now wait for a slow reader; blocked unified writes do not drive inbound heartbeat/idle processing. The high-water mark can trigger readiness draining earlier when batching is enabled. A completed transport flush also clears a cancelled readiness drain.
- Explicit `close()` on unified Tokio and Compio HTTP/2/HTTP/3 streams now shuts down the transport write half after flushing the WebSocket Close frame, preserving queued frames when the handler releases its stream. TCP/TLS streams keep the write half open until the peer's Close so crossing Pings can still receive a Pong. Directly constructed multiplexed streams must opt in with `with_immediate_write_shutdown()`; built-in HTTP/2 and HTTP/3 entry points do so automatically. Repeated Sink close no longer repeats transport shutdown.
- Unified closing now uses one `close_timeout` budget (5 seconds by default) for local Close writes, peer response, automatic control writes, and best-effort shutdown. A peer that remains silent after the final read attempt ends with `ConnectionClosed` once; cleanup failure or expiry preserves an already accepted Close or the original idle/Pong timeout. Deadline expiry alone preserves parsed messages in wire order, but control write failure/timeout still terminates immediately and may discard undelivered Ping/data messages; queued Close suppresses preceding automatic Pong writes. All budgets allow one nonwaiting read poll after expiry, not a fresh attempt on every `next()`; cancelled Compio owned reads are not restarted and completion depends on the transport driver. Compio application sends are rejected after local Close, and `flush()` on a closed unified stream returns `ConnectionClosed`, even with no buffered output, so timed-out owned I/O is never restarted. Compio `next()` remains cancellation-unsafe, including during Close cleanup.
- Shared compression contexts now reuse role-aware encoder pools instead of allocating four encoders per connection while preserving connection-local decoders. Client contexts honor `client_max_window_bits`.
- Compio split writers now keep hard idle/Pong and closing deadlines active while a transport write is pending. EOF aborts immediately, while peer Close and parse errors allow the existing write to finish within `close_timeout`; with `close_timeout = 0`, an immediately writable Close still gets one best-effort poll. A send-only connection that receives no inbound frames now reliably reaches the configured idle timeout (120 seconds by default).
- UTF-8 validation no longer rejects valid multi-byte characters that cross internal SIMD block boundaries on SSE2-only x86 or nightly LoongArch64, PowerPC, and s390x paths; complete inputs now use `simdutf8` and its portable fallback where no dedicated backend exists.

## [2.1.0] - 2026-09-19

### Added

- Added custom HTTP headers to Tokio and Compio HTTP/1.1 client handshakes, with validation that
  prevents malformed fields and conflicts with handshake-managed headers
  (`build_request_with_headers`, `client_handshake_with_headers`, `connect_with_headers`,
  `connect_raw_with_headers`, `connect_to_url_with_headers`, `connect_async_with_headers`). (#17)

### Changed

- The `rustls-*` features no longer select a crypto provider. `rustls` is depended on with only
  `std`, and `tokio-rustls` with `logging` and `tls12`. Applications must enable exactly one
  provider (`ring` or `aws-lc-rs`) on their own direct `rustls` dependency; see the README.
  Sockudo's tests select Ring through a dev-dependency. (#11)

### Fixed

- permessage-deflate with context takeover (`Compression::Dedicated` and the `WindowNKB` modes)
  silently corrupted or killed the stream after any message that did not shrink when compressed:
  the raw message stayed in the sender's LZ77 window but never entered the peer's. Such messages are
  now always sent compressed under context takeover, costing ~6 bytes on incompressible frames. (#14)
- Preserved WebSocket frame bytes read together with Tokio HTTP/1.1 upgrade requests or responses,
  including when the stream is split immediately after connecting. (#17)

## [2.0.2] - 2026-09-19

### Fixed

- Fragmented text messages were re-validated as UTF-8 over the whole
  accumulated message on every fragment (O(n·k)); a 4 MiB text message in
  64-byte fragments (Autobahn 9.3.1) took ~30 s of server CPU. Text is now
  validated incrementally in one linear pass (`utf8::Utf8Stream`).
- Invalid UTF-8 is rejected as soon as it arrives, including mid-frame
  (Autobahn 6.4.x now STRICT). The frame parser unmasks payload bytes as they
  arrive and exposes them via `FrameParser::pending_payload`.
- `WebSocketStream` / `CompressedWebSocketStream` re-created the heartbeat
  timer on every inbound message (v2.0.1 regression). One timer per stream is
  now re-armed lazily; steady-state cost per message is zero timer operations.
- The split reader no longer sends an activity message through the writer
  channel for every inbound data frame; it publishes the inactivity clock via
  an atomic. `ControlRequest::Activity` was removed (internal).
- The split writer driver re-registered its heartbeat sleep on every loop
  iteration; it now keeps a single `Sleep`.
- Removed per-read `Vec<Message>` allocation and per-message `Message` clone
  in the streams and split readers; removed the `Vec<IoSlice>` allocation per
  flush (`CorkBuffer::fill_write_slices`).
- Read buffer regrowth now reserves `RECV_BUFFER_SIZE` instead of 8 KiB, so
  reads are no longer capped at ~8 KiB once the buffer has been shared out.
- Handshake header parsing no longer allocates a `String` per header, and
  `Upgrade` / `Connection` are matched as comma-separated tokens.

### Added

- `utf8::Utf8Stream`, `frame::PendingPayload`, `FrameParser::pending_payload`,
  `CorkBuffer::fill_write_slices`.
- `docs/PERFORMANCE_AUDIT.md` with measurements against tokio-tungstenite and
  the Autobahn suite.

## [2.0.1] - 2026-07-25

### Added

- Added correlated native WebSocket keepalive for Tokio and Compio, including
  plain/compressed unified streams, split streams, Axum, and generic
  TCP/TLS/HTTP transport wrappers.
- Added `Config::{pong_timeout,pong_timeout_close_code,
  pong_timeout_close_reason,close_timeout}` and their builder methods.
- Added typed `Error::HeartbeatTimeout` and `Error::IdleTimeout` causes.
- Added deterministic heartbeat state-machine, Tokio split-driver, Axum
  plain/permessage-deflate, masking, Close, and Compio control-driver tests.

### Changed

- `ping_interval` is now an inbound-inactivity interval rather than a fixed
  cadence. One nonce-bearing Ping may be outstanding; only its exact Pong
  clears the deadline.
- `idle_timeout` now implements its documented hard inbound-idle deadline.
  `Config::uws_defaults()` disables that independent deadline so it cannot
  close before its first keepalive Ping.
- Split writers are bounded command handles. A connection-scoped driver owns
  the transport writer and prioritizes automatic Pong, Close, and heartbeat
  traffic even when the application performs no writes.
- Split readers expose Ping and Pong messages after automatic protocol work.

### Compatibility

- Existing builder-based configuration and `split()` call sites keep the same
  method-level API. Tokio split transports must now be `Send + 'static`
  because the writer driver is connection-scoped Tokio work.
- Adding public `Config` fields is source-breaking for downstream exhaustive
  struct literals; use `Config::builder()` or `..Config::default()`. Adding
  typed public `Error` variants is source-breaking for exhaustive matches.
  These changes are intentional so timeout causes are not collapsed into EOF.

## [2.0.0] - 2026-07-24

### Highlights

- Added native Compio support alongside Tokio, including WebSocket handshakes,
  plain and compressed streams, split readers/writers, HTTP/2, HTTP/3, and
  multiplexed connections.
- Made HTTP/2 and HTTP/3 transport features runtime-neutral so they can be
  paired with either `tokio-runtime` or `compio-runtime`.
- Fixed read-only WebSocket consumers so automatic Pong and Close responses
  are flushed without requiring the caller to drive the Sink side.
- Activated the existing `auto_ping` and `ping_interval` settings so a polled
  read loop sends periodic Ping frames even while its peer is silent.

### Added

- `compio-runtime` feature with Compio-native completion-based APIs.
- Portable Compio polling driver enabled by default through the
  `compio-runtime` feature.
- Runtime and transport end-to-end tests for HTTP/1, HTTP/2, HTTP/3, and
  multiplexed WebSockets across Tokio and Compio.
- A `wtx_bench_echo` example and expanded runtime/transport documentation.

### Changed

- Runtime selection is separate from transport selection. When default
  features are disabled, select `tokio-runtime` or `compio-runtime` explicitly
  and combine it with `http2` or `http3` as needed.
- HTTP/2 Extended CONNECT requests now use the h2 protocol extension API.
- HTTP/3 stream construction retains the connection handles needed by
  multiplexed and long-lived streams.
- Examples, binaries, and benchmarks declare the runtime features they require.
- Corrected benchmark reproduction instructions in the README.

### Fixed

- Incoming Ping frames now flush their automatic Pong before `poll_next`
  yields the Ping to a read-only consumer.
- Incoming Close frames now flush the Close response before the stream reports
  itself closed.
- Compio no longer compiles with a stub driver that panics when creating a
  runtime.

## [1.5.1] - 2026-01-02

### Fixed

- Clippy warnings: allow `large_enum_variant` for `StreamInner` (boxing adds indirection overhead)
- Clippy warnings: collapse nested if statements using let-chains in `extended_connect.rs`

## [1.5.0] - 2026-01-02

### Added

- **mimalloc feature**: Optional high-performance allocator for 10-30% throughput improvement
  - Enable with `features = ["mimalloc"]`
  - Automatically sets mimalloc as the global allocator

### Changed

- **Unified Transport API**: Major refactoring of HTTP/2 and HTTP/3 APIs
  - `H2WebSocketServer` → `WebSocketServer<Http2>`
  - `H3WebSocketServer` → `WebSocketServer<Http3>`
  - `H2WebSocketClient` → `WebSocketClient<Http2>`
  - `H3WebSocketClient` → `WebSocketClient<Http3>`
  - New `Transport` trait with `Http1`, `Http2`, `Http3` marker types
  - Shared `ExtendedConnectRequest`/`ExtendedConnectResponse` types
  - `MultiplexedConnection` for HTTP/2 and HTTP/3 stream multiplexing

- **Stream type renames**:
  - `H2Stream` → `Http2Stream`
  - `H3Stream` → `Http3Stream`

### Removed

- **Unused custom allocators**: Removed `src/alloc.rs` containing unused `SlabPool`, `Arena`, and `BufferPool`
  - These were never integrated into the codebase
  - Use the `slab` crate from tokio-rs if slab allocation is needed

### Migration Guide

```rust
// Before (1.4.x)
use sockudo_ws::http2::H2WebSocketServer;
let server = H2WebSocketServer::new(config);

// After (1.5.0)
use sockudo_ws::{WebSocketServer, Http2};
let server = WebSocketServer::<Http2>::new(config);
```

## [1.4.3] - 2026-01-01

### Fixed

- Critical bug: Misaligned pointer dereference in scalar masking fallback
  - The alignment check was incorrectly using `(i + mask_idx) & 7` instead of checking actual pointer address
  - Could cause panics on architectures that enforce pointer alignment when casting to `*mut u64`
  - Now correctly checks `(ptr_addr + i) & 7` to ensure 8-byte alignment before u64 operations
  - Discovered through fuzzing with cargo-fuzz

## [1.4.2] - 2026-01-01

### Added

- Custom SSE2 UTF-8 validation for x86/x86_64 CPUs without SSE4.2 support
  - `simdutf8` crate only supports SSE4.2+ (introduced in 2008)
  - New SSE2 implementation provides SIMD acceleration for older CPUs (SSE2 available since 2001)
  - Uses ASCII fast-path detection: checks if all bytes in 16-byte chunks are ASCII (< 0x80)
  - Falls back to `simdutf8` when SSE4.2+ is available for optimal performance
  - No feature flags required, works on stable Rust

### Fixed

- Clippy warnings: removed unnecessary `return` statements in UTF-8 validation dispatch
- Clippy warnings: simplified redundant closures in benchmarks

## [1.4.1] - 2026-01-01

### Changed

- Updated all dependencies to latest versions with `^` for automatic compatible updates:
  - tokio: ^1.48
  - rustls: ^0.23
  - tokio-rustls: ^0.26
  - webpki-roots: ^1.0 (major version bump)
  - rustls-native-certs: ^0.8
  - rustls-platform-verifier: ^0.6
  - quinn: ^0.11
  - h3: ^0.0.8, h3-quinn: ^0.0.10

### Fixed

- CI build failure caused by rustls-platform-verifier 0.4 incompatibility with webpki::Error trait bounds

## [1.4.0] - 2026-01-01

### Added

- Custom SIMD UTF-8 validation for architectures not covered by simdutf8:
  - LoongArch64 (LSX/LASX) with ASCII fast-path optimization
  - PowerPC/PowerPC64 (AltiVec) with ASCII fast-path optimization
  - s390x (z13 vectors) with ASCII fast-path optimization
- All custom implementations require the `nightly` feature flag

### Implementation Details

ASCII fast-path strategy: Check if all bytes in a 16/32-byte chunk have high bit unset (< 0x80). If pure ASCII, skip validation for that chunk; if non-ASCII, fall back to scalar validation.

#### Architecture Support Matrix

| Architecture | Masking | UTF-8 |
|---|---|---|
| x86_64 (AVX-512/AVX2/SSE4.2) | Yes | Yes (simdutf8) |
| x86_64 (SSE2 only) | Yes | Yes (custom) |
| x86 (SSE2) | Yes | Yes (custom) |
| aarch64 (NEON) | Yes | Yes (simdutf8) |
| arm (NEON) | Yes | Yes (simdutf8) |
| loongarch64 (LSX/LASX) | Yes | Yes (custom) |
| powerpc/powerpc64 (AltiVec) | Yes | Yes (custom) |
| s390x (z13 vectors) | Yes | Yes (custom) |

## [1.3.0] - 2026-01-01

### Added

- Multi-architecture SIMD support:
  - LoongArch64: LSX (128-bit) and LASX (256-bit) SIMD
  - PowerPC/PowerPC64: AltiVec SIMD
  - s390x: z13 vector instructions
  - ARM 32-bit: NEON SIMD (nightly)
- Fuzzing infrastructure with 4 targets:
  - Frame parsing (`parse_frame`)
  - Masking operations (`unmask`)
  - UTF-8 validation (`utf8_validation`)
  - Protocol round-trip (`protocol`)
- TLS configuration options:
  - `native-tls`
  - `rustls-webpki-roots`
  - `rustls-native-roots`
  - `rustls-platform-verifier`
- Configurable SHA-1 backends: `ring`, `aws_lc_rs`, `openssl`, `sha1_smol`
- Configurable RNG options: `fastrand`, `getrandom`, `rand_rng`
- `nightly` feature flag for additional SIMD architectures

### Changed

- Optimized small frame handling (borrowed from tokio-websockets)
- Zero-copy messaging via Bytes type
- Alignment-aware SIMD implementations
- Improved error categorization
- Enhanced HTTP/2 and HTTP/3 timeout management

### Added (API)

- Backpressure API for flow control

### Credits

- [tokio-websockets](https://github.com/Gelbpunkt/tokio-websockets)
- [fastwebsockets](https://github.com/denoland/fastwebsockets)
- [uWebSockets](https://github.com/uNetworking/uWebSockets)

## [1.2.0] - 2025-12-30

### Performance

- Now the fastest Rust WebSocket library (~17% faster than fastwebsockets and web-socket)
- Benchmark results (100,000 iterations):
  - sockudo-ws: 10.2ms total
  - fastwebsockets: 12.0ms total
  - web-socket: 12.2ms total

### Added

- Zero-copy `RawMessage` API:
  - `Text(Bytes)` - UTF-8 validated, zero-copy text
  - `Binary(Bytes)`
  - `Ping(Bytes)` and `Pong(Bytes)`
  - `Close(Option<CloseReason>)`
- `process_raw()` and `process_raw_into()` methods on Protocol layer

### Changed

- Inline masking during copy (single-pass encoding)
- Unsafe pointer writes for frame headers (reduced bounds-checking)
- 8-byte chunk processing for faster masking
- Fast-path frame parsing for small unmasked frames

## [1.1.1] - 2025-12-29

### Fixed

- Resolved all clippy warnings
- Fixed fmt issues
- Changed `Arc` to `Rc` for tokio-uring TcpStream (lacks Send+Sync)
- Cleaner error handling via `std::io::Error::other()`
- Simplified boolean expressions and collapsed nested if statements

## [1.1.0] - 2025-12-29

### Added

- HTTP/2 WebSocket support (RFC 8441):
  - Extended CONNECT protocol
  - `H2WebSocketServer`, `H2WebSocketClient`, `H2Stream`
  - Multiplexed WebSocket connections over HTTP/2
- HTTP/3 WebSocket support (RFC 9220):
  - WebSocket over QUIC
  - `H3WebSocketServer`, `H3WebSocketClient`, `H3Stream`
  - Zero round-trip time (0-RTT)
  - No head-of-line blocking
- io_uring transport (Linux):
  - `UringStream` wrapper for tokio-uring
  - `RegisteredBufferPool` for zero-copy operations
  - Compatible with HTTP/2 and HTTP/3
- Feature flags:
  - `http2`: HTTP/2 Extended CONNECT
  - `http3`: HTTP/3 over QUIC
  - `io-uring`: Linux io_uring async I/O
  - `all-transports`: All transport protocols
  - `full`: Complete feature set with axum integration

### Changed

- Unified API: All transports use `WebSocketStream<S>` interface

## [1.0.0] - 2025-12-29

### Added

- Initial release
- Ultra-low latency WebSocket implementation
- SIMD acceleration (AVX2, AVX-512, NEON)
- permessage-deflate compression support
- Split streams for concurrent read/write
- Passes all 517 Autobahn test cases
- Outperforms uWebSockets in benchmarks

[2.1.0]: https://github.com/sockudo/sockudo-ws/compare/v2.0.2...v2.1.0
[2.0.2]: https://github.com/sockudo/sockudo-ws/compare/v2.0.1...v2.0.2
[2.0.1]: https://github.com/sockudo/sockudo-ws/compare/v2.0.0...v2.0.1
[2.0.0]: https://github.com/sockudo/sockudo-ws/compare/v1.7.5...v2.0.0
[1.5.1]: https://github.com/RustNSparks/sockudo-ws/compare/v1.5.0...v1.5.1
[1.5.0]: https://github.com/RustNSparks/sockudo-ws/compare/v1.4.3...v1.5.0
[1.4.3]: https://github.com/RustNSparks/sockudo-ws/compare/v1.4.2...v1.4.3
[1.4.2]: https://github.com/RustNSparks/sockudo-ws/compare/v1.4.1...v1.4.2
[1.4.1]: https://github.com/RustNSparks/sockudo-ws/compare/v1.4.0...v1.4.1
[1.4.0]: https://github.com/RustNSparks/sockudo-ws/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/RustNSparks/sockudo-ws/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/RustNSparks/sockudo-ws/compare/v1.1.1...v1.2.0
[1.1.1]: https://github.com/RustNSparks/sockudo-ws/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/RustNSparks/sockudo-ws/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/RustNSparks/sockudo-ws/releases/tag/v1.0.0
