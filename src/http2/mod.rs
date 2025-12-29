//! HTTP/2 WebSocket support (RFC 8441)
//!
//! This module provides WebSocket bootstrapping over HTTP/2 using the
//! Extended CONNECT Protocol defined in RFC 8441.
//!
//! # Overview
//!
//! RFC 8441 allows WebSocket connections to be established over HTTP/2 streams,
//! enabling multiplexing of multiple WebSocket connections over a single TCP connection.
//!
//! Key differences from HTTP/1.1 WebSocket upgrade:
//! - Uses CONNECT method with `:protocol = websocket` pseudo-header
//! - No `Upgrade` or `Connection` headers
//! - No `Sec-WebSocket-Key`/`Accept` validation (handled by HTTP/2)
//! - WebSocket runs on a single multiplexed HTTP/2 stream
//!
//! # Server Example
//!
//! ```ignore
//! use sockudo_ws::{Config, Message};
//! use sockudo_ws::http2::H2WebSocketServer;
//! use futures_util::StreamExt;
//!
//! #[tokio::main]
//! async fn main() {
//!     let listener = tokio::net::TcpListener::bind("127.0.0.1:8443").await.unwrap();
//!     let server = H2WebSocketServer::new(Config::default());
//!
//!     loop {
//!         let (stream, _) = listener.accept().await.unwrap();
//!         // Note: TLS is typically required for HTTP/2
//!         let tls_stream = do_tls_handshake(stream).await;
//!
//!         server.serve(tls_stream, |mut ws, req| async move {
//!             println!("WebSocket connection to: {}", req.path);
//!             while let Some(msg) = ws.next().await {
//!                 if let Ok(msg) = msg {
//!                     ws.send(msg).await.ok();
//!                 }
//!             }
//!         }).await.ok();
//!     }
//! }
//! ```
//!
//! # Client Example
//!
//! ```ignore
//! use sockudo_ws::{Config, Message};
//! use sockudo_ws::http2::H2WebSocketClient;
//! use futures_util::SinkExt;
//!
//! #[tokio::main]
//! async fn main() {
//!     let stream = tokio::net::TcpStream::connect("server:443").await.unwrap();
//!     let tls_stream = do_tls_handshake(stream).await;
//!
//!     let client = H2WebSocketClient::new(Config::default());
//!     let mut ws = client.connect(tls_stream, "wss://server/ws", None).await.unwrap();
//!
//!     ws.send(Message::Text("Hello over HTTP/2!".into())).await.unwrap();
//! }
//! ```

mod client;
mod handshake;
mod server;
mod stream;

pub use client::H2WebSocketClient;
pub use handshake::{build_h2_response, H2HandshakeRequest, H2HandshakeResponse};
pub use server::H2WebSocketServer;
pub use stream::H2Stream;

/// HTTP/2 SETTINGS_ENABLE_CONNECT_PROTOCOL parameter (RFC 8441)
///
/// When set to 1, indicates that the server supports the Extended CONNECT method
/// for bootstrapping WebSocket connections over HTTP/2.
pub const SETTINGS_ENABLE_CONNECT_PROTOCOL: u16 = 0x08;
