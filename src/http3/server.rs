//! HTTP/3 WebSocket server implementation (RFC 9220)
//!
//! This module provides `H3WebSocketServer`, a server that accepts WebSocket
//! connections over HTTP/3/QUIC using the Extended CONNECT protocol.
//!
//! # RFC 9220 Compliance
//!
//! This implementation follows RFC 9220 "Bootstrapping WebSockets with HTTP/3":
//!
//! 1. **SETTINGS_ENABLE_CONNECT_PROTOCOL**: Advertised in HTTP/3 settings (0x08 = 1)
//! 2. **Extended CONNECT**: Accepts requests with `:method=CONNECT`, `:protocol=websocket`
//! 3. **200 OK**: Successful upgrade response
//! 4. **501 Not Implemented**: Returned for unsupported `:protocol` values
//! 5. **Stream closure**: FIN for orderly close, H3_REQUEST_CANCELLED for RST
//!
//! # Architecture
//!
//! The server uses the `h3` crate for proper HTTP/3 frame parsing and QPACK
//! header compression. Each QUIC connection can handle multiple concurrent
//! WebSocket streams (multiplexing).

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes, BytesMut};
use h3::server::Connection as H3Connection;
use http::{Method, StatusCode};
use quinn::{Endpoint, ServerConfig};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::error::{Error, Result};
use crate::protocol::Role;
use crate::{Config, WebSocketStream};

use super::handshake::{H3HandshakeRequest, build_h3_error_response, build_h3_response};

/// HTTP/3 WebSocket server
///
/// Accepts QUIC connections and handles Extended CONNECT requests
/// for WebSocket upgrades per RFC 9220.
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::{Config, Message};
/// use sockudo_ws::http3::H3WebSocketServer;
/// use futures_util::StreamExt;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let tls_config = load_server_tls_config()?;
///
///     let server = H3WebSocketServer::bind(
///         "0.0.0.0:443".parse()?,
///         tls_config,
///         Config::default(),
///     ).await?;
///
///     server.serve(|mut ws, req| async move {
///         println!("HTTP/3 WebSocket at: {}", req.path);
///         while let Some(msg) = ws.next().await {
///             if let Ok(msg) = msg {
///                 ws.send(msg).await.ok();
///             }
///         }
///     }).await?;
///
///     Ok(())
/// }
/// ```
pub struct H3WebSocketServer {
    endpoint: Endpoint,
    config: Config,
}

impl H3WebSocketServer {
    /// Bind to an address and create a new HTTP/3 WebSocket server
    ///
    /// # Arguments
    ///
    /// * `addr` - Socket address to bind to (e.g., "0.0.0.0:443")
    /// * `tls_config` - TLS configuration (required for QUIC)
    /// * `ws_config` - WebSocket configuration
    pub async fn bind(
        addr: SocketAddr,
        tls_config: rustls::ServerConfig,
        ws_config: Config,
    ) -> Result<Self> {
        // Create QUIC server config from rustls config
        let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)
            .map_err(|_| Error::HandshakeFailed("invalid TLS config"))?;

        let server_config = ServerConfig::with_crypto(Arc::new(quic_config));

        // Create QUIC endpoint
        let endpoint = Endpoint::server(server_config, addr).map_err(Error::Io)?;

        Ok(Self {
            endpoint,
            config: ws_config,
        })
    }

    /// Create from an existing QUIC endpoint
    ///
    /// Use this when you need more control over the QUIC configuration,
    /// such as enabling BBR congestion control for better performance
    /// in lossy networks.
    pub fn from_endpoint(endpoint: Endpoint, ws_config: Config) -> Self {
        Self {
            endpoint,
            config: ws_config,
        }
    }

    /// Get the local address the server is bound to
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.local_addr().map_err(Error::Io)
    }

    /// Get the WebSocket configuration
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Serve WebSocket connections with full RFC 9220 compliance
    ///
    /// This accepts QUIC connections, performs HTTP/3 handshake with
    /// SETTINGS_ENABLE_CONNECT_PROTOCOL, and handles Extended CONNECT
    /// requests for WebSocket.
    ///
    /// # Arguments
    ///
    /// * `handler` - Async function to handle each WebSocket connection
    pub async fn serve<F, Fut>(self, handler: F) -> Result<()>
    where
        F: Fn(WebSocketStream<H3DataStream>, H3HandshakeRequest) -> Fut
            + Clone
            + Send
            + Sync
            + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        while let Some(incoming) = self.endpoint.accept().await {
            let handler = handler.clone();
            let config = self.config.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_h3_connection(incoming, handler, config).await {
                    eprintln!("HTTP/3 connection error: {}", e);
                }
            });
        }

        Ok(())
    }

    /// Serve with a request filter
    ///
    /// Like `serve`, but allows filtering or validating requests
    /// before accepting them as WebSocket connections.
    ///
    /// Return `false` from the filter to reject the connection with 403 Forbidden.
    pub async fn serve_with_filter<F, Fut, Filter>(self, filter: Filter, handler: F) -> Result<()>
    where
        F: Fn(WebSocketStream<H3DataStream>, H3HandshakeRequest) -> Fut
            + Clone
            + Send
            + Sync
            + 'static,
        Fut: Future<Output = ()> + Send + 'static,
        Filter: Fn(&H3HandshakeRequest) -> bool + Clone + Send + Sync + 'static,
    {
        while let Some(incoming) = self.endpoint.accept().await {
            let handler = handler.clone();
            let filter = filter.clone();
            let config = self.config.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    handle_h3_connection_filtered(incoming, filter, handler, config).await
                {
                    eprintln!("HTTP/3 connection error: {}", e);
                }
            });
        }

        Ok(())
    }

    /// Close the server endpoint
    pub fn close(&self, error_code: quinn::VarInt, reason: &[u8]) {
        self.endpoint.close(error_code, reason);
    }
}

/// Handle a single QUIC connection with proper HTTP/3 framing
async fn handle_h3_connection<F, Fut>(
    incoming: quinn::Incoming,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H3DataStream>, H3HandshakeRequest) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let connection = incoming.await.map_err(Error::from)?;

    // Create HTTP/3 connection with h3 crate
    // This handles SETTINGS exchange including SETTINGS_ENABLE_CONNECT_PROTOCOL
    let mut h3_conn: H3Connection<h3_quinn::Connection, Bytes> =
        H3Connection::new(h3_quinn::Connection::new(connection))
            .await
            .map_err(Error::from)?;

    // Process HTTP/3 requests
    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                // Resolve the request to get the actual request and stream
                let (request, stream) = match resolver.resolve_request().await {
                    Ok(parts) => parts,
                    Err(e) => {
                        eprintln!("Failed to resolve request: {}", e);
                        continue;
                    }
                };

                let handler = handler.clone();
                let config = config.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_h3_request(request, stream, handler, config).await {
                        eprintln!("HTTP/3 request error: {}", e);
                    }
                });
            }
            Ok(None) => {
                // Connection closed gracefully
                break;
            }
            Err(e) => {
                eprintln!("HTTP/3 accept error: {}", e);
                break;
            }
        }
    }

    Ok(())
}

/// Handle a single QUIC connection with filtering
async fn handle_h3_connection_filtered<F, Fut, Filter>(
    incoming: quinn::Incoming,
    filter: Filter,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H3DataStream>, H3HandshakeRequest) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    Filter: Fn(&H3HandshakeRequest) -> bool + Clone + Send + 'static,
{
    let connection = incoming.await.map_err(Error::from)?;

    let mut h3_conn: H3Connection<h3_quinn::Connection, Bytes> =
        H3Connection::new(h3_quinn::Connection::new(connection))
            .await
            .map_err(Error::from)?;

    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                let (request, stream) = match resolver.resolve_request().await {
                    Ok(parts) => parts,
                    Err(e) => {
                        eprintln!("Failed to resolve request: {}", e);
                        continue;
                    }
                };

                let handler = handler.clone();
                let filter = filter.clone();
                let config = config.clone();

                tokio::spawn(async move {
                    if let Err(e) =
                        handle_h3_request_filtered(request, stream, filter, handler, config).await
                    {
                        eprintln!("HTTP/3 request error: {}", e);
                    }
                });
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("HTTP/3 accept error: {}", e);
                break;
            }
        }
    }

    Ok(())
}

/// Handle a single HTTP/3 request (Extended CONNECT for WebSocket)
async fn handle_h3_request<F, Fut>(
    request: http::Request<()>,
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H3DataStream>, H3HandshakeRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    // Check if this is a CONNECT request
    if request.method() != Method::CONNECT {
        // Not a CONNECT request - return 405 Method Not Allowed
        let response =
            build_h3_error_response(StatusCode::METHOD_NOT_ALLOWED, Some("Expected CONNECT"));
        stream.send_response(response).await.ok();
        return Ok(());
    }

    // Parse the Extended CONNECT request
    // Extract :protocol from h3 extensions
    let protocol_header = request
        .extensions()
        .get::<h3::ext::Protocol>()
        .map(|p| p.as_str().to_string());

    let mut ws_req = H3HandshakeRequest::from_request(&request)
        .ok_or_else(|| Error::HandshakeFailed("invalid CONNECT request"))?;

    // Set protocol from h3 extension if not already set
    if ws_req.protocol.is_none() {
        ws_req.protocol = protocol_header;
    }

    // Validate per RFC 9220
    if let Err(status) = ws_req.validate() {
        let reason = match status {
            StatusCode::NOT_IMPLEMENTED => Some("Unsupported :protocol value"),
            StatusCode::BAD_REQUEST => Some("Invalid WebSocket request"),
            _ => None,
        };
        let response = build_h3_error_response(status, reason);
        stream.send_response(response).await.ok();
        return Ok(());
    }

    // Send 200 OK response to accept the WebSocket connection
    let response = build_h3_response(None, None);
    stream.send_response(response).await.map_err(Error::from)?;

    // Wrap the h3 stream for WebSocket I/O
    let h3_stream = H3DataStream::new(stream);
    let ws = WebSocketStream::from_raw(h3_stream, Role::Server, config);

    handler(ws, ws_req).await;

    Ok(())
}

/// Handle request with filtering
async fn handle_h3_request_filtered<F, Fut, Filter>(
    request: http::Request<()>,
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    filter: Filter,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H3DataStream>, H3HandshakeRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    Filter: Fn(&H3HandshakeRequest) -> bool + Send + 'static,
{
    if request.method() != Method::CONNECT {
        let response =
            build_h3_error_response(StatusCode::METHOD_NOT_ALLOWED, Some("Expected CONNECT"));
        stream.send_response(response).await.ok();
        return Ok(());
    }

    let protocol_header = request
        .extensions()
        .get::<h3::ext::Protocol>()
        .map(|p| p.as_str().to_string());

    let mut ws_req = H3HandshakeRequest::from_request(&request)
        .ok_or_else(|| Error::HandshakeFailed("invalid CONNECT request"))?;

    if ws_req.protocol.is_none() {
        ws_req.protocol = protocol_header;
    }

    // Apply filter before validation
    if !filter(&ws_req) {
        let response = build_h3_error_response(StatusCode::FORBIDDEN, Some("Rejected by filter"));
        stream.send_response(response).await.ok();
        return Ok(());
    }

    if let Err(status) = ws_req.validate() {
        let response = build_h3_error_response(status, None);
        stream.send_response(response).await.ok();
        return Ok(());
    }

    let response = build_h3_response(None, None);
    stream.send_response(response).await.ok();

    let h3_stream = H3DataStream::new(stream);
    let ws = WebSocketStream::from_raw(h3_stream, Role::Server, config);

    handler(ws, ws_req).await;

    Ok(())
}

impl std::fmt::Debug for H3WebSocketServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3WebSocketServer")
            .field("local_addr", &self.endpoint.local_addr().ok())
            .field("config", &self.config)
            .finish()
    }
}

// ============================================================================
// H3DataStream - AsyncRead/AsyncWrite wrapper for h3 request streams
// ============================================================================

/// Wrapper around h3 request stream that implements AsyncRead + AsyncWrite
///
/// After the HTTP/3 CONNECT handshake, this stream is used for raw
/// WebSocket frame data exchange.
pub struct H3DataStream {
    stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    read_buf: BytesMut,
}

impl H3DataStream {
    fn new(stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(64 * 1024),
        }
    }
}

impl AsyncRead for H3DataStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // First drain any buffered data
        if !this.read_buf.is_empty() {
            let to_copy = std::cmp::min(buf.remaining(), this.read_buf.len());
            buf.put_slice(&this.read_buf.split_to(to_copy));
            return Poll::Ready(Ok(()));
        }

        // Use a boxed future to work around borrow checker limitations
        let mut fut = Box::pin(this.stream.recv_data());

        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(Some(mut data))) => {
                // data is impl Buf, use Buf trait methods
                let data_len = data.remaining();
                let to_copy = std::cmp::min(buf.remaining(), data_len);

                // Copy data to output buffer
                let chunk = data.copy_to_bytes(to_copy);
                buf.put_slice(&chunk);

                // Buffer any remaining data
                if data.has_remaining() {
                    while data.has_remaining() {
                        this.read_buf.extend_from_slice(data.chunk());
                        let len = data.chunk().len();
                        data.advance(len);
                    }
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(None)) => {
                // Stream finished
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for H3DataStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // h3's send_data takes Bytes
        let data = Bytes::copy_from_slice(buf);
        let fut = self.stream.send_data(data);
        tokio::pin!(fut);

        match fut.poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // h3/QUIC handles flushing internally
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fut = self.stream.finish();
        tokio::pin!(fut);

        match fut.poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

// SAFETY: H3DataStream is Send because h3's RequestStream is Send
unsafe impl Send for H3DataStream {}
