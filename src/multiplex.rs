//! Unified multiplexed connection implementation
//!
//! This module provides `MultiplexedConnection<T>`, which allows opening
//! multiple WebSocket streams over a single HTTP/2 or HTTP/3 connection.
//!
//! # Example
//!
//! ```ignore
//! use sockudo_ws::{WebSocketClient, Http2, Http3, Config};
//!
//! // HTTP/2 multiplexing
//! let client = WebSocketClient::<Http2>::new(Config::default());
//! let mut mux = client.connect_multiplexed(tls_stream).await?;
//!
//! let ws1 = mux.open_websocket("wss://example.com/chat", None).await?;
//! let ws2 = mux.open_websocket("wss://example.com/notifications", None).await?;
//!
//! // HTTP/3 multiplexing
//! let client = WebSocketClient::<Http3>::new(Config::default());
//! let mut mux = client.connect_multiplexed(addr, "example.com", tls_config).await?;
//!
//! let ws1 = mux.open_websocket("/chat", None).await?;
//! let ws2 = mux.open_websocket("/notifications", None).await?;
//! ```

use std::marker::PhantomData;

#[cfg(feature = "http3")]
use std::net::SocketAddr;

use crate::error::{Error, Result};
use crate::protocol::Role;
use crate::transport::Transport;
use crate::{Config, WebSocketStream};

#[cfg(feature = "http2")]
use crate::transport::Http2;

#[cfg(feature = "http3")]
use crate::transport::Http3;

// ============================================================================
// HTTP/2 imports
// ============================================================================

#[cfg(feature = "http2")]
use {
    bytes::Bytes,
    h2::client::SendRequest,
    http::{Method, Request, Uri},
};

#[cfg(feature = "http2")]
use crate::stream::Stream;

// ============================================================================
// HTTP/3 imports
// ============================================================================

#[cfg(feature = "http3")]
use bytes::Bytes as H3Bytes;

#[cfg(feature = "http3")]
use crate::stream::Stream as H3Stream;

#[cfg(feature = "http3")]
use http::StatusCode;

// ============================================================================
// MultiplexedConnection
// ============================================================================

/// A multiplexed connection that can create multiple WebSocket streams
///
/// This allows opening multiple WebSocket connections over a single
/// HTTP/2 or HTTP/3 connection, which is more efficient than creating
/// separate connections.
pub struct MultiplexedConnection<T: Transport> {
    inner: MultiplexInner<T>,
    config: Config,
}

enum MultiplexInner<T: Transport> {
    #[cfg(feature = "http2")]
    Http2 { send_request: SendRequest<Bytes> },
    #[cfg(feature = "http3")]
    Http3 {
        connection: quinn::Connection,
        send_request: h3::client::SendRequest<h3_quinn::OpenStreams, H3Bytes>,
        server_name: String,
        server_port: u16,
    },
    #[allow(dead_code)]
    Phantom(PhantomData<T>),
}

// ============================================================================
// HTTP/2 MultiplexedConnection Implementation
// ============================================================================

#[cfg(feature = "http2")]
impl MultiplexedConnection<Http2> {
    /// Create a new HTTP/2 multiplexed connection (internal use)
    pub(crate) fn new_http2(send_request: SendRequest<Bytes>, config: Config) -> Self {
        Self {
            inner: MultiplexInner::Http2 { send_request },
            config,
        }
    }

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
    ) -> Result<WebSocketStream<Stream<Http2>>> {
        let send_request = match &mut self.inner {
            MultiplexInner::Http2 { send_request } => send_request,
            _ => unreachable!(),
        };

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

        let (response_future, send_stream) = send_request
            .send_request(request, false)
            .map_err(Error::from)?;

        let response = response_future.await.map_err(Error::from)?;

        if response.status() != http::StatusCode::OK {
            return Err(Error::HandshakeFailed("server rejected WebSocket upgrade"));
        }

        let recv_stream = response.into_body();
        let h2_stream = Stream::<Http2>::from_h2(send_stream, recv_stream);

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

// ============================================================================
// HTTP/3 MultiplexedConnection Implementation
// ============================================================================

#[cfg(feature = "http3")]
impl MultiplexedConnection<Http3> {
    /// Create a new HTTP/3 multiplexed connection (internal use)
    pub(crate) fn new_http3(
        connection: quinn::Connection,
        send_request: h3::client::SendRequest<h3_quinn::OpenStreams, H3Bytes>,
        server_name: String,
        server_port: u16,
        config: Config,
    ) -> Self {
        Self {
            inner: MultiplexInner::Http3 {
                connection,
                send_request,
                server_name,
                server_port,
            },
            config,
        }
    }

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
    ) -> Result<WebSocketStream<H3Stream<Http3>>> {
        use http::{Method, Request};

        let (send_request, server_name, server_port) = match &mut self.inner {
            MultiplexInner::Http3 {
                send_request,
                server_name,
                server_port,
                ..
            } => (send_request, server_name.clone(), *server_port),
            _ => unreachable!(),
        };

        let uri = format!("https://{}:{}{}", server_name, server_port, path);

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

        let mut stream = send_request
            .send_request(request)
            .await
            .map_err(Error::from)?;

        let response = stream.recv_response().await.map_err(Error::from)?;

        if response.status() != StatusCode::OK {
            return Err(Error::HandshakeFailed("server rejected WebSocket upgrade"));
        }

        let h3_stream = H3Stream::<Http3>::from_h3_client(stream);

        Ok(WebSocketStream::from_raw(
            h3_stream,
            Role::Client,
            self.config.clone(),
        ))
    }

    /// Check if the connection is still open
    pub fn is_open(&self) -> bool {
        match &self.inner {
            MultiplexInner::Http3 { connection, .. } => connection.close_reason().is_none(),
            _ => unreachable!(),
        }
    }

    /// Get connection statistics
    pub fn stats(&self) -> quinn::ConnectionStats {
        match &self.inner {
            MultiplexInner::Http3 { connection, .. } => connection.stats(),
            _ => unreachable!(),
        }
    }

    /// Get the remote address
    pub fn remote_addr(&self) -> SocketAddr {
        match &self.inner {
            MultiplexInner::Http3 { connection, .. } => connection.remote_address(),
            _ => unreachable!(),
        }
    }

    /// Close the connection
    ///
    /// Uses H3_NO_ERROR (0x100) for graceful closure.
    pub fn close(&self) {
        match &self.inner {
            MultiplexInner::Http3 { connection, .. } => {
                connection.close(quinn::VarInt::from_u32(0x100), b"done");
            }
            _ => unreachable!(),
        }
    }
}

// ============================================================================
// Debug implementations
// ============================================================================

#[cfg(feature = "http2")]
impl std::fmt::Debug for MultiplexedConnection<Http2> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiplexedConnection")
            .field("transport", &"HTTP/2")
            .field("is_ready", &self.is_ready())
            .finish()
    }
}

#[cfg(feature = "http3")]
impl std::fmt::Debug for MultiplexedConnection<Http3> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiplexedConnection")
            .field("transport", &"HTTP/3")
            .field("remote_addr", &self.remote_addr())
            .field("is_open", &self.is_open())
            .finish()
    }
}
