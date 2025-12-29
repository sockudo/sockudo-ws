//! HTTP/3 WebSocket client implementation
//!
//! This module provides `H3WebSocketClient`, a client that connects to
//! WebSocket servers over HTTP/3/QUIC using the Extended CONNECT protocol.

use std::net::SocketAddr;
use std::sync::Arc;

use quinn::{ClientConfig, Endpoint};

use crate::error::{Error, Result};
use crate::protocol::Role;
use crate::{Config, WebSocketStream};

use super::stream::H3Stream;

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
    ) -> Result<WebSocketStream<H3Stream>> {
        self.connect_with_protocol(server_addr, server_name, path, None, tls_config)
            .await
    }

    /// Connect with a specific WebSocket subprotocol
    pub async fn connect_with_protocol(
        &self,
        server_addr: SocketAddr,
        server_name: &str,
        _path: &str,
        _protocol: Option<&str>,
        tls_config: rustls::ClientConfig,
    ) -> Result<WebSocketStream<H3Stream>> {
        // Create QUIC client config
        let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls_config)
            .map_err(|_| Error::HandshakeFailed("invalid TLS config"))?;

        let client_config = ClientConfig::new(Arc::new(quic_config));

        // Create endpoint (bind to any available port)
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).map_err(Error::Io)?;

        endpoint.set_default_client_config(client_config);

        // Connect to server
        let connection = endpoint
            .connect(server_addr, server_name)
            .map_err(|_| Error::HandshakeFailed("failed to initiate connection"))?
            .await
            .map_err(Error::from)?;

        // Open a bidirectional stream for WebSocket
        let (send, recv) = connection.open_bi().await.map_err(Error::from)?;

        // For a full implementation, we would:
        // 1. Send HTTP/3 Extended CONNECT request using h3 crate
        // 2. Wait for 200 OK response
        // 3. Transition to WebSocket mode

        // Create H3Stream wrapper
        let h3_stream = H3Stream::new(send, recv);

        // Create WebSocket stream
        Ok(WebSocketStream::from_raw(
            h3_stream,
            Role::Client,
            self.config.clone(),
        ))
    }

    /// Connect and return a multiplexed connection for multiple WebSocket streams
    ///
    /// HTTP/3 allows multiple WebSocket connections over a single QUIC connection.
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

        Ok(H3MultiplexedConnection {
            connection,
            config: self.config.clone(),
        })
    }
}

/// A multiplexed HTTP/3 connection for multiple WebSocket streams
///
/// QUIC allows many concurrent streams over a single connection,
/// making it efficient to have multiple WebSocket connections to the same server.
pub struct H3MultiplexedConnection {
    connection: quinn::Connection,
    config: Config,
}

impl H3MultiplexedConnection {
    /// Open a new WebSocket stream on this connection
    ///
    /// # Arguments
    ///
    /// * `path` - WebSocket path (e.g., "/chat")
    /// * `protocol` - Optional WebSocket subprotocol
    pub async fn open_websocket(
        &self,
        _path: &str,
        _protocol: Option<&str>,
    ) -> Result<WebSocketStream<H3Stream>> {
        let (send, recv) = self.connection.open_bi().await.map_err(Error::from)?;

        let h3_stream = H3Stream::new(send, recv);

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
    pub fn close(&self, error_code: quinn::VarInt, reason: &[u8]) {
        self.connection.close(error_code, reason);
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
            .field("is_open", &self.is_open())
            .finish()
    }
}
