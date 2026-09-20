# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Added custom HTTP headers to Tokio and Compio HTTP/1.1 client handshakes, with validation that
  prevents malformed fields and conflicts with handshake-managed headers.
- Added Tokio compressed-stream constructors that accept post-handshake bytes,
  plus request-aware Tokio and Compio `server_handshake_with` callbacks for
  selecting subprotocol and extension responses.
- Added server-preference subprotocol policies to Tokio HTTP/1.1, HTTP/2, and
  HTTP/3 servers, Compio transports, and Axum upgrades.
- Added reproducible service benchmarks for shared-compressor contention and
  Pub/Sub publishing during membership churn.
- Added `DeflateWindowBits`, which represents the 9–15-bit window sizes supported
  by the configured compression backend.

### Changed

- Encoded buffer overflow is terminal; `WouldBlock` remains retryable. The default
  outbound limit is 1 MiB, independently of inbound message limits.
- Generic deflate configs reject unsupported 8-bit encoder windows; role-aware
  server negotiation can use a supported decoder for an 8-bit client window.
- Received HTTP lists ignore empty elements, while outbound metadata remains validated.
- HTTP/3 applies its configured defaults, including a 1 MiB send window and
  1350-byte maximum UDP payload.
- Native io_uring read/write methods require mutable access. Direct I/O through
  shared transport handles bypasses bridge ordering.
- Compio HTTP/2 entry points require `Splittable`; other transports can use
  `compio::io::util::Split::new`. Custom reads must cooperate with cancellation
  for prompt automatic Ping; only an existing idle/Pong deadline bounds recovery.

- Explicit `send_coalesced()` calls can coalesce outbound frames while parsed
  inbound messages remain queued (`Config::write_coalescing`, default on).
  A read batch of N messages answered with N coalesced sends is one vectored
  write instead of N. Standard `SinkExt::send()` and `SinkExt::flush()` always
  flush; callers using coalescing must flush before pausing reads or waiting
  for a reply that depends on buffered output.
- Native split writes are coordinated through a connection-scoped driver with
  bounded application/control queues. Cancelling an accepted application write
  closes the connection before another application frame can follow it. This also applies before
  transport progress; retain the send future across timeout/select boundaries
  when the connection must remain usable.
- Server-side uncompressed data payloads of 8 KiB or more are queued by reference
  behind their frame header. Segmented writes use vectored I/O when supported;
  contiguous small frames retain the single-buffer write path.
- `CorkBuffer` preserves write order with frozen `Bytes` segments followed by an
  open tail buffer. Large payloads count toward the stream backpressure limit.
- Generic frame masking uses portable 64-byte blocks; architecture-specific
  kernels remain selected on supported targets.

- Tokio streams now use `quanta` for heartbeat and activity timestamps by
  default. The `test-util` feature selects Tokio time for deterministic virtual-
  time tests; `full` excludes `test-util`, while `--all-features` includes it.
- Custom `Compression::Shared` contexts now use role-specific pools for
  asymmetric window settings. The default remains a process-wide four-slot
  synchronous encoder pool, with decoders kept per connection.
- Pub/Sub membership indexes are updated under one state lock. Publish operations
  snapshot recipients under a read lock and send after unlocking; removal after
  the snapshot does not revoke an already-selected delivery.
- Deflate configuration now uses `DeflateWindowBits`, so codec, context, pool,
  and compressed-stream constructors remain infallible with valid window sizes.
- HTTP/1.1 request/response builders now return `Result` after validating fields.
  Default server handshakes no longer echo a client's subprotocol list without
  an application selection policy.
- HTTP/1.1 parsing now uses standard URI and authority types, accepts only
  `http`/`https` absolute-form targets, and accepts repeated Content-Length
  fields only when every comma-separated value is zero.

### Fixed

- Preserved WebSocket frame bytes read together with HTTP/1.1 upgrade requests
  or responses in the high-level Tokio APIs, and enabled direct Tokio
  compressed-stream callers to replay those bytes.
- Preserved Compio read buffers when a heartbeat deadline cancels an in-flight
  read, including partial headers and payloads.
- Accepted legal final DEFLATE blocks across consecutive messages while retaining
  only the negotiated takeover dictionary, and rejected invalid bytes following
  a final block.
- Rejected unsupported 8-bit deflate windows instead of allowing the backend to
  panic; incompatible remote offers are declined.
- Preserved parser, fragmentation, compression, and pending terminal-error state
  when plain or compressed Tokio/Compio streams are split after reading begins.
- Applied client permessage-deflate constraints during Axum negotiation so the
  response header and codec use the same negotiated configuration.
- Tightened HTTP/1.1 upgrade validation for HTTP version, Host, WebSocket keys,
  exact header tokens, subprotocols, extensions, and reserved framing headers;
  combined repeatable protocol/extension fields and accepted absolute request
  targets.
- Validated selected subprotocols and extension responses on HTTP/1.1, HTTP/2,
  and HTTP/3 clients, and stopped HTTP/2 servers from echoing a complete client
  offer as one selection.
- Bounded decompression output allocation by the configured message limit and
  stopped retaining a full compressed-input copy between messages.
- Made delivery independent of read chunking when valid messages precede a
  terminal frame error: complete messages are yielded before the error once.
- Discarded queued Compio messages after an automatic control-frame write fails,
  and made HTTP/2 and HTTP/3 subprotocol matching case-sensitive.
- Reset deflate decoders without replacing their allocation and aligned public
  parameter parsing with supported encoder limits. Role-aware server negotiation
  can map `client_max_window_bits=8` to a supported decoder; generic configs
  reject it because they can also construct client encoders.

### Compatibility

- Compio HTTP/2 entry points require `Splittable`; other transports can use
  `compio::io::util::Split::new`. Custom reads used with automatic Ping must
  cooperate with cancellation; only an existing idle/Pong deadline bounds recovery.

- io_uring native read/write methods now require mutable access so they cannot
  bypass pending bridge operations. Shared handles remain for inspection and configuration.

- HTTP/3 applies its configured transport values, including the default 1 MiB
  send window and 1350-byte maximum UDP payload.

- `DeflateConfig` window fields, `Compression::window_bits()`, and the deflate
  window constants now use `DeflateWindowBits`; `DeflateEncoder::new` and
  `DeflateDecoder::new` accept that type. `MIN_WINDOW_BITS` is now 9 and
  `Compression::Window256B` was removed because the configured backend does
  not support an 8-bit encoder window.
- HTTP request/response builders now return `Result`, and `HandshakeRequest`
  path, protocol, and extension fields use `Cow<str>` so repeated fields and
  absolute request targets can be normalized. These changes affect source
  compatibility. Incoming HTTP lists ignore empty elements; outbound builders
  continue to reject invalid metadata.
- Restored Compio `server_handshake_with_extensions` as a deprecated wrapper;
  request-aware code should use `server_handshake_with`.

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
