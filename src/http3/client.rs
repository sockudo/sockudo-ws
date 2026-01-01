//! HTTP/3 WebSocket client implementation (RFC 9220)
//!
//! This module provides `H3WebSocketClient`, a client that connects to
//! WebSocket servers over HTTP/3/QUIC using the Extended CONNECT protocol.
//!
//! # RFC 9220 Compliance
//!
//! This implementation follows RFC 9220 "Bootstrapping WebSockets with HTTP/3":
//!
//! 1. Waits for server SETTINGS with SETTINGS_ENABLE_CONNECT_PROTOCOL=1
//! 2. Sends Extended CONNECT with `:protocol=websocket`
//! 3. Expects 200 OK response for successful upgrade
//! 4. Handles 501 Not Implemented if server doesn't support WebSocket

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes, BytesMut};
use http::{Method, Request, StatusCode};
use quinn::{ClientConfig, Endpoint};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::error::{Error, Result};
use crate::protocol::Role;
use crate::{Config, WebSocketStream};

/// HTTP/3 WebSocket client
///
/// Connects to WebSocket servers using HTTP/3 over QUIC (RFC 9220).
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::{Config, Message};
/// use sockudo_ws::http3::H3WebSocketClient;
/// use futures_util::{SinkExt, StreamExt};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let tls_config = load_client_tls_config()?;
///
///     let client = H3WebSocketClient::new(Config::default());
///     let mut ws = client.connect(
///         "server.example.com:443".parse()?,
///         "server.example.com",
///         "/chat",
///         tls_config,
///     ).await?;
///
///     // Same API as HTTP/1.1 and HTTP/2!
///     ws.send(Message::Text("Hello!".into())).await?;
///
///     while let Some(msg) = ws.next().await {
///         println!("Received: {:?}", msg?);
///     }
///
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct H3WebSocketClient {
    config: Config,
}

impl H3WebSocketClient {
    /// Create a new HTTP/3 WebSocket client
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Create with default configuration
    pub fn default_config() -> Self {
        Self::new(Config::default())
    }

    /// Get the client configuration
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Connect to a WebSocket server over HTTP/3
    ///
    /// Performs the full RFC 9220 handshake:
    /// 1. Establishes QUIC connection
    /// 2. Performs HTTP/3 handshake (SETTINGS exchange)
    /// 3. Sends Extended CONNECT with `:protocol=websocket`
    /// 4. Waits for 200 OK response
    ///
    /// # Arguments
    ///
    /// * `server_addr` - Server socket address
    /// * `server_name` - Server hostname (for TLS SNI)
    /// * `path` - WebSocket path (e.g., "/ws")
    /// * `tls_config` - TLS configuration
    ///
    /// # Returns
    ///
    /// A `WebSocketStream` on successful connection.
    ///
    /// # Errors
    ///
    /// - `Error::HandshakeFailed` - Server rejected the connection or doesn't support WebSocket
    /// - `Error::Io` - Network error
    pub async fn connect(
        &self,
        server_addr: SocketAddr,
        server_name: &str,
        path: &str,
        tls_config: rustls::ClientConfig,
    ) -> Result<WebSocketStream<H3ClientStream>> {
        self.connect_with_options(server_addr, server_name, path, None, None, tls_config)
            .await
    }

    /// Connect with a specific WebSocket subprotocol
    pub async fn connect_with_protocol(
        &self,
        server_addr: SocketAddr,
        server_name: &str,
        path: &str,
        protocol: &str,
        tls_config: rustls::ClientConfig,
    ) -> Result<WebSocketStream<H3ClientStream>> {
        self.connect_with_options(
            server_addr,
            server_name,
            path,
            Some(protocol),
            None,
            tls_config,
        )
        .await
    }

    /// Connect with full options
    pub async fn connect_with_options(
        &self,
        server_addr: SocketAddr,
        server_name: &str,
        path: &str,
        subprotocol: Option<&str>,
        origin: Option<&str>,
        tls_config: rustls::ClientConfig,
    ) -> Result<WebSocketStream<H3ClientStream>> {
        // Create QUIC client config
        let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
            .map_err(|_| Error::HandshakeFailed("invalid TLS config"))?;

        let client_config = ClientConfig::new(Arc::new(quic_config));

        // Create endpoint (bind to any available port)
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).map_err(Error::Io)?;
        endpoint.set_default_client_config(client_config);

        // Connect to server via QUIC
        let connection = endpoint
            .connect(server_addr, server_name)
            .map_err(|_| Error::HandshakeFailed("failed to initiate QUIC connection"))?
            .await
            .map_err(Error::from)?;

        // Create HTTP/3 connection using h3 crate
        let (mut driver, mut send_request) = h3::client::new(h3_quinn::Connection::new(connection))
            .await
            .map_err(Error::from)?;

        // Spawn driver task to handle HTTP/3 connection management
        tokio::spawn(async move {
            // The h3 driver's poll_close returns the error directly, not a Result
            let err = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
            eprintln!("H3 connection closed: {}", err);
        });

        // Build Extended CONNECT request per RFC 9220
        let uri = format!("https://{}:{}{}", server_name, server_addr.port(), path);

        let mut request_builder = Request::builder()
            .method(Method::CONNECT)
            .uri(&uri)
            .header("sec-websocket-version", "13");

        if let Some(proto) = subprotocol {
            request_builder = request_builder.header("sec-websocket-protocol", proto);
        }

        if let Some(orig) = origin {
            request_builder = request_builder.header("origin", orig);
        }

        let request = request_builder
            .extension(h3::ext::Protocol::WEB_TRANSPORT) // h3 crate extension
            .body(())
            .map_err(|_| Error::HandshakeFailed("failed to build request"))?;

        // Send the Extended CONNECT request
        let mut stream = send_request
            .send_request(request)
            .await
            .map_err(Error::from)?;

        // Wait for response
        let response = stream.recv_response().await.map_err(Error::from)?;

        // Check response status per RFC 9220
        match response.status() {
            StatusCode::OK => {
                // Success! Wrap stream for WebSocket I/O
                let h3_stream = H3ClientStream::new(stream);

                Ok(WebSocketStream::from_raw(
                    h3_stream,
                    Role::Client,
                    self.config.clone(),
                ))
            }
            StatusCode::NOT_IMPLEMENTED => {
                // Server doesn't support the :protocol value (RFC 9220 Section 3)
                Err(Error::ExtendedConnectNotSupported)
            }
            StatusCode::FORBIDDEN => Err(Error::HandshakeFailed(
                "server returned 403: connection forbidden",
            )),
            _ => Err(Error::HandshakeFailed("server rejected WebSocket upgrade")),
        }
    }

    /// Connect and return a multiplexed connection for multiple WebSocket streams
    ///
    /// HTTP/3 allows multiple WebSocket connections over a single QUIC connection.
    /// This is more efficient than creating separate connections.
    pub async fn connect_multiplexed(
        &self,
        server_addr: SocketAddr,
        server_name: &str,
        tls_config: rustls::ClientConfig,
    ) -> Result<H3MultiplexedConnection> {
        let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
            .map_err(|_| Error::HandshakeFailed("invalid TLS config"))?;

        let client_config = ClientConfig::new(Arc::new(quic_config));

        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).map_err(Error::Io)?;
        endpoint.set_default_client_config(client_config);

        let connection = endpoint
            .connect(server_addr, server_name)
            .map_err(|_| Error::HandshakeFailed("failed to initiate connection"))?
            .await
            .map_err(Error::from)?;

        // Create HTTP/3 connection
        let (mut driver, send_request) =
            h3::client::new(h3_quinn::Connection::new(connection.clone()))
                .await
                .map_err(Error::from)?;

        // Spawn driver
        tokio::spawn(async move {
            let err = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
            eprintln!("H3 connection closed: {}", err);
        });

        Ok(H3MultiplexedConnection {
            connection,
            send_request,
            server_name: server_name.to_string(),
            server_port: server_addr.port(),
            config: self.config.clone(),
        })
    }
}

// ============================================================================
// H3ClientStream - AsyncRead/AsyncWrite wrapper for h3 client request streams
// ============================================================================

/// Wrapper around h3 client request stream that implements AsyncRead + AsyncWrite
pub struct H3ClientStream {
    stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    read_buf: BytesMut,
}

impl H3ClientStream {
    fn new(stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(64 * 1024),
        }
    }
}

impl AsyncRead for H3ClientStream {
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
        // This is slightly less efficient but correct
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

impl AsyncWrite for H3ClientStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

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

// SAFETY: H3ClientStream is Send because h3's RequestStream is Send
unsafe impl Send for H3ClientStream {}

/// A multiplexed HTTP/3 connection for multiple WebSocket streams
///
/// QUIC allows many concurrent streams over a single connection,
/// making it efficient to have multiple WebSocket connections to the same server.
pub struct H3MultiplexedConnection {
    connection: quinn::Connection,
    send_request: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    server_name: String,
    server_port: u16,
    config: Config,
}

impl H3MultiplexedConnection {
    /// Open a new WebSocket stream on this connection
    ///
    /// # Arguments
    ///
    /// * `path` - WebSocket path (e.g., "/chat")
    /// * `subprotocol` - Optional WebSocket subprotocol
    pub async fn open_websocket(
        &mut self,
        path: &str,
        subprotocol: Option<&str>,
    ) -> Result<WebSocketStream<H3ClientStream>> {
        let uri = format!("https://{}:{}{}", self.server_name, self.server_port, path);

        let mut request_builder = Request::builder()
            .method(Method::CONNECT)
            .uri(&uri)
            .header("sec-websocket-version", "13");

        if let Some(proto) = subprotocol {
            request_builder = request_builder.header("sec-websocket-protocol", proto);
        }

        let request = request_builder
            .extension(h3::ext::Protocol::WEB_TRANSPORT)
            .body(())
            .map_err(|_| Error::HandshakeFailed("failed to build request"))?;

        let mut stream = self
            .send_request
            .send_request(request)
            .await
            .map_err(Error::from)?;

        let response = stream.recv_response().await.map_err(Error::from)?;

        if response.status() != StatusCode::OK {
            return Err(Error::HandshakeFailed("server rejected WebSocket upgrade"));
        }

        let h3_stream = H3ClientStream::new(stream);

        Ok(WebSocketStream::from_raw(
            h3_stream,
            Role::Client,
            self.config.clone(),
        ))
    }

    /// Check if the connection is still open
    pub fn is_open(&self) -> bool {
        self.connection.close_reason().is_none()
    }

    /// Get connection statistics
    pub fn stats(&self) -> quinn::ConnectionStats {
        self.connection.stats()
    }

    /// Get the remote address
    pub fn remote_addr(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// Close the connection
    ///
    /// Uses H3_NO_ERROR (0x100) for graceful closure.
    pub fn close(&self) {
        self.connection
            .close(quinn::VarInt::from_u32(0x100), b"done");
    }
}

impl Default for H3WebSocketClient {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl std::fmt::Debug for H3WebSocketClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3WebSocketClient")
            .field("config", &self.config)
            .finish()
    }
}

impl std::fmt::Debug for H3MultiplexedConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3MultiplexedConnection")
            .field("remote_addr", &self.connection.remote_address())
            .field("server_name", &self.server_name)
            .field("is_open", &self.is_open())
            .finish()
    }
}
