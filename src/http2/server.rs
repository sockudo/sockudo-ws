//! HTTP/2 WebSocket server implementation
//!
//! This module provides `H2WebSocketServer`, a server that accepts WebSocket
//! connections over HTTP/2 using the Extended CONNECT protocol (RFC 8441).

use std::future::Future;

use bytes::Bytes;
use h2::server::{self, SendResponse};
use http::{Request, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{Error, Result};
use crate::protocol::Role;
use crate::{Config, WebSocketStream};

use super::handshake::{build_h2_error_response, build_h2_response, H2HandshakeRequest};
use super::stream::H2Stream;

/// HTTP/2 WebSocket server
///
/// Accepts HTTP/2 connections and handles Extended CONNECT requests
/// for WebSocket upgrades (RFC 8441).
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::{Config, Message};
/// use sockudo_ws::http2::H2WebSocketServer;
/// use futures_util::StreamExt;
///
/// let server = H2WebSocketServer::new(Config::default());
///
/// // For each accepted TCP connection (with TLS):
/// server.serve(tls_stream, |mut ws, req| async move {
///     println!("WebSocket at: {}", req.path);
///     while let Some(msg) = ws.next().await {
///         if let Ok(msg) = msg {
///             ws.send(msg).await.ok();
///         }
///     }
/// }).await?;
/// ```
#[derive(Clone)]
pub struct H2WebSocketServer {
    config: Config,
}

impl H2WebSocketServer {
    /// Create a new HTTP/2 WebSocket server with the given configuration
    pub fn new(config: Config) -> Self {
        Self { config }
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
        F: Fn(WebSocketStream<H2Stream>, H2HandshakeRequest) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let config = self.config.clone();

        // Build h2 server connection with Extended CONNECT enabled
        let mut h2_builder = server::Builder::new();

        #[cfg(feature = "http2")]
        {
            h2_builder
                .initial_window_size(config.http2.initial_stream_window_size)
                .initial_connection_window_size(config.http2.initial_connection_window_size)
                .max_concurrent_streams(config.http2.max_concurrent_streams);
        }

        // Enable Extended CONNECT protocol (RFC 8441)
        h2_builder.enable_connect_protocol();

        let mut h2_conn = h2_builder.handshake(stream).await.map_err(Error::from)?;

        // Accept and handle streams
        while let Some(result) = h2_conn.accept().await {
            let (request, respond) = result.map_err(Error::from)?;

            let handler = handler.clone();
            let config = config.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_request(request, respond, handler, config).await {
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
        F: Fn(WebSocketStream<H2Stream>, H2HandshakeRequest) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
        Filter: Fn(&H2HandshakeRequest) -> bool + Clone + Send + 'static,
    {
        let config = self.config.clone();

        let mut h2_builder = server::Builder::new();

        #[cfg(feature = "http2")]
        {
            h2_builder
                .initial_window_size(config.http2.initial_stream_window_size)
                .initial_connection_window_size(config.http2.initial_connection_window_size)
                .max_concurrent_streams(config.http2.max_concurrent_streams);
        }

        h2_builder.enable_connect_protocol();

        let mut h2_conn = h2_builder.handshake(stream).await.map_err(Error::from)?;

        while let Some(result) = h2_conn.accept().await {
            let (request, respond) = result.map_err(Error::from)?;

            let handler = handler.clone();
            let filter = filter.clone();
            let config = config.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    handle_request_filtered(request, respond, filter, handler, config).await
                {
                    eprintln!("HTTP/2 WebSocket error: {}", e);
                }
            });
        }

        Ok(())
    }
}

/// Handle a single HTTP/2 request, potentially upgrading to WebSocket
async fn handle_request<F, Fut>(
    request: Request<h2::RecvStream>,
    mut respond: SendResponse<Bytes>,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H2Stream>, H2HandshakeRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    // Check if this is an Extended CONNECT for WebSocket
    if let Some(ws_req) = H2HandshakeRequest::from_request(&request) {
        // Accept the WebSocket upgrade
        let response = build_h2_response(
            ws_req.protocol.as_deref(),
            None, // Extensions negotiation could be added here
        );

        let send_stream = respond
            .send_response(response, false)
            .map_err(Error::from)?;

        let recv_stream = request.into_body();

        // Create H2Stream wrapper
        let h2_stream = H2Stream::new(send_stream, recv_stream);

        // Create WebSocketStream over H2Stream
        let ws = WebSocketStream::from_raw(h2_stream, Role::Server, config);

        // Call user handler
        handler(ws, ws_req).await;

        Ok(())
    } else {
        // Not a WebSocket request - send error response
        let response = build_h2_error_response(
            StatusCode::BAD_REQUEST,
            Some("Expected Extended CONNECT for WebSocket"),
        );
        respond.send_response(response, true).map_err(Error::from)?;
        Ok(())
    }
}

/// Handle a single HTTP/2 request with filtering
async fn handle_request_filtered<F, Fut, Filter>(
    request: Request<h2::RecvStream>,
    mut respond: SendResponse<Bytes>,
    filter: Filter,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(WebSocketStream<H2Stream>, H2HandshakeRequest) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    Filter: Fn(&H2HandshakeRequest) -> bool + Send + 'static,
{
    if let Some(ws_req) = H2HandshakeRequest::from_request(&request) {
        // Apply filter
        if !filter(&ws_req) {
            let response = build_h2_error_response(
                StatusCode::FORBIDDEN,
                Some("WebSocket connection rejected by filter"),
            );
            respond.send_response(response, true).map_err(Error::from)?;
            return Ok(());
        }

        // Accept the WebSocket upgrade
        let response = build_h2_response(ws_req.protocol.as_deref(), None);

        let send_stream = respond
            .send_response(response, false)
            .map_err(Error::from)?;

        let recv_stream = request.into_body();
        let h2_stream = H2Stream::new(send_stream, recv_stream);
        let ws = WebSocketStream::from_raw(h2_stream, Role::Server, config);

        handler(ws, ws_req).await;

        Ok(())
    } else {
        let response = build_h2_error_response(
            StatusCode::BAD_REQUEST,
            Some("Expected Extended CONNECT for WebSocket"),
        );
        respond.send_response(response, true).map_err(Error::from)?;
        Ok(())
    }
}

impl Default for H2WebSocketServer {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl std::fmt::Debug for H2WebSocketServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H2WebSocketServer")
            .field("config", &self.config)
            .finish()
    }
}
