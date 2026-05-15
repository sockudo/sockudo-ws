//! Native Compio runtime support.
//!
//! Compio uses completion-based I/O traits, which are intentionally different
//! from Tokio's poll-based `AsyncRead` and `AsyncWrite`. This module exposes a
//! native async-method API for Compio streams instead of adapting through Tokio.

#[cfg(any(feature = "http2", feature = "http3"))]
use std::future::Future;
use std::io;
#[cfg(feature = "http2")]
use std::pin::Pin;
use std::sync::mpsc;

#[cfg(any(feature = "http2", feature = "http3"))]
use ::compio::buf::IoBufMut;
use ::compio::buf::{BufResult, IoBuf};
use ::compio::io::util::Splittable;
use ::compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(any(feature = "http2", feature = "http3"))]
use bytes::Buf;
use bytes::{Bytes, BytesMut};

use crate::Config;
use crate::error::{CloseReason, Error, Result};
use crate::handshake::{
    HandshakeResult, build_request, build_response, generate_accept_key, generate_key,
    parse_request, parse_response, validate_accept_key,
};
use crate::protocol::{Message, Protocol, Role};

#[cfg(any(feature = "http2", feature = "http3"))]
use crate::extended_connect::{
    ExtendedConnectRequest, build_extended_connect_error, build_extended_connect_response,
};

/// Re-exported Compio `#[main]` runtime macro for users of `compio-runtime`.
pub use ::compio::main;
/// Re-exported Compio networking primitives for users of `compio-runtime`.
pub use ::compio::net;
/// Re-exported Compio runtime utilities for users of `compio-runtime`.
pub use ::compio::runtime;

const DEFAULT_HIGH_WATER_MARK: usize = 64 * 1024;
const DEFAULT_LOW_WATER_MARK: usize = 16 * 1024;
const MAX_HEADER_SIZE: usize = 8192;
const READ_RESERVE: usize = 8192;
const MIN_READ_SPARE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompioStreamState {
    Open,
    CloseSent,
    Closed,
}

#[derive(Debug, Clone)]
enum ControlRequest {
    Pong(Bytes),
    CloseResponse,
}

async fn read_more<R>(reader: &mut R, buf: &mut BytesMut) -> io::Result<usize>
where
    R: AsyncRead + ?Sized,
{
    if buf.capacity().saturating_sub(buf.len()) < MIN_READ_SPARE {
        buf.reserve(READ_RESERVE);
    }

    let read_buf = std::mem::take(buf);
    let BufResult(res, read_buf) = reader.append(read_buf).await;
    *buf = read_buf;
    res
}

async fn write_all_owned<W, B>(writer: &mut W, buf: B) -> io::Result<()>
where
    W: AsyncWrite + ?Sized,
    B: IoBuf,
{
    let BufResult(res, _) = writer.write_all(buf).await;
    res
}

async fn flush_bytes<W>(writer: &mut W, buf: &mut BytesMut) -> Result<()>
where
    W: AsyncWrite + ?Sized,
{
    if !buf.as_ref().is_empty() {
        let write_buf = std::mem::take(buf);
        let BufResult(res, mut write_buf) = writer.write_all(write_buf).await;
        if res.is_ok() {
            write_buf.clear();
        }
        *buf = write_buf;
        res?;
    }

    writer.flush().await?;
    Ok(())
}

/// Perform a server-side WebSocket handshake over a Compio stream.
pub async fn server_handshake<S>(stream: &mut S) -> Result<HandshakeResult>
where
    S: AsyncRead + AsyncWrite + ?Sized,
{
    server_handshake_with_extensions(stream, None).await
}

/// Perform a server-side WebSocket handshake and include an extension response.
///
/// This is useful when the caller has already negotiated extensions such as
/// `permessage-deflate` and needs the `Sec-WebSocket-Extensions` response
/// header to be sent during upgrade.
pub async fn server_handshake_with_extensions<S>(
    stream: &mut S,
    response_extensions: Option<&str>,
) -> Result<HandshakeResult>
where
    S: AsyncRead + AsyncWrite + ?Sized,
{
    let mut buf = BytesMut::with_capacity(4096);

    loop {
        if buf.len() > MAX_HEADER_SIZE {
            return Err(Error::InvalidHttp("request too large"));
        }

        let n = read_more(stream, &mut buf).await?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }

        if let Some((req, consumed)) = parse_request(&buf)? {
            let path = req.path.to_string();
            let protocol = req.protocol.map(String::from);
            let extensions = req.extensions.map(String::from);
            let accept_key = generate_accept_key(req.key);
            let response = build_response(&accept_key, req.protocol, response_extensions);

            write_all_owned(stream, response).await?;
            stream.flush().await?;

            let leftover = if consumed < buf.len() {
                Some(buf.split_off(consumed).freeze())
            } else {
                None
            };

            return Ok(HandshakeResult {
                path,
                protocol,
                extensions,
                leftover,
            });
        }
    }
}

/// Perform a client-side WebSocket handshake over a Compio stream.
pub async fn client_handshake<S>(
    stream: &mut S,
    host: &str,
    path: &str,
    protocol: Option<&str>,
) -> Result<HandshakeResult>
where
    S: AsyncRead + AsyncWrite + ?Sized,
{
    let key = generate_key();
    let request = build_request(host, path, &key, protocol, None);

    write_all_owned(stream, request).await?;
    stream.flush().await?;

    let mut buf = BytesMut::with_capacity(4096);

    loop {
        if buf.len() > MAX_HEADER_SIZE {
            return Err(Error::InvalidHttp("response too large"));
        }

        let n = read_more(stream, &mut buf).await?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }

        if let Some((res, consumed)) = parse_response(&buf)? {
            let accept = res
                .accept
                .ok_or(Error::HandshakeFailed("missing Sec-WebSocket-Accept"))?;
            if !validate_accept_key(&key, accept) {
                return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Accept"));
            }

            let res_protocol = res.protocol.map(String::from);
            let res_extensions = res.extensions.map(String::from);

            let leftover = if consumed < buf.len() {
                Some(buf.split_off(consumed).freeze())
            } else {
                None
            };

            return Ok(HandshakeResult {
                path: path.to_string(),
                protocol: res_protocol,
                extensions: res_extensions,
                leftover,
            });
        }
    }
}

/// Accept an already-connected Compio transport as a server WebSocket.
pub async fn accept_async<S>(
    mut stream: S,
    config: Config,
) -> Result<(CompioWebSocketStream<S>, HandshakeResult)>
where
    S: AsyncRead + AsyncWrite,
{
    let handshake = server_handshake(&mut stream).await?;
    let ws =
        CompioWebSocketStream::server_with_leftover(stream, config, handshake.leftover.clone());
    Ok((ws, handshake))
}

/// Connect an already-connected Compio transport as a client WebSocket.
pub async fn connect_async<S>(
    mut stream: S,
    host: &str,
    path: &str,
    protocol: Option<&str>,
    config: Config,
) -> Result<(CompioWebSocketStream<S>, HandshakeResult)>
where
    S: AsyncRead + AsyncWrite,
{
    let handshake = client_handshake(&mut stream, host, path, protocol).await?;
    let ws =
        CompioWebSocketStream::client_with_leftover(stream, config, handshake.leftover.clone());
    Ok((ws, handshake))
}

#[cfg(any(feature = "http2", feature = "http3"))]
fn copy_into_compio_buf<B>(dst: &mut B, src: &mut BytesMut) -> usize
where
    B: IoBufMut,
{
    let len = src.len().min(dst.buf_capacity());
    if len == 0 {
        return 0;
    }

    // SAFETY:
    // - `dst.buf_mut_ptr()` points to at least `dst.buf_capacity()` writable bytes.
    // - `len` is capped to that capacity and to the initialized source length.
    // - We set the initialized length to exactly the copied byte count.
    unsafe {
        std::ptr::copy_nonoverlapping(src.as_ptr(), dst.buf_mut_ptr().cast::<u8>(), len);
        dst.set_len(len);
    }
    Buf::advance(src, len);
    len
}

#[cfg(feature = "http3")]
type CompioH3BidiStream = ::compio::quic::h3::BidiStream<Bytes>;
#[cfg(feature = "http3")]
type CompioH3ClientRequestStream = h3::client::RequestStream<CompioH3BidiStream, Bytes>;
#[cfg(feature = "http3")]
type CompioH3ServerRequestStream = h3::server::RequestStream<CompioH3BidiStream, Bytes>;
#[cfg(feature = "http3")]
type CompioH3SendRequest = h3::client::SendRequest<::compio::quic::h3::OpenStreams, Bytes>;

/// HTTP/2 stream exposed through native Compio I/O traits.
#[cfg(feature = "http2")]
pub struct CompioHttp2Stream {
    send: h2::SendStream<Bytes>,
    recv: h2::RecvStream,
    recv_buf: BytesMut,
    recv_eof: bool,
    capacity_needed: usize,
}

#[cfg(feature = "http2")]
impl CompioHttp2Stream {
    /// Create a stream from h2 send and receive halves after Extended CONNECT.
    pub fn new(send: h2::SendStream<Bytes>, recv: h2::RecvStream) -> Self {
        Self {
            send,
            recv,
            recv_buf: BytesMut::with_capacity(crate::RECV_BUFFER_SIZE),
            recv_eof: false,
            capacity_needed: 0,
        }
    }

    /// Get the underlying h2 send stream.
    pub fn send_stream(&self) -> &h2::SendStream<Bytes> {
        &self.send
    }

    /// Get the underlying h2 receive stream.
    pub fn recv_stream(&self) -> &h2::RecvStream {
        &self.recv
    }
}

#[cfg(feature = "http2")]
impl AsyncRead for CompioHttp2Stream {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        if !self.recv_buf.is_empty() {
            let len = copy_into_compio_buf(&mut buf, &mut self.recv_buf);
            return BufResult(Ok(len), buf);
        }

        if self.recv_eof {
            return BufResult(Ok(0), buf);
        }

        match std::future::poll_fn(|cx| Pin::new(&mut self.recv).poll_data(cx)).await {
            Some(Ok(mut data)) => {
                let len = data.len();
                let _ = self.recv.flow_control().release_capacity(len);

                self.recv_buf.reserve(data.len());
                while data.has_remaining() {
                    let chunk = data.chunk();
                    self.recv_buf.extend_from_slice(chunk);
                    let len = chunk.len();
                    data.advance(len);
                }

                let len = copy_into_compio_buf(&mut buf, &mut self.recv_buf);
                BufResult(Ok(len), buf)
            }
            Some(Err(e)) => BufResult(Err(io::Error::other(e)), buf),
            None => {
                self.recv_eof = true;
                BufResult(Ok(0), buf)
            }
        }
    }
}

#[cfg(feature = "http2")]
impl AsyncWrite for CompioHttp2Stream {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let bytes = buf.as_init();
        if bytes.is_empty() {
            return BufResult(Ok(0), buf);
        }

        if self.capacity_needed > 0 || self.send.capacity() == 0 {
            self.send.reserve_capacity(bytes.len());
            self.capacity_needed = bytes.len();
        }

        let capacity = match std::future::poll_fn(|cx| self.send.poll_capacity(cx)).await {
            Some(Ok(capacity)) => capacity,
            Some(Err(e)) => return BufResult(Err(io::Error::other(e)), buf),
            None => {
                return BufResult(
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "HTTP/2 stream closed",
                    )),
                    buf,
                );
            }
        };

        let len = capacity.min(bytes.len());
        let data = Bytes::copy_from_slice(&bytes[..len]);

        match self.send.send_data(data, false) {
            Ok(()) => {
                self.capacity_needed = 0;
                BufResult(Ok(len), buf)
            }
            Err(e) => BufResult(Err(io::Error::other(e)), buf),
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.send
            .send_data(Bytes::new(), true)
            .map_err(io::Error::other)
    }
}

/// Multiplexed HTTP/2 connection driven by the Compio runtime.
#[cfg(feature = "http2")]
pub struct CompioHttp2Connection {
    send_request: h2::client::SendRequest<Bytes>,
    config: Config,
}

#[cfg(feature = "http2")]
impl CompioHttp2Connection {
    /// Open another WebSocket stream over this HTTP/2 connection.
    pub async fn open_websocket(
        &mut self,
        uri: &str,
        protocol: Option<&str>,
    ) -> Result<CompioWebSocketStream<CompioHttp2Stream>> {
        let uri: http::Uri = uri
            .parse()
            .map_err(|_| Error::HandshakeFailed("invalid URI"))?;

        let authority = uri
            .authority()
            .ok_or(Error::HandshakeFailed("URI missing authority"))?
            .to_string();
        let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        let scheme = uri.scheme_str().unwrap_or("https");
        let full_uri = format!("{}://{}{}", scheme, authority, path);

        let mut req = http::Request::builder()
            .method(http::Method::CONNECT)
            .uri(&full_uri)
            .header("sec-websocket-version", "13");

        if let Some(protocol) = protocol {
            req = req.header("sec-websocket-protocol", protocol);
        }

        let mut request = req
            .body(())
            .map_err(|_| Error::HandshakeFailed("failed to build request"))?;
        request
            .extensions_mut()
            .insert(h2::ext::Protocol::from_static("websocket"));

        let (response, send_stream) = self
            .send_request
            .send_request(request, false)
            .map_err(Error::from)?;
        let response = response.await.map_err(Error::from)?;

        if response.status() != http::StatusCode::OK {
            return Err(Error::HandshakeFailed("server rejected WebSocket upgrade"));
        }

        let stream = CompioHttp2Stream::new(send_stream, response.into_body());
        Ok(CompioWebSocketStream::client(stream, self.config.clone()))
    }
}

/// Connect to an HTTP/2 WebSocket endpoint over a Compio stream.
#[cfg(feature = "http2")]
pub async fn connect_http2<S>(
    stream: S,
    uri: &str,
    protocol: Option<&str>,
    config: Config,
) -> Result<CompioWebSocketStream<CompioHttp2Stream>>
where
    S: AsyncRead + AsyncWrite + 'static,
{
    let mut conn = connect_http2_multiplexed(stream, config).await?;
    conn.open_websocket(uri, protocol).await
}

/// Create a multiplexed HTTP/2 connection over a Compio stream.
#[cfg(feature = "http2")]
pub async fn connect_http2_multiplexed<S>(
    stream: S,
    config: Config,
) -> Result<CompioHttp2Connection>
where
    S: AsyncRead + AsyncWrite + 'static,
{
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let stream = ::compio::io::compat::AsyncStream::new(stream).compat();
    let mut builder = h2::client::Builder::new();
    builder
        .initial_window_size(config.http2.initial_stream_window_size)
        .initial_connection_window_size(config.http2.initial_connection_window_size);

    let (send_request, conn) = builder.handshake(stream).await.map_err(Error::from)?;

    runtime::spawn(async move {
        if let Err(e) = conn.await {
            eprintln!("HTTP/2 connection error: {}", e);
        }
    })
    .detach();

    Ok(CompioHttp2Connection {
        send_request,
        config,
    })
}

/// Serve HTTP/2 WebSocket streams from an already negotiated Compio transport.
#[cfg(feature = "http2")]
pub async fn serve_http2<S, F, Fut>(stream: S, config: Config, handler: F) -> Result<()>
where
    S: AsyncRead + AsyncWrite + 'static,
    F: Fn(CompioWebSocketStream<CompioHttp2Stream>, ExtendedConnectRequest) -> Fut
        + Clone
        + 'static,
    Fut: Future<Output = ()> + 'static,
{
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let stream = ::compio::io::compat::AsyncStream::new(stream).compat();
    let mut builder = h2::server::Builder::new();
    builder
        .initial_window_size(config.http2.initial_stream_window_size)
        .initial_connection_window_size(config.http2.initial_connection_window_size)
        .max_concurrent_streams(config.http2.max_concurrent_streams);
    builder.enable_connect_protocol();

    let mut conn = builder.handshake(stream).await.map_err(Error::from)?;

    while let Some(result) = conn.accept().await {
        let Ok((request, respond)) = result else {
            break;
        };
        let handler = handler.clone();
        let config = config.clone();

        runtime::spawn(async move {
            if let Err(e) = handle_http2_request(request, respond, handler, config).await {
                eprintln!("HTTP/2 WebSocket error: {}", e);
            }
        })
        .detach();
    }

    Ok(())
}

#[cfg(feature = "http2")]
async fn handle_http2_request<F, Fut>(
    request: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(CompioWebSocketStream<CompioHttp2Stream>, ExtendedConnectRequest) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    if let Some(mut ws_req) = ExtendedConnectRequest::from_request(&request) {
        if ws_req.protocol.is_none() {
            ws_req.protocol = Some("websocket".to_string());
        }

        if let Err(status) = ws_req.validate() {
            let response = build_extended_connect_error(status, None);
            respond.send_response(response, true).ok();
            return Ok(());
        }

        let response = build_extended_connect_response(None, None);
        let send_stream = respond
            .send_response(response, false)
            .map_err(Error::from)?;
        let recv_stream = request.into_body();
        let stream = CompioHttp2Stream::new(send_stream, recv_stream);
        let ws = CompioWebSocketStream::server(stream, config);
        handler(ws, ws_req).await;
    } else {
        let response = build_extended_connect_error(
            http::StatusCode::METHOD_NOT_ALLOWED,
            Some("Expected CONNECT"),
        );
        respond.send_response(response, true).ok();
    }

    Ok(())
}

/// HTTP/3 client stream exposed through native Compio I/O traits.
#[cfg(feature = "http3")]
pub struct CompioHttp3ClientStream {
    stream: CompioH3ClientRequestStream,
    recv_buf: BytesMut,
    _endpoint: Option<::compio::quic::Endpoint>,
    _send_request: Option<CompioH3SendRequest>,
}

#[cfg(feature = "http3")]
impl CompioHttp3ClientStream {
    /// Create a Compio HTTP/3 client stream from an established request stream.
    pub fn new(
        stream: CompioH3ClientRequestStream,
        endpoint: Option<::compio::quic::Endpoint>,
        send_request: Option<CompioH3SendRequest>,
    ) -> Self {
        Self {
            stream,
            recv_buf: BytesMut::with_capacity(crate::RECV_BUFFER_SIZE),
            _endpoint: endpoint,
            _send_request: send_request,
        }
    }
}

#[cfg(feature = "http3")]
impl AsyncRead for CompioHttp3ClientStream {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        if self.recv_buf.is_empty() {
            match self.stream.recv_data().await {
                Ok(Some(mut data)) => {
                    while data.has_remaining() {
                        let chunk = data.chunk();
                        self.recv_buf.extend_from_slice(chunk);
                        let len = chunk.len();
                        data.advance(len);
                    }
                }
                Ok(None) => return BufResult(Ok(0), buf),
                Err(e) => return BufResult(Err(io::Error::other(e)), buf),
            }
        }

        let len = copy_into_compio_buf(&mut buf, &mut self.recv_buf);
        BufResult(Ok(len), buf)
    }
}

#[cfg(feature = "http3")]
impl AsyncWrite for CompioHttp3ClientStream {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let bytes = buf.as_init();
        if bytes.is_empty() {
            return BufResult(Ok(0), buf);
        }

        let len = bytes.len();
        let data = Bytes::copy_from_slice(bytes);
        match self.stream.send_data(data).await {
            Ok(()) => BufResult(Ok(len), buf),
            Err(e) => BufResult(Err(io::Error::other(e)), buf),
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.finish().await.map_err(io::Error::other)
    }
}

/// HTTP/3 server stream exposed through native Compio I/O traits.
#[cfg(feature = "http3")]
pub struct CompioHttp3ServerStream {
    stream: CompioH3ServerRequestStream,
    recv_buf: BytesMut,
}

#[cfg(feature = "http3")]
impl CompioHttp3ServerStream {
    /// Create a Compio HTTP/3 server stream after Extended CONNECT.
    pub fn new(stream: CompioH3ServerRequestStream) -> Self {
        Self {
            stream,
            recv_buf: BytesMut::with_capacity(crate::RECV_BUFFER_SIZE),
        }
    }
}

#[cfg(feature = "http3")]
impl AsyncRead for CompioHttp3ServerStream {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        if self.recv_buf.is_empty() {
            match self.stream.recv_data().await {
                Ok(Some(mut data)) => {
                    while data.has_remaining() {
                        let chunk = data.chunk();
                        self.recv_buf.extend_from_slice(chunk);
                        let len = chunk.len();
                        data.advance(len);
                    }
                }
                Ok(None) => return BufResult(Ok(0), buf),
                Err(e) => return BufResult(Err(io::Error::other(e)), buf),
            }
        }

        let len = copy_into_compio_buf(&mut buf, &mut self.recv_buf);
        BufResult(Ok(len), buf)
    }
}

#[cfg(feature = "http3")]
impl AsyncWrite for CompioHttp3ServerStream {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let bytes = buf.as_init();
        if bytes.is_empty() {
            return BufResult(Ok(0), buf);
        }

        let len = bytes.len();
        let data = Bytes::copy_from_slice(bytes);
        match self.stream.send_data(data).await {
            Ok(()) => BufResult(Ok(len), buf),
            Err(e) => BufResult(Err(io::Error::other(e)), buf),
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.finish().await.map_err(io::Error::other)
    }
}

/// Multiplexed HTTP/3 connection driven by the Compio runtime.
#[cfg(feature = "http3")]
pub struct CompioHttp3Connection {
    endpoint: ::compio::quic::Endpoint,
    send_request: CompioH3SendRequest,
    server_name: String,
    server_port: u16,
    config: Config,
}

#[cfg(feature = "http3")]
impl CompioHttp3Connection {
    /// Open another WebSocket stream over this HTTP/3 connection.
    pub async fn open_websocket(
        &mut self,
        path: &str,
        protocol: Option<&str>,
    ) -> Result<CompioWebSocketStream<CompioHttp3ClientStream>> {
        let uri = format!("https://{}:{}{}", self.server_name, self.server_port, path);

        let mut req = http::Request::builder()
            .method(http::Method::CONNECT)
            .uri(&uri)
            .header("sec-websocket-version", "13");

        if let Some(protocol) = protocol {
            req = req.header("sec-websocket-protocol", protocol);
        }

        let request = req
            .extension(h3::ext::Protocol::WEB_TRANSPORT)
            .body(())
            .map_err(|_| Error::HandshakeFailed("failed to build request"))?;

        let mut stream = self
            .send_request
            .send_request(request)
            .await
            .map_err(Error::from)?;
        let response = stream.recv_response().await.map_err(Error::from)?;

        if response.status() != http::StatusCode::OK {
            return Err(Error::HandshakeFailed("server rejected WebSocket upgrade"));
        }

        let stream = CompioHttp3ClientStream::new(
            stream,
            Some(self.endpoint.clone()),
            Some(self.send_request.clone()),
        );
        Ok(CompioWebSocketStream::client(stream, self.config.clone()))
    }

    /// Get the local UDP address backing this connection.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Close the underlying endpoint and its active connections.
    pub fn close(&self) {
        self.endpoint
            .close(::compio::quic::VarInt::from_u32(0x100), b"done");
    }
}

/// Connect to an HTTP/3 WebSocket endpoint over Compio QUIC.
#[cfg(feature = "http3")]
pub async fn connect_http3(
    server_addr: std::net::SocketAddr,
    server_name: &str,
    path: &str,
    protocol: Option<&str>,
    tls_config: rustls::ClientConfig,
    config: Config,
) -> Result<CompioWebSocketStream<CompioHttp3ClientStream>> {
    let mut conn = connect_http3_multiplexed(server_addr, server_name, tls_config, config).await?;
    conn.open_websocket(path, protocol).await
}

/// Create a multiplexed HTTP/3 connection over Compio QUIC.
#[cfg(feature = "http3")]
pub async fn connect_http3_multiplexed(
    server_addr: std::net::SocketAddr,
    server_name: &str,
    tls_config: rustls::ClientConfig,
    config: Config,
) -> Result<CompioHttp3Connection> {
    let bind_ip = if server_addr.is_ipv6() {
        std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
    } else {
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
    };

    let endpoint = ::compio::quic::ClientBuilder::new_with_rustls_client_config(tls_config)
        .with_alpn_protocols(&["h3"])
        .bind(std::net::SocketAddr::new(bind_ip, 0))
        .await
        .map_err(Error::Io)?;

    let conn = endpoint
        .connect(server_addr, server_name, None)
        .map_err(|e| Error::Http3(e.to_string()))?
        .await
        .map_err(|e| Error::Http3(e.to_string()))?;

    let mut builder = ::compio::quic::h3::client::builder();
    builder.enable_extended_connect(true);
    let (mut driver, send_request) = builder
        .build::<_, ::compio::quic::h3::OpenStreams, Bytes>(conn)
        .await
        .map_err(Error::from)?;

    runtime::spawn(async move {
        let _ = driver.wait_idle().await;
    })
    .detach();

    Ok(CompioHttp3Connection {
        endpoint,
        send_request,
        server_name: server_name.to_string(),
        server_port: server_addr.port(),
        config,
    })
}

/// HTTP/3 WebSocket server backed by Compio QUIC.
#[cfg(feature = "http3")]
pub struct CompioHttp3Server {
    endpoint: ::compio::quic::Endpoint,
    config: Config,
}

#[cfg(feature = "http3")]
impl CompioHttp3Server {
    /// Bind a Compio HTTP/3 WebSocket server.
    pub async fn bind(
        addr: std::net::SocketAddr,
        tls_config: rustls::ServerConfig,
        config: Config,
    ) -> Result<Self> {
        let endpoint = ::compio::quic::ServerBuilder::new_with_rustls_server_config(tls_config)
            .with_alpn_protocols(&["h3"])
            .bind(addr)
            .await
            .map_err(Error::Io)?;

        Ok(Self { endpoint, config })
    }

    /// Build a server from an existing Compio QUIC endpoint.
    pub fn from_endpoint(endpoint: ::compio::quic::Endpoint, config: Config) -> Self {
        Self { endpoint, config }
    }

    /// Get the local UDP address.
    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Serve HTTP/3 WebSocket connections until the endpoint closes.
    pub async fn serve<F, Fut>(self, handler: F) -> Result<()>
    where
        F: Fn(CompioWebSocketStream<CompioHttp3ServerStream>, ExtendedConnectRequest) -> Fut
            + Clone
            + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        while let Some(incoming) = self.endpoint.wait_incoming().await {
            let handler = handler.clone();
            let config = self.config.clone();

            runtime::spawn(async move {
                if let Err(e) = handle_http3_connection(incoming, handler, config).await {
                    eprintln!("HTTP/3 connection error: {}", e);
                }
            })
            .detach();
        }

        Ok(())
    }

    /// Close the underlying endpoint.
    pub fn close(&self, error_code: ::compio::quic::VarInt, reason: &[u8]) {
        self.endpoint.close(error_code, reason);
    }
}

#[cfg(feature = "http3")]
async fn handle_http3_connection<F, Fut>(
    incoming: ::compio::quic::Incoming,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(CompioWebSocketStream<CompioHttp3ServerStream>, ExtendedConnectRequest) -> Fut
        + Clone
        + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let conn = incoming.await.map_err(|e| Error::Http3(e.to_string()))?;
    let mut builder = ::compio::quic::h3::server::builder();
    builder
        .enable_extended_connect(true)
        .enable_webtransport(true)
        .max_webtransport_sessions(1024);
    let mut conn = builder.build::<_, Bytes>(conn).await.map_err(Error::from)?;

    loop {
        let Some(resolver) = (match conn.accept().await {
            Ok(resolver) => resolver,
            Err(_) => break,
        }) else {
            break;
        };

        let (request, stream) = resolver.resolve_request().await.map_err(Error::from)?;
        let handler = handler.clone();
        let config = config.clone();

        runtime::spawn(async move {
            if let Err(e) = handle_http3_request(request, stream, handler, config).await {
                eprintln!("HTTP/3 request error: {}", e);
            }
        })
        .detach();
    }

    Ok(())
}

#[cfg(feature = "http3")]
async fn handle_http3_request<F, Fut>(
    request: http::Request<()>,
    mut stream: CompioH3ServerRequestStream,
    handler: F,
    config: Config,
) -> Result<()>
where
    F: Fn(CompioWebSocketStream<CompioHttp3ServerStream>, ExtendedConnectRequest) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    if request.method() != http::Method::CONNECT {
        let response = build_extended_connect_error(
            http::StatusCode::METHOD_NOT_ALLOWED,
            Some("Expected CONNECT"),
        );
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
        .ok_or(Error::HandshakeFailed("invalid CONNECT request"))?;

    if ws_req.protocol.is_none() {
        ws_req.protocol = protocol_header;
    }

    if let Err(status) = ws_req.validate() {
        let response = build_extended_connect_error(status, None);
        stream.send_response(response).await.ok();
        return Ok(());
    }

    let response = build_extended_connect_response(None, None);
    stream.send_response(response).await.map_err(Error::from)?;

    let ws = CompioWebSocketStream::server(CompioHttp3ServerStream::new(stream), config);
    handler(ws, ws_req).await;

    Ok(())
}

/// A WebSocket stream over a native Compio transport.
pub struct CompioWebSocketStream<S> {
    inner: S,
    protocol: Protocol,
    read_buf: BytesMut,
    write_buf: BytesMut,
    state: CompioStreamState,
    config: Config,
    pending_messages: Vec<Message>,
    pending_index: usize,
    high_water_mark: usize,
    low_water_mark: usize,
}

impl<S> CompioWebSocketStream<S>
where
    S: AsyncRead + AsyncWrite,
{
    /// Create a WebSocket stream from an already-upgraded connection.
    pub fn from_raw(inner: S, role: Role, config: Config) -> Self {
        Self::from_raw_with_leftover(inner, role, config, None)
    }

    /// Create a WebSocket stream with bytes already read after the handshake.
    pub fn from_raw_with_leftover(
        inner: S,
        role: Role,
        config: Config,
        leftover: Option<Bytes>,
    ) -> Self {
        let mut read_buf = BytesMut::with_capacity(crate::RECV_BUFFER_SIZE);
        if let Some(leftover) = leftover {
            read_buf.extend_from_slice(&leftover);
        }

        Self {
            inner,
            protocol: Protocol::new(role, config.max_frame_size, config.max_message_size),
            read_buf,
            write_buf: BytesMut::with_capacity(config.write_buffer_size),
            state: CompioStreamState::Open,
            config,
            pending_messages: Vec::new(),
            pending_index: 0,
            high_water_mark: DEFAULT_HIGH_WATER_MARK,
            low_water_mark: DEFAULT_LOW_WATER_MARK,
        }
    }

    /// Create a server-side WebSocket stream.
    pub fn server(inner: S, config: Config) -> Self {
        Self::from_raw(inner, Role::Server, config)
    }

    /// Create a server-side stream with post-handshake leftover bytes.
    pub fn server_with_leftover(inner: S, config: Config, leftover: Option<Bytes>) -> Self {
        Self::from_raw_with_leftover(inner, Role::Server, config, leftover)
    }

    /// Create a client-side WebSocket stream.
    pub fn client(inner: S, config: Config) -> Self {
        Self::from_raw(inner, Role::Client, config)
    }

    /// Create a client-side stream with post-handshake leftover bytes.
    pub fn client_with_leftover(inner: S, config: Config, leftover: Option<Bytes>) -> Self {
        Self::from_raw_with_leftover(inner, Role::Client, config, leftover)
    }

    /// Get a reference to the underlying stream.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// Get a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    /// Consume this WebSocket stream and return the underlying stream.
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Check whether the WebSocket is closed.
    pub fn is_closed(&self) -> bool {
        self.state == CompioStreamState::Closed
    }

    /// Check whether the write buffer is above the high water mark.
    pub fn is_backpressured(&self) -> bool {
        self.write_buf.len() > self.high_water_mark
    }

    /// Check whether the write buffer is below the low water mark.
    pub fn is_write_buffer_low(&self) -> bool {
        self.write_buf.len() <= self.low_water_mark
    }

    /// Get the pending write buffer length.
    pub fn write_buffer_len(&self) -> usize {
        self.write_buf.len()
    }

    /// Get the pending read buffer length.
    pub fn read_buffer_len(&self) -> usize {
        self.read_buf.len()
    }

    /// Set the high water mark for backpressure.
    pub fn set_high_water_mark(&mut self, size: usize) {
        self.high_water_mark = size;
    }

    /// Set the low water mark for backpressure.
    pub fn set_low_water_mark(&mut self, size: usize) {
        self.low_water_mark = size;
    }

    /// Get the high water mark for backpressure.
    pub fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }

    /// Get the low water mark for backpressure.
    pub fn low_water_mark(&self) -> usize {
        self.low_water_mark
    }

    /// Receive the next WebSocket message.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        loop {
            if self.state == CompioStreamState::Closed {
                return None;
            }

            if let Some(msg) = self.next_pending_message() {
                return Some(self.handle_incoming_message(msg).await);
            }

            match self.process_read_buf() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => return Some(Err(e)),
            }

            match read_more(&mut self.inner, &mut self.read_buf).await {
                Ok(0) => {
                    self.state = CompioStreamState::Closed;
                    return None;
                }
                Ok(_) => {}
                Err(e) => return Some(Err(e.into())),
            }
        }
    }

    /// Send a WebSocket message.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        if self.state == CompioStreamState::Closed {
            return Err(Error::ConnectionClosed);
        }

        if msg.is_close() {
            self.state = CompioStreamState::CloseSent;
        }

        self.protocol.encode_message(&msg, &mut self.write_buf)?;
        self.flush().await
    }

    /// Send a text message.
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.send(Message::text(text)).await
    }

    /// Send a binary message.
    pub async fn send_binary(&mut self, data: Bytes) -> Result<()> {
        self.send(Message::Binary(data)).await
    }

    /// Send a close frame.
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        if self.state != CompioStreamState::Open {
            return Ok(());
        }

        self.send(Message::Close(Some(CloseReason::new(code, reason))))
            .await
    }

    /// Flush pending writes to the underlying Compio stream.
    pub async fn flush(&mut self) -> Result<()> {
        flush_bytes(&mut self.inner, &mut self.write_buf).await
    }

    fn process_read_buf(&mut self) -> Result<bool> {
        if self.read_buf.is_empty() {
            return Ok(false);
        }

        let messages = self.protocol.process(&mut self.read_buf)?;
        if messages.is_empty() {
            Ok(false)
        } else {
            self.pending_messages = messages;
            self.pending_index = 0;
            Ok(true)
        }
    }

    fn next_pending_message(&mut self) -> Option<Message> {
        if self.pending_index < self.pending_messages.len() {
            let msg = self.pending_messages[self.pending_index].clone();
            self.pending_index += 1;
            if self.pending_index >= self.pending_messages.len() {
                self.pending_messages.clear();
                self.pending_index = 0;
            }
            Some(msg)
        } else {
            None
        }
    }

    async fn handle_incoming_message(&mut self, msg: Message) -> Result<Message> {
        match &msg {
            Message::Ping(data) => {
                self.protocol.encode_pong(data, &mut self.write_buf);
                self.flush().await?;
            }
            Message::Close(reason) => {
                if self.state == CompioStreamState::Open {
                    self.protocol.encode_close_response(&mut self.write_buf);
                    self.flush().await?;
                }
                self.state = CompioStreamState::Closed;
                return Ok(Message::Close(reason.clone()));
            }
            _ => {}
        }

        Ok(msg)
    }
}

impl<S> CompioWebSocketStream<S>
where
    S: Splittable,
    S::ReadHalf: AsyncRead,
    S::WriteHalf: AsyncWrite,
{
    /// Split the WebSocket stream into independent Compio read and write halves.
    pub fn split(
        self,
    ) -> (
        CompioSplitReader<S::ReadHalf>,
        CompioSplitWriter<S::WriteHalf>,
    ) {
        let (reader, writer) = Splittable::split(self.inner);
        let (control_tx, control_rx) = mpsc::channel();

        let reader_protocol = Protocol::new(
            self.protocol.role,
            self.config.max_frame_size,
            self.config.max_message_size,
        );

        (
            CompioSplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                pending_messages: self.pending_messages,
                pending_index: self.pending_index,
                control_tx,
                closed: self.state == CompioStreamState::Closed,
            },
            CompioSplitWriter {
                writer,
                protocol: self.protocol,
                write_buf: BytesMut::with_capacity(self.config.write_buffer_size),
                control_rx,
                closed: self.state == CompioStreamState::Closed,
            },
        )
    }
}

/// Builder for native Compio WebSocket streams.
pub struct CompioWebSocketStreamBuilder {
    config: Config,
    role: Role,
    high_water_mark: usize,
    low_water_mark: usize,
}

impl CompioWebSocketStreamBuilder {
    /// Create a new Compio stream builder.
    pub fn new() -> Self {
        Self {
            config: Config::default(),
            role: Role::Server,
            high_water_mark: DEFAULT_HIGH_WATER_MARK,
            low_water_mark: DEFAULT_LOW_WATER_MARK,
        }
    }

    /// Set the endpoint role.
    pub fn role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    /// Set the maximum message size.
    pub fn max_message_size(mut self, size: usize) -> Self {
        self.config.max_message_size = size;
        self
    }

    /// Set the maximum frame size.
    pub fn max_frame_size(mut self, size: usize) -> Self {
        self.config.max_frame_size = size;
        self
    }

    /// Set the write buffer size.
    pub fn write_buffer_size(mut self, size: usize) -> Self {
        self.config.write_buffer_size = size;
        self
    }

    /// Set the high water mark.
    pub fn high_water_mark(mut self, size: usize) -> Self {
        self.high_water_mark = size;
        self
    }

    /// Set the low water mark.
    pub fn low_water_mark(mut self, size: usize) -> Self {
        self.low_water_mark = size;
        self
    }

    /// Build a Compio WebSocket stream.
    pub fn build<S>(self, stream: S) -> CompioWebSocketStream<S>
    where
        S: AsyncRead + AsyncWrite,
    {
        let mut ws = CompioWebSocketStream::from_raw(stream, self.role, self.config);
        ws.high_water_mark = self.high_water_mark;
        ws.low_water_mark = self.low_water_mark;
        ws
    }
}

impl Default for CompioWebSocketStreamBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Read half of a split Compio WebSocket stream.
pub struct CompioSplitReader<R> {
    reader: R,
    protocol: Protocol,
    read_buf: BytesMut,
    pending_messages: Vec<Message>,
    pending_index: usize,
    control_tx: mpsc::Sender<ControlRequest>,
    closed: bool,
}

/// Write half of a split Compio WebSocket stream.
pub struct CompioSplitWriter<W> {
    writer: W,
    protocol: Protocol,
    write_buf: BytesMut,
    control_rx: mpsc::Receiver<ControlRequest>,
    closed: bool,
}

impl<R> CompioSplitReader<R>
where
    R: AsyncRead,
{
    /// Receive the next non-control message.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        loop {
            if self.closed {
                return None;
            }

            if self.pending_index < self.pending_messages.len() {
                let msg = self.pending_messages[self.pending_index].clone();
                self.pending_index += 1;

                if self.pending_index >= self.pending_messages.len() {
                    self.pending_messages.clear();
                    self.pending_index = 0;
                }

                match &msg {
                    Message::Ping(data) => {
                        let _ = self.control_tx.send(ControlRequest::Pong(data.clone()));
                        continue;
                    }
                    Message::Close(reason) => {
                        if !self.closed {
                            let _ = self.control_tx.send(ControlRequest::CloseResponse);
                            self.closed = true;
                        }
                        return Some(Ok(Message::Close(reason.clone())));
                    }
                    Message::Pong(_) => continue,
                    _ => return Some(Ok(msg)),
                }
            }

            if !self.read_buf.is_empty() {
                match self.protocol.process(&mut self.read_buf) {
                    Ok(messages) if !messages.is_empty() => {
                        self.pending_messages = messages;
                        self.pending_index = 0;
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => return Some(Err(e)),
                }
            }

            match read_more(&mut self.reader, &mut self.read_buf).await {
                Ok(0) => {
                    self.closed = true;
                    return None;
                }
                Ok(_) => {}
                Err(e) => return Some(Err(e.into())),
            }
        }
    }

    /// Check whether the reader is closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl<W> CompioSplitWriter<W>
where
    W: AsyncWrite,
{
    /// Send a WebSocket message.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        if self.closed {
            return Err(Error::ConnectionClosed);
        }

        self.process_control_requests().await?;

        if msg.is_close() {
            self.closed = true;
        }

        self.protocol.encode_message(&msg, &mut self.write_buf)?;
        self.flush().await
    }

    /// Send a text message.
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.send(Message::text(text)).await
    }

    /// Send a binary message.
    pub async fn send_binary(&mut self, data: Bytes) -> Result<()> {
        self.send(Message::Binary(data)).await
    }

    /// Send a close frame.
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        self.send(Message::Close(Some(CloseReason::new(code, reason))))
            .await
    }

    /// Flush pending data and control responses.
    pub async fn flush(&mut self) -> Result<()> {
        self.process_control_requests().await?;
        flush_bytes(&mut self.writer, &mut self.write_buf).await
    }

    /// Check whether the writer is closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    async fn process_control_requests(&mut self) -> Result<()> {
        while let Ok(req) = self.control_rx.try_recv() {
            match req {
                ControlRequest::Pong(data) => {
                    self.protocol.encode_pong(&data, &mut self.write_buf);
                }
                ControlRequest::CloseResponse => {
                    self.protocol.encode_close_response(&mut self.write_buf);
                    self.closed = true;
                }
            }
        }

        if !self.write_buf.is_empty() {
            flush_bytes(&mut self.writer, &mut self.write_buf).await?;
        }

        Ok(())
    }
}

// ============================================================================
// permessage-deflate support
// ============================================================================

#[cfg(feature = "permessage-deflate")]
use crate::deflate::DeflateConfig;
#[cfg(feature = "permessage-deflate")]
use crate::protocol::{CompressedProtocol, CompressedReaderProtocol, CompressedWriterProtocol};

/// A compressed WebSocket stream over a native Compio transport.
#[cfg(feature = "permessage-deflate")]
pub struct CompioCompressedWebSocketStream<S> {
    inner: S,
    protocol: CompressedProtocol,
    read_buf: BytesMut,
    write_buf: BytesMut,
    state: CompioStreamState,
    config: Config,
    pending_messages: Vec<Message>,
    pending_index: usize,
    high_water_mark: usize,
    low_water_mark: usize,
}

#[cfg(feature = "permessage-deflate")]
impl<S> CompioCompressedWebSocketStream<S>
where
    S: AsyncRead + AsyncWrite,
{
    /// Create a server-side compressed WebSocket stream.
    pub fn server(inner: S, config: Config, deflate_config: DeflateConfig) -> Self {
        Self::server_with_leftover(inner, config, deflate_config, None)
    }

    /// Create a server-side compressed stream with post-handshake leftover bytes.
    pub fn server_with_leftover(
        inner: S,
        config: Config,
        deflate_config: DeflateConfig,
        leftover: Option<Bytes>,
    ) -> Self {
        let mut read_buf = BytesMut::with_capacity(crate::RECV_BUFFER_SIZE);
        if let Some(leftover) = leftover {
            read_buf.extend_from_slice(&leftover);
        }

        Self {
            inner,
            protocol: CompressedProtocol::server(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            ),
            read_buf,
            write_buf: BytesMut::with_capacity(config.write_buffer_size),
            state: CompioStreamState::Open,
            config,
            pending_messages: Vec::new(),
            pending_index: 0,
            high_water_mark: DEFAULT_HIGH_WATER_MARK,
            low_water_mark: DEFAULT_LOW_WATER_MARK,
        }
    }

    /// Create a client-side compressed WebSocket stream.
    pub fn client(inner: S, config: Config, deflate_config: DeflateConfig) -> Self {
        Self::client_with_leftover(inner, config, deflate_config, None)
    }

    /// Create a client-side compressed stream with post-handshake leftover bytes.
    pub fn client_with_leftover(
        inner: S,
        config: Config,
        deflate_config: DeflateConfig,
        leftover: Option<Bytes>,
    ) -> Self {
        let mut read_buf = BytesMut::with_capacity(crate::RECV_BUFFER_SIZE);
        if let Some(leftover) = leftover {
            read_buf.extend_from_slice(&leftover);
        }

        Self {
            inner,
            protocol: CompressedProtocol::client(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            ),
            read_buf,
            write_buf: BytesMut::with_capacity(config.write_buffer_size),
            state: CompioStreamState::Open,
            config,
            pending_messages: Vec::new(),
            pending_index: 0,
            high_water_mark: DEFAULT_HIGH_WATER_MARK,
            low_water_mark: DEFAULT_LOW_WATER_MARK,
        }
    }

    /// Receive the next WebSocket message.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        loop {
            if self.state == CompioStreamState::Closed {
                return None;
            }

            if let Some(msg) = self.next_pending_message() {
                return Some(self.handle_incoming_message(msg).await);
            }

            match self.process_read_buf() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => return Some(Err(e)),
            }

            match read_more(&mut self.inner, &mut self.read_buf).await {
                Ok(0) => {
                    self.state = CompioStreamState::Closed;
                    return None;
                }
                Ok(_) => {}
                Err(e) => return Some(Err(e.into())),
            }
        }
    }

    /// Send a WebSocket message.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        if self.state == CompioStreamState::Closed {
            return Err(Error::ConnectionClosed);
        }

        if msg.is_close() {
            self.state = CompioStreamState::CloseSent;
        }

        self.protocol.encode_message(&msg, &mut self.write_buf)?;
        self.flush().await
    }

    /// Send a text message.
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.send(Message::text(text)).await
    }

    /// Send a binary message.
    pub async fn send_binary(&mut self, data: Bytes) -> Result<()> {
        self.send(Message::Binary(data)).await
    }

    /// Send a close frame.
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        if self.state != CompioStreamState::Open {
            return Ok(());
        }

        self.send(Message::Close(Some(CloseReason::new(code, reason))))
            .await
    }

    /// Flush pending writes.
    pub async fn flush(&mut self) -> Result<()> {
        flush_bytes(&mut self.inner, &mut self.write_buf).await
    }

    /// Check whether the stream is closed.
    pub fn is_closed(&self) -> bool {
        self.state == CompioStreamState::Closed || self.protocol.is_closed()
    }

    /// Check whether the write buffer is above the high water mark.
    pub fn is_backpressured(&self) -> bool {
        self.write_buf.len() > self.high_water_mark
    }

    /// Get the pending write buffer length.
    pub fn write_buffer_len(&self) -> usize {
        self.write_buf.len()
    }

    /// Set the high water mark for backpressure.
    pub fn set_high_water_mark(&mut self, size: usize) {
        self.high_water_mark = size;
    }

    /// Set the low water mark for backpressure.
    pub fn set_low_water_mark(&mut self, size: usize) {
        self.low_water_mark = size;
    }

    fn process_read_buf(&mut self) -> Result<bool> {
        if self.read_buf.is_empty() {
            return Ok(false);
        }

        let messages = self.protocol.process(&mut self.read_buf)?;
        if messages.is_empty() {
            Ok(false)
        } else {
            self.pending_messages = messages;
            self.pending_index = 0;
            Ok(true)
        }
    }

    fn next_pending_message(&mut self) -> Option<Message> {
        if self.pending_index < self.pending_messages.len() {
            let msg = self.pending_messages[self.pending_index].clone();
            self.pending_index += 1;
            if self.pending_index >= self.pending_messages.len() {
                self.pending_messages.clear();
                self.pending_index = 0;
            }
            Some(msg)
        } else {
            None
        }
    }

    async fn handle_incoming_message(&mut self, msg: Message) -> Result<Message> {
        match &msg {
            Message::Ping(data) => {
                self.protocol.encode_pong(data, &mut self.write_buf);
                self.flush().await?;
            }
            Message::Close(reason) => {
                if self.state == CompioStreamState::Open {
                    self.protocol.encode_close_response(&mut self.write_buf);
                    self.flush().await?;
                }
                self.state = CompioStreamState::Closed;
                return Ok(Message::Close(reason.clone()));
            }
            _ => {}
        }

        Ok(msg)
    }
}

#[cfg(feature = "permessage-deflate")]
impl<S> CompioCompressedWebSocketStream<S>
where
    S: Splittable,
    S::ReadHalf: AsyncRead,
    S::WriteHalf: AsyncWrite,
{
    /// Split the compressed WebSocket stream into Compio read and write halves.
    pub fn split(
        self,
    ) -> (
        CompioCompressedSplitReader<S::ReadHalf>,
        CompioCompressedSplitWriter<S::WriteHalf>,
    ) {
        let (reader, writer) = Splittable::split(self.inner);
        let (control_tx, control_rx) = mpsc::channel();
        let (reader_protocol, writer_protocol) = self
            .protocol
            .split(self.config.max_frame_size, self.config.max_message_size);

        (
            CompioCompressedSplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                pending_messages: self.pending_messages,
                pending_index: self.pending_index,
                control_tx,
                closed: self.state == CompioStreamState::Closed,
            },
            CompioCompressedSplitWriter {
                writer,
                protocol: writer_protocol,
                write_buf: BytesMut::with_capacity(self.config.write_buffer_size),
                control_rx,
                closed: self.state == CompioStreamState::Closed,
            },
        )
    }
}

/// Read half of a split compressed Compio WebSocket stream.
#[cfg(feature = "permessage-deflate")]
pub struct CompioCompressedSplitReader<R> {
    reader: R,
    protocol: CompressedReaderProtocol,
    read_buf: BytesMut,
    pending_messages: Vec<Message>,
    pending_index: usize,
    control_tx: mpsc::Sender<ControlRequest>,
    closed: bool,
}

/// Write half of a split compressed Compio WebSocket stream.
#[cfg(feature = "permessage-deflate")]
pub struct CompioCompressedSplitWriter<W> {
    writer: W,
    protocol: CompressedWriterProtocol,
    write_buf: BytesMut,
    control_rx: mpsc::Receiver<ControlRequest>,
    closed: bool,
}

#[cfg(feature = "permessage-deflate")]
impl<R> CompioCompressedSplitReader<R>
where
    R: AsyncRead,
{
    /// Receive the next non-control message.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        loop {
            if self.closed {
                return None;
            }

            if self.pending_index < self.pending_messages.len() {
                let msg = self.pending_messages[self.pending_index].clone();
                self.pending_index += 1;

                if self.pending_index >= self.pending_messages.len() {
                    self.pending_messages.clear();
                    self.pending_index = 0;
                }

                match &msg {
                    Message::Ping(data) => {
                        let _ = self.control_tx.send(ControlRequest::Pong(data.clone()));
                        continue;
                    }
                    Message::Close(reason) => {
                        if !self.closed {
                            let _ = self.control_tx.send(ControlRequest::CloseResponse);
                            self.closed = true;
                        }
                        return Some(Ok(Message::Close(reason.clone())));
                    }
                    Message::Pong(_) => continue,
                    _ => return Some(Ok(msg)),
                }
            }

            if !self.read_buf.is_empty() {
                match self.protocol.process(&mut self.read_buf) {
                    Ok(messages) if !messages.is_empty() => {
                        self.pending_messages = messages;
                        self.pending_index = 0;
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => return Some(Err(e)),
                }
            }

            match read_more(&mut self.reader, &mut self.read_buf).await {
                Ok(0) => {
                    self.closed = true;
                    return None;
                }
                Ok(_) => {}
                Err(e) => return Some(Err(e.into())),
            }
        }
    }

    /// Check whether the reader is closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

#[cfg(feature = "permessage-deflate")]
impl<W> CompioCompressedSplitWriter<W>
where
    W: AsyncWrite,
{
    /// Send a WebSocket message.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        if self.closed {
            return Err(Error::ConnectionClosed);
        }

        self.process_control_requests().await?;

        if msg.is_close() {
            self.closed = true;
        }

        self.protocol.encode_message(&msg, &mut self.write_buf)?;
        self.flush().await
    }

    /// Send a text message.
    pub async fn send_text(&mut self, text: impl Into<String>) -> Result<()> {
        self.send(Message::text(text)).await
    }

    /// Send a binary message.
    pub async fn send_binary(&mut self, data: Bytes) -> Result<()> {
        self.send(Message::Binary(data)).await
    }

    /// Send a close frame.
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        self.send(Message::Close(Some(CloseReason::new(code, reason))))
            .await
    }

    /// Flush pending data and control responses.
    pub async fn flush(&mut self) -> Result<()> {
        self.process_control_requests().await?;
        flush_bytes(&mut self.writer, &mut self.write_buf).await
    }

    /// Check whether the writer is closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    async fn process_control_requests(&mut self) -> Result<()> {
        while let Ok(req) = self.control_rx.try_recv() {
            match req {
                ControlRequest::Pong(data) => {
                    self.protocol.encode_pong(&data, &mut self.write_buf);
                }
                ControlRequest::CloseResponse => {
                    self.protocol.encode_close_response(&mut self.write_buf);
                    self.closed = true;
                }
            }
        }

        if !self.write_buf.is_empty() {
            flush_bytes(&mut self.writer, &mut self.write_buf).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::compio::net::{TcpListener, TcpStream};

    #[cfg(feature = "http3")]
    fn install_test_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[compio::test]
    async fn compio_handshake_and_echo_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = ::compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut ws, handshake) = accept_async(stream, Config::default()).await.unwrap();
            assert_eq!(handshake.path, "/chat");

            let msg = ws.next().await.unwrap().unwrap();
            assert!(matches!(&msg, Message::Text(text) if text == "hello"));
            ws.send(msg).await.unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut client, handshake) =
            connect_async(stream, &addr.to_string(), "/chat", None, Config::default())
                .await
                .unwrap();

        assert_eq!(handshake.path, "/chat");
        client.send_text("hello").await.unwrap();

        let echoed = client.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "hello"));

        server.await.unwrap();
    }

    #[compio::test]
    async fn compio_split_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = ::compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (ws, _) = accept_async(stream, Config::default()).await.unwrap();
            let (mut reader, mut writer) = ws.split();

            let msg = reader.next().await.unwrap().unwrap();
            assert!(matches!(&msg, Message::Binary(bytes) if bytes.as_ref() == b"payload"));
            writer.send(msg).await.unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut client, _) =
            connect_async(stream, &addr.to_string(), "/", None, Config::default())
                .await
                .unwrap();

        client
            .send_binary(Bytes::from_static(b"payload"))
            .await
            .unwrap();

        let echoed = client.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Binary(bytes) if bytes.as_ref() == b"payload"));

        server.await.unwrap();
    }

    #[cfg(feature = "http2")]
    #[compio::test]
    async fn compio_http2_echo_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = ::compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_http2(stream, Config::default(), |mut ws, req| async move {
                assert_eq!(req.path, "/h2");
                let msg = ws.next().await.unwrap().unwrap();
                assert!(matches!(&msg, Message::Text(text) if text == "h2"));
                ws.send(msg).await.unwrap();
            })
            .await
            .unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = connect_http2(
            stream,
            &format!("https://{}/h2", addr),
            None,
            Config::default(),
        )
        .await
        .unwrap();

        client.send_text("h2").await.unwrap();
        let echoed = client.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "h2"));
        drop(client);

        server.await.unwrap();
    }

    #[cfg(feature = "http2")]
    #[compio::test]
    async fn compio_http2_multiplexed_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = ::compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_http2(stream, Config::default(), |mut ws, req| async move {
                assert!(matches!(req.path.as_str(), "/one" | "/two"));
                let msg = ws.next().await.unwrap().unwrap();
                ws.send(msg).await.unwrap();
            })
            .await
            .unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut mux = connect_http2_multiplexed(stream, Config::default())
            .await
            .unwrap();

        let mut one = mux
            .open_websocket(&format!("https://{}/one", addr), None)
            .await
            .unwrap();
        let mut two = mux
            .open_websocket(&format!("https://{}/two", addr), None)
            .await
            .unwrap();

        one.send_text("one").await.unwrap();
        two.send_text("two").await.unwrap();

        let echoed = one.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "one"));
        let echoed = two.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "two"));

        drop(one);
        drop(two);
        drop(mux);
        server.await.unwrap();
    }

    #[cfg(feature = "http3")]
    #[compio::test]
    async fn compio_http3_echo_round_trip() {
        install_test_crypto_provider();

        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

        let server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();

        let endpoint = ::compio::quic::ServerBuilder::new_with_rustls_server_config(server_tls)
            .with_alpn_protocols(&["h3"])
            .bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = endpoint.local_addr().unwrap();
        let server = CompioHttp3Server::from_endpoint(endpoint.clone(), Config::default());

        let server_task = ::compio::runtime::spawn(async move {
            server
                .serve(|mut ws, req| async move {
                    assert_eq!(req.path, "/h3");
                    let msg = ws.next().await.unwrap().unwrap();
                    assert!(matches!(&msg, Message::Text(text) if text == "h3"));
                    ws.send(msg).await.unwrap();
                })
                .await
                .unwrap();
        });

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        let mut client = connect_http3(
            addr,
            "localhost",
            "/h3",
            None,
            client_tls,
            Config::default(),
        )
        .await
        .unwrap();

        client.send_text("h3").await.unwrap();
        let echoed = client.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "h3"));

        endpoint.close(::compio::quic::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }

    #[cfg(feature = "http3")]
    #[compio::test]
    async fn compio_http3_multiplexed_round_trip() {
        install_test_crypto_provider();

        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

        let server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();

        let endpoint = ::compio::quic::ServerBuilder::new_with_rustls_server_config(server_tls)
            .with_alpn_protocols(&["h3"])
            .bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = endpoint.local_addr().unwrap();
        let server = CompioHttp3Server::from_endpoint(endpoint.clone(), Config::default());

        let server_task = ::compio::runtime::spawn(async move {
            server
                .serve(|mut ws, req| async move {
                    assert!(matches!(req.path.as_str(), "/one" | "/two"));
                    let msg = ws.next().await.unwrap().unwrap();
                    ws.send(msg).await.unwrap();
                })
                .await
                .unwrap();
        });

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        let mut mux = connect_http3_multiplexed(addr, "localhost", client_tls, Config::default())
            .await
            .unwrap();

        let mut one = mux.open_websocket("/one", None).await.unwrap();
        let mut two = mux.open_websocket("/two", None).await.unwrap();

        one.send_text("one").await.unwrap();
        two.send_text("two").await.unwrap();

        let echoed = one.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "one"));
        let echoed = two.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "two"));

        drop(one);
        drop(two);
        mux.close();
        endpoint.close(::compio::quic::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }

    #[cfg(feature = "permessage-deflate")]
    #[compio::test]
    async fn compio_compressed_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = ::compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = CompioCompressedWebSocketStream::server(
                stream,
                Config::default(),
                DeflateConfig::default(),
            );

            let msg = ws.next().await.unwrap().unwrap();
            assert!(matches!(&msg, Message::Text(text) if text == "compressed"));
            ws.send(msg).await.unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = CompioCompressedWebSocketStream::client(
            stream,
            Config::default(),
            DeflateConfig::default(),
        );

        client.send_text("compressed").await.unwrap();

        let echoed = client.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "compressed"));

        server.await.unwrap();
    }
}
