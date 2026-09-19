//! io_uring-backed async stream
//!
//! This module provides `UringStream`, a buffered compatibility wrapper around
//! tokio-uring's `TcpStream` that implements Tokio's `AsyncRead` and
//! `AsyncWrite` traits.
//!
//! # Using io_uring as a TCP transport
//!
//! `UringStream` is a **transport layer** - it can be used as the underlying
//! TCP connection for TCP-based protocols:
//!
//! ```ignore
//! // io_uring + HTTP/1.1 WebSocket
//! let uring_stream = UringStream::new(tcp_stream);
//! let ws = WebSocketStream::server(uring_stream, config);
//!
//! // io_uring + HTTP/2 WebSocket
//! let uring_stream = UringStream::new(tcp_stream);
//! let tls_stream = tls_accept(uring_stream).await?;  // TLS over io_uring
//! server.serve(tls_stream, handler).await?;          // H2 over TLS over io_uring
//! ```

use std::future::Future;
use std::io;
use std::net::Shutdown;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_uring::net::TcpStream as UringTcpStream;

const IO_BUFFER_SIZE: usize = 64 * 1024;

type ReadOperation = Pin<Box<dyn Future<Output = (io::Result<usize>, Vec<u8>)> + 'static>>;
type WriteOperation = Pin<Box<dyn Future<Output = (io::Result<()>, Vec<u8>)> + 'static>>;

/// Buffered Tokio `AsyncRead`/`AsyncWrite` bridge for tokio-uring TCP streams
///
/// The bridge copies between borrowed poll-based buffers and owned buffers used
/// by tokio-uring completion operations. This allows io_uring streams to be
/// used with protocols that expect standard async I/O traits, including:
/// - Direct WebSocket (HTTP/1.1)
/// - HTTP/2 (via h2 crate)
/// - TLS (via tokio-rustls with io_uring support)
///
/// # Example: io_uring + HTTP/2 WebSocket
///
/// ```ignore
/// use sockudo_ws::io_uring::UringStream;
/// use sockudo_ws::http2::H2WebSocketServer;
///
/// #[tokio_uring::main]
/// async fn main() {
///     let listener = tokio_uring::net::TcpListener::bind(addr)?;
///     let server = H2WebSocketServer::new(Config::default());
///
///     loop {
///         let (tcp_stream, _) = listener.accept().await?;
///
///         // Wrap TCP in UringStream for io_uring I/O
///         let uring_stream = UringStream::new(tcp_stream);
///
///         // Add TLS (required for HTTP/2)
///         let tls_stream = tls_acceptor.accept(uring_stream).await?;
///
///         // HTTP/2 WebSocket server uses io_uring transport!
///         tokio_uring::spawn(async move {
///             server.serve(tls_stream, |ws, req| async move {
///                 // Handle WebSocket over HTTP/2 over TLS over io_uring
///             }).await.ok();
///         });
///     }
/// }
/// ```
pub struct UringStream {
    /// The underlying tokio-uring TCP stream
    inner: Rc<UringTcpStream>,
    /// Read buffer for bridging completion-based to poll-based I/O
    read_state: ReadState,
    /// Write state
    write_state: WriteState,
}

/// State for pending read operations
struct ReadState {
    /// Buffer for pending read data
    buffer: Option<Vec<u8>>,
    /// Amount of valid data in buffer
    data_len: usize,
    /// Current read position
    read_pos: usize,
    /// EOF is permanent for the socket; do not resubmit completed reads.
    eof: bool,
    /// Pending read operation
    pending_read: Option<ReadOperation>,
}

/// State for pending write operations
struct WriteState {
    /// Data accepted by `poll_write` and waiting to be flushed
    buffer: Option<Vec<u8>>,
    /// Pending flush operation
    pending_write: Option<WriteOperation>,
}

impl UringStream {
    /// Create a new UringStream from a tokio-uring TcpStream
    ///
    /// This wraps the io_uring-based stream to provide standard AsyncRead/AsyncWrite
    /// traits, allowing it to work with any async protocol implementation.
    pub fn new(stream: UringTcpStream) -> Self {
        Self {
            inner: Rc::new(stream),
            read_state: ReadState {
                buffer: Some(vec![0u8; IO_BUFFER_SIZE]),
                data_len: 0,
                read_pos: 0,
                pending_read: None,
                eof: false,
            },
            write_state: WriteState {
                buffer: Some(Vec::with_capacity(IO_BUFFER_SIZE)),
                pending_write: None,
            },
        }
    }

    /// Create from a standard TcpStream (converts to io_uring)
    ///
    /// This is useful when you have an existing std::net::TcpStream
    /// and want to use it with io_uring.
    pub fn from_std(stream: std::net::TcpStream) -> io::Result<Self> {
        let uring_stream = UringTcpStream::from_std(stream);
        Ok(Self::new(uring_stream))
    }

    /// Get a reference to the underlying tokio-uring stream
    pub fn get_ref(&self) -> &UringTcpStream {
        &self.inner
    }

    /// Check if there's buffered data available for reading
    pub fn has_buffered_data(&self) -> bool {
        self.read_state.read_pos < self.read_state.data_len
    }

    fn poll_flush_buffer(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.write_state.pending_write.is_none() {
            let buffer = self
                .write_state
                .buffer
                .as_ref()
                .expect("write buffer must be available without a pending operation");
            if buffer.is_empty() {
                return Poll::Ready(Ok(()));
            }

            let buffer = self
                .write_state
                .buffer
                .take()
                .expect("write buffer must be available without a pending operation");
            let stream = Rc::clone(&self.inner);
            self.write_state.pending_write =
                Some(Box::pin(async move { stream.write_all(buffer).await }));
        }

        let operation = self
            .write_state
            .pending_write
            .as_mut()
            .expect("flush must have a pending operation");
        match operation.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready((result, mut buffer)) => {
                self.write_state.pending_write = None;
                buffer.clear();
                self.write_state.buffer = Some(buffer);
                Poll::Ready(result)
            }
        }
    }
}

impl AsyncRead for UringStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;

        if buf.remaining() == 0 || this.read_state.eof {
            return Poll::Ready(Ok(()));
        }

        loop {
            // First, try to satisfy from the internal buffer
            if this.read_state.read_pos < this.read_state.data_len {
                let read_buf = this
                    .read_state
                    .buffer
                    .as_ref()
                    .expect("read buffer must be available after completion");
                let available = this.read_state.data_len - this.read_state.read_pos;
                let to_copy = std::cmp::min(available, buf.remaining());

                buf.put_slice(
                    &read_buf[this.read_state.read_pos..this.read_state.read_pos + to_copy],
                );
                this.read_state.read_pos += to_copy;
                return Poll::Ready(Ok(()));
            }

            if this.read_state.pending_read.is_none() {
                this.read_state.read_pos = 0;
                this.read_state.data_len = 0;
                let read_buf = this
                    .read_state
                    .buffer
                    .take()
                    .expect("read buffer must be available without a pending operation");
                let stream = Rc::clone(&this.inner);
                this.read_state.pending_read =
                    Some(Box::pin(async move { stream.read(read_buf).await }));
            }

            let operation = this
                .read_state
                .pending_read
                .as_mut()
                .expect("read must have a pending operation");
            match operation.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready((result, read_buf)) => {
                    this.read_state.pending_read = None;
                    this.read_state.buffer = Some(read_buf);
                    match result {
                        Ok(0) => {
                            this.read_state.eof = true;
                            return Poll::Ready(Ok(()));
                        }
                        Ok(read) => this.read_state.data_len = read,
                        Err(error) => return Poll::Ready(Err(error)),
                    }
                }
            }
        }
    }
}

impl AsyncWrite for UringStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if this.write_state.pending_write.is_some() {
            ready!(this.poll_flush_buffer(cx))?;
        }

        let write_buf = this
            .write_state
            .buffer
            .as_mut()
            .expect("write buffer must be available without a pending operation");
        if write_buf.len() == write_buf.capacity() {
            ready!(this.poll_flush_buffer(cx))?;
        }

        let write_buf = this
            .write_state
            .buffer
            .as_mut()
            .expect("write buffer must be available without a pending operation");
        let written = std::cmp::min(buf.len(), write_buf.capacity() - write_buf.len());
        write_buf.extend_from_slice(&buf[..written]);
        Poll::Ready(Ok(written))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush_buffer(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_flush_buffer(cx))?;
        Poll::Ready(self.inner.shutdown(Shutdown::Write))
    }
}

impl std::fmt::Debug for UringStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UringStream")
            .field("has_buffered_data", &self.has_buffered_data())
            .finish()
    }
}

// ============================================================================
// Native io_uring async methods (preferred over trait impls)
// ============================================================================

impl UringStream {
    /// Read data using io_uring (native async, preferred method)
    ///
    /// This avoids the bridge's copy when no earlier poll-based read is pending
    /// or buffered. Any bytes already read by the bridge are returned first.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut buf = vec![0u8; 4096];
    /// let (result, buf) = stream.read_native(buf).await;
    /// let n = result?;
    /// // buf[..n] contains the data
    /// ```
    pub async fn read_native(&mut self, mut buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        if self.has_buffered_data() || self.read_state.pending_read.is_some() || self.read_state.eof
        {
            let initialized = buf.len();
            buf.clear();
            let mut read_buf = ReadBuf::uninit(buf.spare_capacity_mut());
            let result =
                std::future::poll_fn(|cx| Pin::new(&mut *self).poll_read(cx, &mut read_buf)).await;
            let filled = read_buf.filled().len();
            // SAFETY: the old prefix was initialized before clear, and ReadBuf
            // guarantees initialization of its filled prefix.
            unsafe {
                buf.set_len(initialized.max(filled));
            }
            return (result.map(|()| filled), buf);
        }
        self.inner.read(buf).await
    }

    /// Write data using io_uring (native async, preferred method)
    ///
    /// This avoids the buffering and copy required by the poll-based
    /// compatibility bridge after flushing any earlier poll-based writes.
    pub async fn write_native(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        if let Err(error) = std::future::poll_fn(|cx| self.poll_flush_buffer(cx)).await {
            return (Err(error), buf);
        }
        self.inner.write(buf).submit().await
    }

    /// Write all data using io_uring, after flushing earlier poll-based writes.
    pub async fn write_all_native(&mut self, buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        if let Err(error) = std::future::poll_fn(|cx| self.poll_flush_buffer(cx)).await {
            return (Err(error), buf);
        }
        self.inner.write_all(buf).await
    }
}

// ============================================================================
// Helper for using UringStream with HTTP/2
// ============================================================================

/// Adapter for using UringStream with protocols expecting poll-based I/O
///
/// This provides a bridge between io_uring's completion-based model and
/// the poll-based AsyncRead/AsyncWrite traits used by most Rust async code.
///
/// For best performance, use the native async methods when possible.
pub struct UringStreamAdapter {
    stream: UringStream,
}

impl UringStreamAdapter {
    /// Create a new adapter
    pub fn new(stream: UringStream) -> Self {
        Self { stream }
    }

    /// Get buffered data if available
    pub fn buffered_data(&self) -> &[u8] {
        let state = &self.stream.read_state;
        state
            .buffer
            .as_ref()
            .map(|buffer| &buffer[state.read_pos..state.data_len])
            .unwrap_or_default()
    }

    /// Consume buffered data
    pub fn consume(&mut self, amt: usize) {
        let state = &mut self.stream.read_state;
        state.read_pos += amt.min(state.data_len - state.read_pos);
    }
}

impl AsyncRead for UringStreamAdapter {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for UringStreamAdapter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
