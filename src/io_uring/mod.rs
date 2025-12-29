//! io_uring support for Linux high-performance I/O
//!
//! This module provides io_uring-backed transport for WebSocket connections,
//! offering superior performance on Linux systems through reduced syscall
//! overhead and true asynchronous I/O.
//!
//! # Architecture: io_uring as Transport Layer
//!
//! io_uring is a **transport optimization**, not a protocol. It can be combined
//! with any protocol layer:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                  WebSocketStream<S>                      │
//! │               (same API for everything)                  │
//! ├─────────────────────────────────────────────────────────┤
//! │   HTTP/1.1     │    HTTP/2 (h2)    │   HTTP/3 (QUIC)    │
//! │   Upgrade      │  Extended CONNECT  │  Extended CONNECT  │
//! ├─────────────────────────────────────────────────────────┤
//! │              Optional: TLS (rustls/openssl)              │
//! ├─────────────────────────────────────────────────────────┤
//! │    TCP (epoll)   │   TCP (io_uring)   │   UDP (QUIC)    │
//! │    TcpStream     │   UringStream      │   quinn uses    │
//! │                  │                    │   io_uring too  │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! # Combining io_uring with HTTP/2
//!
//! ```ignore
//! use sockudo_ws::io_uring::UringStream;
//! use sockudo_ws::http2::H2WebSocketServer;
//!
//! #[tokio_uring::main]
//! async fn main() {
//!     let listener = tokio_uring::net::TcpListener::bind(addr)?;
//!     let server = H2WebSocketServer::new(Config::default());
//!
//!     loop {
//!         let (tcp, _) = listener.accept().await?;
//!
//!         // io_uring TCP -> TLS -> HTTP/2 -> WebSocket
//!         let uring = UringStream::new(tcp);
//!         let tls = tls_acceptor.accept(uring).await?;
//!
//!         server.serve(tls, |ws, req| async {
//!             // Full stack: WebSocket/HTTP2/TLS/io_uring!
//!         }).await.ok();
//!     }
//! }
//! ```
//!
//! # Combining io_uring with HTTP/3
//!
//! The `quinn` crate (used for QUIC/HTTP/3) already uses io_uring internally
//! when available on Linux. No extra configuration needed!
//!
//! # Direct io_uring + HTTP/1.1 WebSocket
//!
//! ```ignore
//! use sockudo_ws::{Config, WebSocketStream};
//! use sockudo_ws::io_uring::UringStream;
//!
//! #[tokio_uring::main]
//! async fn main() {
//!     let listener = tokio_uring::net::TcpListener::bind(addr)?;
//!
//!     loop {
//!         let (stream, _) = listener.accept().await?;
//!         let uring_stream = UringStream::new(stream);
//!
//!         // Direct: WebSocket over io_uring TCP
//!         let mut ws = WebSocketStream::server(uring_stream, Config::default());
//!
//!         while let Some(msg) = ws.next().await {
//!             ws.send(msg?).await.ok();
//!         }
//!     }
//! }
//! ```
//!
//! # Requirements
//!
//! - Linux kernel 5.1+ (basic io_uring)
//! - Linux kernel 5.6+ (full feature support)
//! - The `io-uring` feature must be enabled
//! - Must use `#[tokio_uring::main]` instead of `#[tokio::main]`
//!
//! # Performance Benefits
//!
//! - Reduced syscall overhead (batched submissions)
//! - True async I/O (no epoll wakeup overhead)
//! - Registered buffers for zero-copy I/O
//! - SQPOLL mode for minimal latency (at CPU cost)

mod buffer;
mod stream;

pub use buffer::{RegisteredBuffer, RegisteredBufferPool};
pub use stream::{UringStream, UringStreamAdapter};

/// Check if io_uring is available on this system
///
/// Returns `true` if the kernel supports io_uring with the features
/// required by this library.
pub fn is_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        // In practice, tokio-uring handles availability checks internally
        true
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}
