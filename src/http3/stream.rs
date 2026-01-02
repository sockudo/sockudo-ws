//! HTTP/3 stream wrappers implementing AsyncRead + AsyncWrite
//!
//! This module provides stream wrappers around QUIC and h3 streams
//! that implement tokio's `AsyncRead` and `AsyncWrite` traits for use with
//! `WebSocketStream`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes, BytesMut};
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pin_project! {
    /// A wrapper around raw QUIC streams that implements AsyncRead + AsyncWrite
    ///
    /// This allows QUIC streams to be used with `WebSocketStream`,
    /// providing the same API as TCP-based or HTTP/2-based WebSocket connections.
    ///
    /// After the HTTP/3 Extended CONNECT handshake completes with a 200 OK,
    /// the stream transitions to raw WebSocket frame mode using this wrapper.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use sockudo_ws::{Config, WebSocketStream, Http3};
    /// use sockudo_ws::http3::stream::Http3Stream;
    ///
    /// // After HTTP/3 handshake and Extended CONNECT negotiation
    /// let stream = Http3Stream::new(send_stream, recv_stream);
    /// let mut ws = WebSocketStream::server(stream, Config::default());
    ///
    /// // Use the exact same API as HTTP/1.1 or HTTP/2!
    /// while let Some(msg) = ws.next().await {
    ///     ws.send(msg?).await?;
    /// }
    /// ```
    pub struct Http3Stream {
        #[pin]
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        recv_buf: BytesMut,
        pending_bytes: Option<Bytes>,
        recv_finished: bool,
    }
}

impl Http3Stream {
    /// Create a new Http3Stream from quinn send and receive streams
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

impl AsyncRead for Http3Stream {
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

impl AsyncWrite for Http3Stream {
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

impl std::fmt::Debug for Http3Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http3Stream")
            .field("stream_id", &self.send.id())
            .field("recv_buf_len", &self.recv_buf.len())
            .field("recv_finished", &self.recv_finished)
            .finish()
    }
}

// ============================================================================
// Http3ServerStream - For server-side h3 request streams
// ============================================================================

/// Wrapper around h3 server request stream that implements AsyncRead + AsyncWrite
///
/// After the HTTP/3 CONNECT handshake, this stream is used for raw
/// WebSocket frame data exchange on the server side.
pub struct Http3ServerStream {
    stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    read_buf: BytesMut,
}

impl Http3ServerStream {
    /// Create a new Http3ServerStream
    pub fn new(stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(64 * 1024),
        }
    }
}

impl AsyncRead for Http3ServerStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // First drain any buffered data
        if !this.read_buf.is_empty() {
            let to_copy = std::cmp::min(buf.remaining(), this.read_buf.len());
            buf.put_slice(&this.read_buf.split_to(to_copy));
            return Poll::Ready(Ok(()));
        }

        // Use a boxed future to work around borrow checker limitations
        let mut fut = Box::pin(this.stream.recv_data());

        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(Some(mut data))) => {
                // data is impl Buf, use Buf trait methods
                let data_len = data.remaining();
                let to_copy = std::cmp::min(buf.remaining(), data_len);

                // Copy data to output buffer
                let chunk = data.copy_to_bytes(to_copy);
                buf.put_slice(&chunk);

                // Buffer any remaining data
                if data.has_remaining() {
                    while data.has_remaining() {
                        this.read_buf.extend_from_slice(data.chunk());
                        let len = data.chunk().len();
                        data.advance(len);
                    }
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(None)) => {
                // Stream finished
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Http3ServerStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        // h3's send_data takes Bytes
        let data = Bytes::copy_from_slice(buf);
        let fut = self.stream.send_data(data);
        tokio::pin!(fut);

        match fut.poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // h3/QUIC handles flushing internally
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fut = self.stream.finish();
        tokio::pin!(fut);

        match fut.poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl std::fmt::Debug for Http3ServerStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http3ServerStream")
            .field("read_buf_len", &self.read_buf.len())
            .finish()
    }
}

// SAFETY: Http3ServerStream is Send because h3's RequestStream is Send
unsafe impl Send for Http3ServerStream {}

// ============================================================================
// Http3ClientStream - For client-side h3 request streams
// ============================================================================

/// Wrapper around h3 client request stream that implements AsyncRead + AsyncWrite
pub struct Http3ClientStream {
    stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    read_buf: BytesMut,
}

impl Http3ClientStream {
    /// Create a new Http3ClientStream
    pub fn new(stream: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(64 * 1024),
        }
    }
}

impl AsyncRead for Http3ClientStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // First drain any buffered data
        if !this.read_buf.is_empty() {
            let to_copy = std::cmp::min(buf.remaining(), this.read_buf.len());
            buf.put_slice(&this.read_buf.split_to(to_copy));
            return Poll::Ready(Ok(()));
        }

        // Use a boxed future to work around borrow checker limitations
        let mut fut = Box::pin(this.stream.recv_data());

        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(Some(mut data))) => {
                // data is impl Buf, use Buf trait methods
                let data_len = data.remaining();
                let to_copy = std::cmp::min(buf.remaining(), data_len);

                // Copy data to output buffer
                let chunk = data.copy_to_bytes(to_copy);
                buf.put_slice(&chunk);

                // Buffer any remaining data
                if data.has_remaining() {
                    while data.has_remaining() {
                        this.read_buf.extend_from_slice(data.chunk());
                        let len = data.chunk().len();
                        data.advance(len);
                    }
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(None)) => {
                // Stream finished
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Http3ClientStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let data = Bytes::copy_from_slice(buf);
        let fut = self.stream.send_data(data);
        tokio::pin!(fut);

        match fut.poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fut = self.stream.finish();
        tokio::pin!(fut);

        match fut.poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::other(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl std::fmt::Debug for Http3ClientStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http3ClientStream")
            .field("read_buf_len", &self.read_buf.len())
            .finish()
    }
}

// SAFETY: Http3ClientStream is Send because h3's RequestStream is Send
unsafe impl Send for Http3ClientStream {}

#[cfg(test)]
mod tests {
    #[test]
    fn test_http3stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<super::Http3Stream>();
        assert_send::<super::Http3ServerStream>();
        assert_send::<super::Http3ClientStream>();
    }
}
