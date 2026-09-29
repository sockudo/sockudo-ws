# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [3.0.0] - 2026-09-29

### Enhancements

- Added `init_clock()` to initialize the Tokio connection clock before constructing the runtime, moving the first quanta calibration read out of latency-sensitive work (#58)
- Added `CompressedWebSocketStream::client_with_leftover` and `server_with_leftover` for post-handshake frame bytes, including when splitting before the first read (#42)
- Added `WebSocketServer<Http1>::protocols` to select HTTP/1 subprotocols in server preference order; without a list, the first offered protocol is still selected (#80)
- Added `negotiate_server_deflate` and `DeflateNegotiation` for server-side `permessage-deflate` offer negotiation (#32)
- Added `with_immediate_write_shutdown()` for directly constructed HTTP/2 and HTTP/3 streams to shut down the write half after an explicit Close; built-in entry points enable it automatically (#38)
- Added zero-copy sends for server data payloads of 8 KiB or more, queued by reference behind their frame header and written with vectored I/O (`CorkBuffer::push_segment`, `cork::ZERO_COPY_MIN`) (#18)
- Added `TCP_NODELAY` to sockets created by built-in Tokio HTTP/1 URL clients and listener servers; client option errors propagate, listener errors are reported per connection, and caller-provided streams keep their settings (#47)

### Breaking Changes

- Added `Config::validate_text_utf8` and builder method (default `true`); disabling it delivers unvalidated Text bytes through Tokio and Compio unified, split, plain, and compressed readers while Close reasons and checked text accessors stay validated; struct-literal callers must add the field (#107)
- Added `Config::write_coalescing` and builder method (default `true`) to batch frames across Tokio `SinkExt::feed` calls until `flush()`; struct-literal callers must add the field (#18, #22)
- Added `Http2Config::max_frame_size` and `ConfigBuilder::http2_max_frame_size` for the advertised HTTP/2 receive frame limit on Tokio and Compio endpoints (default 16 KiB); values outside 16,384–16,777,215 return a handshake error, and struct-literal callers must add the field (#106)
- Changed Tokio heartbeat and activity timestamps, including unified Close deadlines, to quanta; paused-time tests must enable this crate's new `test-util` feature (excluded from `full`) rather than only `tokio/test-util`, and Compio clocks are unchanged (#58)
- Changed Tokio Sink readiness to drain queued output at the smaller of the high-water mark and `max_backpressure`, or before each frame with `write_coalescing` disabled, so `feed`, `send_all`, and `forward` may wait for a slow reader (#22, #35)
- Changed ordinary Tokio unified and split readers to deliver each parsed message before parsing the rest of a buffered burst; later malformed frames surface on a subsequent `next()`, writes stay allowed until then, heartbeat or idle timeouts take precedence over the unparsed tail, and compressed and Compio readers keep batch parsing (#96, #109)
- Changed ordinary Tokio unified readers to parse the already-buffered tail after accepting a Ping, so a buffered Close suppresses the automatic Pong; accepted messages stay in wire order and transfer through `split()`
- Changed `SplitWriter` and `CompressedSplitWriter` to write through a transport sink shared with the control driver instead of a command channel; automatic control frames interleave at frame boundaries, cancelling a send after it acquires the sink closes the connection, and `SplitWriter::send` requires `S: AsyncWrite + Unpin` (#18, #20)
- Changed unified Tokio and Compio `flush()` on a closed stream to return `ConnectionClosed` even with no buffered output, and Compio application sends after a local Close to fail (#38)
- Changed Tokio plain unified streams to discard queued output after a parse error before cleanup shutdown; `SinkExt::close` still attempts shutdown after EOF or a read or parse error when no encoded output remains (#109)
- Changed DEFLATE encoder windows to `DeflateWindowBits` (9–15) across `DeflateConfig` fields, window constants, and codec constructors; `Compression::window_bits()` now returns `Option<DeflateWindowBits>` (#32)
- Removed `Compression::Window256B`; 8-bit encoder windows are rejected instead of panicking or widening, while servers still accept 8-bit client streams with a larger decoder window (#32)
- Changed `permessage-deflate` negotiation to decline offers without `client_max_window_bits` when the server policy allows fewer than 15 client window bits, and public offer parsing to reject duplicate or empty parameters, malformed quoted values, leading zeroes, and non-ASCII optional whitespace (#32)
- Changed `HandshakeRequest.path` to `Cow<'_, str>` (use `.as_ref()` for `&str`); built-in Tokio and Compio HTTP/1 servers normalize absolute HTTP/HTTPS targets to path and query, report their authority as `host`, and reject invalid percent escapes, fragments, unsupported forms or schemes, userinfo, empty hosts, and nonnumeric ports; Axum's upgrade extractor is unaffected (#87)
- Changed `http` to a mandatory dependency, including default-feature builds (#87)
- Changed default Tokio and Compio HTTP/1 servers to select the client's first offered subprotocol instead of echoing the whole offer, so `HandshakeResult::protocol` holds the selected protocol (#86)
- Changed built-in Tokio and Compio HTTP/3 endpoints to apply the previously ignored QUIC idle timeout, stream receive window, maximum UDP payload size, and Extended CONNECT settings, with defaults preserving Quinn's previous values; out-of-range limits, a zero receive window, or `enable_0rtt = true` return errors, 0-RTT requests are rejected, and caller-provided endpoints keep their settings (#43)
- Changed the Compio dependency to 0.19; Compio HTTP/2 entry points require `Splittable` transports, so wrap others with `compio::io::util::Split::new` (#40)
- Changed Compio automatic Ping to require pending custom reads to cooperate with cancellation; with idle timeout disabled, a nonzero `pong_timeout` also bounds read-buffer recovery and reports `HeartbeatTimeout`, while zero for both leaves recovery unbounded (#40)
- Changed native io_uring `read_native`, `write_native`, and `write_all_native` to take `&mut self`, so concurrent reads and writes require exclusive direct I/O through `get_ref`; `has_recommended_kernel()` now requires Linux 5.10 (#30)

### Security

- Fixed `Message::as_text` and `Message::into_text` performing unchecked UTF-8 conversion on publicly constructed Text payloads; invalid payloads now return `None` (#19)
- Fixed Tokio receive and DEFLATE paths treating uninitialized buffer capacity as initialized (#23)
- Fixed control bytes in handshake-managed HTTP/1 client request fields (host, target, key, protocol, and extensions) allowing request-line or header injection; the raw `build_request` remains unchecked (#78)
- Fixed DEFLATE decompression exceeding `max_message_size` when the final inflate call produced more than the remaining limit (#76)

### Fixes

- Fixed messages accepted before a malformed frame in the same read being dropped; Tokio and Compio readers deliver them before the parse error and reject new writes once it is known (#36, #96)
- Fixed Tokio plain and compressed unified streams stalling hard idle and Pong deadlines while an automatic control write or queued output is blocked (#108)
- Fixed Tokio plain and compressed unified streams staying pending without a heartbeat deadline when an automatic control write blocks after a later parse error has been discovered
- Fixed Tokio plain unified `SinkExt::close` retrying failed or abandoned transport shutdown attempts
- Fixed Tokio split readers dropping the peer's Close reason from their automatic Close response after splitting (#109)
- Fixed Tokio plain and compressed split readers and writers observing closure separately from its terminal cause; the first cause is published atomically, and compressed reads keep a cause published during I/O over the transport error (#97)
- Fixed Tokio split drivers ignoring heartbeat, idle, and close deadlines while a control or application write is blocked; interrupted sends return the same typed timeout (#75)
- Fixed Tokio split transports staying open after terminal deadlines while reader or writer handles remain alive (#100)
- Fixed split `close()` exceeding `close_timeout` while waiting behind blocked control writes, and a concurrent peer Close queuing a duplicate Close; once the budget starts, cancelling `close()` does not reopen the connection (#101)
- Fixed Compio split writers ignoring hard idle, Pong, and closing deadlines while a transport write is pending, so send-only connections reach the configured idle timeout (#98)
- Fixed Compio heartbeat wakeups losing partially read frames on unified, compressed, HTTP/2, and HTTP/3 transports (#40)
- Fixed split idle timers expiring at a stale deadline after the reader published newer activity (#69)
- Fixed accepted non-final data frames, including empty continuations and compressed fragments, not refreshing inbound activity on Tokio and Compio streams and split readers; fragments do not postpone Pong or Close deadlines (#37)
- Fixed an outstanding Ping postponing an earlier hard idle deadline; such connections now close with `IdleTimeout` (#91)
- Fixed explicit unified `close()` on Tokio and Compio HTTP/2 and HTTP/3 streams losing queued frames when the handler releases its stream, and repeated Sink close repeating transport shutdown; TCP and TLS keep the write half open until the peer's Close (#38)
- Fixed unified closing without an overall bound; Close writes, peer response, automatic control writes, and shutdown share one `close_timeout` budget (default 5 s) while preserving an accepted Close or the original idle or Pong timeout (#38)
- Fixed Tokio Sink readiness ignoring `max_backpressure`, which is a soft queue threshold rather than a message size limit; zero drains any pending output (#35)
- Fixed frame size limits skipping partially received short frames, and single-frame Text and Binary messages bypassing the message size limit in typed, raw, and compression-capable protocols (#24)
- Fixed mixed raw and typed fragment processing skipping UTF-8 validation of accumulated bytes or split code points; messages completed through the raw API remain unvalidated (#25)
- Fixed splitting Tokio and Compio streams discarding partially parsed frames and fragment or UTF-8 state; compressed splits keep parser progress under the supplied limits (#26)
- Fixed compressed frame parsers accepting RSV1 on continuation and control frames; they are rejected as soon as the base header arrives (#29)
- Fixed UTF-8 validation rejecting valid multibyte characters across internal SIMD block boundaries on SSE2-only x86 and nightly LoongArch64, PowerPC, and s390x; complete inputs now use `simdutf8` (#28)
- Fixed DEFLATE compression truncating large incompressible context-takeover messages when output filled after the input was consumed (#68)
- Fixed DEFLATE decompression after BFINAL blocks losing context for later messages, dropping later streams in the same message, and accepting trailing invalid data (#88)
- Fixed `Compression::Shared` contexts allocating four encoders per connection; role-aware encoder pools are shared while decoders stay connection-local, and client contexts honor `client_max_window_bits` (#60)
- Fixed built-in HTTP/1 handshakes accepting malformed `Sec-WebSocket-Protocol` or `Sec-WebSocket-Extensions` fields and duplicate client subprotocol offers (#86)
- Fixed Tokio and Compio HTTP/1 clients accepting an unoffered, case-mismatched, or repeated `Sec-WebSocket-Protocol` selection (#79)
- Fixed built-in HTTP/1 handshakes accepting non-HTTP/1.1 messages, an empty request Host, request keys that do not decode to 16 bytes, or responses without an exact `Upgrade: websocket` and a `Connection: Upgrade` token; Axum's upgrade extractor is unchanged (#81)
- Fixed built-in HTTP/1 servers accepting upgrade requests with a nonzero or invalid `Content-Length` or any `Transfer-Encoding`; checked client requests reject these custom headers (#82)
- Fixed built-in HTTP/1 handshakes accepting repeated request `Sec-WebSocket-Key` or `Sec-WebSocket-Version` and response `Sec-WebSocket-Accept` or `Sec-WebSocket-Extensions` fields (#83)
- Fixed the 8 KiB HTTP/1 handshake limit counting WebSocket frame bytes read with the headers; oversized incomplete headers remain rejected (#77)
- Fixed HTTP/1 handshake nonces using a timestamp-seeded byte loop instead of the selected RNG backend, with nonce state separate from frame masking; use `getrandom` or `rand_rng` when cryptographic output is required (#48)
- Fixed HTTP/1 `Stream` and Axum `UpgradedStream` not forwarding vectored writes or reporting vectored-write capability (#70)
- Fixed Tokio HTTP/3 servers not advertising `SETTINGS_ENABLE_CONNECT_PROTOCOL` when Extended CONNECT is enabled (#43)
- Fixed Tokio HTTP/3 writes failing with `H3_INTERNAL_ERROR` when QUIC flow control returned Pending (#39)
- Fixed Tokio and Compio HTTP/3 zero-capacity reads waiting for or consuming DATA (#63, #94)
- Fixed Tokio and Compio HTTP/2 adapters reporting premature EOF on empty DATA frames and consuming DATA on zero-capacity reads (#105)
- Fixed cancelled native Compio HTTP/3 DATA writes leaving the stream usable; both directions now abort with `H3_REQUEST_CANCELLED` while the connection can open new streams (#84)
- Fixed io_uring completion operations not being driven across poll calls, buffered writes not flushing before shutdown, and the `io-uring` feature not enabling the required Tokio integration (#30)
- Fixed PubSub subscriber, socket-ID, and topic indexes updating non-atomically, allowing duplicate socket IDs and stale membership; publication delivers to a recipient snapshot taken atomically with membership changes, and receivers wake outside the membership lock (#27)

### Internal Improvements

- Added the bundled Rust Autobahn conformance suite to CI and the pre-publish workflow (#85)
- Added codec, fan-out, stream, receive-state, and paced-delivery benchmarks and diagnostics (#44, #45, #46)
- Added masked frame parsing and short UTF-8 validation boundary regression tests (#103, #104)
- Added focused Clippy lints and retained benchmark debug symbols
- Improved CI with parallel feature-combination nextest runs, `rust-cache`, and `rstest` table-driven cases (#102)
- Improved CI caching by rebuilding CPU-specific native-target artifacts on each runner (#99)
- Reorganized benchmarks into layered suites with unique IDs and controlled stream I/O, compression-context, and transport coverage
- Refined `CorkBuffer` into an ordered list of `Bytes` segments plus an open tail buffer; `write_bytes` keeps output order and `write` no longer spills into an overflow queue (#18)
- Fixed Axum TCP heartbeat tests stalling under paused-clock auto-advance by using real time (#41)
- Optimized ordinary Tokio split reads by polling the transport before registering terminal waiters (#97)
- Optimized unified Tokio plain and compressed flushes by writing contiguous cork buffers without vectored-write setup (#57)
- Optimized Tokio receive buffers by reclaiming empty windows once buffered input reaches half the window, and reserving a fresh window before reading when retained payloads prevent reuse, trading earlier allocation for fewer partial-frame copies (#53, #110)
- Optimized Tokio and Compio HTTP/2 and HTTP/3 adapters by retaining owned DATA chunks instead of copying through a preallocated 64 KiB buffer; raw QUIC wrappers drop their unused buffer (#54, #63, #94)
- Optimized outgoing `no_context_takeover` compression by clearing DEFLATE history at message boundaries, including `Compression::Shared`, `Window1KB`, `Window2KB`, and `DeflateConfig::low_memory()` (#55)
- Optimized client frame encoding with vectorized copy-and-mask blocks, using 256-byte blocks for long payloads (#50, #112)
- Optimized frame masking on aarch64 and other non-x86 targets with a 64-byte block loop (#18)
- Optimized receive unmasking across partial reads with integer mask-phase rotation (#114)
- Optimized ordinary Tokio send success paths and skipped inbound clock reads when heartbeat tracking is disabled (#108, #111)
- Optimized Compio reads by polling ready input before registering heartbeat timers (#113)
- Optimized Compio streams and split readers by reusing message vectors and publishing inbound activity through a shared cell (#18)
- Optimized PubSub recipient selection with positional topic indexes and shared sender handles (#115)
- Added `quanta` v0.13 dependency for Tokio heartbeat timestamps (#58)
- Enabled the `quanta` `mock` feature for tests; benchmarks inherit it through Cargo feature unification, so their clock reads cost more than in production
- Added `libz-rs-sys` dependency for DEFLATE decompression across final blocks (#88)
- Added `rstest` dev dependency (#102)
- Removed `dashmap` dependency (#27)
- Removed `tokio-websockets` dev dependency
- Upgraded `aws-lc-rs` crate to v1.18
- Upgraded `base64` crate to v0.23
- Upgraded `bytes` crate to v1.12
- Upgraded `compio` crate to v0.19 (#40)
- Upgraded `criterion` crate to v0.8
- Upgraded `fastrand` crate to v2.5
- Upgraded `flate2` crate to v1.1.10 (#23, #88)
- Upgraded `getrandom` crate to v0.4
- Upgraded `http` crate to v1.5 (#87)
- Upgraded `httparse` crate to v1.10
- Upgraded `hyper` crate to v1.11
- Upgraded `rand` crate to v0.10
- Upgraded `rustls-platform-verifier` crate to v0.7
- Upgraded `sha1` crate to v0.11
- Upgraded `socket2` crate to v0.6
- Upgraded `tokio` crate to v1.53
- Upgraded `rcgen` crate (dev) to v0.14
- Upgraded `rustls-pemfile` crate (dev) to v2.2
- Upgraded `tokio-tungstenite` crate (dev) to v0.30

### Documentation Updates

- Documented Tokio Sink `feed`/`flush` batching, readiness thresholds, and `write_coalescing` (#22, #35)
- Documented unified `close_timeout` semantics, including undelivered messages lost on control-write failure and Compio `next()` not being cancellation-safe during Close cleanup (#38)
- Documented split send cancellation boundaries and split Close timeout semantics (#20, #101)
- Documented split stream ownership and the shared transport and sink locks (#108)
- Documented the `validate_text_utf8` byte contract (#107)
- Documented io_uring bridge contracts and kernel requirements, and compiled the runtime and HTTP/2 examples as doctests (#30)
- Documented HTTP/2 receive frame limits and HTTP/3 endpoint ownership, configuration errors, and receive-buffer costs (#43, #106)
- Documented the raw `build_request` as unchecked; use `build_request_with_headers` for external values (#78)
- Documented the production and paused-time clock features (#58)
- Documented UTF-8 validation backend coverage by architecture (#28)
- Documented running the bundled Autobahn suite (#85)
- Added codec, stream, and delivery benchmark guides and a benchmark suite overview in `benches/README.md` (#44, #45, #46)
- Updated `docs/PERFORMANCE_AUDIT.md` with post-2.1.0 write batching, zero-copy send, split writer, masking, and Compio results, and PubSub snapshot delivery (#18, #27)

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

[3.0.0]: https://github.com/sockudo/sockudo-ws/compare/v2.1.0...v3.0.0
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
