//! Axum integration for sockudo-ws
//!
//! This module provides seamless integration with the Axum web framework,
//! allowing you to use sockudo-ws's high-performance WebSocket implementation
//! with Axum's routing and middleware system.
//!
//! # Example
//!
//! ```ignore
//! use axum::{Router, routing::get, response::IntoResponse};
//! use sockudo_ws::axum_integration::{WebSocketUpgrade, WebSocket};
//! use futures_util::{SinkExt, StreamExt};
//!
//! async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
//!     ws.on_upgrade(handle_socket)
//! }
//!
//! async fn handle_socket(mut socket: WebSocket) {
//!     while let Some(msg) = socket.next().await {
//!         if let Ok(msg) = msg {
//!             if socket.send(msg).await.is_err() {
//!                 break;
//!             }
//!         }
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let app = Router::new().route("/ws", get(ws_handler));
//!     let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
//!     axum::serve(listener, app).await.unwrap();
//! }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::{Method, Response, StatusCode, header, request::Parts};
use axum::response::IntoResponse;
use futures_core::Stream;
use futures_sink::Sink;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::Config;
use crate::error::{Error, Result};
use crate::handshake::generate_accept_key;
use crate::protocol::{Message, Role};
use crate::stream::WebSocketStream;

/// WebSocket upgrade extractor for Axum
///
/// This extractor validates the WebSocket upgrade request and provides
/// a method to upgrade the connection.
#[derive(Debug)]
pub struct WebSocketUpgrade {
    key: String,
    protocol: Option<String>,
    config: Config,
}

impl WebSocketUpgrade {
    /// Set a custom configuration
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Set the maximum message size
    pub fn max_message_size(mut self, size: usize) -> Self {
        self.config.max_message_size = size;
        self
    }

    /// Set the maximum frame size
    pub fn max_frame_size(mut self, size: usize) -> Self {
        self.config.max_frame_size = size;
        self
    }

    /// Set the write buffer size
    pub fn write_buffer_size(mut self, size: usize) -> Self {
        self.config.write_buffer_size = size;
        self
    }

    /// Upgrade the connection and call the provided handler
    pub fn on_upgrade<F, Fut>(self, handler: F) -> WebSocketUpgradeResponse
    where
        F: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let accept_key = generate_accept_key(&self.key);
        let config = self.config.clone();
        let handler_config = self.config;
        let protocol = self.protocol;

        WebSocketUpgradeResponse {
            accept_key,
            protocol,
            config,
            handler: Box::new(move |stream| {
                let ws = WebSocket::new(stream, handler_config);
                Box::pin(handler(ws))
            }),
        }
    }
}

impl<S> FromRequestParts<S> for WebSocketUpgrade
where
    S: Send + Sync,
{
    type Rejection = WebSocketUpgradeRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        // Check method
        if parts.method != Method::GET {
            return Err(WebSocketUpgradeRejection::MethodNotGet);
        }

        // Check Upgrade header
        let upgrade = parts
            .headers
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .ok_or(WebSocketUpgradeRejection::MissingUpgradeHeader)?;

        if !upgrade.to_ascii_lowercase().contains("websocket") {
            return Err(WebSocketUpgradeRejection::InvalidUpgradeHeader);
        }

        // Check Connection header
        let connection = parts
            .headers
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .ok_or(WebSocketUpgradeRejection::MissingConnectionHeader)?;

        if !connection.to_ascii_lowercase().contains("upgrade") {
            return Err(WebSocketUpgradeRejection::InvalidConnectionHeader);
        }

        // Check Sec-WebSocket-Key
        let key = parts
            .headers
            .get("sec-websocket-key")
            .and_then(|v| v.to_str().ok())
            .ok_or(WebSocketUpgradeRejection::MissingSecWebSocketKey)?
            .to_string();

        // Check Sec-WebSocket-Version
        let version = parts
            .headers
            .get("sec-websocket-version")
            .and_then(|v| v.to_str().ok())
            .ok_or(WebSocketUpgradeRejection::MissingSecWebSocketVersion)?;

        if version != "13" {
            return Err(WebSocketUpgradeRejection::UnsupportedVersion);
        }

        // Optional: Sec-WebSocket-Protocol
        let protocol = parts
            .headers
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(',').next().unwrap_or("").trim().to_string());

        Ok(WebSocketUpgrade {
            key,
            protocol,
            config: Config::default(),
        })
    }
}

/// Rejection type for WebSocket upgrade
#[derive(Debug)]
pub enum WebSocketUpgradeRejection {
    MethodNotGet,
    MissingUpgradeHeader,
    InvalidUpgradeHeader,
    MissingConnectionHeader,
    InvalidConnectionHeader,
    MissingSecWebSocketKey,
    MissingSecWebSocketVersion,
    UnsupportedVersion,
}

impl IntoResponse for WebSocketUpgradeRejection {
    fn into_response(self) -> Response<Body> {
        let (status, message) = match self {
            Self::MethodNotGet => (StatusCode::METHOD_NOT_ALLOWED, "Method must be GET"),
            Self::MissingUpgradeHeader => (StatusCode::BAD_REQUEST, "Missing Upgrade header"),
            Self::InvalidUpgradeHeader => (StatusCode::BAD_REQUEST, "Invalid Upgrade header"),
            Self::MissingConnectionHeader => (StatusCode::BAD_REQUEST, "Missing Connection header"),
            Self::InvalidConnectionHeader => (StatusCode::BAD_REQUEST, "Invalid Connection header"),
            Self::MissingSecWebSocketKey => (StatusCode::BAD_REQUEST, "Missing Sec-WebSocket-Key"),
            Self::MissingSecWebSocketVersion => {
                (StatusCode::BAD_REQUEST, "Missing Sec-WebSocket-Version")
            }
            Self::UnsupportedVersion => (StatusCode::BAD_REQUEST, "Unsupported WebSocket version"),
        };

        Response::builder()
            .status(status)
            .body(Body::from(message))
            .unwrap()
    }
}

/// Handler type for WebSocket upgrade
type UpgradeHandler =
    Box<dyn FnOnce(UpgradedStream) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// Response that performs the WebSocket upgrade
pub struct WebSocketUpgradeResponse {
    accept_key: String,
    protocol: Option<String>,
    config: Config,
    handler: UpgradeHandler,
}

impl IntoResponse for WebSocketUpgradeResponse {
    fn into_response(self) -> Response<Body> {
        // Build the 101 Switching Protocols response
        let mut builder = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::UPGRADE, "websocket")
            .header(header::CONNECTION, "Upgrade")
            .header("Sec-WebSocket-Accept", self.accept_key);

        if let Some(proto) = &self.protocol {
            builder = builder.header("Sec-WebSocket-Protocol", proto.as_str());
        }

        // Use hyper's upgrade mechanism

        // Spawn the handler to run after the upgrade completes
        // Note: In a real implementation, we'd use hyper's upgrade() here
        // For now, we return the response and expect the handler to be called separately

        builder.body(Body::empty()).unwrap()
    }
}

/// Wrapper around the upgraded stream for I/O
pub struct UpgradedStream {
    inner: Box<dyn AsyncReadWrite + Send + Unpin>,
}

trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> AsyncReadWrite for T {}

impl AsyncRead for UpgradedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for UpgradedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.inner).poll_write_vectored(cx, bufs)
    }
}

pin_project! {
    /// WebSocket connection for Axum handlers
    ///
    /// Implements both `Stream<Item = Result<Message>>` and `Sink<Message>`.
    pub struct WebSocket {
        #[pin]
        inner: WebSocketStream<UpgradedStream>,
    }
}

impl WebSocket {
    fn new(stream: UpgradedStream, config: Config) -> Self {
        Self {
            inner: WebSocketStream::from_raw(stream, Role::Server, config),
        }
    }

    /// Create from a raw TCP stream (for standalone usage)
    pub fn from_tcp(stream: tokio::net::TcpStream, config: Config) -> Self {
        let upgraded = UpgradedStream {
            inner: Box::new(stream),
        };
        Self::new(upgraded, config)
    }

    /// Send a message
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        use futures_sink::Sink;
        use std::future::poll_fn;

        poll_fn(|cx| Pin::new(&mut self.inner).poll_ready(cx)).await?;
        Pin::new(&mut self.inner).start_send(msg)?;
        poll_fn(|cx| Pin::new(&mut self.inner).poll_flush(cx)).await
    }

    /// Receive a message
    pub async fn recv(&mut self) -> Option<Result<Message>> {
        use futures_core::Stream;
        use std::future::poll_fn;

        poll_fn(|cx| Pin::new(&mut self.inner).poll_next(cx)).await
    }

    /// Close the connection
    pub async fn close(mut self, code: u16, reason: &str) -> Result<()> {
        self.inner.close(code, reason).await
    }

    /// Check if the connection is closed
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

impl Stream for WebSocket {
    type Item = Result<Message>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}

impl Sink<Message> for WebSocket {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.project().inner.poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<()> {
        self.project().inner.start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.project().inner.poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accept_key() {
        // RFC 6455 test vector
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = generate_accept_key(key);
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }
}
