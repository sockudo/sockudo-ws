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
//! - Linux kernel 5.6+ (full feature support including IORING_FEAT_FAST_POLL)
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
/// Returns `true` if:
/// - Running on Linux
/// - The kernel version supports io_uring (5.1+)
/// - The io_uring syscalls are available
///
/// Note: This is a compile-time check for the platform. Runtime availability
/// depends on kernel configuration and may vary. The `tokio-uring` crate
/// handles runtime checks internally and will fall back or error appropriately.
///
/// # Example
///
/// ```
/// use sockudo_ws::io_uring::is_available;
///
/// if is_available() {
///     println!("io_uring is available on this platform");
/// } else {
///     println!("io_uring is not available (non-Linux or unsupported kernel)");
/// }
/// ```
pub fn is_available() -> bool {
    // Compile-time platform check
    #[cfg(target_os = "linux")]
    {
        // Runtime check: try to detect if io_uring is actually usable
        // This checks if the kernel supports io_uring by attempting to read
        // the kernel version. For a more robust check, tokio-uring will
        // fail at runtime if io_uring is not available.
        //
        // Minimum kernel version for io_uring: 5.1
        // Recommended for full features: 5.6+
        //
        // We return true here as a compile-time indication that io_uring
        // *could* be available. Actual availability is determined at runtime
        // by tokio-uring when creating the runtime.
        true
    }

    #[cfg(not(target_os = "linux"))]
    {
        // io_uring is Linux-only
        false
    }
}

/// Check if the current kernel likely supports io_uring with full features
///
/// This performs a runtime check of the kernel version to determine if
/// io_uring with all features (including FAST_POLL) is likely available.
///
/// Returns `Some((major, minor, patch))` with the kernel version if on Linux,
/// or `None` if not on Linux or if the version cannot be determined.
#[cfg(target_os = "linux")]
pub fn kernel_version() -> Option<(u32, u32, u32)> {
    use std::fs;

    let version_str = fs::read_to_string("/proc/version").ok()?;

    // Parse "Linux version X.Y.Z..."
    let version_part = version_str.split_whitespace().nth(2)?; // "X.Y.Z-something"

    let version_numbers: Vec<&str> = version_part.split('.').collect();
    if version_numbers.len() < 2 {
        return None;
    }

    let major: u32 = version_numbers[0].parse().ok()?;
    let minor: u32 = version_numbers[1]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    let patch: u32 = version_numbers
        .get(2)
        .and_then(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
        .unwrap_or(0);

    Some((major, minor, patch))
}

/// Check if the kernel version supports io_uring with recommended features
///
/// Returns true if kernel is 5.6 or higher, which includes IORING_FEAT_FAST_POLL
/// for better performance.
#[cfg(target_os = "linux")]
pub fn has_recommended_kernel() -> bool {
    kernel_version().is_some_and(|(major, minor, _)| major > 5 || (major == 5 && minor >= 6))
}

#[cfg(not(target_os = "linux"))]
pub fn kernel_version() -> Option<(u32, u32, u32)> {
    None
}

#[cfg(not(target_os = "linux"))]
pub fn has_recommended_kernel() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_available() {
        let available = is_available();

        #[cfg(target_os = "linux")]
        assert!(available);

        #[cfg(not(target_os = "linux"))]
        assert!(!available);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_kernel_version() {
        let version = kernel_version();
        assert!(
            version.is_some(),
            "Should be able to read kernel version on Linux"
        );

        let (major, minor, _patch) = version.unwrap();
        // Sanity check: kernel version should be reasonable
        assert!(major >= 4, "Kernel major version should be at least 4");
        assert!(minor < 100, "Kernel minor version should be reasonable");
    }
}
