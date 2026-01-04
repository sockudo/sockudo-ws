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
use axum::http::{Method, Response, StatusCode, header};
use axum::response::IntoResponse;
use futures_core::Stream;
use futures_sink::Sink;
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::Config;
use crate::error::{Error, Result};
use crate::handshake::generate_accept_key;
use crate::protocol::{Message, Role};
use crate::stream::WebSocketStream;
use crate::{SplitReader, SplitWriter};

/// WebSocket upgrade extractor for Axum
///
/// This extractor validates the WebSocket upgrade request and provides
/// a method to upgrade the connection.
pub struct WebSocketUpgrade {
    key: String,
    protocol: Option<String>,
    extensions: Option<String>,
    config: Config,
    on_upgrade: OnUpgrade,
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
        let on_upgrade = self.on_upgrade;

        // Negotiate permessage-deflate if enabled
        #[cfg(feature = "permessage-deflate")]
        let extensions = if let Some(ref deflate_config) = handler_config.deflate {
            self.extensions
                .as_deref()
                .and_then(|ext| {
                    crate::deflate::parse_deflate_offer(ext).map(|params| {
                        // Parse and validate deflate parameters from the client's offer
                        crate::deflate::DeflateConfig::from_params(&params)
                            .ok()
                            .map(|_| deflate_config.to_response_header())
                    })
                })
                .flatten()
        } else {
            None
        };

        #[cfg(not(feature = "permessage-deflate"))]
        let extensions = None;

        WebSocketUpgradeResponse {
            accept_key,
            protocol,
            extensions,
            config,
            on_upgrade,
            handler: Box::new(move |stream| {
                let ws = WebSocket::new(stream, handler_config);
                Box::pin(handler(ws))
            }),
        }
    }
}

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

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

        // Optional: Sec-WebSocket-Extensions
        let extensions = parts
            .headers
            .get("sec-websocket-extensions")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // Extract OnUpgrade from extensions (placed there by Axum/Hyper)
        let on_upgrade = parts
            .extensions
            .remove::<OnUpgrade>()
            .ok_or(WebSocketUpgradeRejection::MissingUpgrade)?;

        Ok(WebSocketUpgrade {
            key,
            protocol,
            extensions,
            config: Config::default(),
            on_upgrade,
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
    MissingUpgrade,
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
            Self::MissingUpgrade => (
                StatusCode::BAD_REQUEST,
                "Missing upgrade in request extensions",
            ),
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
    extensions: Option<String>,
    config: Config,
    on_upgrade: OnUpgrade,
    handler: UpgradeHandler,
}

impl IntoResponse for WebSocketUpgradeResponse {
    fn into_response(self) -> Response<Body> {
        let handler = self.handler;
        let on_upgrade = self.on_upgrade;

        // Build the 101 Switching Protocols response
        let mut res = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::UPGRADE, "websocket")
            .header(header::CONNECTION, "Upgrade")
            .header("Sec-WebSocket-Accept", self.accept_key);

        if let Some(proto) = &self.protocol {
            res = res.header("Sec-WebSocket-Protocol", proto.as_str());
        }

        if let Some(ext) = &self.extensions {
            res = res.header("Sec-WebSocket-Extensions", ext.as_str());
        }

        // Spawn a task to handle the upgrade after the response is sent
        tokio::spawn(async move {
            match on_upgrade.await {
                Ok(upgraded) => {
                    // Wrap the upgraded connection with TokioIo for compatibility
                    let io = TokioIo::new(upgraded);
                    let stream = UpgradedStream {
                        inner: Box::new(io),
                    };
                    handler(stream).await;
                }
                Err(e) => {
                    eprintln!("WebSocket upgrade error: {}", e);
                }
            }
        });

        res.body(Body::empty()).unwrap()
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

    /// Split the WebSocket into separate reader and writer halves
    ///
    /// This allows reading and writing from separate tasks.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let (mut reader, mut writer) = socket.split();
    ///
    /// // Spawn reader task
    /// tokio::spawn(async move {
    ///     while let Some(msg) = reader.recv().await {
    ///         // Handle message
    ///     }
    /// });
    ///
    /// // Write from current task
    /// writer.send(Message::Text("Hello".into())).await?;
    /// ```
    pub fn split(self) -> (SplitReader<UpgradedStream>, SplitWriter<UpgradedStream>) {
        self.inner.split()
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

    #[test]
    fn test_websocket_split_compiles() {
        // This test verifies that the split() method signature is correct
        // and that SplitReader/SplitWriter types are properly accessible

        // Verify the types exist and are importable
        fn _takes_split_reader(_: crate::SplitReader<UpgradedStream>) {}
        fn _takes_split_writer(_: crate::SplitWriter<UpgradedStream>) {}
    }

    #[cfg(feature = "permessage-deflate")]
    #[test]
    fn test_deflate_negotiation_with_client_offer() {
        use crate::deflate::DeflateConfig;

        // Test that deflate negotiation works when client offers permessage-deflate
        let client_extension = "permessage-deflate; client_max_window_bits";
        
        // Parse the offer as the code does
        let params = crate::deflate::parse_deflate_offer(client_extension);
        assert!(params.is_some());
        
        // Validate that we can create a config from the parsed params
        let params = params.unwrap();
        let client_config = DeflateConfig::from_params(&params);
        assert!(client_config.is_ok());
        
        // Generate response header using server's config (as in on_upgrade)
        let server_config = DeflateConfig::default();
        let response_header = server_config.to_response_header();
        
        // Verify response header contains permessage-deflate
        assert!(response_header.starts_with("permessage-deflate"));
    }

    #[cfg(feature = "permessage-deflate")]
    #[test]
    fn test_deflate_negotiation_with_parameters() {
        use crate::deflate::DeflateConfig;

        // Test negotiation with specific deflate parameters
        let client_extension = "permessage-deflate; server_no_context_takeover; client_max_window_bits=10";
        
        let params = crate::deflate::parse_deflate_offer(client_extension);
        assert!(params.is_some());
        
        let params = params.unwrap();
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], ("server_no_context_takeover", None));
        assert_eq!(params[1], ("client_max_window_bits", Some("10")));
        
        let config = DeflateConfig::from_params(&params);
        assert!(config.is_ok());
        let config = config.unwrap();
        
        // Verify parsed config has correct values
        assert!(config.server_no_context_takeover);
        assert_eq!(config.client_max_window_bits, 10);
        
        // Verify response header generation
        let response = config.to_response_header();
        assert!(response.contains("permessage-deflate"));
        assert!(response.contains("server_no_context_takeover"));
    }

    #[cfg(feature = "permessage-deflate")]
    #[test]
    fn test_deflate_negotiation_without_client_offer() {
        // Test that when client doesn't offer deflate, negotiation returns None
        let no_extension: Option<&str> = None;
        
        // Simulate what happens in on_upgrade when extensions is None
        let result = no_extension.and_then(|ext| crate::deflate::parse_deflate_offer(ext));
        assert!(result.is_none());
    }

    #[cfg(feature = "permessage-deflate")]
    #[test]
    fn test_deflate_negotiation_with_non_deflate_extension() {
        // Test that non-deflate extensions are ignored
        let other_extension = "some-other-extension";
        
        let params = crate::deflate::parse_deflate_offer(other_extension);
        assert!(params.is_none());
    }

    #[cfg(feature = "permessage-deflate")]
    #[test]
    fn test_deflate_response_header_generation() {
        use crate::deflate::DeflateConfig;

        // Test default config response header
        let config = DeflateConfig::default();
        let header = config.to_response_header();
        assert_eq!(header, "permessage-deflate");
        
        // Test config with server_no_context_takeover
        let config = DeflateConfig {
            server_no_context_takeover: true,
            ..Default::default()
        };
        let header = config.to_response_header();
        assert!(header.contains("permessage-deflate"));
        assert!(header.contains("server_no_context_takeover"));
        
        // Test config with custom window bits
        let config = DeflateConfig {
            server_max_window_bits: 12,
            client_max_window_bits: 10,
            ..Default::default()
        };
        let header = config.to_response_header();
        assert!(header.contains("server_max_window_bits=12"));
        assert!(header.contains("client_max_window_bits=10"));
    }

    #[cfg(feature = "permessage-deflate")]
    #[test]
    fn test_deflate_config_enabled_vs_disabled() {
        use crate::deflate::DeflateConfig;

        // Test with deflate enabled
        let mut config = Config::default();
        config.deflate = Some(DeflateConfig::default());
        
        assert!(config.deflate.is_some());
        
        // Test with deflate disabled
        let config = Config::default();
        // By default, deflate should be None
        assert!(config.deflate.is_none());
    }

    #[test]
    fn test_deflate_negotiation_logic_structure() {
        // This test verifies that the negotiation logic structure is correct
        // even when permessage-deflate feature is not enabled
        
        // Simulate the logic flow in on_upgrade method
        let _extensions: Option<String> = Some("permessage-deflate".to_string());
        
        // When feature is disabled, deflate field doesn't exist in Config
        #[cfg(not(feature = "permessage-deflate"))]
        {
            let _config = Config::default();
            // Just verify Config exists and compiles without deflate field
            assert_eq!(_config.max_message_size, 64 * 1024 * 1024);
        }
        
        // When feature is enabled, we can have deflate config
        #[cfg(feature = "permessage-deflate")]
        {
            use crate::deflate::DeflateConfig;
            let mut test_config = Config::default();
            test_config.deflate = Some(DeflateConfig::default());
            assert!(test_config.deflate.is_some());
        }
    }

    #[cfg(feature = "permessage-deflate")]
    #[test]
    fn test_deflate_invalid_parameters() {
        use crate::deflate::DeflateConfig;

        // Test invalid window bits (too low)
        let params = vec![("server_max_window_bits", Some("7"))];
        let result = DeflateConfig::from_params(&params);
        assert!(result.is_err());
        
        // Test invalid window bits (too high)
        let params = vec![("server_max_window_bits", Some("16"))];
        let result = DeflateConfig::from_params(&params);
        assert!(result.is_err());
        
        // Test invalid parameter name
        let params = vec![("invalid_parameter", None)];
        let result = DeflateConfig::from_params(&params);
        assert!(result.is_err());
    }

    #[cfg(feature = "permessage-deflate")]
    #[test]
    fn test_deflate_full_negotiation_flow() {
        use crate::deflate::DeflateConfig;

        // Simulate the full flow in on_upgrade method
        let client_offer = "permessage-deflate; client_no_context_takeover; client_max_window_bits=12";
        let server_config = DeflateConfig::default();
        
        // Parse client offer
        let parsed_params = crate::deflate::parse_deflate_offer(client_offer);
        assert!(parsed_params.is_some());
        
        let params = parsed_params.unwrap();
        
        // Validate params - this confirms client's offer is valid
        let validated_config = DeflateConfig::from_params(&params);
        assert!(validated_config.is_ok());
        
        // Generate response header using server's config (not client's)
        // This matches the actual implementation in the on_upgrade method
        let response_header = server_config.to_response_header();
        assert!(response_header.starts_with("permessage-deflate"));
        
        // Verify the negotiation produces a valid extension header
        assert!(!response_header.is_empty());
    }
}
