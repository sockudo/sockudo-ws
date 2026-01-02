//! Unified transport stream type
//!
//! This module provides `Stream<T>`, a generic wrapper over transport-specific
//! stream implementations that provides a unified `AsyncRead + AsyncWrite` interface.
//!
//! # Example
//!
//! ```ignore
//! use sockudo_ws::{Stream, Http1, Http2, Http3, WebSocketStream, Config};
//!
//! // HTTP/1.1 - wrap a TLS stream
//! let stream: Stream<Http1> = Stream::new(tls_stream);
//! let ws = WebSocketStream::client(stream, Config::default());
//!
//! // HTTP/2 - wrap h2 streams
//! let stream: Stream<Http2> = Stream::from_h2(send_stream, recv_stream);
//! let ws = WebSocketStream::server(stream, Config::default());
//!
//! // HTTP/3 - wrap QUIC streams
//! let stream: Stream<Http3> = Stream::from_quic(send_stream, recv_stream);
//! let ws = WebSocketStream::server(stream, Config::default());
//! ```

use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(any(feature = "http2", feature = "http3"))]
use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::transport::{Http1, Transport};

#[cfg(feature = "http2")]
use crate::transport::Http2;

#[cfg(feature = "http3")]
use crate::transport::Http3;

// ============================================================================
// Stream<T> - Unified transport stream
// ============================================================================

/// A unified stream type generic over transport protocol
///
/// This type wraps transport-specific stream implementations and provides
/// a common `AsyncRead + AsyncWrite` interface for use with `WebSocketStream`.
///
/// # Type Parameters
///
/// - `Stream<Http1>` - For HTTP/1.1 WebSocket over TCP/TLS
/// - `Stream<Http2>` - For HTTP/2 WebSocket via Extended CONNECT (RFC 8441)
/// - `Stream<Http3>` - For HTTP/3 WebSocket via Extended CONNECT (RFC 9220)
pub struct Stream<T: Transport> {
    inner: StreamInner,
    _marker: PhantomData<T>,
}

// Internal enum holding the actual stream implementation
enum StreamInner {
    // HTTP/1.1: Any AsyncRead + AsyncWrite stream (boxed for type erasure)
    Http1(Box<dyn Http1Stream>),

    // HTTP/2: h2's send/receive streams
    #[cfg(feature = "http2")]
    Http2(Http2StreamInner),

    // HTTP/3: QUIC streams (server or client variant)
    #[cfg(feature = "http3")]
    Http3(Http3StreamInner),
}

// ============================================================================
// HTTP/1.1 Stream Implementation
// ============================================================================

/// Trait object for HTTP/1.1 streams (type-erased AsyncRead + AsyncWrite)
trait Http1Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Http1Stream for T {}

impl Stream<Http1> {
    /// Create a new HTTP/1.1 stream from any AsyncRead + AsyncWrite type
    ///
    /// # Example
    ///
    /// ```ignore
    /// use tokio::net::TcpStream;
    /// use sockudo_ws::{Stream, Http1};
    ///
    /// let tcp = TcpStream::connect("example.com:80").await?;
    /// let stream: Stream<Http1> = Stream::new(tcp);
    /// ```
    pub fn new<S>(inner: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            inner: StreamInner::Http1(Box::new(inner)),
            _marker: PhantomData,
        }
    }
}

impl AsyncRead for Stream<Http1> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.inner {
            StreamInner::Http1(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
            #[cfg(any(feature = "http2", feature = "http3"))]
            _ => unreachable!(),
        }
    }
}

impl AsyncWrite for Stream<Http1> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.inner {
            StreamInner::Http1(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
            #[cfg(any(feature = "http2", feature = "http3"))]
            _ => unreachable!(),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.inner {
            StreamInner::Http1(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
            #[cfg(any(feature = "http2", feature = "http3"))]
            _ => unreachable!(),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.inner {
            StreamInner::Http1(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
            #[cfg(any(feature = "http2", feature = "http3"))]
            _ => unreachable!(),
        }
    }
}

impl fmt::Debug for Stream<Http1> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream<Http1>").finish()
    }
}

// ============================================================================
// HTTP/2 Stream Implementation
// ============================================================================

#[cfg(feature = "http2")]
struct Http2StreamInner {
    send: h2::SendStream<Bytes>,
    recv: h2::RecvStream,
    recv_buf: BytesMut,
    recv_eof: bool,
    capacity_needed: usize,
}

#[cfg(feature = "http2")]
impl Stream<Http2> {
    /// Create a new HTTP/2 stream from h2 send and receive streams
    ///
    /// This is typically called after an Extended CONNECT handshake completes.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use sockudo_ws::{Stream, Http2};
    ///
    /// // After HTTP/2 handshake
    /// let stream: Stream<Http2> = Stream::from_h2(send_stream, recv_stream);
    /// ```
    pub fn from_h2(send: h2::SendStream<Bytes>, recv: h2::RecvStream) -> Self {
        Self {
            inner: StreamInner::Http2(Http2StreamInner {
                send,
                recv,
                recv_buf: BytesMut::with_capacity(64 * 1024),
                recv_eof: false,
                capacity_needed: 0,
            }),
            _marker: PhantomData,
        }
    }

    /// Get a reference to the underlying h2 send stream
    pub fn send_stream(&self) -> Option<&h2::SendStream<Bytes>> {
        match &self.inner {
            StreamInner::Http2(inner) => Some(&inner.send),
            _ => None,
        }
    }

    /// Get a mutable reference to the underlying h2 send stream
    pub fn send_stream_mut(&mut self) -> Option<&mut h2::SendStream<Bytes>> {
        match &mut self.inner {
            StreamInner::Http2(inner) => Some(&mut inner.send),
            _ => None,
        }
    }
}

#[cfg(feature = "http2")]
impl AsyncRead for Stream<Http2> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let inner = match &mut self.inner {
            StreamInner::Http2(inner) => inner,
            _ => unreachable!(),
        };

        // First, try to satisfy from the internal buffer
        if !inner.recv_buf.is_empty() {
            let to_copy = std::cmp::min(buf.remaining(), inner.recv_buf.len());
            buf.put_slice(&inner.recv_buf.split_to(to_copy));
            return Poll::Ready(Ok(()));
        }

        // Check if we've already received EOF
        if inner.recv_eof {
            return Poll::Ready(Ok(()));
        }

        // Poll the h2 RecvStream for more data
        match Pin::new(&mut inner.recv).poll_data(cx) {
            Poll::Ready(Some(Ok(mut data))) => {
                // Release flow control capacity back to sender
                let len = data.len();
                let _ = inner.recv.flow_control().release_capacity(len);

                // Copy what we can to the output buffer
                let to_copy = std::cmp::min(buf.remaining(), data.len());
                buf.put_slice(&data.split_to(to_copy));

                // Buffer any remainder
                if data.has_remaining() {
                    inner.recv_buf.extend_from_slice(data.chunk());
                }

                Poll::Ready(Ok(()))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(io::Error::other(e))),
            Poll::Ready(None) => {
                inner.recv_eof = true;
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(feature = "http2")]
impl AsyncWrite for Stream<Http2> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let inner = match &mut self.inner {
            StreamInner::Http2(inner) => inner,
            _ => unreachable!(),
        };

        // Reserve capacity if needed
        if inner.capacity_needed > 0 || inner.send.capacity() == 0 {
            inner.send.reserve_capacity(buf.len());
            inner.capacity_needed = buf.len();
        }

        // Poll for capacity to become available
        match inner.send.poll_capacity(cx) {
            Poll::Ready(Some(Ok(capacity))) => {
                let to_send = std::cmp::min(capacity, buf.len());
                let data = Bytes::copy_from_slice(&buf[..to_send]);

                inner
                    .send
                    .send_data(data, false)
                    .map_err(io::Error::other)?;

                inner.capacity_needed = 0;
                Poll::Ready(Ok(to_send))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(io::Error::other(e))),
            Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "HTTP/2 stream closed",
            ))),
            Poll::Pending => {
                inner.capacity_needed = buf.len();
                Poll::Pending
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // h2 handles flushing internally at the connection level
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let inner = match &mut self.inner {
            StreamInner::Http2(inner) => inner,
            _ => unreachable!(),
        };

        // Send empty DATA frame with END_STREAM flag
        inner
            .send
            .send_data(Bytes::new(), true)
            .map_err(io::Error::other)?;
        Poll::Ready(Ok(()))
    }
}

#[cfg(feature = "http2")]
impl fmt::Debug for Stream<Http2> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            StreamInner::Http2(inner) => f
                .debug_struct("Stream<Http2>")
                .field("recv_buf_len", &inner.recv_buf.len())
                .field("recv_eof", &inner.recv_eof)
                .finish(),
            _ => unreachable!(),
        }
    }
}

// ============================================================================
// HTTP/3 Stream Implementation
// ============================================================================

#[cfg(feature = "http3")]
enum Http3StreamInner {
    /// Raw QUIC streams (for direct QUIC usage)
    Raw {
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        recv_buf: BytesMut,
        recv_finished: bool,
    },
    /// Server-side h3 request stream
    Server {
        stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
        read_buf: BytesMut,
    },
    /// Client-side h3 request stream
    Client {
        stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
        read_buf: BytesMut,
    },
}

#[cfg(feature = "http3")]
impl Stream<Http3> {
    /// Create from raw QUIC send/receive streams
    ///
    /// Use this when working directly with QUIC without the h3 layer.
    pub fn from_quic(send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
        Self {
            inner: StreamInner::Http3(Http3StreamInner::Raw {
                send,
                recv,
                recv_buf: BytesMut::with_capacity(64 * 1024),
                recv_finished: false,
            }),
            _marker: PhantomData,
        }
    }

    /// Create from a bidirectional QUIC stream tuple
    pub fn from_quic_bi(stream: (quinn::SendStream, quinn::RecvStream)) -> Self {
        Self::from_quic(stream.0, stream.1)
    }

    /// Create from an h3 server request stream
    ///
    /// This is used on the server side after receiving an Extended CONNECT request.
    pub fn from_h3_server(
        stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    ) -> Self {
        Self {
            inner: StreamInner::Http3(Http3StreamInner::Server {
                stream,
                read_buf: BytesMut::with_capacity(64 * 1024),
            }),
            _marker: PhantomData,
        }
    }

    /// Create from an h3 client request stream
    ///
    /// This is used on the client side after sending an Extended CONNECT request.
    pub fn from_h3_client(
        stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    ) -> Self {
        Self {
            inner: StreamInner::Http3(Http3StreamInner::Client {
                stream,
                read_buf: BytesMut::with_capacity(64 * 1024),
            }),
            _marker: PhantomData,
        }
    }

    /// Get the QUIC stream ID (if using raw QUIC streams)
    pub fn stream_id(&self) -> Option<quinn::StreamId> {
        match &self.inner {
            StreamInner::Http3(Http3StreamInner::Raw { send, .. }) => Some(send.id()),
            _ => None,
        }
    }
}

#[cfg(feature = "http3")]
impl AsyncRead for Stream<Http3> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.inner {
            StreamInner::Http3(Http3StreamInner::Raw {
                recv,
                recv_buf,
                recv_finished,
                ..
            }) => {
                // First drain buffered data
                if !recv_buf.is_empty() {
                    let to_copy = std::cmp::min(buf.remaining(), recv_buf.len());
                    buf.put_slice(&recv_buf.split_to(to_copy));
                    return Poll::Ready(Ok(()));
                }

                if *recv_finished {
                    return Poll::Ready(Ok(()));
                }

                // Poll QUIC stream
                match recv.poll_read_buf(cx, buf) {
                    Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                    Poll::Ready(Err(e)) => {
                        if matches!(e, quinn::ReadError::Reset(_)) {
                            *recv_finished = true;
                        }
                        Poll::Ready(Err(io::Error::other(e)))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            StreamInner::Http3(Http3StreamInner::Server { stream, read_buf }) => {
                // First drain buffered data
                if !read_buf.is_empty() {
                    let to_copy = std::cmp::min(buf.remaining(), read_buf.len());
                    buf.put_slice(&read_buf.split_to(to_copy));
                    return Poll::Ready(Ok(()));
                }

                // Poll h3 server stream
                let mut fut = Box::pin(stream.recv_data());
                match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(Some(mut data))) => {
                        let data_len = data.remaining();
                        let to_copy = std::cmp::min(buf.remaining(), data_len);
                        let chunk = data.copy_to_bytes(to_copy);
                        buf.put_slice(&chunk);

                        // Buffer remainder
                        if data.has_remaining() {
                            while data.has_remaining() {
                                read_buf.extend_from_slice(data.chunk());
                                let len = data.chunk().len();
                                data.advance(len);
                            }
                        }
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Ok(None)) => Poll::Ready(Ok(())),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
                    Poll::Pending => Poll::Pending,
                }
            }
            StreamInner::Http3(Http3StreamInner::Client { stream, read_buf }) => {
                // First drain buffered data
                if !read_buf.is_empty() {
                    let to_copy = std::cmp::min(buf.remaining(), read_buf.len());
                    buf.put_slice(&read_buf.split_to(to_copy));
                    return Poll::Ready(Ok(()));
                }

                // Poll h3 client stream
                let mut fut = Box::pin(stream.recv_data());
                match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(Some(mut data))) => {
                        let data_len = data.remaining();
                        let to_copy = std::cmp::min(buf.remaining(), data_len);
                        let chunk = data.copy_to_bytes(to_copy);
                        buf.put_slice(&chunk);

                        // Buffer remainder
                        if data.has_remaining() {
                            while data.has_remaining() {
                                read_buf.extend_from_slice(data.chunk());
                                let len = data.chunk().len();
                                data.advance(len);
                            }
                        }
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Ok(None)) => Poll::Ready(Ok(())),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
                    Poll::Pending => Poll::Pending,
                }
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(feature = "http3")]
impl AsyncWrite for Stream<Http3> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        match &mut self.inner {
            StreamInner::Http3(Http3StreamInner::Raw { send, .. }) => {
                match Pin::new(send).poll_write(cx, buf) {
                    Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e))),
                    Poll::Pending => Poll::Pending,
                }
            }
            StreamInner::Http3(Http3StreamInner::Server { stream, .. }) => {
                let data = Bytes::copy_from_slice(buf);
                let fut = stream.send_data(data);
                tokio::pin!(fut);

                match fut.poll(cx) {
                    Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
                    Poll::Pending => Poll::Pending,
                }
            }
            StreamInner::Http3(Http3StreamInner::Client { stream, .. }) => {
                let data = Bytes::copy_from_slice(buf);
                let fut = stream.send_data(data);
                tokio::pin!(fut);

                match fut.poll(cx) {
                    Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
                    Poll::Pending => Poll::Pending,
                }
            }
            _ => unreachable!(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // QUIC/h3 handles flushing internally
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.inner {
            StreamInner::Http3(Http3StreamInner::Raw { send, .. }) => match send.finish() {
                Ok(()) => Poll::Ready(Ok(())),
                Err(e) => Poll::Ready(Err(io::Error::other(e))),
            },
            StreamInner::Http3(Http3StreamInner::Server { stream, .. }) => {
                let fut = stream.finish();
                tokio::pin!(fut);

                match fut.poll(cx) {
                    Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
                    Poll::Pending => Poll::Pending,
                }
            }
            StreamInner::Http3(Http3StreamInner::Client { stream, .. }) => {
                let fut = stream.finish();
                tokio::pin!(fut);

                match fut.poll(cx) {
                    Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                    Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
                    Poll::Pending => Poll::Pending,
                }
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(feature = "http3")]
impl fmt::Debug for Stream<Http3> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            StreamInner::Http3(Http3StreamInner::Raw {
                recv_buf,
                recv_finished,
                ..
            }) => f
                .debug_struct("Stream<Http3>")
                .field("variant", &"Raw")
                .field("recv_buf_len", &recv_buf.len())
                .field("recv_finished", recv_finished)
                .finish(),
            StreamInner::Http3(Http3StreamInner::Server { read_buf, .. }) => f
                .debug_struct("Stream<Http3>")
                .field("variant", &"Server")
                .field("read_buf_len", &read_buf.len())
                .finish(),
            StreamInner::Http3(Http3StreamInner::Client { read_buf, .. }) => f
                .debug_struct("Stream<Http3>")
                .field("variant", &"Client")
                .field("read_buf_len", &read_buf.len())
                .finish(),
            _ => unreachable!(),
        }
    }
}

// ============================================================================
// Send + Sync implementations
// ============================================================================

// SAFETY: Stream<T> is Send because all inner types are Send
unsafe impl Send for Stream<Http1> {}
#[cfg(feature = "http2")]
unsafe impl Send for Stream<Http2> {}
#[cfg(feature = "http3")]
unsafe impl Send for Stream<Http3> {}

// SAFETY: Stream<T> is Sync for types where inner is Sync
// Http1 inner is a Box<dyn Http1Stream> which is Send but not necessarily Sync
// We don't implement Sync for Http1 to be safe

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Stream<Http1>>();
        #[cfg(feature = "http2")]
        assert_send::<Stream<Http2>>();
        #[cfg(feature = "http3")]
        assert_send::<Stream<Http3>>();
    }
}
