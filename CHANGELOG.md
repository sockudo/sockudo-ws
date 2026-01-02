# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
