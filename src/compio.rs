//! Native Compio runtime support.
//!
//! Compio uses completion-based I/O traits, which are intentionally different
//! from Tokio's poll-based `AsyncRead` and `AsyncWrite`. This module exposes a
//! native async-method API for Compio streams instead of adapting through Tokio.

use std::cell::Cell;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
#[cfg(feature = "http2")]
use std::pin::Pin;
use std::rc::Rc;
#[cfg(feature = "http3")]
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(any(feature = "http2", feature = "http3"))]
use ::compio::buf::IoBufMut;
use ::compio::buf::{BufResult, IoBuf};
use ::compio::driver::ErrorExt;
use ::compio::io::util::Splittable;
use ::compio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use ::compio::runtime::{CancelToken, FutureExt as CompioFutureExt};
#[cfg(any(feature = "http2", feature = "http3"))]
use bytes::Buf;
use bytes::{Bytes, BytesMut};
use futures_channel::{mpsc, oneshot};
use futures_util::{FutureExt, SinkExt, StreamExt};

use crate::Config;
use crate::error::{CloseReason, Error, Result};
use crate::handshake::{
    HandshakeResult, HandshakeSelection, build_request_with_headers, build_response_inner,
    generate_accept_key, generate_key, parse_request, parse_response, select_subprotocol,
    validate_accept_key, validate_selected_protocol, validate_server_selection,
    validate_supported_protocols,
};
use crate::heartbeat::{Deadline, Heartbeat, bounded_close_reason};
use crate::protocol::{Message, Protocol, Role};

#[cfg(any(feature = "http2", feature = "http3"))]
use crate::extended_connect::{
    ExtendedConnectRequest, build_extended_connect_error, build_extended_connect_response,
    validate_extended_connect_response,
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

const SPLIT_CONTROL_CAPACITY: usize = 32;
const SPLIT_APPLICATION_CAPACITY: usize = 32;
const SPLIT_OPEN: u8 = 0;
const SPLIT_CLOSING: u8 = 1;
const SPLIT_CLOSED: u8 = 2;

#[derive(Debug, Clone)]
enum ControlRequest {
    Pong(Bytes, Instant),
    PeerPing(Bytes, Instant),
    PeerClose,
}

#[derive(Debug)]
enum ApplicationRequest {
    Send(Message, oneshot::Sender<Result<()>>),
    Flush(oneshot::Sender<Result<()>>),
}

enum CompioReadOutcome {
    Read(io::Result<usize>),
    Terminal,
}

#[derive(Debug, Clone, Copy)]
enum CompioTerminalCause {
    ConnectionClosed,
    HeartbeatTimeout,
    IdleTimeout,
}

impl CompioTerminalCause {
    fn from_error(error: &Error) -> Self {
        match error {
            Error::HeartbeatTimeout => Self::HeartbeatTimeout,
            Error::IdleTimeout => Self::IdleTimeout,
            _ => Self::ConnectionClosed,
        }
    }

    fn error(self) -> Error {
        match self {
            Self::ConnectionClosed => Error::ConnectionClosed,
            Self::HeartbeatTimeout => Error::HeartbeatTimeout,
            Self::IdleTimeout => Error::IdleTimeout,
        }
    }
}

struct CompioSplitShared {
    /// Clock epoch shared by the reader and the writer driver
    clock_epoch: Instant,
    track_activity: bool,
    /// Milliseconds since `clock_epoch` of the last inbound data frame (reader -> driver)
    last_data_ms: Cell<u64>,
    status: Cell<u8>,
    terminal: Cell<Option<CompioTerminalCause>>,
}

impl CompioSplitShared {
    fn new(closed: bool, config: &Config) -> Rc<Self> {
        Rc::new(Self {
            clock_epoch: Instant::now(),
            track_activity: (config.auto_ping && config.ping_interval != 0)
                || config.idle_timeout != 0,
            last_data_ms: Cell::new(0),
            status: Cell::new(if closed { SPLIT_CLOSED } else { SPLIT_OPEN }),
            terminal: Cell::new(closed.then_some(CompioTerminalCause::ConnectionClosed)),
        })
    }

    fn is_open(&self) -> bool {
        self.status.get() == SPLIT_OPEN
    }

    fn record_data_activity(&self) {
        if self.track_activity {
            self.last_data_ms
                .set(self.clock_epoch.elapsed().as_millis() as u64);
        }
    }

    fn begin_closing(&self) {
        if self.is_open() {
            self.status.set(SPLIT_CLOSING);
        }
    }

    fn terminate(&self, cause: CompioTerminalCause) {
        if self.status.replace(SPLIT_CLOSED) != SPLIT_CLOSED {
            self.terminal.set(Some(cause));
        }
    }

    fn error(&self) -> Error {
        self.terminal
            .get()
            .unwrap_or(CompioTerminalCause::ConnectionClosed)
            .error()
    }

    fn read_terminal(&self) -> Option<Result<Message>> {
        match self.error() {
            Error::ConnectionClosed => None,
            error => Some(Err(error)),
        }
    }
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

#[derive(Debug)]
struct PollReadCancelled;

impl std::fmt::Display for PollReadCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("poll-based read cancelled")
    }
}

impl std::error::Error for PollReadCancelled {}

fn poll_read_cancelled() -> io::Error {
    io::Error::other(PollReadCancelled)
}

fn read_was_cancelled(result: &io::Result<usize>) -> bool {
    result.is_cancelled()
        || result.as_ref().is_err_and(|error| {
            error
                .get_ref()
                .is_some_and(|source| source.is::<PollReadCancelled>())
        })
}

#[cfg(any(feature = "http2", feature = "http3"))]
async fn poll_read_until_cancelled<F>(read: F) -> Option<F::Output>
where
    F: Future,
{
    let Some(cancel) = CancelToken::current().await else {
        return Some(read.await);
    };
    let read = read.fuse();
    let cancelled = cancel.wait().fuse();
    futures_util::pin_mut!(read, cancelled);

    // Prefer data that became ready at the deadline. This preserves the
    // fail-slow behavior used by native Compio reads.
    futures_util::select_biased! {
        result = read => Some(result),
        () = cancelled => None,
    }
}

enum DeadlineReadOutcome {
    Read(io::Result<usize>),
    Deadline(Deadline, Option<io::Result<usize>>),
}

async fn read_more_until<R>(
    reader: &mut R,
    buf: &mut BytesMut,
    deadline: Deadline,
    hard_timeout: Option<Deadline>,
    epoch: Instant,
) -> DeadlineReadOutcome
where
    R: AsyncRead + ?Sized,
{
    let cancel = CancelToken::new();
    let read = CompioFutureExt::with_cancel(read_more(reader, buf), cancel.clone()).fuse();
    let delay = Duration::from_millis(
        deadline
            .at()
            .saturating_sub(epoch.elapsed().as_millis() as u64),
    );
    let timer = ::compio::time::sleep(delay).fuse();
    futures_util::pin_mut!(read, timer);

    futures_util::select! {
        result = read => DeadlineReadOutcome::Read(result),
        () = timer => {
            cancel.cancel();
            if !matches!(deadline, Deadline::Ping(_)) {
                // The connection is terminal; it will never reuse this buffer.
                // A custom reader must not postpone a hard timeout indefinitely.
                return DeadlineReadOutcome::Deadline(deadline, None);
            }
            // Continuing after Ping requires the owned buffer back. Native Compio
            // reads and our poll-based adapters cooperate with the cancellation.
            // A hard idle/Pong deadline still bounds a non-cooperative reader.
            let hard_timer = async {
                let Some(at) = hard_timeout else {
                    return std::future::pending::<Deadline>().await;
                };
                ::compio::time::sleep(Duration::from_millis(
                    at.at().saturating_sub(epoch.elapsed().as_millis() as u64)
                )).await;
                at
            }.fuse();
            futures_util::pin_mut!(hard_timer);
            futures_util::select_biased! {
                at = hard_timer => DeadlineReadOutcome::Deadline(at, None),
                result = read => {
                    let result = (!read_was_cancelled(&result)).then_some(result);
                    DeadlineReadOutcome::Deadline(deadline, result)
                },
            }
        }
    }
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

trait CompioSplitEncoder: 'static {
    fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()>;
    fn encode_pong(&mut self, payload: &[u8], buf: &mut BytesMut);
    fn encode_close_response(&mut self, buf: &mut BytesMut);
}

impl CompioSplitEncoder for Protocol {
    fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()> {
        Protocol::encode_message(self, msg, buf)
    }

    fn encode_pong(&mut self, payload: &[u8], buf: &mut BytesMut) {
        Protocol::encode_pong(self, payload, buf);
    }

    fn encode_close_response(&mut self, buf: &mut BytesMut) {
        Protocol::encode_close_response(self, buf);
    }
}

/// Perform a server-side WebSocket handshake over a Compio stream.
pub async fn server_handshake<S>(stream: &mut S) -> Result<HandshakeResult>
where
    S: AsyncRead + AsyncWrite + ?Sized,
{
    server_handshake_with(stream, |_| Ok(HandshakeSelection::default())).await
}

/// Perform a server-side WebSocket handshake and include an extension response.
///
/// This is useful when the caller has already negotiated extensions such as
/// `permessage-deflate` and needs the `Sec-WebSocket-Extensions` response
/// header to be sent during upgrade. The extension must have been offered by
/// the client. No subprotocol is selected; use [`server_handshake_with`] when
/// request-aware selection is required.
#[deprecated(note = "use server_handshake_with for request-aware extension negotiation")]
pub async fn server_handshake_with_extensions<S>(
    stream: &mut S,
    response_extensions: Option<&str>,
) -> Result<HandshakeResult>
where
    S: AsyncRead + AsyncWrite + ?Sized,
{
    let extensions = response_extensions.map(str::to_owned);
    server_handshake_with(stream, move |_| {
        Ok(HandshakeSelection {
            protocol: None,
            extensions,
        })
    })
    .await
}

/// Perform a server-side WebSocket handshake with request-aware selection.
///
/// The callback receives the validated request before a response is written.
/// Its selected protocol and extension names must have been offered by the
/// client.
pub async fn server_handshake_with<S, F>(stream: &mut S, select: F) -> Result<HandshakeResult>
where
    S: AsyncRead + AsyncWrite + ?Sized,
    F: FnOnce(&crate::handshake::HandshakeRequest<'_>) -> Result<HandshakeSelection>,
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
            let selection = select(&req)?;
            validate_server_selection(&req, &selection)?;
            let path = req.path.to_string();
            let accept_key = generate_accept_key(req.key);
            let response = build_response_inner(
                &accept_key,
                selection.protocol.as_deref(),
                selection.extensions.as_deref(),
            );

            write_all_owned(stream, response).await?;
            stream.flush().await?;

            let leftover = if consumed < buf.len() {
                Some(buf.split_off(consumed).freeze())
            } else {
                None
            };

            return Ok(HandshakeResult {
                path,
                protocol: selection.protocol,
                extensions: selection.extensions,
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
    client_handshake_with_headers(stream, host, path, protocol, None).await
}

/// Perform a client-side handshake with additional HTTP headers.
///
/// Header names and values are validated before any bytes are written. Headers
/// managed by the WebSocket handshake cannot be supplied through
/// `extra_headers`.
///
/// Once writing begins, cancelling this future leaves the stream in an
/// indeterminate handshake state and the stream should not be reused.
pub async fn client_handshake_with_headers<S>(
    stream: &mut S,
    host: &str,
    path: &str,
    protocol: Option<&str>,
    extra_headers: Option<&[(String, String)]>,
) -> Result<HandshakeResult>
where
    S: AsyncRead + AsyncWrite + ?Sized,
{
    let key = generate_key();
    let request = build_request_with_headers(host, path, &key, protocol, None, extra_headers)?;

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
            validate_selected_protocol(protocol, res.protocol)?;
            if res.extensions.is_some() {
                return Err(Error::HandshakeFailed(
                    "server returned an unoffered extension",
                ));
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
    stream: S,
    config: Config,
) -> Result<(CompioWebSocketStream<S>, HandshakeResult)>
where
    S: AsyncRead + AsyncWrite,
{
    accept_async_with_protocols(stream, config, std::iter::empty::<String>()).await
}

/// Accept a Compio transport using supported subprotocols in server preference order.
pub async fn accept_async_with_protocols<S, I, P>(
    mut stream: S,
    config: Config,
    protocols: I,
) -> Result<(CompioWebSocketStream<S>, HandshakeResult)>
where
    S: AsyncRead + AsyncWrite,
    I: IntoIterator<Item = P>,
    P: Into<String>,
{
    let protocols = protocols.into_iter().map(Into::into).collect::<Vec<_>>();
    validate_supported_protocols(&protocols)?;
    let handshake = server_handshake_with(&mut stream, |request| {
        Ok(HandshakeSelection {
            protocol: select_subprotocol(request.protocol.as_deref(), &protocols)
                .map(str::to_owned),
            extensions: None,
        })
    })
    .await?;
    let ws =
        CompioWebSocketStream::server_with_leftover(stream, config, handshake.leftover.clone());
    Ok((ws, handshake))
}

/// Connect an already-connected Compio transport as a client WebSocket.
pub async fn connect_async<S>(
    stream: S,
    host: &str,
    path: &str,
    protocol: Option<&str>,
    config: Config,
) -> Result<(CompioWebSocketStream<S>, HandshakeResult)>
where
    S: AsyncRead + AsyncWrite,
{
    connect_async_with_headers(stream, host, path, protocol, None, config).await
}

/// Connect an already-connected Compio transport with additional HTTP headers.
///
/// Headers managed by the HTTP upgrade handshake cannot be overridden.
/// Once writing begins, cancelling this future leaves the stream in an
/// indeterminate handshake state and the stream should not be reused.
pub async fn connect_async_with_headers<S>(
    mut stream: S,
    host: &str,
    path: &str,
    protocol: Option<&str>,
    extra_headers: Option<&[(String, String)]>,
    config: Config,
) -> Result<(CompioWebSocketStream<S>, HandshakeResult)>
where
    S: AsyncRead + AsyncWrite,
{
    let handshake =
        client_handshake_with_headers(&mut stream, host, path, protocol, extra_headers).await?;
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
impl Drop for CompioHttp2Stream {
    fn drop(&mut self) {
        // Close the send half before h2 drops the last stream references. Otherwise,
        // h2 may reset the stream and discard DATA still queued by the handler.
        let _ = self.send.send_data(Bytes::new(), true);
    }
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

        let read = std::future::poll_fn(|cx| Pin::new(&mut self.recv).poll_data(cx));
        let Some(result) = poll_read_until_cancelled(read).await else {
            return BufResult(Err(poll_read_cancelled()), buf);
        };

        match result {
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
        validate_extended_connect_response(response.headers(), protocol)?;

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
    S: Splittable + 'static,
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
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
    S: Splittable + 'static,
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
{
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let stream = Box::pin(::compio::io::compat::AsyncStream::new(stream)).compat();
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
    S: Splittable + 'static,
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
    F: Fn(CompioWebSocketStream<CompioHttp2Stream>, ExtendedConnectRequest) -> Fut
        + Clone
        + 'static,
    Fut: Future<Output = ()> + 'static,
{
    serve_http2_with_protocols(stream, config, std::iter::empty::<String>(), handler).await
}

/// Serve HTTP/2 WebSocket streams with supported subprotocols in server preference order.
#[cfg(feature = "http2")]
pub async fn serve_http2_with_protocols<S, F, Fut, I, P>(
    stream: S,
    config: Config,
    protocols: I,
    handler: F,
) -> Result<()>
where
    S: Splittable + 'static,
    S::ReadHalf: AsyncRead + Unpin,
    S::WriteHalf: AsyncWrite + Unpin,
    F: Fn(CompioWebSocketStream<CompioHttp2Stream>, ExtendedConnectRequest) -> Fut
        + Clone
        + 'static,
    Fut: Future<Output = ()> + 'static,
    I: IntoIterator<Item = P>,
    P: Into<String>,
{
    use tokio_util::compat::FuturesAsyncReadCompatExt;

    let protocols = protocols.into_iter().map(Into::into).collect::<Vec<_>>();
    validate_supported_protocols(&protocols)?;
    let protocols: Rc<[String]> = protocols.into();
    let stream = Box::pin(::compio::io::compat::AsyncStream::new(stream)).compat();
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
        let protocols = protocols.clone();

        runtime::spawn(async move {
            if let Err(e) = handle_http2_request(request, respond, handler, config, protocols).await
            {
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
    protocols: Rc<[String]>,
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

        let selected_protocol = select_subprotocol(ws_req.subprotocols.as_deref(), &protocols);
        let response = build_extended_connect_response(selected_protocol, None);
        ws_req.selected_subprotocol = selected_protocol.map(str::to_owned);
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
            let Some(result) = poll_read_until_cancelled(self.stream.recv_data()).await else {
                return BufResult(Err(poll_read_cancelled()), buf);
            };
            match result {
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
            let Some(result) = poll_read_until_cancelled(self.stream.recv_data()).await else {
                return BufResult(Err(poll_read_cancelled()), buf);
            };
            match result {
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
        if !self.config.http3.enable_connect_protocol {
            return Err(Error::ExtendedConnectNotSupported);
        }

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

        match response.status() {
            http::StatusCode::OK => {}
            http::StatusCode::NOT_IMPLEMENTED => {
                return Err(Error::ExtendedConnectNotSupported);
            }
            _ => return Err(Error::HandshakeFailed("server rejected WebSocket upgrade")),
        }
        validate_extended_connect_response(response.headers(), protocol)?;

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
    mut tls_config: rustls::ClientConfig,
    config: Config,
) -> Result<CompioHttp3Connection> {
    crate::http3::validate_config(&config.http3)?;
    if !config.http3.enable_connect_protocol {
        return Err(Error::ExtendedConnectNotSupported);
    }
    let transport_config = crate::http3::quic_transport_config(&config.http3)?;
    let endpoint_config = crate::http3::quic_endpoint_config(&config.http3)?;
    // Do not let caller-provided TLS settings bypass the 0-RTT rejection above.
    tls_config.enable_early_data = false;
    tls_config.alpn_protocols = vec![b"h3".to_vec()];

    let bind_ip = if server_addr.is_ipv6() {
        std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
    } else {
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
    };

    let quic_config = ::compio::quic::crypto::rustls::QuicClientConfig::try_from(tls_config)
        .map_err(|_| Error::HandshakeFailed("invalid TLS config"))?;
    let mut client_config = ::compio::quic::ClientConfig::new(Arc::new(quic_config));
    client_config.transport_config(transport_config);
    let socket = ::compio::net::UdpSocket::bind(std::net::SocketAddr::new(bind_ip, 0))
        .await
        .map_err(Error::Io)?;
    let endpoint =
        ::compio::quic::Endpoint::new(socket, endpoint_config, None, Some(client_config))
            .map_err(Error::Io)?;

    let conn = endpoint
        .connect(server_addr, server_name, None)
        .map_err(|e| Error::Http3(e.to_string()))?
        .await
        .map_err(|e| Error::Http3(e.to_string()))?;

    let mut builder = ::compio::quic::h3::client::builder();
    builder.enable_extended_connect(config.http3.enable_connect_protocol);
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
    protocols: Rc<[String]>,
}

#[cfg(feature = "http3")]
impl CompioHttp3Server {
    /// Bind a Compio HTTP/3 WebSocket server.
    pub async fn bind(
        addr: std::net::SocketAddr,
        mut tls_config: rustls::ServerConfig,
        config: Config,
    ) -> Result<Self> {
        let transport_config = crate::http3::quic_transport_config(&config.http3)?;
        let endpoint_config = crate::http3::quic_endpoint_config(&config.http3)?;
        // Do not let caller-provided TLS settings bypass the 0-RTT rejection above.
        tls_config.max_early_data_size = 0;
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        let quic_config = ::compio::quic::crypto::rustls::QuicServerConfig::try_from(tls_config)
            .map_err(|_| Error::HandshakeFailed("invalid TLS config"))?;
        let mut server_config = ::compio::quic::ServerConfig::with_crypto(Arc::new(quic_config));
        server_config.transport_config(transport_config);
        let socket = ::compio::net::UdpSocket::bind(addr)
            .await
            .map_err(Error::Io)?;
        let endpoint =
            ::compio::quic::Endpoint::new(socket, endpoint_config, Some(server_config), None)
                .map_err(Error::Io)?;

        Ok(Self {
            endpoint,
            config,
            protocols: Rc::default(),
        })
    }

    /// Build a server from an existing Compio QUIC endpoint.
    ///
    /// Transport and TLS settings come from the supplied endpoint. HTTP/3
    /// protocol settings still come from the WebSocket configuration.
    pub fn from_endpoint(endpoint: ::compio::quic::Endpoint, config: Config) -> Self {
        Self {
            endpoint,
            config,
            protocols: Rc::default(),
        }
    }

    /// Set supported WebSocket subprotocols in server preference order.
    pub fn protocols<I, P>(mut self, protocols: I) -> Result<Self>
    where
        I: IntoIterator<Item = P>,
        P: Into<String>,
    {
        let protocols = protocols.into_iter().map(Into::into).collect::<Vec<_>>();
        validate_supported_protocols(&protocols)?;
        self.protocols = protocols.into();
        Ok(self)
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
        crate::http3::validate_config(&self.config.http3)?;
        while let Some(incoming) = self.endpoint.wait_incoming().await {
            let handler = handler.clone();
            let config = self.config.clone();
            let protocols = self.protocols.clone();

            runtime::spawn(async move {
                if let Err(e) = handle_http3_connection(incoming, handler, config, protocols).await
                {
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
    protocols: Rc<[String]>,
) -> Result<()>
where
    F: Fn(CompioWebSocketStream<CompioHttp3ServerStream>, ExtendedConnectRequest) -> Fut
        + Clone
        + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let conn = incoming.await.map_err(|e| Error::Http3(e.to_string()))?;
    let mut builder = ::compio::quic::h3::server::builder();
    let enable_connect_protocol = config.http3.enable_connect_protocol;
    builder
        .enable_extended_connect(enable_connect_protocol)
        .enable_webtransport(enable_connect_protocol)
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
        let protocols = protocols.clone();

        runtime::spawn(async move {
            if let Err(e) = handle_http3_request(request, stream, handler, config, protocols).await
            {
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
    protocols: Rc<[String]>,
) -> Result<()>
where
    F: Fn(CompioWebSocketStream<CompioHttp3ServerStream>, ExtendedConnectRequest) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    if !config.http3.enable_connect_protocol {
        let response = build_extended_connect_error(
            http::StatusCode::NOT_IMPLEMENTED,
            Some("Extended CONNECT is disabled"),
        );
        stream.send_response(response).await.ok();
        return Ok(());
    }

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

    let selected_protocol = select_subprotocol(ws_req.subprotocols.as_deref(), &protocols);
    let response = build_extended_connect_response(selected_protocol, None);
    stream.send_response(response).await.map_err(Error::from)?;
    ws_req.selected_subprotocol = selected_protocol.map(str::to_owned);

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
    // Deliver accepted messages before a later parse failure.
    pending_terminal_error: Option<Error>,
    clock_epoch: Instant,
    heartbeat: Heartbeat,
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

        let clock_epoch = Instant::now();
        let heartbeat = Heartbeat::new(&config, 0);
        Self {
            inner,
            protocol: Protocol::new(role, config.max_frame_size, config.max_message_size),
            read_buf,
            write_buf: BytesMut::with_capacity(config.write_buffer_size),
            state: CompioStreamState::Open,
            config,
            pending_messages: Vec::new(),
            pending_index: 0,
            pending_terminal_error: None,
            clock_epoch,
            heartbeat,
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
    ///
    /// With automatic Ping enabled, custom `AsyncRead` implementations must
    /// cooperate with Compio's current `CancelToken` so a pending read can return
    /// its owned buffer before Ping is sent. The built-in transports do this.
    /// Hard idle/Pong timeouts terminate without waiting for buffer recovery.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        loop {
            if self.state == CompioStreamState::Closed {
                return None;
            }

            if let Some(msg) = self.next_pending_message() {
                let now = self.clock_epoch.elapsed().as_millis() as u64;
                let pong = match &msg {
                    Message::Pong(payload) => Some(payload),
                    _ => None,
                };
                self.heartbeat.on_inbound(now, pong);
                return Some(self.handle_incoming_message(msg).await);
            }

            if let Some(error) = self.pending_terminal_error.take() {
                self.heartbeat.stop();
                self.state = CompioStreamState::Closed;
                return Some(Err(error));
            }

            match self.process_read_buf() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    self.pending_index = 0;
                    self.pending_terminal_error = Some(error);
                    continue;
                }
            }

            let read_result = if let Some(deadline) = self.heartbeat.next_deadline() {
                match read_more_until(
                    &mut self.inner,
                    &mut self.read_buf,
                    deadline,
                    self.heartbeat.next_timeout(),
                    self.clock_epoch,
                )
                .await
                {
                    DeadlineReadOutcome::Read(result) => Some(result),
                    DeadlineReadOutcome::Deadline(deadline, read_result) => {
                        let now = self.clock_epoch.elapsed().as_millis() as u64;
                        match deadline {
                            Deadline::Ping(at) if at <= now => {
                                if let Some(payload) = self.heartbeat.ping_due(now) {
                                    if let Err(error) = self.protocol.encode_message(
                                        &Message::Ping(payload),
                                        &mut self.write_buf,
                                    ) {
                                        return Some(Err(error));
                                    }
                                    if let Err(error) = self.flush().await {
                                        return Some(Err(error));
                                    }
                                    self.heartbeat.ping_flushed(
                                        self.clock_epoch.elapsed().as_millis() as u64,
                                    );
                                }
                                read_result
                            }
                            Deadline::Pong(at) if at <= now => {
                                self.heartbeat.stop();
                                self.state = CompioStreamState::CloseSent;
                                let close = Message::Close(Some(CloseReason::new(
                                    self.config.pong_timeout_close_code,
                                    bounded_close_reason(&self.config.pong_timeout_close_reason),
                                )));
                                let _ = self.protocol.encode_message(&close, &mut self.write_buf);
                                let _ = ::compio::time::timeout(
                                    Duration::from_secs(self.config.close_timeout.into()),
                                    self.flush(),
                                )
                                .await;
                                self.state = CompioStreamState::Closed;
                                return Some(Err(Error::HeartbeatTimeout));
                            }
                            Deadline::Idle(at) if at <= now => {
                                self.heartbeat.stop();
                                self.state = CompioStreamState::CloseSent;
                                let close = Message::Close(Some(CloseReason::new(
                                    CloseReason::GOING_AWAY,
                                    "Connection idle timeout",
                                )));
                                let _ = self.protocol.encode_message(&close, &mut self.write_buf);
                                let _ = ::compio::time::timeout(
                                    Duration::from_secs(self.config.close_timeout.into()),
                                    self.flush(),
                                )
                                .await;
                                self.state = CompioStreamState::Closed;
                                return Some(Err(Error::IdleTimeout));
                            }
                            _ => read_result,
                        }
                    }
                }
            } else {
                Some(read_more(&mut self.inner, &mut self.read_buf).await)
            };

            let Some(read_result) = read_result else {
                continue;
            };
            match read_result {
                Ok(0) => {
                    self.heartbeat.stop();
                    self.state = CompioStreamState::Closed;
                    return None;
                }
                Ok(_) => {}
                Err(e) => {
                    self.heartbeat.stop();
                    self.state = CompioStreamState::Closed;
                    return Some(Err(e.into()));
                }
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
            self.heartbeat.stop();
        }

        self.protocol.encode_message(&msg, &mut self.write_buf)?;
        if self.write_buf.len() > self.config.max_backpressure {
            self.write_buf.clear();
            self.heartbeat.stop();
            self.state = CompioStreamState::Closed;
            return Err(Error::BufferFull);
        }
        if let Err(error) = self.flush().await {
            self.heartbeat.stop();
            self.state = CompioStreamState::Closed;
            return Err(error);
        }
        Ok(())
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

        if self
            .protocol
            .process_into_with_activity(&mut self.read_buf, &mut self.pending_messages)?
            && self.heartbeat.tracks_activity()
        {
            self.heartbeat
                .on_inbound(self.clock_epoch.elapsed().as_millis() as u64, None);
        }
        self.pending_index = 0;
        Ok(!self.pending_messages.is_empty())
    }

    fn next_pending_message(&mut self) -> Option<Message> {
        if self.pending_index < self.pending_messages.len() {
            // Move the message out; the consumed slot is never returned again.
            let msg = std::mem::replace(
                &mut self.pending_messages[self.pending_index],
                Message::Close(None),
            );
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
                if let Err(error) = self.flush().await {
                    self.heartbeat.stop();
                    self.state = CompioStreamState::Closed;
                    return Err(error);
                }
            }
            Message::Close(reason) => {
                self.heartbeat.stop();
                self.pending_messages.clear();
                self.pending_index = 0;
                self.pending_terminal_error = None;
                self.read_buf.clear();
                if self.state == CompioStreamState::Open {
                    self.protocol.encode_close_response(&mut self.write_buf);
                    if let Err(error) = self.flush().await {
                        self.state = CompioStreamState::Closed;
                        return Err(error);
                    }
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
    S::WriteHalf: AsyncWrite + 'static,
{
    /// Split the WebSocket stream into independent Compio read and write halves.
    ///
    /// Idle/Pong deadlines cancel blocked writes and release the driver's writer
    /// before reporting the timeout. Compio may retain native I/O operations and
    /// reader references, so this does not guarantee immediate transport closure.
    pub fn split(
        self,
    ) -> (
        CompioSplitReader<S::ReadHalf>,
        CompioSplitWriter<S::WriteHalf>,
    ) {
        let (reader, writer) = Splittable::split(self.inner);
        let (control_tx, control_rx) = mpsc::channel(SPLIT_CONTROL_CAPACITY);
        let (application_tx, application_rx) = mpsc::channel(SPLIT_APPLICATION_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::unbounded();
        let (terminal_tx, terminal_rx) = mpsc::unbounded();
        let shared = CompioSplitShared::new(self.state != CompioStreamState::Open, &self.config);

        let writer_protocol = Protocol::new(
            self.protocol.role,
            self.config.max_frame_size,
            self.config.max_message_size,
        );
        let reader_protocol = self.protocol;

        ::compio::runtime::spawn(compio_split_writer_driver(
            writer,
            writer_protocol,
            self.config,
            CompioDriverChannels {
                control_rx,
                application_rx,
                cancel_rx,
                terminal_tx,
                shared: shared.clone(),
            },
        ))
        .detach();

        (
            CompioSplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                pending_messages: self.pending_messages,
                pending_index: self.pending_index,
                pending_terminal_error: self.pending_terminal_error,
                read_budget: SPLIT_CONTROL_CAPACITY,
                control_tx,
                terminal_rx,
                cancel_tx: cancel_tx.clone(),
                shared: shared.clone(),
                terminal_reported: false,
            },
            CompioSplitWriter {
                application_tx,
                cancel_tx,
                shared,
                _writer: PhantomData,
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
    // Deliver accepted messages before a later parse failure.
    pending_terminal_error: Option<Error>,
    read_budget: usize,
    control_tx: mpsc::Sender<ControlRequest>,
    terminal_rx: mpsc::UnboundedReceiver<()>,
    cancel_tx: mpsc::UnboundedSender<()>,
    shared: Rc<CompioSplitShared>,
    terminal_reported: bool,
}

/// Write half of a split Compio WebSocket stream.
///
/// Pending sends and flushes enforce idle/Pong deadlines while the connection
/// is open. On expiry, the driver cancels the write and releases its writer
/// before completing pending requests.
pub struct CompioSplitWriter<W> {
    application_tx: mpsc::Sender<ApplicationRequest>,
    cancel_tx: mpsc::UnboundedSender<()>,
    shared: Rc<CompioSplitShared>,
    _writer: PhantomData<fn() -> W>,
}

impl<R> CompioSplitReader<R>
where
    R: AsyncRead,
{
    /// Receive the next message, including Ping and Pong control frames.
    ///
    /// Buffered ordinary messages bypass the writer's control queue. Cooperative
    /// yields preserve them; waiting on the control queue is not cancellation-safe.
    /// Fatal read/parse errors and EOF return without waiting for the driver;
    /// they also queue cancellation of pending writes.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        compio_consume_read_budget(&mut self.read_budget).await;
        loop {
            if self.pending_terminal_error.is_none() {
                if self.terminal_reported {
                    return None;
                }
                if self.shared.status.get() == SPLIT_CLOSED {
                    self.terminal_reported = true;
                    return self.shared.read_terminal();
                }
            }

            if self.pending_index < self.pending_messages.len() {
                // Move the message out; the consumed slot is never returned again.
                let msg = std::mem::replace(
                    &mut self.pending_messages[self.pending_index],
                    Message::Close(None),
                );
                self.pending_index += 1;

                if self.pending_index >= self.pending_messages.len() {
                    self.pending_messages.clear();
                    self.pending_index = 0;
                }

                let request = match &msg {
                    Message::Ping(data) => ControlRequest::PeerPing(data.clone(), Instant::now()),
                    Message::Pong(data) => ControlRequest::Pong(data.clone(), Instant::now()),
                    Message::Close(_) => {
                        self.shared.begin_closing();
                        self.pending_messages.clear();
                        self.pending_index = 0;
                        self.pending_terminal_error = None;
                        self.read_buf.clear();
                        self.terminal_reported = true;
                        ControlRequest::PeerClose
                    }
                    _ => {
                        // Data frames refresh activity without a channel round trip.
                        self.shared.record_data_activity();
                        return Some(Ok(msg));
                    }
                };
                if self.control_tx.send(request).await.is_err() {
                    self.terminal_reported = true;
                    return self.shared.read_terminal();
                }
                return Some(Ok(msg));
            }

            if let Some(error) = self.pending_terminal_error.take() {
                compio_cancel_read(&self.shared, &self.cancel_tx);
                self.terminal_reported = true;
                return Some(Err(error));
            }

            if !self.read_buf.is_empty() {
                match self
                    .protocol
                    .process_into_with_activity(&mut self.read_buf, &mut self.pending_messages)
                {
                    Ok(fragment_activity) => {
                        if fragment_activity {
                            self.shared.record_data_activity();
                        }
                        self.pending_index = 0;
                        if !self.pending_messages.is_empty() {
                            continue;
                        }
                    }
                    Err(error) => {
                        self.pending_index = 0;
                        self.pending_terminal_error = Some(error);
                        continue;
                    }
                }
            }

            let outcome = {
                let read = read_more(&mut self.reader, &mut self.read_buf).fuse();
                let terminal = self.terminal_rx.next().fuse();
                futures_util::pin_mut!(read, terminal);
                futures_util::select_biased! {
                    _ = terminal => CompioReadOutcome::Terminal,
                    result = read => CompioReadOutcome::Read(result),
                }
            };
            match outcome {
                CompioReadOutcome::Read(Ok(0)) => {
                    compio_cancel_read(&self.shared, &self.cancel_tx);
                    self.terminal_reported = true;
                    return None;
                }
                CompioReadOutcome::Read(Ok(_)) => {}
                CompioReadOutcome::Read(Err(error)) => {
                    compio_cancel_read(&self.shared, &self.cancel_tx);
                    self.terminal_reported = true;
                    return Some(Err(error.into()));
                }
                CompioReadOutcome::Terminal => {
                    self.terminal_reported = true;
                    return self.shared.read_terminal();
                }
            }
        }
    }

    /// Check whether the reader is closed.
    pub fn is_closed(&self) -> bool {
        !self.shared.is_open()
    }
}

impl<R> Drop for CompioSplitReader<R> {
    fn drop(&mut self) {
        let _ = self.cancel_tx.unbounded_send(());
    }
}

fn compio_cancel_read(shared: &CompioSplitShared, cancel_tx: &mpsc::UnboundedSender<()>) {
    // Cancel independently of the control queue so a blocked write cannot hold
    // up EOF or a fatal read error. The driver releases its writer asynchronously.
    shared.begin_closing();
    let _ = cancel_tx.unbounded_send(());
}

impl<W> CompioSplitWriter<W> {
    /// Send a WebSocket message.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        let (tx, rx) = oneshot::channel();
        self.application_tx
            .send(ApplicationRequest::Send(msg, tx))
            .await
            .map_err(|_| self.current_error())?;
        rx.await.map_err(|_| self.current_error())?
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
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        let (tx, rx) = oneshot::channel();
        self.application_tx
            .send(ApplicationRequest::Flush(tx))
            .await
            .map_err(|_| self.current_error())?;
        rx.await.map_err(|_| self.current_error())?
    }

    /// Check whether the writer is closed.
    pub fn is_closed(&self) -> bool {
        !self.shared.is_open()
    }

    fn current_error(&self) -> Error {
        self.shared.error()
    }
}

impl<W> Drop for CompioSplitWriter<W> {
    fn drop(&mut self) {
        let _ = self.cancel_tx.unbounded_send(());
    }
}

// Keep the former control queue's bounded read burst without waiting on the
// writer. Compio has no task-budget API, so this budget belongs to each reader.
async fn compio_consume_read_budget(budget: &mut usize) {
    if *budget == 0 {
        // Reset before yielding: cancellation must not repeatedly stall the same
        // message when the caller creates another next() future.
        *budget = SPLIT_CONTROL_CAPACITY;
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                std::task::Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        })
        .await;
    }
    *budget -= 1;
}

enum CompioDriverWake {
    Cancel,
    Control(Option<ControlRequest>),
    Application(Option<ApplicationRequest>),
    Timer,
}

struct CompioDriverChannels {
    control_rx: mpsc::Receiver<ControlRequest>,
    application_rx: mpsc::Receiver<ApplicationRequest>,
    cancel_rx: mpsc::UnboundedReceiver<()>,
    terminal_tx: mpsc::UnboundedSender<()>,
    shared: Rc<CompioSplitShared>,
}

async fn compio_split_writer_driver<W, E>(
    mut writer: W,
    mut encoder: E,
    config: Config,
    channels: CompioDriverChannels,
) where
    W: AsyncWrite,
    E: CompioSplitEncoder,
{
    let CompioDriverChannels {
        mut control_rx,
        mut application_rx,
        mut cancel_rx,
        terminal_tx,
        shared,
    } = channels;
    let epoch = shared.clock_epoch;
    let mut heartbeat = Heartbeat::new(&config, epoch.elapsed().as_millis() as u64);
    let mut closing = CompioClosing {
        deadline: None,
        timeout: Duration::from_secs(config.close_timeout.into()),
    };
    let mut local_close_sent = false;
    let mut write_buf = BytesMut::with_capacity(config.write_buffer_size);
    let mut deferred_control = None;
    let mut failed_request = None;

    let cause = loop {
        if shared.status.get() == SPLIT_CLOSED {
            break CompioTerminalCause::ConnectionClosed;
        }
        // Pick up data-frame activity published by the reader without a channel.
        heartbeat.on_inbound(shared.last_data_ms.get(), None);
        let now_ms = epoch.elapsed().as_millis() as u64;
        let heartbeat_deadline = heartbeat.next_deadline();
        let heartbeat_delay = heartbeat_deadline
            .map(|deadline| Duration::from_millis(deadline.at().saturating_sub(now_ms)))
            .unwrap_or(Duration::from_secs(365 * 24 * 60 * 60));
        let close_delay = closing
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(365 * 24 * 60 * 60));
        let timer_delay = heartbeat_delay.min(close_delay);

        let wake = {
            let cancel = cancel_rx.next().fuse();
            let control = async {
                match deferred_control.take() {
                    Some(request) => Some(request),
                    None => control_rx.next().await,
                }
            }
            .fuse();
            let application = application_rx.next().fuse();
            let sleep = ::compio::time::sleep(timer_delay);
            futures_util::pin_mut!(sleep);
            // Check the clock on every poll, including when the runtime has not
            // dispatched the timer yet but application requests are already ready.
            let timer = std::future::poll_fn(|cx| {
                if closing.deadline.is_some_and(|at| Instant::now() >= at)
                    || heartbeat_deadline
                        .is_some_and(|deadline| epoch.elapsed().as_millis() as u64 >= deadline.at())
                {
                    std::task::Poll::Ready(())
                } else {
                    sleep.as_mut().poll(cx)
                }
            })
            .fuse();
            futures_util::pin_mut!(cancel, control, application, timer);
            futures_util::select_biased! {
                _ = cancel => CompioDriverWake::Cancel,
                request = control => CompioDriverWake::Control(request),
                // Visible control events retain priority so timely Pong receipts
                // are processed before deciding whether their deadline expired.
                _ = timer => CompioDriverWake::Timer,
                request = application => CompioDriverWake::Application(request),
            }
        };

        match wake {
            CompioDriverWake::Cancel
            | CompioDriverWake::Control(None)
            | CompioDriverWake::Application(None) => break CompioTerminalCause::ConnectionClosed,
            CompioDriverWake::Control(Some(request)) => match request {
                ControlRequest::Pong(payload, received_at) => {
                    let received_ms =
                        received_at.saturating_duration_since(epoch).as_millis() as u64;
                    heartbeat.on_inbound(received_ms, Some(&payload));
                }
                ControlRequest::PeerPing(payload, received_at) => {
                    let received_ms =
                        received_at.saturating_duration_since(epoch).as_millis() as u64;
                    heartbeat.on_inbound(received_ms, None);
                    write_buf.clear();
                    encoder.encode_pong(&payload, &mut write_buf);
                    if let Err(error) = await_compio_write(
                        flush_bytes(&mut writer, &mut write_buf),
                        &mut heartbeat,
                        &mut control_rx,
                        &mut deferred_control,
                        &mut cancel_rx,
                        &shared,
                        &mut closing,
                    )
                    .await
                    {
                        break CompioTerminalCause::from_error(&error);
                    }
                }
                ControlRequest::PeerClose => {
                    heartbeat.stop();
                    let deadline = closing.begin();
                    if !local_close_sent {
                        write_buf.clear();
                        encoder.encode_close_response(&mut write_buf);
                        let _ = ::compio::time::timeout(
                            deadline.saturating_duration_since(Instant::now()),
                            flush_bytes(&mut writer, &mut write_buf),
                        )
                        .await;
                    }
                    break CompioTerminalCause::ConnectionClosed;
                }
            },
            CompioDriverWake::Application(Some(request)) => match request {
                ApplicationRequest::Send(message, completion) => {
                    if !shared.is_open() {
                        let _ = completion.send(Err(Error::ConnectionClosed));
                        continue;
                    }
                    let is_close = message.is_close();
                    if is_close {
                        shared.begin_closing();
                        heartbeat.stop();
                        local_close_sent = true;
                        closing.begin();
                    }
                    write_buf.clear();
                    let result = match encoder.encode_message(&message, &mut write_buf) {
                        Ok(()) if write_buf.len() > config.max_backpressure => {
                            Err(Error::BufferFull)
                        }
                        Ok(()) => {
                            await_compio_write(
                                flush_bytes(&mut writer, &mut write_buf),
                                &mut heartbeat,
                                &mut control_rx,
                                &mut deferred_control,
                                &mut cancel_rx,
                                &shared,
                                &mut closing,
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    if let Err(error) = result {
                        let cause = CompioTerminalCause::from_error(&error);
                        failed_request = Some((completion, error));
                        break cause;
                    }
                    let _ = completion.send(Ok(()));
                }
                ApplicationRequest::Flush(completion) => {
                    write_buf.clear();
                    let result = await_compio_write(
                        flush_bytes(&mut writer, &mut write_buf),
                        &mut heartbeat,
                        &mut control_rx,
                        &mut deferred_control,
                        &mut cancel_rx,
                        &shared,
                        &mut closing,
                    )
                    .await;
                    if let Err(error) = result {
                        let cause = CompioTerminalCause::from_error(&error);
                        failed_request = Some((completion, error));
                        break cause;
                    }
                    let _ = completion.send(Ok(()));
                }
            },
            CompioDriverWake::Timer => {
                // Recheck activity recorded while the old timer was asleep.
                heartbeat.on_inbound(shared.last_data_ms.get(), None);
                if closing
                    .deadline
                    .is_some_and(|deadline| deadline <= Instant::now())
                {
                    break CompioTerminalCause::ConnectionClosed;
                }

                let now_ms = epoch.elapsed().as_millis() as u64;
                match heartbeat.next_deadline() {
                    Some(Deadline::Ping(at)) if at <= now_ms => {
                        if let Some(payload) = heartbeat.ping_due(now_ms) {
                            write_buf.clear();
                            if encoder
                                .encode_message(&Message::Ping(payload), &mut write_buf)
                                .is_err()
                            {
                                break CompioTerminalCause::ConnectionClosed;
                            }
                            if let Err(error) = await_compio_write(
                                flush_bytes(&mut writer, &mut write_buf),
                                &mut heartbeat,
                                &mut control_rx,
                                &mut deferred_control,
                                &mut cancel_rx,
                                &shared,
                                &mut closing,
                            )
                            .await
                            {
                                break CompioTerminalCause::from_error(&error);
                            }
                            heartbeat.ping_flushed(epoch.elapsed().as_millis() as u64);
                        }
                    }
                    Some(Deadline::Pong(at)) if at <= now_ms => {
                        compio_timeout_close(
                            &mut writer,
                            &mut encoder,
                            &config,
                            config.pong_timeout_close_code,
                            &config.pong_timeout_close_reason,
                        )
                        .await;
                        break CompioTerminalCause::HeartbeatTimeout;
                    }
                    Some(Deadline::Idle(at)) if at <= now_ms => {
                        compio_timeout_close(
                            &mut writer,
                            &mut encoder,
                            &config,
                            CloseReason::GOING_AWAY,
                            "Connection idle timeout",
                        )
                        .await;
                        break CompioTerminalCause::IdleTimeout;
                    }
                    _ => {}
                }
            }
        }
    };

    // A dropped Compio write future can leave a kernel operation alive. Release
    // the driver's writer before publishing its terminal result; native I/O and
    // retained reader references remain managed by Compio.
    shared.begin_closing();
    drop(writer);
    shared.terminate(cause);
    let _ = terminal_tx.unbounded_send(());
    if let Some((completion, error)) = failed_request {
        let _ = completion.send(Err(error));
    }
}

// One budget covers finishing the current frame and the Close response.
struct CompioClosing {
    deadline: Option<Instant>,
    timeout: Duration,
}

impl CompioClosing {
    fn begin(&mut self) -> Instant {
        *self
            .deadline
            .get_or_insert_with(|| Instant::now() + self.timeout)
    }
}

// Keep the same owned write alive across controls and timer wakes. Restarting
// write_all after a partial write would duplicate bytes in the current frame.
async fn await_compio_write(
    write: impl Future<Output = Result<()>>,
    heartbeat: &mut Heartbeat,
    control_rx: &mut mpsc::Receiver<ControlRequest>,
    deferred_control: &mut Option<ControlRequest>,
    cancel_rx: &mut mpsc::UnboundedReceiver<()>,
    shared: &CompioSplitShared,
    closing: &mut CompioClosing,
) -> Result<()> {
    let write = write.fuse();
    futures_util::pin_mut!(write);
    loop {
        heartbeat.on_inbound(shared.last_data_ms.get(), None);
        let deadline = heartbeat.next_timeout();
        let close_at = closing.deadline;
        let can_read_control = !matches!(deferred_control, Some(ControlRequest::PeerClose));
        let control = async {
            if can_read_control {
                control_rx.next().await
            } else {
                std::future::pending().await
            }
        }
        .fuse();
        let cancel = cancel_rx.next().fuse();
        let timer = async {
            let heartbeat_delay = deadline.map(|deadline| {
                Duration::from_millis(
                    deadline
                        .at()
                        .saturating_sub(shared.clock_epoch.elapsed().as_millis() as u64),
                )
            });
            let close_delay = close_at.map(|at| at.saturating_duration_since(Instant::now()));
            let delay = match (heartbeat_delay, close_delay) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) | (None, Some(a)) => a,
                (None, None) => return std::future::pending::<()>().await,
            };
            let sleep = ::compio::time::sleep(delay);
            futures_util::pin_mut!(sleep);
            std::future::poll_fn(|cx| {
                if close_at.is_some_and(|at| Instant::now() >= at)
                    || deadline.is_some_and(|deadline| {
                        shared.clock_epoch.elapsed().as_millis() as u64 >= deadline.at()
                    })
                {
                    std::task::Poll::Ready(())
                } else {
                    sleep.as_mut().poll(cx)
                }
            })
            .await;
        }
        .fuse();
        futures_util::pin_mut!(control, cancel, timer);
        futures_util::select_biased! {
            _ = cancel => return Err(Error::ConnectionClosed),
            request = control => match request {
                Some(ControlRequest::Pong(payload, received_at)) => {
                    let received_ms = received_at.saturating_duration_since(shared.clock_epoch).as_millis() as u64;
                    heartbeat.on_inbound(received_ms, Some(&payload));
                }
                Some(ControlRequest::PeerPing(payload, received_at)) => {
                    let received_ms = received_at.saturating_duration_since(shared.clock_epoch).as_millis() as u64;
                    heartbeat.on_inbound(received_ms, None);
                    // RFC 6455 §5.5.3 allows replying only to the latest queued
                    // Ping. Keep this bounded while a matching Pong can pass it.
                    *deferred_control = Some(ControlRequest::PeerPing(payload, received_at));
                }
                Some(ControlRequest::PeerClose) => {
                    // Complete the current frame before encoding a Close response,
                    // within the same closing budget as that response.
                    heartbeat.stop();
                    closing.begin();
                    *deferred_control = Some(ControlRequest::PeerClose);
                }
                None => return Err(Error::ConnectionClosed),
            },
            _ = timer => {
                if close_at.is_some_and(|at| Instant::now() >= at) {
                    return Err(Error::ConnectionClosed);
                }
                heartbeat.on_inbound(shared.last_data_ms.get(), None);
                let now_ms = shared.clock_epoch.elapsed().as_millis() as u64;
                match heartbeat.next_timeout() {
                    Some(Deadline::Pong(at)) if at <= now_ms => return Err(Error::HeartbeatTimeout),
                    Some(Deadline::Idle(at)) if at <= now_ms => return Err(Error::IdleTimeout),
                    _ => {}
                }
            },
            result = write => return result,
        }
    }
}

async fn compio_timeout_close<W, E>(
    writer: &mut W,
    encoder: &mut E,
    config: &Config,
    code: u16,
    reason: &str,
) where
    W: AsyncWrite,
    E: CompioSplitEncoder,
{
    let mut buf = BytesMut::with_capacity(128);
    let close = Message::Close(Some(CloseReason::new(code, bounded_close_reason(reason))));
    if encoder.encode_message(&close, &mut buf).is_ok() {
        let _ = ::compio::time::timeout(
            Duration::from_secs(config.close_timeout.into()),
            flush_bytes(writer, &mut buf),
        )
        .await;
    }
}

// ============================================================================
// permessage-deflate support
// ============================================================================

#[cfg(feature = "permessage-deflate")]
use crate::deflate::DeflateConfig;
#[cfg(feature = "permessage-deflate")]
use crate::protocol::{CompressedProtocol, CompressedReaderProtocol, CompressedWriterProtocol};

#[cfg(feature = "permessage-deflate")]
impl CompioSplitEncoder for CompressedWriterProtocol {
    fn encode_message(&mut self, msg: &Message, buf: &mut BytesMut) -> Result<()> {
        CompressedWriterProtocol::encode_message(self, msg, buf)
    }

    fn encode_pong(&mut self, payload: &[u8], buf: &mut BytesMut) {
        CompressedWriterProtocol::encode_pong(self, payload, buf);
    }

    fn encode_close_response(&mut self, buf: &mut BytesMut) {
        CompressedWriterProtocol::encode_close_response(self, buf);
    }
}

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
    // Deliver accepted messages before a later parse failure.
    pending_terminal_error: Option<Error>,
    clock_epoch: Instant,
    heartbeat: Heartbeat,
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

        let clock_epoch = Instant::now();
        let heartbeat = Heartbeat::new(&config, 0);
        let protocol = if config.compression.is_shared() {
            CompressedProtocol::server_with_shared_compression(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            )
        } else {
            CompressedProtocol::server(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            )
        };
        Self {
            inner,
            protocol,
            read_buf,
            write_buf: BytesMut::with_capacity(config.write_buffer_size),
            state: CompioStreamState::Open,
            config,
            pending_messages: Vec::new(),
            pending_index: 0,
            pending_terminal_error: None,
            clock_epoch,
            heartbeat,
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

        let clock_epoch = Instant::now();
        let heartbeat = Heartbeat::new(&config, 0);
        let protocol = if config.compression.is_shared() {
            CompressedProtocol::client_with_shared_compression(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            )
        } else {
            CompressedProtocol::client(
                config.max_frame_size,
                config.max_message_size,
                deflate_config,
            )
        };
        Self {
            inner,
            protocol,
            read_buf,
            write_buf: BytesMut::with_capacity(config.write_buffer_size),
            state: CompioStreamState::Open,
            config,
            pending_messages: Vec::new(),
            pending_index: 0,
            pending_terminal_error: None,
            clock_epoch,
            heartbeat,
            high_water_mark: DEFAULT_HIGH_WATER_MARK,
            low_water_mark: DEFAULT_LOW_WATER_MARK,
        }
    }

    /// Receive the next WebSocket message.
    ///
    /// With automatic Ping enabled, custom `AsyncRead` implementations must
    /// cooperate with Compio's current `CancelToken` so a pending read can return
    /// its owned buffer before Ping is sent. The built-in transports do this.
    /// Hard idle/Pong timeouts terminate without waiting for buffer recovery.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        loop {
            if self.state == CompioStreamState::Closed {
                return None;
            }

            if let Some(msg) = self.next_pending_message() {
                let now = self.clock_epoch.elapsed().as_millis() as u64;
                let pong = match &msg {
                    Message::Pong(payload) => Some(payload),
                    _ => None,
                };
                self.heartbeat.on_inbound(now, pong);
                return Some(self.handle_incoming_message(msg).await);
            }

            if let Some(error) = self.pending_terminal_error.take() {
                self.heartbeat.stop();
                self.state = CompioStreamState::Closed;
                return Some(Err(error));
            }

            match self.process_read_buf() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    self.pending_index = 0;
                    self.pending_terminal_error = Some(error);
                    continue;
                }
            }

            let read_result = if let Some(deadline) = self.heartbeat.next_deadline() {
                match read_more_until(
                    &mut self.inner,
                    &mut self.read_buf,
                    deadline,
                    self.heartbeat.next_timeout(),
                    self.clock_epoch,
                )
                .await
                {
                    DeadlineReadOutcome::Read(result) => Some(result),
                    DeadlineReadOutcome::Deadline(deadline, read_result) => {
                        let now = self.clock_epoch.elapsed().as_millis() as u64;
                        match deadline {
                            Deadline::Ping(at) if at <= now => {
                                if let Some(payload) = self.heartbeat.ping_due(now) {
                                    if let Err(error) = self.protocol.encode_message(
                                        &Message::Ping(payload),
                                        &mut self.write_buf,
                                    ) {
                                        return Some(Err(error));
                                    }
                                    if let Err(error) = self.flush().await {
                                        return Some(Err(error));
                                    }
                                    self.heartbeat.ping_flushed(
                                        self.clock_epoch.elapsed().as_millis() as u64,
                                    );
                                }
                                read_result
                            }
                            Deadline::Pong(at) if at <= now => {
                                self.heartbeat.stop();
                                self.state = CompioStreamState::CloseSent;
                                let close = Message::Close(Some(CloseReason::new(
                                    self.config.pong_timeout_close_code,
                                    bounded_close_reason(&self.config.pong_timeout_close_reason),
                                )));
                                let _ = self.protocol.encode_message(&close, &mut self.write_buf);
                                let _ = ::compio::time::timeout(
                                    Duration::from_secs(self.config.close_timeout.into()),
                                    self.flush(),
                                )
                                .await;
                                self.state = CompioStreamState::Closed;
                                return Some(Err(Error::HeartbeatTimeout));
                            }
                            Deadline::Idle(at) if at <= now => {
                                self.heartbeat.stop();
                                self.state = CompioStreamState::CloseSent;
                                let close = Message::Close(Some(CloseReason::new(
                                    CloseReason::GOING_AWAY,
                                    "Connection idle timeout",
                                )));
                                let _ = self.protocol.encode_message(&close, &mut self.write_buf);
                                let _ = ::compio::time::timeout(
                                    Duration::from_secs(self.config.close_timeout.into()),
                                    self.flush(),
                                )
                                .await;
                                self.state = CompioStreamState::Closed;
                                return Some(Err(Error::IdleTimeout));
                            }
                            _ => read_result,
                        }
                    }
                }
            } else {
                Some(read_more(&mut self.inner, &mut self.read_buf).await)
            };
            let Some(read_result) = read_result else {
                continue;
            };
            match read_result {
                Ok(0) => {
                    self.heartbeat.stop();
                    self.state = CompioStreamState::Closed;
                    return None;
                }
                Ok(_) => {}
                Err(e) => {
                    self.heartbeat.stop();
                    self.state = CompioStreamState::Closed;
                    return Some(Err(e.into()));
                }
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
            self.heartbeat.stop();
        }

        self.protocol.encode_message(&msg, &mut self.write_buf)?;
        if self.write_buf.len() > self.config.max_backpressure {
            self.write_buf.clear();
            self.heartbeat.stop();
            self.state = CompioStreamState::Closed;
            return Err(Error::BufferFull);
        }
        if let Err(error) = self.flush().await {
            self.heartbeat.stop();
            self.state = CompioStreamState::Closed;
            return Err(error);
        }
        Ok(())
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

        if self
            .protocol
            .process_into_with_activity(&mut self.read_buf, &mut self.pending_messages)?
            && self.heartbeat.tracks_activity()
        {
            self.heartbeat
                .on_inbound(self.clock_epoch.elapsed().as_millis() as u64, None);
        }
        self.pending_index = 0;
        Ok(!self.pending_messages.is_empty())
    }

    fn next_pending_message(&mut self) -> Option<Message> {
        if self.pending_index < self.pending_messages.len() {
            // Move the message out; the consumed slot is never returned again.
            let msg = std::mem::replace(
                &mut self.pending_messages[self.pending_index],
                Message::Close(None),
            );
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
                if let Err(error) = self.flush().await {
                    self.heartbeat.stop();
                    self.state = CompioStreamState::Closed;
                    return Err(error);
                }
            }
            Message::Close(reason) => {
                self.heartbeat.stop();
                self.pending_messages.clear();
                self.pending_index = 0;
                self.pending_terminal_error = None;
                self.read_buf.clear();
                if self.state == CompioStreamState::Open {
                    self.protocol.encode_close_response(&mut self.write_buf);
                    if let Err(error) = self.flush().await {
                        self.state = CompioStreamState::Closed;
                        return Err(error);
                    }
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
    S::WriteHalf: AsyncWrite + 'static,
{
    /// Split the compressed WebSocket stream into Compio read and write halves.
    ///
    /// Blocked writes enforce idle/Pong deadlines with the same cancellation
    /// and transport lifetime semantics as [`CompioWebSocketStream::split`].
    pub fn split(
        self,
    ) -> (
        CompioCompressedSplitReader<S::ReadHalf>,
        CompioCompressedSplitWriter<S::WriteHalf>,
    ) {
        let (reader, writer) = Splittable::split(self.inner);
        let (control_tx, control_rx) = mpsc::channel(SPLIT_CONTROL_CAPACITY);
        let (application_tx, application_rx) = mpsc::channel(SPLIT_APPLICATION_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::unbounded();
        let (terminal_tx, terminal_rx) = mpsc::unbounded();
        let shared = CompioSplitShared::new(self.state != CompioStreamState::Open, &self.config);
        let (reader_protocol, writer_protocol) = self
            .protocol
            .split(self.config.max_frame_size, self.config.max_message_size);

        ::compio::runtime::spawn(compio_split_writer_driver(
            writer,
            writer_protocol,
            self.config,
            CompioDriverChannels {
                control_rx,
                application_rx,
                cancel_rx,
                terminal_tx,
                shared: shared.clone(),
            },
        ))
        .detach();

        (
            CompioCompressedSplitReader {
                reader,
                protocol: reader_protocol,
                read_buf: self.read_buf,
                pending_messages: self.pending_messages,
                pending_index: self.pending_index,
                pending_terminal_error: self.pending_terminal_error,
                read_budget: SPLIT_CONTROL_CAPACITY,
                control_tx,
                terminal_rx,
                cancel_tx: cancel_tx.clone(),
                shared: shared.clone(),
                terminal_reported: false,
            },
            CompioCompressedSplitWriter {
                application_tx,
                cancel_tx,
                shared,
                _writer: PhantomData,
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
    // Deliver accepted messages before a later parse failure.
    pending_terminal_error: Option<Error>,
    read_budget: usize,
    control_tx: mpsc::Sender<ControlRequest>,
    terminal_rx: mpsc::UnboundedReceiver<()>,
    cancel_tx: mpsc::UnboundedSender<()>,
    shared: Rc<CompioSplitShared>,
    terminal_reported: bool,
}

/// Write half of a split compressed Compio WebSocket stream.
#[cfg(feature = "permessage-deflate")]
pub struct CompioCompressedSplitWriter<W> {
    application_tx: mpsc::Sender<ApplicationRequest>,
    cancel_tx: mpsc::UnboundedSender<()>,
    shared: Rc<CompioSplitShared>,
    _writer: PhantomData<fn() -> W>,
}

#[cfg(feature = "permessage-deflate")]
impl<R> CompioCompressedSplitReader<R>
where
    R: AsyncRead,
{
    /// Receive the next message, including Ping and Pong control frames.
    ///
    /// Buffered ordinary messages bypass the writer's control queue. Cooperative
    /// yields preserve them; waiting on the control queue is not cancellation-safe.
    /// Fatal read/parse errors and EOF return without waiting for the driver;
    /// they also queue cancellation of pending writes.
    pub async fn next(&mut self) -> Option<Result<Message>> {
        compio_consume_read_budget(&mut self.read_budget).await;
        loop {
            if self.pending_terminal_error.is_none() {
                if self.terminal_reported {
                    return None;
                }
                if self.shared.status.get() == SPLIT_CLOSED {
                    self.terminal_reported = true;
                    return self.shared.read_terminal();
                }
            }

            if self.pending_index < self.pending_messages.len() {
                // Move the message out; the consumed slot is never returned again.
                let msg = std::mem::replace(
                    &mut self.pending_messages[self.pending_index],
                    Message::Close(None),
                );
                self.pending_index += 1;

                if self.pending_index >= self.pending_messages.len() {
                    self.pending_messages.clear();
                    self.pending_index = 0;
                }

                let request = match &msg {
                    Message::Ping(data) => ControlRequest::PeerPing(data.clone(), Instant::now()),
                    Message::Pong(data) => ControlRequest::Pong(data.clone(), Instant::now()),
                    Message::Close(_) => {
                        self.shared.begin_closing();
                        self.pending_messages.clear();
                        self.pending_index = 0;
                        self.pending_terminal_error = None;
                        self.read_buf.clear();
                        self.terminal_reported = true;
                        ControlRequest::PeerClose
                    }
                    _ => {
                        // Data frames refresh activity without a channel round trip.
                        self.shared.record_data_activity();
                        return Some(Ok(msg));
                    }
                };
                if self.control_tx.send(request).await.is_err() {
                    self.terminal_reported = true;
                    return self.shared.read_terminal();
                }
                return Some(Ok(msg));
            }

            if let Some(error) = self.pending_terminal_error.take() {
                compio_cancel_read(&self.shared, &self.cancel_tx);
                self.terminal_reported = true;
                return Some(Err(error));
            }

            if !self.read_buf.is_empty() {
                match self
                    .protocol
                    .process_into_with_activity(&mut self.read_buf, &mut self.pending_messages)
                {
                    Ok(fragment_activity) => {
                        if fragment_activity {
                            self.shared.record_data_activity();
                        }
                        self.pending_index = 0;
                        if !self.pending_messages.is_empty() {
                            continue;
                        }
                    }
                    Err(error) => {
                        self.pending_index = 0;
                        self.pending_terminal_error = Some(error);
                        continue;
                    }
                }
            }

            let outcome = {
                let read = read_more(&mut self.reader, &mut self.read_buf).fuse();
                let terminal = self.terminal_rx.next().fuse();
                futures_util::pin_mut!(read, terminal);
                futures_util::select_biased! {
                    _ = terminal => CompioReadOutcome::Terminal,
                    result = read => CompioReadOutcome::Read(result),
                }
            };
            match outcome {
                CompioReadOutcome::Read(Ok(0)) => {
                    compio_cancel_read(&self.shared, &self.cancel_tx);
                    self.terminal_reported = true;
                    return None;
                }
                CompioReadOutcome::Read(Ok(_)) => {}
                CompioReadOutcome::Read(Err(error)) => {
                    compio_cancel_read(&self.shared, &self.cancel_tx);
                    self.terminal_reported = true;
                    return Some(Err(error.into()));
                }
                CompioReadOutcome::Terminal => {
                    self.terminal_reported = true;
                    return self.shared.read_terminal();
                }
            }
        }
    }

    /// Check whether the reader is closed.
    pub fn is_closed(&self) -> bool {
        !self.shared.is_open()
    }
}

#[cfg(feature = "permessage-deflate")]
impl<R> Drop for CompioCompressedSplitReader<R> {
    fn drop(&mut self) {
        let _ = self.cancel_tx.unbounded_send(());
    }
}

#[cfg(feature = "permessage-deflate")]
impl<W> CompioCompressedSplitWriter<W> {
    /// Send a WebSocket message.
    pub async fn send(&mut self, msg: Message) -> Result<()> {
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        let (tx, rx) = oneshot::channel();
        self.application_tx
            .send(ApplicationRequest::Send(msg, tx))
            .await
            .map_err(|_| self.current_error())?;
        rx.await.map_err(|_| self.current_error())?
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
        if !self.shared.is_open() {
            return Err(self.current_error());
        }
        let (tx, rx) = oneshot::channel();
        self.application_tx
            .send(ApplicationRequest::Flush(tx))
            .await
            .map_err(|_| self.current_error())?;
        rx.await.map_err(|_| self.current_error())?
    }

    /// Check whether the writer is closed.
    pub fn is_closed(&self) -> bool {
        !self.shared.is_open()
    }

    fn current_error(&self) -> Error {
        self.shared.error()
    }
}

#[cfg(feature = "permessage-deflate")]
impl<W> Drop for CompioCompressedSplitWriter<W> {
    fn drop(&mut self) {
        let _ = self.cancel_tx.unbounded_send(());
    }
}

#[cfg(test)]
mod receive_tests;

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
    async fn compio_http1_client_sends_custom_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = ::compio::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = BytesMut::with_capacity(4096);
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                assert!(read_more(&mut stream, &mut request).await.unwrap() > 0);
            }
            let request = std::str::from_utf8(&request).unwrap();
            assert!(request.contains("Authorization: Bearer token\r\n"));

            let key = request
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("Sec-WebSocket-Key")
                        .then(|| value.trim())
                })
                .unwrap();
            let response =
                crate::handshake::build_response(&generate_accept_key(key), None, None).unwrap();
            write_all_owned(&mut stream, response).await.unwrap();
            stream.flush().await.unwrap();
        });
        let headers = vec![("Authorization".to_string(), "Bearer token".to_string())];

        let stream = TcpStream::connect(addr).await.unwrap();
        let (_, handshake) = connect_async_with_headers(
            stream,
            &addr.to_string(),
            "/ws",
            None,
            Some(&headers),
            Config::default(),
        )
        .await
        .unwrap();

        assert_eq!(handshake.path, "/ws");
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

    #[compio::test]
    async fn compio_split_driver_replies_to_ping_without_application_write() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = ::compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let config = Config::builder().auto_ping(false).idle_timeout(0).build();
            let (ws, _) = accept_async(stream, config).await.unwrap();
            let (mut reader, _writer) = ws.split();

            assert!(matches!(
                reader.next().await,
                Some(Ok(Message::Ping(payload))) if payload == b"pin"[..]
            ));
            assert!(matches!(reader.next().await, Some(Ok(Message::Close(_)))));
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let (mut client, _) = connect_async(stream, &addr.to_string(), "/", None, config)
            .await
            .unwrap();
        client
            .send(Message::Ping(Bytes::from_static(b"pin")))
            .await
            .unwrap();
        assert!(matches!(
            client.next().await,
            Some(Ok(Message::Pong(payload))) if payload == b"pin"[..]
        ));
        client.close(1000, "").await.unwrap();
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

        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der =
            rustls::pki_types::PrivateKeyDer::try_from(signing_key.serialize_der()).unwrap();

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

        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der =
            rustls::pki_types::PrivateKeyDer::try_from(signing_key.serialize_der()).unwrap();

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

    #[cfg(feature = "permessage-deflate")]
    #[compio::test]
    async fn compio_compressed_split_driver_replies_to_ping() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = ::compio::runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let config = Config::builder().auto_ping(false).idle_timeout(0).build();
            let ws =
                CompioCompressedWebSocketStream::server(stream, config, DeflateConfig::default());
            let (mut reader, _writer) = ws.split();
            assert!(matches!(
                reader.next().await,
                Some(Ok(Message::Ping(payload))) if payload == b"pin"[..]
            ));
            assert!(matches!(reader.next().await, Some(Ok(Message::Close(_)))));
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut client =
            CompioCompressedWebSocketStream::client(stream, config, DeflateConfig::default());
        client
            .send(Message::Ping(Bytes::from_static(b"pin")))
            .await
            .unwrap();
        assert!(matches!(
            client.next().await,
            Some(Ok(Message::Pong(payload))) if payload == b"pin"[..]
        ));
        client.close(1000, "").await.unwrap();
        server.await.unwrap();
    }
}

#[cfg(test)]
#[path = "compio/deadline_tests.rs"]
mod deadline_tests;
