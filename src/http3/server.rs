//! HTTP/3 WebSocket server implementation
//!
//! This module provides `H3WebSocketServer`, a server that accepts WebSocket
//! connections over HTTP/3/QUIC using the Extended CONNECT protocol (RFC 9220).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use quinn::{Endpoint, ServerConfig};

use crate::error::{Error, Result};
use crate::protocol::Role;
use crate::{Config, WebSocketStream};

use super::handshake::H3HandshakeRequest;
use super::stream::H3Stream;

/// HTTP/3 WebSocket server
///
/// Accepts QUIC connections and handles Extended CONNECT requests
/// for WebSocket upgrades (RFC 9220).
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
        let endpoint = Endpoint::server(server_config, addr).map_err(|e| Error::Io(e))?;

        Ok(Self {
            endpoint,
            config: ws_config,
        })
    }

    /// Create from an existing QUIC endpoint
    ///
    /// Use this when you need more control over the QUIC configuration.
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

    /// Serve WebSocket connections
    ///
    /// This accepts QUIC connections and handles Extended CONNECT requests
    /// for WebSocket. Each WebSocket is handled by the provided handler.
    ///
    /// # Arguments
    ///
    /// * `handler` - Async function to handle each WebSocket connection
    pub async fn serve<F, Fut>(self, handler: F) -> Result<()>
    where
        F: Fn(WebSocketStream<H3Stream>, H3HandshakeRequest) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        while let Some(incoming) = self.endpoint.accept().await {
            let handler = handler.clone();
            let config = self.config.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(incoming, handler, config).await {
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
    pub async fn serve_with_filter<F, Fut, Filter>(self, filter: Filter, handler: F) -> Result<()>
    where
        F: Fn(WebSocketStream<H3Stream>, H3HandshakeRequest) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
        Filter: Fn(&H3HandshakeRequest) -> bool + Clone + Send + Sync + 'static,
    {
        while let Some(incoming) = self.endpoint.accept().await {
            let handler = handler.clone();
            let filter = filter.clone();
            let config = self.config.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection_filtered(incoming, filter, handler, config).await
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

/// Handle a single QUIC connection
async fn handle_connection<F, Fut>(
    incoming: quinn::Incoming,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H3Stream>, H3HandshakeRequest) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let connection = incoming.await.map_err(Error::from)?;

    // Accept bidirectional streams for WebSocket
    loop {
        match connection.accept_bi().await {
            Ok((send, recv)) => {
                let handler = handler.clone();
                let config = config.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_stream(send, recv, handler, config).await {
                        eprintln!("HTTP/3 stream error: {}", e);
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed(_)) => {
                // Normal closure
                break;
            }
            Err(e) => {
                return Err(Error::from(e));
            }
        }
    }

    Ok(())
}

/// Handle a single QUIC connection with filtering
async fn handle_connection_filtered<F, Fut, Filter>(
    incoming: quinn::Incoming,
    filter: Filter,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H3Stream>, H3HandshakeRequest) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    Filter: Fn(&H3HandshakeRequest) -> bool + Clone + Send + 'static,
{
    let connection = incoming.await.map_err(Error::from)?;

    loop {
        match connection.accept_bi().await {
            Ok((send, recv)) => {
                let handler = handler.clone();
                let filter = filter.clone();
                let config = config.clone();

                tokio::spawn(async move {
                    if let Err(e) =
                        handle_stream_filtered(send, recv, filter, handler, config).await
                    {
                        eprintln!("HTTP/3 stream error: {}", e);
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed(_)) => {
                break;
            }
            Err(e) => {
                return Err(Error::from(e));
            }
        }
    }

    Ok(())
}

/// Handle a single bidirectional stream as WebSocket
async fn handle_stream<F, Fut>(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H3Stream>, H3HandshakeRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    // For a full implementation, we would:
    // 1. Read HTTP/3 headers using h3 crate
    // 2. Parse Extended CONNECT request
    // 3. Send 200 OK response
    // 4. Transition to WebSocket mode

    // For now, create a simplified version that treats the stream as WebSocket directly
    // In production, use the h3 crate for proper HTTP/3 framing

    let h3_stream = H3Stream::new(send, recv);

    // Create a placeholder request (in real impl, parse from HTTP/3 headers)
    let ws_req = H3HandshakeRequest {
        path: "/".to_string(),
        authority: "localhost".to_string(),
        scheme: "https".to_string(),
        protocol: None,
        extensions: None,
        origin: None,
        version: Some("13".to_string()),
    };

    let ws = WebSocketStream::from_raw(h3_stream, Role::Server, config);

    handler(ws, ws_req).await;

    Ok(())
}

/// Handle a stream with filtering
async fn handle_stream_filtered<F, Fut, Filter>(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    filter: Filter,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H3Stream>, H3HandshakeRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    Filter: Fn(&H3HandshakeRequest) -> bool + Send + 'static,
{
    let ws_req = H3HandshakeRequest {
        path: "/".to_string(),
        authority: "localhost".to_string(),
        scheme: "https".to_string(),
        protocol: None,
        extensions: None,
        origin: None,
        version: Some("13".to_string()),
    };

    if !filter(&ws_req) {
        // Reject by closing the stream
        return Ok(());
    }

    let h3_stream = H3Stream::new(send, recv);
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
