//! Unified WebSocket server implementation
//!
//! This module provides `WebSocketServer<T>`, a generic WebSocket server
//! that works with both HTTP/2 and HTTP/3 transports.
//!
//! # Example
//!
//! ```ignore
//! use sockudo_ws::{WebSocketServer, Http2, Http3, Config};
//! use futures_util::StreamExt;
//!
//! // HTTP/2 server
//! let server = WebSocketServer::<Http2>::new(Config::default());
//! server.serve(tls_stream, |mut ws, req| async move {
//!     while let Some(msg) = ws.next().await {
//!         // echo messages
//!         if let Ok(msg) = msg {
//!             ws.send(msg).await.ok();
//!         }
//!     }
//! }).await?;
//!
//! // HTTP/3 server
//! let server = WebSocketServer::<Http3>::bind(addr, tls_config, Config::default()).await?;
//! server.serve(|mut ws, req| async move {
//!     // handle connection
//! }).await?;
//! ```

use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

#[cfg(feature = "http3")]
use std::net::SocketAddr;

use crate::error::{Error, Result};
use crate::protocol::Role;
use crate::transport::{Http1, Transport};
use crate::{Config, WebSocketStream};

#[cfg(feature = "http2")]
use crate::transport::Http2;

#[cfg(feature = "http3")]
use crate::transport::Http3;

// Import Bytes for http3 only when http2 is not enabled (http2 imports it in its block)
#[cfg(all(feature = "http3", not(feature = "http2")))]
use bytes::Bytes;

#[cfg(any(feature = "http2", feature = "http3"))]
use crate::extended_connect::{
    ExtendedConnectRequest, build_extended_connect_error, build_extended_connect_response,
};

// ============================================================================
// HTTP/1.1 imports
// ============================================================================

use crate::handshake::{self, HandshakeResult};
use crate::stream::Stream;
use tokio::io::{AsyncRead, AsyncWrite};

// ============================================================================
// HTTP/2 imports
// ============================================================================

#[cfg(feature = "http2")]
use {
    bytes::Bytes,
    h2::server::{self, SendResponse},
    http::{Request, StatusCode},
};

// ============================================================================
// HTTP/3 imports
// ============================================================================

#[cfg(feature = "http3")]
use {
    h3::server::Connection as H3Connection,
    http::Method,
    quinn::{Endpoint, ServerConfig},
};

// ============================================================================
// WebSocketServer
// ============================================================================

/// WebSocket server generic over transport protocol
///
/// Use `WebSocketServer<Http2>` for HTTP/2 or `WebSocketServer<Http3>` for HTTP/3.
pub struct WebSocketServer<T: Transport> {
    config: Config,
    inner: ServerInner<T>,
}

// Server inner state - different for each transport
enum ServerInner<T: Transport> {
    Http1(Option<Arc<[String]>>),
    #[cfg(feature = "http2")]
    Http2(PhantomData<T>),
    #[cfg(feature = "http3")]
    Http3 {
        endpoint: Endpoint,
    },
    #[allow(dead_code)]
    Phantom(PhantomData<T>),
}

// ============================================================================
// HTTP/1.1 Server Implementation
// ============================================================================

impl WebSocketServer<Http1> {
    /// Create a new HTTP/1.1 WebSocket server with the given configuration
    pub fn new(config: Config) -> Self {
        Self {
            config,
            inner: ServerInner::Http1(None),
        }
    }

    /// Set supported WebSocket subprotocols in server preference order.
    ///
    /// The server selects the first exact match from this list. If no protocol
    /// matches, the handshake succeeds without a selected protocol and
    /// [`HandshakeResult::protocol`] is `None`. Browser clients that offered
    /// protocols reject a response without a selection.
    ///
    /// Passing an empty iterator disables the default selection of the
    /// client's first offer.
    ///
    /// # Errors
    ///
    /// Returns an error if any configured subprotocol is not a valid token.
    pub fn protocols<I, P>(mut self, protocols: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        let protocols = protocols
            .into_iter()
            .map(|protocol| protocol.as_ref().to_owned())
            .collect::<Vec<_>>();
        handshake::validate_supported_protocols(&protocols)?;
        self.inner = ServerInner::Http1(Some(protocols.into()));
        Ok(self)
    }

    fn supported_protocols(&self) -> Option<&[String]> {
        let ServerInner::Http1(protocols) = &self.inner else {
            unreachable!("HTTP/1 server has a non-HTTP/1 inner state");
        };
        protocols.as_deref()
    }

    /// Create a new HTTP/1.1 WebSocket server with default configuration
    pub fn default_config() -> Self {
        Self::new(Config::default())
    }

    /// Get the server configuration
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Accept a WebSocket connection on an existing stream
    ///
    /// This performs the HTTP/1.1 WebSocket upgrade handshake and returns
    /// a `WebSocketStream` on success.
    ///
    /// # Arguments
    ///
    /// * `stream` - The underlying transport (TCP or TLS stream)
    ///
    /// # Returns
    ///
    /// A tuple of the WebSocketStream and handshake result containing
    /// the request path, negotiated protocol, etc.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use sockudo_ws::{WebSocketServer, Http1, Config};
    /// use tokio::net::TcpListener;
    ///
    /// let listener = TcpListener::bind("0.0.0.0:8080").await?;
    /// let server = WebSocketServer::<Http1>::new(Config::default());
    ///
    /// while let Ok((stream, _)) = listener.accept().await {
    ///     let (ws, handshake) = server.accept(stream).await?;
    ///     // handle ws...
    /// }
    /// ```
    pub async fn accept<S>(
        &self,
        mut stream: S,
    ) -> Result<(WebSocketStream<Stream<Http1>>, HandshakeResult)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // Perform the HTTP/1.1 WebSocket handshake
        let handshake_result = handshake::server_handshake_with_supported_protocols(
            &mut stream,
            self.supported_protocols(),
        )
        .await?;

        // Wrap in Stream<Http1>
        let stream = Stream::<Http1>::new(stream);

        // Preserve frame bytes read together with the HTTP upgrade request.
        let ws = WebSocketStream::from_raw_with_leftover(
            stream,
            Role::Server,
            self.config.clone(),
            handshake_result.leftover.clone(),
        );

        Ok((ws, handshake_result))
    }

    /// Accept a WebSocket connection without type erasure
    ///
    /// This method performs the WebSocket handshake and returns a `WebSocketStream<S>`
    /// directly, without wrapping in `Stream<Http1>`. This is useful for:
    ///
    /// - **io_uring**: `UringStream` uses `Rc` internally and is not `Send`
    /// - **Single-threaded runtimes**: When `Send` is not required
    /// - **Avoiding type erasure**: When you want to keep the concrete stream type
    ///
    /// # Example with io_uring
    ///
    /// ```ignore
    /// use sockudo_ws::{WebSocketServer, Http1, Config};
    /// use sockudo_ws::io_uring::UringStream;
    ///
    /// #[tokio_uring::main]
    /// async fn main() {
    ///     let listener = tokio_uring::net::TcpListener::bind(addr)?;
    ///     let server = WebSocketServer::<Http1>::new(Config::default());
    ///
    ///     loop {
    ///         let (tcp, _) = listener.accept().await?;
    ///         let uring_stream = UringStream::new(tcp);
    ///
    ///         let (ws, handshake) = server.accept_raw(uring_stream).await?;
    ///         // ws is WebSocketStream<UringStream> - no Send required!
    ///     }
    /// }
    /// ```
    pub async fn accept_raw<S>(
        &self,
        mut stream: S,
    ) -> Result<(WebSocketStream<S>, HandshakeResult)>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // Perform the HTTP/1.1 WebSocket handshake
        let handshake_result = handshake::server_handshake_with_supported_protocols(
            &mut stream,
            self.supported_protocols(),
        )
        .await?;

        // Preserve frame bytes read together with the HTTP upgrade request.
        let ws = WebSocketStream::from_raw_with_leftover(
            stream,
            Role::Server,
            self.config.clone(),
            handshake_result.leftover.clone(),
        );

        Ok((ws, handshake_result))
    }

    /// Serve WebSocket connections from a TCP listener
    ///
    /// This is a convenience method that accepts connections from a listener
    /// and calls the handler for each successful WebSocket upgrade.
    /// Accepted TCP sockets have `TCP_NODELAY` enabled when supported.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use sockudo_ws::{WebSocketServer, Http1, Config};
    /// use tokio::net::TcpListener;
    ///
    /// let listener = TcpListener::bind("0.0.0.0:8080").await?;
    /// let server = WebSocketServer::<Http1>::new(Config::default());
    ///
    /// server.serve(listener, |mut ws, handshake| async move {
    ///     println!("Connection to {}", handshake.path);
    ///     while let Some(msg) = ws.next().await {
    ///         // echo messages
    ///     }
    /// }).await?;
    /// ```
    pub async fn serve<F, Fut>(&self, listener: tokio::net::TcpListener, handler: F) -> Result<()>
    where
        F: Fn(WebSocketStream<Stream<Http1>>, HandshakeResult) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        loop {
            let (stream, _addr) = listener.accept().await.map_err(Error::Io)?;
            let handler = handler.clone();
            let server = self.clone();

            tokio::spawn(async move {
                // A socket-option failure need not prevent a valid handshake.
                // Report it locally and keep serving this connection.
                if let Err(error) = stream.set_nodelay(true) {
                    eprintln!("WebSocket socket setup error: {error}");
                }
                match server.accept(stream).await {
                    Ok((ws, handshake)) => {
                        handler(ws, handshake).await;
                    }
                    Err(e) => {
                        eprintln!("WebSocket handshake error: {}", e);
                    }
                }
            });
        }
    }
}

impl Default for WebSocketServer<Http1> {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl Clone for WebSocketServer<Http1> {
    fn clone(&self) -> Self {
        let ServerInner::Http1(protocols) = &self.inner else {
            unreachable!("HTTP/1 server has a non-HTTP/1 inner state");
        };
        Self {
            config: self.config.clone(),
            inner: ServerInner::Http1(protocols.clone()),
        }
    }
}

// ============================================================================
// HTTP/2 Server Implementation
// ============================================================================

#[cfg(feature = "http2")]
impl WebSocketServer<Http2> {
    /// Create a new HTTP/2 WebSocket server with the given configuration
    pub fn new(config: Config) -> Self {
        Self {
            config,
            inner: ServerInner::Http2(PhantomData),
        }
    }

    /// Create a new HTTP/2 WebSocket server with default configuration
    pub fn default_config() -> Self {
        Self::new(Config::default())
    }

    /// Get the server configuration
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Serve WebSocket connections on an HTTP/2 connection
    ///
    /// This method accepts HTTP/2 streams and handles Extended CONNECT requests
    /// for WebSocket upgrades. Each WebSocket connection is handled by the
    /// provided handler function.
    ///
    /// # Arguments
    ///
    /// * `stream` - The underlying transport (typically a TLS stream)
    /// * `handler` - Async function to handle each WebSocket connection
    ///
    /// # Returns
    ///
    /// Returns when the HTTP/2 connection is closed or an error occurs.
    pub async fn serve<S, F, Fut>(&self, stream: S, handler: F) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        F: Fn(WebSocketStream<Stream<Http2>>, ExtendedConnectRequest) -> Fut
            + Clone
            + Send
            + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let config = self.config.clone();

        // Build h2 server connection with Extended CONNECT enabled
        let mut h2_builder = server::Builder::new();

        h2_builder
            .initial_window_size(config.http2.initial_stream_window_size)
            .initial_connection_window_size(config.http2.initial_connection_window_size)
            .max_concurrent_streams(config.http2.max_concurrent_streams);

        // Enable Extended CONNECT protocol (RFC 8441)
        h2_builder.enable_connect_protocol();

        let mut h2_conn = h2_builder.handshake(stream).await.map_err(Error::from)?;

        // Accept and handle streams
        while let Some(result) = h2_conn.accept().await {
            let (request, respond) = result.map_err(Error::from)?;

            let handler = handler.clone();
            let config = config.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_http2_request(request, respond, handler, config).await {
                    eprintln!("HTTP/2 WebSocket error: {}", e);
                }
            });
        }

        Ok(())
    }

    /// Serve WebSocket connections with a filter for which requests to accept
    ///
    /// This is like `serve` but allows you to filter or validate requests
    /// before accepting them as WebSocket connections.
    pub async fn serve_with_filter<S, F, Fut, Filter>(
        &self,
        stream: S,
        filter: Filter,
        handler: F,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        F: Fn(WebSocketStream<Stream<Http2>>, ExtendedConnectRequest) -> Fut
            + Clone
            + Send
            + 'static,
        Fut: Future<Output = ()> + Send + 'static,
        Filter: Fn(&ExtendedConnectRequest) -> bool + Clone + Send + 'static,
    {
        let config = self.config.clone();

        let mut h2_builder = server::Builder::new();

        h2_builder
            .initial_window_size(config.http2.initial_stream_window_size)
            .initial_connection_window_size(config.http2.initial_connection_window_size)
            .max_concurrent_streams(config.http2.max_concurrent_streams);

        h2_builder.enable_connect_protocol();

        let mut h2_conn = h2_builder.handshake(stream).await.map_err(Error::from)?;

        while let Some(result) = h2_conn.accept().await {
            let (request, respond) = result.map_err(Error::from)?;

            let handler = handler.clone();
            let filter = filter.clone();
            let config = config.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    handle_http2_request_filtered(request, respond, filter, handler, config).await
                {
                    eprintln!("HTTP/2 WebSocket error: {}", e);
                }
            });
        }

        Ok(())
    }
}

#[cfg(feature = "http2")]
impl Default for WebSocketServer<Http2> {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

#[cfg(feature = "http2")]
impl Clone for WebSocketServer<Http2> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            inner: ServerInner::Http2(PhantomData),
        }
    }
}

// ============================================================================
// HTTP/2 Request Handlers
// ============================================================================

#[cfg(feature = "http2")]
async fn handle_http2_request<F, Fut>(
    request: Request<h2::RecvStream>,
    mut respond: SendResponse<Bytes>,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<Stream<Http2>>, ExtendedConnectRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    // Check if this is an Extended CONNECT for WebSocket
    if let Some(mut ws_req) = ExtendedConnectRequest::from_request(&request) {
        // Set protocol to websocket for Extended CONNECT
        if ws_req.protocol.is_none() {
            ws_req.protocol = Some("websocket".to_string());
        }

        // Accept the WebSocket upgrade
        let response = build_extended_connect_response(ws_req.subprotocols.as_deref(), None);

        let send_stream = respond
            .send_response(response, false)
            .map_err(Error::from)?;

        let recv_stream = request.into_body();

        // Create Stream<Http2> wrapper
        let h2_stream = Stream::<Http2>::from_h2(send_stream, recv_stream);

        // Create WebSocketStream over Stream<Http2>
        let ws = WebSocketStream::from_raw(h2_stream, Role::Server, config)
            .with_immediate_write_shutdown();

        // Call user handler
        handler(ws, ws_req).await;

        Ok(())
    } else {
        // Not a WebSocket request - send error response
        let response = build_extended_connect_error(
            StatusCode::BAD_REQUEST,
            Some("Expected Extended CONNECT for WebSocket"),
        );
        respond.send_response(response, true).map_err(Error::from)?;
        Ok(())
    }
}

#[cfg(feature = "http2")]
async fn handle_http2_request_filtered<F, Fut, Filter>(
    request: Request<h2::RecvStream>,
    mut respond: SendResponse<Bytes>,
    filter: Filter,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<Stream<Http2>>, ExtendedConnectRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    Filter: Fn(&ExtendedConnectRequest) -> bool + Send + 'static,
{
    if let Some(mut ws_req) = ExtendedConnectRequest::from_request(&request) {
        if ws_req.protocol.is_none() {
            ws_req.protocol = Some("websocket".to_string());
        }

        // Apply filter
        if !filter(&ws_req) {
            let response = build_extended_connect_error(
                StatusCode::FORBIDDEN,
                Some("WebSocket connection rejected by filter"),
            );
            respond.send_response(response, true).map_err(Error::from)?;
            return Ok(());
        }

        let response = build_extended_connect_response(ws_req.subprotocols.as_deref(), None);

        let send_stream = respond
            .send_response(response, false)
            .map_err(Error::from)?;

        let recv_stream = request.into_body();
        let h2_stream = Stream::<Http2>::from_h2(send_stream, recv_stream);
        let ws = WebSocketStream::from_raw(h2_stream, Role::Server, config)
            .with_immediate_write_shutdown();

        handler(ws, ws_req).await;

        Ok(())
    } else {
        let response = build_extended_connect_error(
            StatusCode::BAD_REQUEST,
            Some("Expected Extended CONNECT for WebSocket"),
        );
        respond.send_response(response, true).map_err(Error::from)?;
        Ok(())
    }
}

// ============================================================================
// HTTP/3 Server Implementation
// ============================================================================

#[cfg(feature = "http3")]
impl WebSocketServer<Http3> {
    /// Bind to an address and create a new HTTP/3 WebSocket server
    ///
    /// # Arguments
    ///
    /// * `addr` - Socket address to bind to (e.g., "0.0.0.0:443")
    /// * `tls_config` - TLS configuration (required for QUIC)
    /// * `ws_config` - WebSocket configuration
    pub async fn bind(
        addr: SocketAddr,
        mut tls_config: rustls::ServerConfig,
        ws_config: Config,
    ) -> Result<Self> {
        let transport_config = crate::http3::quic_transport_config(&ws_config.http3)?;
        let endpoint_config = crate::http3::quic_endpoint_config(&ws_config.http3)?;

        // HTTP/3 0-RTT is rejected by quic_transport_config. Keep caller-provided
        // TLS settings from silently enabling early data through this API.
        tls_config.max_early_data_size = 0;

        // Create QUIC server config from rustls config
        let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)
            .map_err(|_| Error::HandshakeFailed("invalid TLS config"))?;

        let mut server_config = ServerConfig::with_crypto(Arc::new(quic_config));
        server_config.transport_config(transport_config);

        // Create QUIC endpoint
        let socket = std::net::UdpSocket::bind(addr).map_err(Error::Io)?;
        let endpoint = Endpoint::new(
            endpoint_config,
            Some(server_config),
            socket,
            Arc::new(quinn::TokioRuntime),
        )
        .map_err(Error::Io)?;

        Ok(Self {
            config: ws_config,
            inner: ServerInner::Http3 { endpoint },
        })
    }

    /// Create from an existing QUIC endpoint
    ///
    /// Use this when you need more control over the QUIC configuration,
    /// such as enabling BBR congestion control for better performance
    /// in lossy networks.
    ///
    /// Transport and TLS settings come from the supplied endpoint. HTTP/3
    /// protocol settings still come from the WebSocket configuration.
    pub fn from_endpoint(endpoint: Endpoint, ws_config: Config) -> Self {
        Self {
            config: ws_config,
            inner: ServerInner::Http3 { endpoint },
        }
    }

    /// Get the local address the server is bound to
    pub fn local_addr(&self) -> Result<SocketAddr> {
        match &self.inner {
            ServerInner::Http3 { endpoint } => endpoint.local_addr().map_err(Error::Io),
            _ => unreachable!(),
        }
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
        F: Fn(WebSocketStream<Stream<Http3>>, ExtendedConnectRequest) -> Fut
            + Clone
            + Send
            + Sync
            + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        crate::http3::validate_config(&self.config.http3)?;
        let endpoint = match self.inner {
            ServerInner::Http3 { endpoint } => endpoint,
            _ => unreachable!(),
        };

        while let Some(incoming) = endpoint.accept().await {
            let handler = handler.clone();
            let config = self.config.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_http3_connection(incoming, handler, config).await {
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
        F: Fn(WebSocketStream<Stream<Http3>>, ExtendedConnectRequest) -> Fut
            + Clone
            + Send
            + Sync
            + 'static,
        Fut: Future<Output = ()> + Send + 'static,
        Filter: Fn(&ExtendedConnectRequest) -> bool + Clone + Send + Sync + 'static,
    {
        crate::http3::validate_config(&self.config.http3)?;
        let endpoint = match self.inner {
            ServerInner::Http3 { endpoint } => endpoint,
            _ => unreachable!(),
        };

        while let Some(incoming) = endpoint.accept().await {
            let handler = handler.clone();
            let filter = filter.clone();
            let config = self.config.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    handle_http3_connection_filtered(incoming, filter, handler, config).await
                {
                    eprintln!("HTTP/3 connection error: {}", e);
                }
            });
        }

        Ok(())
    }

    /// Close the server endpoint
    pub fn close(&self, error_code: quinn::VarInt, reason: &[u8]) {
        match &self.inner {
            ServerInner::Http3 { endpoint } => endpoint.close(error_code, reason),
            _ => unreachable!(),
        }
    }
}

// ============================================================================
// HTTP/3 Connection Handlers
// ============================================================================

#[cfg(feature = "http3")]
async fn handle_http3_connection<F, Fut>(
    incoming: quinn::Incoming,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<Stream<Http3>>, ExtendedConnectRequest) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let connection = incoming.await.map_err(Error::from)?;

    let mut builder = h3::server::builder();
    builder.enable_extended_connect(config.http3.enable_connect_protocol);
    let mut h3_conn: H3Connection<h3_quinn::Connection, Bytes> = builder
        .build(h3_quinn::Connection::new(connection))
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
                let config = config.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_http3_request(request, stream, handler, config).await {
                        eprintln!("HTTP/3 request error: {}", e);
                    }
                });
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    Ok(())
}

#[cfg(feature = "http3")]
async fn handle_http3_connection_filtered<F, Fut, Filter>(
    incoming: quinn::Incoming,
    filter: Filter,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<Stream<Http3>>, ExtendedConnectRequest) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    Filter: Fn(&ExtendedConnectRequest) -> bool + Clone + Send + 'static,
{
    let connection = incoming.await.map_err(Error::from)?;

    let mut builder = h3::server::builder();
    builder.enable_extended_connect(config.http3.enable_connect_protocol);
    let mut h3_conn: H3Connection<h3_quinn::Connection, Bytes> = builder
        .build(h3_quinn::Connection::new(connection))
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
                        handle_http3_request_filtered(request, stream, filter, handler, config)
                            .await
                    {
                        eprintln!("HTTP/3 request error: {}", e);
                    }
                });
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    Ok(())
}

#[cfg(feature = "http3")]
async fn handle_http3_request<F, Fut>(
    request: http::Request<()>,
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<Stream<Http3>>, ExtendedConnectRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    use http::StatusCode;

    if !config.http3.enable_connect_protocol {
        let response = build_extended_connect_error(
            StatusCode::NOT_IMPLEMENTED,
            Some("Extended CONNECT is disabled"),
        );
        stream.send_response(response).await.ok();
        return Ok(());
    }

    if request.method() != Method::CONNECT {
        let response =
            build_extended_connect_error(StatusCode::METHOD_NOT_ALLOWED, Some("Expected CONNECT"));
        stream.send_response(response).await.ok();
        return Ok(());
    }

    let protocol_header =
        request
            .extensions()
            .get::<h3::ext::Protocol>()
            .map(|p| match p.as_str() {
                "webtransport" => "websocket".to_string(),
                other => other.to_string(),
            });

    let mut ws_req = ExtendedConnectRequest::from_request(&request)
        .ok_or_else(|| Error::HandshakeFailed("invalid CONNECT request"))?;

    if ws_req.protocol.is_none() {
        ws_req.protocol = protocol_header;
    }

    if let Err(status) = ws_req.validate() {
        let reason = match status {
            StatusCode::NOT_IMPLEMENTED => Some("Unsupported :protocol value"),
            StatusCode::BAD_REQUEST => Some("Invalid WebSocket request"),
            _ => None,
        };
        let response = build_extended_connect_error(status, reason);
        stream.send_response(response).await.ok();
        return Ok(());
    }

    let response = build_extended_connect_response(None, None);
    stream.send_response(response).await.map_err(Error::from)?;

    let h3_stream = Stream::<Http3>::from_h3_server(stream);
    let ws =
        WebSocketStream::from_raw(h3_stream, Role::Server, config).with_immediate_write_shutdown();

    handler(ws, ws_req).await;

    Ok(())
}

#[cfg(feature = "http3")]
async fn handle_http3_request_filtered<F, Fut, Filter>(
    request: http::Request<()>,
    mut stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    filter: Filter,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<Stream<Http3>>, ExtendedConnectRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    Filter: Fn(&ExtendedConnectRequest) -> bool + Send + 'static,
{
    use http::StatusCode;

    if !config.http3.enable_connect_protocol {
        let response = build_extended_connect_error(
            StatusCode::NOT_IMPLEMENTED,
            Some("Extended CONNECT is disabled"),
        );
        stream.send_response(response).await.ok();
        return Ok(());
    }

    if request.method() != Method::CONNECT {
        let response =
            build_extended_connect_error(StatusCode::METHOD_NOT_ALLOWED, Some("Expected CONNECT"));
        stream.send_response(response).await.ok();
        return Ok(());
    }

    let protocol_header =
        request
            .extensions()
            .get::<h3::ext::Protocol>()
            .map(|p| match p.as_str() {
                "webtransport" => "websocket".to_string(),
                other => other.to_string(),
            });

    let mut ws_req = ExtendedConnectRequest::from_request(&request)
        .ok_or_else(|| Error::HandshakeFailed("invalid CONNECT request"))?;

    if ws_req.protocol.is_none() {
        ws_req.protocol = protocol_header;
    }

    if !filter(&ws_req) {
        let response =
            build_extended_connect_error(StatusCode::FORBIDDEN, Some("Rejected by filter"));
        stream.send_response(response).await.ok();
        return Ok(());
    }

    if let Err(status) = ws_req.validate() {
        let response = build_extended_connect_error(status, None);
        stream.send_response(response).await.ok();
        return Ok(());
    }

    let response = build_extended_connect_response(None, None);
    stream.send_response(response).await.ok();

    let h3_stream = Stream::<Http3>::from_h3_server(stream);
    let ws =
        WebSocketStream::from_raw(h3_stream, Role::Server, config).with_immediate_write_shutdown();

    handler(ws, ws_req).await;

    Ok(())
}

// ============================================================================
// Debug implementations
// ============================================================================

impl<T: Transport> std::fmt::Debug for WebSocketServer<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketServer")
            .field("config", &self.config)
            .finish()
    }
}
