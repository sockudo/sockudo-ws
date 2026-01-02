//! Unified WebSocket client implementation
//!
//! This module provides `WebSocketClient<T>`, a generic WebSocket client
//! that works with both HTTP/2 and HTTP/3 transports.
//!
//! # Example
//!
//! ```ignore
//! use sockudo_ws::{WebSocketClient, Http2, Http3, Config, Message};
//! use futures_util::{SinkExt, StreamExt};
//!
//! // HTTP/2 client
//! let client = WebSocketClient::<Http2>::new(Config::default());
//! let mut ws = client.connect(tls_stream, "wss://example.com/ws", None).await?;
//! ws.send(Message::Text("Hello!".into())).await?;
//!
//! // HTTP/3 client
//! let client = WebSocketClient::<Http3>::new(Config::default());
//! let mut ws = client.connect(addr, "example.com", "/ws", tls_config).await?;
//! ws.send(Message::Text("Hello!".into())).await?;
//! ```

use std::marker::PhantomData;

#[cfg(feature = "http3")]
use std::net::SocketAddr;

#[cfg(feature = "http3")]
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::handshake::{self, HandshakeResult};
use crate::protocol::Role;
use crate::stream::Stream;
use crate::transport::{Http1, Transport};
use crate::{Config, WebSocketStream};

#[cfg(feature = "http2")]
use crate::transport::Http2;

#[cfg(feature = "http3")]
use crate::transport::Http3;

#[cfg(any(feature = "http2", feature = "http3"))]
use crate::multiplex::MultiplexedConnection;

use tokio::io::{AsyncRead, AsyncWrite};

// ============================================================================
// HTTP/2 imports
// ============================================================================

#[cfg(feature = "http2")]
use {
    h2::client,
    http::{Method, Request, Uri},
};

// ============================================================================
// HTTP/3 imports
// ============================================================================

#[cfg(feature = "http3")]
use {
    http::StatusCode,
    quinn::{ClientConfig, Endpoint},
};

// ============================================================================
// WebSocketClient
// ============================================================================

/// WebSocket client generic over transport protocol
///
/// Use `WebSocketClient<Http1>` for HTTP/1.1, `WebSocketClient<Http2>` for HTTP/2,
/// or `WebSocketClient<Http3>` for HTTP/3.
pub struct WebSocketClient<T: Transport> {
    config: Config,
    _marker: PhantomData<T>,
}

// ============================================================================
// HTTP/1.1 Client Implementation
// ============================================================================

impl WebSocketClient<Http1> {
    /// Create a new HTTP/1.1 WebSocket client with the given configuration
    pub fn new(config: Config) -> Self {
        Self {
            config,
            _marker: PhantomData,
        }
    }

    /// Create a new HTTP/1.1 WebSocket client with default configuration
    pub fn default_config() -> Self {
        Self::new(Config::default())
    }

    /// Get the client configuration
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Connect to a WebSocket server over an existing stream
    ///
    /// This performs the HTTP/1.1 WebSocket upgrade handshake on the provided
    /// stream and returns a `WebSocketStream` on success.
    ///
    /// # Arguments
    ///
    /// * `stream` - The underlying transport (TCP or TLS stream)
    /// * `host` - The Host header value
    /// * `path` - The request path (e.g., "/ws")
    /// * `protocol` - Optional WebSocket subprotocol to request
    ///
    /// # Returns
    ///
    /// A tuple of the WebSocketStream and handshake result.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use sockudo_ws::{WebSocketClient, Http1, Config};
    /// use tokio::net::TcpStream;
    ///
    /// let stream = TcpStream::connect("example.com:80").await?;
    /// let client = WebSocketClient::<Http1>::new(Config::default());
    /// let (ws, handshake) = client.connect(stream, "example.com", "/ws", None).await?;
    /// ```
    pub async fn connect<S>(
        &self,
        mut stream: S,
        host: &str,
        path: &str,
        protocol: Option<&str>,
    ) -> Result<(WebSocketStream<Stream<Http1>>, HandshakeResult)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // Perform the HTTP/1.1 WebSocket handshake
        let handshake_result =
            handshake::client_handshake(&mut stream, host, path, protocol).await?;

        // Wrap in Stream<Http1>
        let stream = Stream::<Http1>::new(stream);

        // Create WebSocketStream
        let ws = WebSocketStream::from_raw(stream, Role::Client, self.config.clone());

        Ok((ws, handshake_result))
    }

    /// Connect to a WebSocket server using a URI
    ///
    /// This is a convenience method that establishes a TCP connection
    /// and performs the WebSocket handshake.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use sockudo_ws::{WebSocketClient, Http1, Config};
    ///
    /// let client = WebSocketClient::<Http1>::new(Config::default());
    /// let (ws, handshake) = client.connect_to_url("ws://example.com/ws", None).await?;
    /// ```
    pub async fn connect_to_url(
        &self,
        url: &str,
        protocol: Option<&str>,
    ) -> Result<(WebSocketStream<Stream<Http1>>, HandshakeResult)> {
        use tokio::net::TcpStream;

        // Simple URL parsing for ws:// and wss:// URLs
        let (scheme, rest) = url
            .split_once("://")
            .ok_or(Error::HandshakeFailed("invalid URL: missing scheme"))?;

        let default_port = match scheme {
            "ws" => 80u16,
            "wss" => 443u16,
            _ => {
                return Err(Error::HandshakeFailed(
                    "invalid URL scheme: expected ws or wss",
                ));
            }
        };

        // Split host:port from path
        let (host_port, path) = rest
            .find('/')
            .map(|i| (&rest[..i], &rest[i..]))
            .unwrap_or((rest, "/"));

        // Parse host and port
        let (host, port) = if let Some(colon_idx) = host_port.rfind(':') {
            let port_str = &host_port[colon_idx + 1..];
            let port: u16 = port_str
                .parse()
                .map_err(|_| Error::HandshakeFailed("invalid port"))?;
            (&host_port[..colon_idx], port)
        } else {
            (host_port, default_port)
        };

        if host.is_empty() {
            return Err(Error::HandshakeFailed("URL missing host"));
        }

        // Connect to the server
        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr).await.map_err(Error::Io)?;

        // Perform handshake
        self.connect(stream, host, path, protocol).await
    }
}

impl Default for WebSocketClient<Http1> {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl Clone for WebSocketClient<Http1> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            _marker: PhantomData,
        }
    }
}

// ============================================================================
// HTTP/2 Client Implementation
// ============================================================================

#[cfg(feature = "http2")]
impl WebSocketClient<Http2> {
    /// Create a new HTTP/2 WebSocket client with the given configuration
    pub fn new(config: Config) -> Self {
        Self {
            config,
            _marker: PhantomData,
        }
    }

    /// Create a new HTTP/2 WebSocket client with default configuration
    pub fn default_config() -> Self {
        Self::new(Config::default())
    }

    /// Get the client configuration
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Connect to a WebSocket server over HTTP/2
    ///
    /// This establishes an HTTP/2 connection, sends an Extended CONNECT request
    /// for WebSocket, and returns a `WebSocketStream` on success.
    ///
    /// # Arguments
    ///
    /// * `stream` - The underlying transport (typically a TLS stream)
    /// * `uri` - The WebSocket URI (e.g., "wss://example.com/ws")
    /// * `protocol` - Optional WebSocket subprotocol to request
    ///
    /// # Returns
    ///
    /// A `WebSocketStream` that can be used for sending and receiving messages.
    pub async fn connect<S>(
        &self,
        stream: S,
        uri: &str,
        protocol: Option<&str>,
    ) -> Result<WebSocketStream<Stream<Http2>>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.connect_with_headers(stream, uri, protocol, None).await
    }

    /// Connect to a WebSocket server with custom headers
    ///
    /// Like `connect`, but allows specifying additional HTTP headers.
    ///
    /// # Arguments
    ///
    /// * `stream` - The underlying transport
    /// * `uri` - The WebSocket URI
    /// * `protocol` - Optional WebSocket subprotocol
    /// * `extra_headers` - Optional additional headers to include
    pub async fn connect_with_headers<S>(
        &self,
        stream: S,
        uri: &str,
        protocol: Option<&str>,
        extra_headers: Option<&[(String, String)]>,
    ) -> Result<WebSocketStream<Stream<Http2>>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = self.config.clone();

        // Parse URI
        let uri: Uri = uri
            .parse()
            .map_err(|_| Error::HandshakeFailed("invalid URI"))?;

        let authority = uri
            .authority()
            .ok_or(Error::HandshakeFailed("URI missing authority"))?
            .to_string();

        let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");

        let scheme = uri.scheme_str().unwrap_or("https");

        // Build h2 client connection
        let mut h2_builder = client::Builder::new();

        h2_builder
            .initial_window_size(config.http2.initial_stream_window_size)
            .initial_connection_window_size(config.http2.initial_connection_window_size);

        let (mut send_request, h2_conn) =
            h2_builder.handshake(stream).await.map_err(Error::from)?;

        // Spawn connection driver
        tokio::spawn(async move {
            if let Err(e) = h2_conn.await {
                eprintln!("HTTP/2 connection error: {}", e);
            }
        });

        // Build Extended CONNECT request
        let full_uri = format!("{}://{}{}", scheme, authority, path);

        let mut req_builder = Request::builder().method(Method::CONNECT).uri(&full_uri);

        // Add :protocol pseudo-header for Extended CONNECT (RFC 8441)
        req_builder = req_builder.header(":protocol", "websocket");

        // Add WebSocket-specific headers
        if let Some(proto) = protocol {
            req_builder = req_builder.header("sec-websocket-protocol", proto);
        }

        req_builder = req_builder.header("sec-websocket-version", "13");

        // Add extra headers
        if let Some(headers) = extra_headers {
            for (name, value) in headers {
                req_builder = req_builder.header(name.as_str(), value.as_str());
            }
        }

        let request = req_builder
            .body(())
            .map_err(|_| Error::HandshakeFailed("failed to build request"))?;

        // Send the Extended CONNECT request
        let (response_future, send_stream) = send_request
            .send_request(request, false)
            .map_err(Error::from)?;

        // Wait for response
        let response = response_future.await.map_err(Error::from)?;

        // Check response status
        if response.status() != http::StatusCode::OK {
            return Err(Error::HandshakeFailed("server rejected WebSocket upgrade"));
        }

        // Get the receive stream from the response
        let recv_stream = response.into_body();

        // Create Stream<Http2> wrapper
        let h2_stream = Stream::<Http2>::from_h2(send_stream, recv_stream);

        // Create and return WebSocketStream
        Ok(WebSocketStream::from_raw(h2_stream, Role::Client, config))
    }

    /// Connect to multiple WebSocket endpoints over the same HTTP/2 connection
    ///
    /// HTTP/2 allows multiplexing, so you can open multiple WebSocket streams
    /// over a single TCP connection. This method returns a connection handle
    /// that can be used to create additional WebSocket streams.
    pub async fn connect_multiplexed<S>(&self, stream: S) -> Result<MultiplexedConnection<Http2>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = self.config.clone();

        let mut h2_builder = client::Builder::new();

        h2_builder
            .initial_window_size(config.http2.initial_stream_window_size)
            .initial_connection_window_size(config.http2.initial_connection_window_size);

        let (send_request, h2_conn) = h2_builder.handshake(stream).await.map_err(Error::from)?;

        // Spawn connection driver
        tokio::spawn(async move {
            if let Err(e) = h2_conn.await {
                eprintln!("HTTP/2 connection error: {}", e);
            }
        });

        Ok(MultiplexedConnection::new_http2(send_request, config))
    }
}

#[cfg(feature = "http2")]
impl Default for WebSocketClient<Http2> {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

#[cfg(feature = "http2")]
impl Clone for WebSocketClient<Http2> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            _marker: PhantomData,
        }
    }
}

// ============================================================================
// HTTP/3 Client Implementation
// ============================================================================

#[cfg(feature = "http3")]
impl WebSocketClient<Http3> {
    /// Create a new HTTP/3 WebSocket client
    pub fn new(config: Config) -> Self {
        Self {
            config,
            _marker: PhantomData,
        }
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
    pub async fn connect(
        &self,
        server_addr: SocketAddr,
        server_name: &str,
        path: &str,
        tls_config: rustls::ClientConfig,
    ) -> Result<WebSocketStream<Stream<Http3>>> {
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
    ) -> Result<WebSocketStream<Stream<Http3>>> {
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
    ) -> Result<WebSocketStream<Stream<Http3>>> {
        use http::{Method, Request};

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
            .extension(h3::ext::Protocol::WEB_TRANSPORT)
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
                let h3_stream = Stream::<Http3>::from_h3_client(stream);

                Ok(WebSocketStream::from_raw(
                    h3_stream,
                    Role::Client,
                    self.config.clone(),
                ))
            }
            StatusCode::NOT_IMPLEMENTED => Err(Error::ExtendedConnectNotSupported),
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
    ) -> Result<MultiplexedConnection<Http3>> {
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

        Ok(MultiplexedConnection::new_http3(
            connection,
            send_request,
            server_name.to_string(),
            server_addr.port(),
            self.config.clone(),
        ))
    }
}

#[cfg(feature = "http3")]
impl Default for WebSocketClient<Http3> {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

#[cfg(feature = "http3")]
impl Clone for WebSocketClient<Http3> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            _marker: PhantomData,
        }
    }
}

// ============================================================================
// Debug implementations
// ============================================================================

impl<T: Transport> std::fmt::Debug for WebSocketClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketClient")
            .field("config", &self.config)
            .finish()
    }
}
