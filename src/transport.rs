//! Transport markers for HTTP/1.1, HTTP/2, and HTTP/3 WebSocket connections
//!
//! This module provides type markers that enable generic WebSocket APIs
//! over different HTTP transports.
//!
//! # Example
//!
//! ```ignore
//! use sockudo_ws::{WebSocketServer, Http1, Http2, Http3};
//!
//! // HTTP/1.1 server (traditional WebSocket upgrade)
//! let server = WebSocketServer::<Http1>::new(config);
//!
//! // HTTP/2 server (Extended CONNECT)
//! let server = WebSocketServer::<Http2>::new(config);
//!
//! // HTTP/3 server (Extended CONNECT over QUIC)
//! let server = WebSocketServer::<Http3>::bind(addr, tls, config).await?;
//! ```

use std::fmt;

/// HTTP/1.1 transport marker (RFC 6455)
///
/// Use this type parameter to create WebSocket servers and clients
/// that operate over HTTP/1.1 using the traditional WebSocket upgrade.
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::{WebSocketServer, WebSocketClient, Http1, Config};
///
/// // Server
/// let server = WebSocketServer::<Http1>::new(Config::default());
/// server.serve(tcp_listener, handler).await?;
///
/// // Client
/// let client = WebSocketClient::<Http1>::new(Config::default());
/// let ws = client.connect("ws://example.com/ws", None).await?;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Http1;

/// HTTP/2 transport marker (RFC 8441)
///
/// Use this type parameter to create WebSocket servers and clients
/// that operate over HTTP/2 using the Extended CONNECT protocol.
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::{WebSocketServer, WebSocketClient, Http2, Config};
///
/// // Server
/// let server = WebSocketServer::<Http2>::new(Config::default());
/// server.serve(tls_stream, handler).await?;
///
/// // Client
/// let client = WebSocketClient::<Http2>::new(Config::default());
/// let ws = client.connect(stream, "wss://example.com/ws", None).await?;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Http2;

/// HTTP/3 transport marker (RFC 9220)
///
/// Use this type parameter to create WebSocket servers and clients
/// that operate over HTTP/3/QUIC using the Extended CONNECT protocol.
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::{WebSocketServer, WebSocketClient, Http3, Config};
///
/// // Server
/// let server = WebSocketServer::<Http3>::bind(addr, tls_config, Config::default()).await?;
/// server.serve(handler).await?;
///
/// // Client
/// let client = WebSocketClient::<Http3>::new(Config::default());
/// let ws = client.connect(addr, "example.com", "/ws", tls_config).await?;
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Http3;

/// Sealed trait for WebSocket transport types
///
/// This trait is sealed and cannot be implemented outside this crate.
/// It ensures that only `Http1`, `Http2`, and `Http3` can be used as transport markers.
pub trait Transport: private::Sealed + fmt::Debug + Clone + Copy + Send + Sync + 'static {}

impl Transport for Http1 {}
impl Transport for Http2 {}
impl Transport for Http3 {}

mod private {
    pub trait Sealed {}
    impl Sealed for super::Http1 {}
    impl Sealed for super::Http2 {}
    impl Sealed for super::Http3 {}
}

impl fmt::Display for Http1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP/1.1")
    }
}

impl fmt::Display for Http2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP/2")
    }
}

impl fmt::Display for Http3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP/3")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transport_markers_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Http1>();
        assert_send_sync::<Http2>();
        assert_send_sync::<Http3>();
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", Http1), "HTTP/1.1");
        assert_eq!(format!("{}", Http2), "HTTP/2");
        assert_eq!(format!("{}", Http3), "HTTP/3");
    }
}
