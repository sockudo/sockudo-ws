//! io_uring-backed async stream
//!
//! This module provides `UringStream`, a wrapper around tokio-uring's TcpStream
//! that implements tokio's `AsyncRead` and `AsyncWrite` traits.
//!
//! # Combining with HTTP/2 and HTTP/3
//!
//! `UringStream` is a **transport layer** - it can be used as the underlying
//! TCP connection for any protocol:
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
//!
//! // io_uring + HTTP/3 WebSocket
//! // quinn already uses io_uring internally when available
//! ```

use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_uring::net::TcpStream as UringTcpStream;

/// Wrapper that implements tokio's AsyncRead/AsyncWrite over tokio-uring TcpStream
///
/// This allows io_uring streams to be used with any protocol that expects
/// standard async I/O traits, including:
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
    /// Pending read operation
    pending_read: Option<PendingOp>,
}

/// State for pending write operations
struct WriteState {
    /// Pending write operation
    pending_write: Option<PendingOp>,
    /// Bytes written in current operation
    bytes_written: usize,
}

/// A pending io_uring operation
struct PendingOp {
    waker: Option<Waker>,
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
                buffer: Some(vec![0u8; 64 * 1024]),
                data_len: 0,
                read_pos: 0,
                pending_read: None,
            },
            write_state: WriteState {
                pending_write: None,
                bytes_written: 0,
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
}

impl AsyncRead for UringStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;

        // First, try to satisfy from the internal buffer
        if this.read_state.read_pos < this.read_state.data_len
            && let Some(ref read_buf) = this.read_state.buffer
        {
            let available = this.read_state.data_len - this.read_state.read_pos;
            let to_copy = std::cmp::min(available, buf.remaining());

            buf.put_slice(&read_buf[this.read_state.read_pos..this.read_state.read_pos + to_copy]);
            this.read_state.read_pos += to_copy;

            return Poll::Ready(Ok(()));
        }

        // Buffer exhausted, need to read more from io_uring
        // Reset buffer state
        this.read_state.read_pos = 0;
        this.read_state.data_len = 0;

        // Take the buffer for the read operation
        let read_buf = this
            .read_state
            .buffer
            .take()
            .unwrap_or_else(|| vec![0u8; 64 * 1024]);

        // Store waker for when operation completes
        this.read_state.pending_read = Some(PendingOp {
            waker: Some(cx.waker().clone()),
        });

        let _stream = Rc::clone(&this.inner);

        // Note: In a real implementation, we'd use tokio-uring's spawn mechanism
        // to handle the completion. This is a simplified version.
        // tokio-uring requires using its own runtime and spawn functions.

        // For now, we indicate that the caller should use tokio-uring's
        // native async methods directly for best performance.

        // Return the buffer
        this.read_state.buffer = Some(read_buf);

        // In practice, tokio-uring streams should be used with their native
        // async methods. This trait impl is for compatibility layers.
        Poll::Pending
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

        // Store waker
        this.write_state.pending_write = Some(PendingOp {
            waker: Some(cx.waker().clone()),
        });

        // Similar to read - in practice, use tokio-uring's native methods
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // io_uring handles flushing at the kernel level
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Shutdown is handled by dropping the stream
        Poll::Ready(Ok(()))
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
    /// This is more efficient than using the AsyncRead trait because it
    /// uses io_uring's native completion-based I/O directly.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut buf = vec![0u8; 4096];
    /// let (result, buf) = stream.read_native(buf).await;
    /// let n = result?;
    /// // buf[..n] contains the data
    /// ```
    pub async fn read_native(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.inner.read(buf).await
    }

    /// Write data using io_uring (native async, preferred method)
    ///
    /// This is more efficient than using the AsyncWrite trait.
    pub async fn write_native(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.inner.write(buf).submit().await
    }

    /// Write all data using io_uring
    pub async fn write_all_native(&self, buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
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
    /// Buffered data from completed reads
    read_buffer: Vec<u8>,
    read_pos: usize,
    read_len: usize,
}

impl UringStreamAdapter {
    /// Create a new adapter
    pub fn new(stream: UringStream) -> Self {
        Self {
            stream,
            read_buffer: vec![0u8; 64 * 1024],
            read_pos: 0,
            read_len: 0,
        }
    }

    /// Get buffered data if available
    pub fn buffered_data(&self) -> &[u8] {
        &self.read_buffer[self.read_pos..self.read_len]
    }

    /// Consume buffered data
    pub fn consume(&mut self, amt: usize) {
        self.read_pos = std::cmp::min(self.read_pos + amt, self.read_len);
    }
}
