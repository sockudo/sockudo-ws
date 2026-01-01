//! HTTP/3 stream wrapper implementing AsyncRead + AsyncWrite
//!
//! This module provides `H3Stream`, a wrapper around QUIC bidirectional streams
//! that implements tokio's `AsyncRead` and `AsyncWrite` traits for use with
//! `WebSocketStream`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pin_project! {
    /// A wrapper around HTTP/3/QUIC streams that implements AsyncRead + AsyncWrite
    ///
    /// This allows QUIC streams to be used with `WebSocketStream<H3Stream>`,
    /// providing the same API as TCP-based or HTTP/2-based WebSocket connections.
    ///
    /// After the HTTP/3 Extended CONNECT handshake completes with a 200 OK,
    /// the stream transitions to raw WebSocket frame mode using this wrapper.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use sockudo_ws::{Config, WebSocketStream};
    /// use sockudo_ws::http3::H3Stream;
    ///
    /// // After HTTP/3 handshake and Extended CONNECT negotiation
    /// let h3_stream = H3Stream::new(send_stream, recv_stream);
    /// let mut ws = WebSocketStream::server(h3_stream, Config::default());
    ///
    /// // Use the exact same API as HTTP/1.1 or HTTP/2!
    /// while let Some(msg) = ws.next().await {
    ///     ws.send(msg?).await?;
    /// }
    /// ```
    pub struct H3Stream {
        #[pin]
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        recv_buf: BytesMut,
        pending_bytes: Option<Bytes>,
        recv_finished: bool,
    }
}

impl H3Stream {
    /// Create a new H3Stream from quinn send and receive streams
    pub fn new(send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
        Self {
            send,
            recv,
            recv_buf: BytesMut::with_capacity(64 * 1024),
            pending_bytes: None,
            recv_finished: false,
        }
    }

    /// Create from a bidirectional QUIC stream tuple
    pub fn from_bi(stream: (quinn::SendStream, quinn::RecvStream)) -> Self {
        Self::new(stream.0, stream.1)
    }

    /// Get a reference to the send stream
    pub fn send_stream(&self) -> &quinn::SendStream {
        &self.send
    }

    /// Get a mutable reference to the send stream
    pub fn send_stream_mut(&mut self) -> &mut quinn::SendStream {
        &mut self.send
    }

    /// Get a reference to the receive stream
    pub fn recv_stream(&self) -> &quinn::RecvStream {
        &self.recv
    }

    /// Get a mutable reference to the receive stream
    pub fn recv_stream_mut(&mut self) -> &mut quinn::RecvStream {
        &mut self.recv
    }

    /// Check if the receive side is finished
    pub fn is_recv_finished(&self) -> bool {
        self.recv_finished
    }

    /// Get the QUIC stream ID
    pub fn stream_id(&self) -> quinn::StreamId {
        self.send.id()
    }
}

impl AsyncRead for H3Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.project();

        // First, drain any pending bytes from h3 layer
        if let Some(pending) = this.pending_bytes {
            let to_copy = std::cmp::min(buf.remaining(), pending.len());
            buf.put_slice(&pending.split_to(to_copy));
            if pending.is_empty() {
                *this.pending_bytes = None;
            }
            return Poll::Ready(Ok(()));
        }

        // Then try internal buffer
        if !this.recv_buf.is_empty() {
            let to_copy = std::cmp::min(buf.remaining(), this.recv_buf.len());
            buf.put_slice(&this.recv_buf.split_to(to_copy));
            return Poll::Ready(Ok(()));
        }

        // Check if we've already received FIN
        if *this.recv_finished {
            return Poll::Ready(Ok(()));
        }

        // Poll the QUIC RecvStream for more data
        match this.recv.poll_read_buf(cx, buf) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => {
                if matches!(e, quinn::ReadError::Reset(_)) {
                    *this.recv_finished = true;
                }
                Poll::Ready(Err(io::Error::other(e)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for H3Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let this = self.project();

        match this.send.poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // QUIC handles flow control internally and doesn't require explicit flushing
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.project();

        // Finish the send stream (send FIN per RFC 9220 Section 3)
        match this.send.get_mut().finish() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }
}

impl std::fmt::Debug for H3Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3Stream")
            .field("stream_id", &self.send.id())
            .field("recv_buf_len", &self.recv_buf.len())
            .field("recv_finished", &self.recv_finished)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_h3stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<super::H3Stream>();
    }
}
