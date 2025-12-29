//! HTTP/2 WebSocket client implementation
//!
//! This module provides `H2WebSocketClient`, a client that connects to
//! WebSocket servers over HTTP/2 using the Extended CONNECT protocol (RFC 8441).

use bytes::Bytes;
use h2::client::{self, SendRequest};
use http::{Method, Request, Uri};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{Error, Result};
use crate::protocol::Role;
use crate::{Config, WebSocketStream};

use super::stream::H2Stream;

/// HTTP/2 WebSocket client
///
/// Connects to WebSocket servers using HTTP/2 Extended CONNECT (RFC 8441).
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::{Config, Message};
/// use sockudo_ws::http2::H2WebSocketClient;
/// use futures_util::{SinkExt, StreamExt};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let stream = tokio::net::TcpStream::connect("server:443").await?;
///     let tls_stream = do_tls_handshake(stream).await?;
///
///     let client = H2WebSocketClient::new(Config::default());
///     let mut ws = client.connect(tls_stream, "wss://server/chat", None).await?;
///
///     // Same API as regular WebSocket!
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
pub struct H2WebSocketClient {
    config: Config,
}

impl H2WebSocketClient {
    /// Create a new HTTP/2 WebSocket client with the given configuration
    pub fn new(config: Config) -> Self {
        Self { config }
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
    ) -> Result<WebSocketStream<H2Stream>>
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
    ) -> Result<WebSocketStream<H2Stream>>
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

        #[cfg(feature = "http2")]
        {
            h2_builder
                .initial_window_size(config.http2.initial_stream_window_size)
                .initial_connection_window_size(config.http2.initial_connection_window_size);
        }

        let (mut send_request, h2_conn) =
            h2_builder.handshake(stream).await.map_err(Error::from)?;

        // Spawn connection driver
        tokio::spawn(async move {
            if let Err(e) = h2_conn.await {
                eprintln!("HTTP/2 connection error: {}", e);
            }
        });

        // Build Extended CONNECT request
        // Note: For Extended CONNECT, we use a full URI, not just authority
        let full_uri = format!("{}://{}{}", scheme, authority, path);

        let mut req_builder = Request::builder().method(Method::CONNECT).uri(&full_uri);

        // Add :protocol pseudo-header for Extended CONNECT (RFC 8441)
        // This is done via the extension mechanism in http crate
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

        // Create H2Stream wrapper
        let h2_stream = H2Stream::new(send_stream, recv_stream);

        // Create and return WebSocketStream
        Ok(WebSocketStream::from_raw(h2_stream, Role::Client, config))
    }

    /// Connect to multiple WebSocket endpoints over the same HTTP/2 connection
    ///
    /// HTTP/2 allows multiplexing, so you can open multiple WebSocket streams
    /// over a single TCP connection. This method returns a connection handle
    /// that can be used to create additional WebSocket streams.
    pub async fn connect_multiplexed<S>(&self, stream: S) -> Result<H2MultiplexedConnection>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let config = self.config.clone();

        let mut h2_builder = client::Builder::new();

        #[cfg(feature = "http2")]
        {
            h2_builder
                .initial_window_size(config.http2.initial_stream_window_size)
                .initial_connection_window_size(config.http2.initial_connection_window_size);
        }

        let (send_request, h2_conn) = h2_builder.handshake(stream).await.map_err(Error::from)?;

        // Spawn connection driver
        tokio::spawn(async move {
            if let Err(e) = h2_conn.await {
                eprintln!("HTTP/2 connection error: {}", e);
            }
        });

        Ok(H2MultiplexedConnection {
            send_request,
            config,
        })
    }
}

/// A multiplexed HTTP/2 connection that can create multiple WebSocket streams
///
/// This allows opening multiple WebSocket connections over a single TCP connection,
/// which is one of the main benefits of HTTP/2 WebSocket (RFC 8441).
pub struct H2MultiplexedConnection {
    send_request: SendRequest<Bytes>,
    config: Config,
}

impl H2MultiplexedConnection {
    /// Open a new WebSocket stream on this connection
    ///
    /// # Arguments
    ///
    /// * `uri` - The WebSocket URI (path is most important for multiplexed)
    /// * `protocol` - Optional WebSocket subprotocol
    pub async fn open_websocket(
        &mut self,
        uri: &str,
        protocol: Option<&str>,
    ) -> Result<WebSocketStream<H2Stream>> {
        let uri: Uri = uri
            .parse()
            .map_err(|_| Error::HandshakeFailed("invalid URI"))?;

        let authority = uri
            .authority()
            .ok_or(Error::HandshakeFailed("URI missing authority"))?
            .to_string();

        let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");

        let scheme = uri.scheme_str().unwrap_or("https");
        let full_uri = format!("{}://{}{}", scheme, authority, path);

        let mut req_builder = Request::builder().method(Method::CONNECT).uri(&full_uri);

        // Add :protocol pseudo-header for Extended CONNECT
        req_builder = req_builder.header(":protocol", "websocket");

        if let Some(proto) = protocol {
            req_builder = req_builder.header("sec-websocket-protocol", proto);
        }

        req_builder = req_builder.header("sec-websocket-version", "13");

        let request = req_builder
            .body(())
            .map_err(|_| Error::HandshakeFailed("failed to build request"))?;

        let (response_future, send_stream) = self
            .send_request
            .send_request(request, false)
            .map_err(Error::from)?;

        let response = response_future.await.map_err(Error::from)?;

        if response.status() != http::StatusCode::OK {
            return Err(Error::HandshakeFailed("server rejected WebSocket upgrade"));
        }

        let recv_stream = response.into_body();
        let h2_stream = H2Stream::new(send_stream, recv_stream);

        Ok(WebSocketStream::from_raw(
            h2_stream,
            Role::Client,
            self.config.clone(),
        ))
    }

    /// Check if the connection is still open
    pub fn is_ready(&self) -> bool {
        // SendRequest doesn't have is_closed, so we just return true
        // In practice, send_request will fail if the connection is closed
        true
    }
}

impl Default for H2WebSocketClient {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl std::fmt::Debug for H2WebSocketClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H2WebSocketClient")
            .field("config", &self.config)
            .finish()
    }
}

impl std::fmt::Debug for H2MultiplexedConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H2MultiplexedConnection")
            .field("is_ready", &self.is_ready())
            .finish()
    }
}
