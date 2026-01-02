//! Async WebSocket stream implementation
//!
//! This module provides the main `WebSocketStream` type that implements
//! both `Stream` and `Sink` traits for async message handling.
//!
//! # Transport-Generic Streams
//!
//! The `Stream<T>` type provides a unified interface for different transports:
//!
//! - `Stream<Http1>` - Wraps any `AsyncRead + AsyncWrite` (TCP, TLS)
//! - `Stream<Http2>` - Wraps h2's send/receive streams
//! - `Stream<Http3>` - Wraps QUIC/h3 streams
//!
//! # Split Streams
//!
//! For concurrent send/receive, use the `split()` method:
//!
//! ```ignore
//! let (mut reader, mut writer) = ws.split();
//!
//! // Spawn a task for reading
//! let read_task = tokio::spawn(async move {
//!     while let Some(msg) = reader.next().await {
//!         println!("Received: {:?}", msg);
//!     }
//! });
//!
//! // Send messages from another task
//! writer.send(Message::Text("Hello".into())).await?;
//! ```

mod transport_stream;
mod websocket;

pub use transport_stream::Stream;
pub use websocket::*;
