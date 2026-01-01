//! HTTP/3 WebSocket support (RFC 9220)
//!
//! This module provides WebSocket bootstrapping over HTTP/3 using QUIC,
//! implementing the Extended CONNECT Protocol defined in RFC 9220.
//!
//! # RFC 9220 Compliance
//!
//! This implementation follows RFC 9220 "Bootstrapping WebSockets with HTTP/3":
//!
//! - **SETTINGS_ENABLE_CONNECT_PROTOCOL (0x08)**: Server advertises support
//! - **Extended CONNECT**: Client sends `:method=CONNECT` with `:protocol=websocket`
//! - **200 OK response**: Server accepts and transitions to WebSocket mode
//! - **501 Not Implemented**: Server rejects unsupported `:protocol` values
//! - **Stream closure**: FIN maps to orderly close, RST to H3_REQUEST_CANCELLED
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │         WebSocketStream<H3Stream>        │
//! │            (same familiar API)           │
//! ├─────────────────────────────────────────┤
//! │              HTTP/3 Layer                │
//! │    Extended CONNECT (:protocol=websocket)│
//! │         SETTINGS_ENABLE_CONNECT_PROTOCOL │
//! ├─────────────────────────────────────────┤
//! │              QUIC Transport              │
//! │    (multiplexed streams over UDP)        │
//! │    Uses io_uring on Linux automatically  │
//! └─────────────────────────────────────────┘
//! ```
//!
//! # Benefits of HTTP/3 WebSocket
//!
//! - **No head-of-line blocking**: Each WebSocket stream is independent
//! - **Faster connection setup**: 0-RTT support for returning clients
//! - **Better mobile performance**: Handles network changes gracefully
//! - **Multiplexing**: Multiple WebSocket connections over one QUIC connection
//! - **Built-in encryption**: TLS 1.3 is mandatory in QUIC
//!
//! # Server Example
//!
//! ```ignore
//! use sockudo_ws::{Config, Message};
//! use sockudo_ws::http3::H3WebSocketServer;
//! use futures_util::StreamExt;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Load TLS certificate and key (required for QUIC)
//!     let tls_config = load_server_tls_config()?;
//!
//!     let server = H3WebSocketServer::bind(
//!         "0.0.0.0:443".parse()?,
//!         tls_config,
//!         Config::default(),
//!     ).await?;
//!
//!     server.serve(|mut ws, req| async move {
//!         println!("HTTP/3 WebSocket connection to: {}", req.path);
//!
//!         while let Some(msg) = ws.next().await {
//!             if let Ok(msg) = msg {
//!                 ws.send(msg).await.ok();
//!             }
//!         }
//!     }).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Client Example
//!
//! ```ignore
//! use sockudo_ws::{Config, Message};
//! use sockudo_ws::http3::H3WebSocketClient;
//! use futures_util::{SinkExt, StreamExt};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let tls_config = load_client_tls_config()?;
//!
//!     let client = H3WebSocketClient::new(Config::default());
//!     let mut ws = client.connect(
//!         "server.example.com:443".parse()?,
//!         "server.example.com",
//!         "/ws",
//!         tls_config,
//!     ).await?;
//!
//!     // Same API as HTTP/1.1 and HTTP/2!
//!     ws.send(Message::Text("Hello over HTTP/3!".into())).await?;
//!
//!     while let Some(msg) = ws.next().await {
//!         println!("Received: {:?}", msg?);
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! # io_uring Integration
//!
//! The `quinn` crate (QUIC implementation) automatically uses io_uring
//! on Linux when available, providing optimal performance without
//! any extra configuration.

mod client;
mod handshake;
mod server;
mod stream;

pub use client::H3WebSocketClient;
pub use handshake::{H3HandshakeRequest, build_h3_error_response, build_h3_response};
pub use server::H3WebSocketServer;
pub use stream::H3Stream;

/// HTTP/3 SETTINGS_ENABLE_CONNECT_PROTOCOL parameter (RFC 9220, Section 5)
///
/// Value: 0x08 (same as HTTP/2 per RFC 8441)
/// Default: 0 (disabled)
///
/// When set to 1, indicates the server supports Extended CONNECT
/// with the `:protocol` pseudo-header for WebSocket bootstrapping.
pub const SETTINGS_ENABLE_CONNECT_PROTOCOL: u64 = 0x08;

/// The `:protocol` pseudo-header value for WebSocket (RFC 9220)
pub const PROTOCOL_WEBSOCKET: &str = "websocket";

/// H3_REQUEST_CANCELLED error code for stream reset (RFC 9114, Section 8.1)
///
/// Used when closing a WebSocket connection abnormally, analogous to
/// TCP RST in RFC 6455.
pub const H3_REQUEST_CANCELLED: u64 = 0x10c;
