//! HTTP/2 stream wrapper implementing AsyncRead + AsyncWrite
//!
//! This module provides `Http2Stream`, a wrapper around h2's `SendStream` and `RecvStream`
//! that implements tokio's `AsyncRead` and `AsyncWrite` traits. This allows HTTP/2 streams
//! to be used with the existing `WebSocketStream<S>` generic type.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes, BytesMut};
use h2::{RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A wrapper around h2 send/receive streams that implements AsyncRead + AsyncWrite
///
/// This allows HTTP/2 streams to be used with `WebSocketStream<Http2Stream>`,
/// providing the same API as TCP-based WebSocket connections.
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::{Config, WebSocketStream, Http2};
/// use sockudo_ws::http2::stream::Http2Stream;
///
/// // After HTTP/2 handshake and Extended CONNECT negotiation
/// let stream = Http2Stream::new(send_stream, recv_stream);
/// let mut ws = WebSocketStream::server(stream, Config::default());
///
/// // Use the same API as regular WebSocket
/// while let Some(msg) = ws.next().await {
///     ws.send(msg?).await?;
/// }
/// ```
pub struct Http2Stream {
    send: SendStream<Bytes>,
    recv: RecvStream,
    recv_buf: BytesMut,
    /// Track if we've received END_STREAM
    recv_eof: bool,
    /// Track if we need to reserve capacity
    capacity_needed: usize,
}

impl Http2Stream {
    /// Create a new Http2Stream from h2 send and receive streams
    pub fn new(send: SendStream<Bytes>, recv: RecvStream) -> Self {
        Self {
            send,
            recv,
            recv_buf: BytesMut::with_capacity(64 * 1024),
            recv_eof: false,
            capacity_needed: 0,
        }
    }

    /// Get a reference to the underlying send stream
    pub fn send_stream(&self) -> &SendStream<Bytes> {
        &self.send
    }

    /// Get a mutable reference to the underlying send stream
    pub fn send_stream_mut(&mut self) -> &mut SendStream<Bytes> {
        &mut self.send
    }

    /// Get a reference to the underlying receive stream
    pub fn recv_stream(&self) -> &RecvStream {
        &self.recv
    }

    /// Get a mutable reference to the underlying receive stream
    pub fn recv_stream_mut(&mut self) -> &mut RecvStream {
        &mut self.recv
    }
}

impl AsyncRead for Http2Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // First, try to satisfy from the internal buffer
        if !self.recv_buf.is_empty() {
            let to_copy = std::cmp::min(buf.remaining(), self.recv_buf.len());
            buf.put_slice(&self.recv_buf.split_to(to_copy));
            return Poll::Ready(Ok(()));
        }

        // Check if we've already received EOF
        if self.recv_eof {
            return Poll::Ready(Ok(()));
        }

        // Poll the h2 RecvStream for more data
        match Pin::new(&mut self.recv).poll_data(cx) {
            Poll::Ready(Some(Ok(mut data))) => {
                // Release flow control capacity back to sender
                let len = data.len();
                let _ = self.recv.flow_control().release_capacity(len);

                // Copy what we can to the output buffer
                let to_copy = std::cmp::min(buf.remaining(), data.len());
                buf.put_slice(&data.split_to(to_copy));

                // Buffer any remainder
                if data.has_remaining() {
                    self.recv_buf.extend_from_slice(data.chunk());
                }

                Poll::Ready(Ok(()))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(io::Error::other(e))),
            Poll::Ready(None) => {
                // Stream ended (END_STREAM received)
                self.recv_eof = true;
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Http2Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // Reserve capacity if needed
        if self.capacity_needed > 0 || self.send.capacity() == 0 {
            self.send.reserve_capacity(buf.len());
            self.capacity_needed = buf.len();
        }

        // Poll for capacity to become available
        match self.send.poll_capacity(cx) {
            Poll::Ready(Some(Ok(capacity))) => {
                let to_send = std::cmp::min(capacity, buf.len());
                let data = Bytes::copy_from_slice(&buf[..to_send]);

                self.send.send_data(data, false).map_err(io::Error::other)?;

                self.capacity_needed = 0;
                Poll::Ready(Ok(to_send))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(io::Error::other(e))),
            Poll::Ready(None) => {
                // Stream was reset
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "HTTP/2 stream closed",
                )))
            }
            Poll::Pending => {
                self.capacity_needed = buf.len();
                Poll::Pending
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // h2 handles flushing internally at the connection level
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Send empty DATA frame with END_STREAM flag
        self.send
            .send_data(Bytes::new(), true)
            .map_err(io::Error::other)?;
        Poll::Ready(Ok(()))
    }
}

impl std::fmt::Debug for Http2Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http2Stream")
            .field("recv_buf_len", &self.recv_buf.len())
            .field("recv_eof", &self.recv_eof)
            .field("capacity_needed", &self.capacity_needed)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_http2stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<super::Http2Stream>();
    }
}
